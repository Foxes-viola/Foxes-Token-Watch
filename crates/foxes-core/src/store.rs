//! 记录文件的增量扫描：发现文件、只解析有变化的部分，并顺带读取各 agent 的运行登记。

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::lenient::{u64_lenient, Text, TimeMs};
use crate::live::Processes;
use crate::model::{Agent, RateLimits};
use crate::sources::baishitong::Feed;
use crate::sources::{self, aux_path, discover, FileEntry, FileSig, Recorded, RecordedCard, Roots, ScanCtx};
use crate::timeutil::{now_ms, DAY, MINUTE, SECOND};

const DISCOVER_EVERY_MS: i64 = 5 * SECOND;
/// 每隔多少次目录扫描，对全部文件做一次精确的大小 / 时间核对。
/// （目录项里的大小和时间对正被写入的文件可能滞后，精确核对要逐个打开文件。）
const DEEP_EVERY: u64 = 12;
const PROCESSES_EVERY_MS: i64 = 3 * SECOND;
/// 最近改动过的文件每次刷新都核对
const HOT_MS: i64 = 30 * MINUTE;
/// 近几天改动过的文件每次目录扫描都核对
const WARM_MS: i64 = 3 * DAY;
const MAX_THREADS: usize = 8;

/// Claude Code 登记的一个运行中进程（`~/.claude/sessions/<pid>.json`）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClaudeProc {
    pub pid: u32,
    pub session_id: String,
    /// 小写；见过的值有 `busy`
    pub status: String,
    pub status_updated_ms: i64,
    pub updated_ms: i64,
    pub started_ms: i64,
    pub name: String,
    pub cwd: String,
    pub kind: String,
    pub entrypoint: String,
    pub version: String,
    /// 进程是否还在（进程列表不可用时按存活算）
    pub alive: bool,
}

#[derive(Deserialize)]
struct RawClaudeProc<'a> {
    #[serde(default, deserialize_with = "u64_lenient")]
    pid: u64,
    #[serde(rename = "sessionId", borrow, default)]
    session_id: Text<'a>,
    #[serde(borrow, default)]
    status: Text<'a>,
    #[serde(rename = "statusUpdatedAt", default)]
    status_updated_at: TimeMs,
    #[serde(rename = "updatedAt", default)]
    updated_at: TimeMs,
    #[serde(rename = "startedAt", default)]
    started_at: TimeMs,
    #[serde(borrow, default)]
    name: Text<'a>,
    #[serde(borrow, default)]
    cwd: Text<'a>,
    #[serde(borrow, default)]
    kind: Text<'a>,
    #[serde(borrow, default)]
    entrypoint: Text<'a>,
    #[serde(borrow, default)]
    version: Text<'a>,
}

/// 只取 pid，文件里的其他字段（包括令牌）不解析、不保留。
#[derive(Deserialize)]
struct RawHub {
    #[serde(default, deserialize_with = "u64_lenient")]
    pid: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanStats {
    /// 用量历史（`history.json`）读不出来或写不进去时的原因
    pub history_error: Option<String>,
    /// 用量历史的概况：本机记了多少段会话，别的电脑带过来多少
    pub history: crate::history::HistoryInfo,
    pub files: usize,
    pub bytes: u64,
    pub events: usize,
    /// 当前读不出来的文件数
    pub failing: usize,
    /// 最近一次刷新解析了多少个文件、花了多久
    pub parsed: usize,
    pub scan_ms: u64,
    pub first_scan_ms: u64,
    pub scans: u64,
    /// 最近一次汇总（把全部请求统计成界面上的数）花了多久；由调用汇总的一方填
    pub analysis_ms: u64,
    /// 统计线程出错后自动重来的次数，和最近一次出错的位置、原因；由应用填
    pub monitor_restarts: u32,
    pub monitor_error: Option<String>,
}

pub struct Store {
    roots: Roots,
    files: HashMap<PathBuf, FileEntry>,
    antigravity_models: Mutex<HashMap<u64, String>>,
    processes: Processes,
    processes_at_ms: i64,
    /// 上次快照时已确认不在的登记 pid（避免为它们反复重拍快照）
    dead_pids: HashSet<u32>,
    claude_procs: Vec<ClaudeProc>,
    claude_registry: bool,
    antigravity_hubs: Vec<u32>,
    /// Kiro 客户端缓存的账户额度，按数据库文件记下签名，文件没变就不重读
    kiro_limits: HashMap<PathBuf, (FileSig, Option<RateLimits>)>,
    /// 用量历史里以前的运行记下的插件卡片（见 [`ScanCtx::recorded`]）
    recorded: Recorded,
    /// 百事通（客户版）插件对话的来源；没打开就不读
    baishitong: Option<Feed>,
    last_discover_ms: i64,
    discovers: u64,
    version: u64,
    stats: ScanStats,
    /// 来源已经不在、由用量历史顶上的会话（见 [`crate::history`]）
    restored: Vec<FileEntry>,
}

fn precise_sig(agent: Agent, path: &Path) -> Option<FileSig> {
    let sig = FileSig::of(path)?;
    Some(match aux_path(agent, path) {
        Some(aux) => sig.with_aux(&aux),
        None => sig,
    })
}

fn is_due(entry: &FileEntry, now: i64) -> bool {
    (entry.recheck_at_ms > 0 && now >= entry.recheck_at_ms) || (entry.retry_at_ms > 0 && now >= entry.retry_at_ms)
}

fn apply(entry: &mut FileEntry, sig: FileSig, ctx: &ScanCtx) {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let rewritten = sig.size == entry.sig.size && sig.mtime_ms != entry.sig.mtime_ms;
        if !matches!(entry.agent, Agent::Antigravity | Agent::Cursor) && (sig.size < entry.sig.size || sig.size < entry.offset || rewritten) {
            // 文件被截断或重写
            entry.reset();
        }
        sources::update(entry, ctx)
    }));
    match outcome {
        Ok(Ok(())) => {
            entry.errors = 0;
            entry.last_error = None;
            entry.retry_at_ms = 0;
        }
        Ok(Err(error)) => {
            entry.errors = entry.errors.saturating_add(1);
            entry.last_error = Some(error.to_string());
            let backoff = (2 * SECOND << entry.errors.min(5)).min(MINUTE);
            entry.retry_at_ms = ctx.now_ms + backoff;
        }
        Err(_) => {
            // 解析器自身出错：丢弃这个文件的半成品结果，隔一阵再试，不影响其他文件
            entry.reset();
            entry.corrupt = true;
            entry.errors = 100;
            entry.last_error = Some("解析器异常，等待重试".into());
            entry.retry_at_ms = ctx.now_ms + 10 * MINUTE;
        }
    }
    entry.sig = sig;
}

fn small_json(path: &Path, limit: u64) -> Option<Vec<u8>> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > limit {
        return None;
    }
    fs::read(path).ok()
}

fn read_claude_registry(roots: &Roots) -> (bool, Vec<ClaudeProc>) {
    let mut seen = false;
    let mut procs = Vec::new();
    for root in &roots.claude {
        let Ok(entries) = fs::read_dir(root.join("sessions")) else {
            continue;
        };
        seen = true;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Some(bytes) = small_json(&path, 256 * 1024) else {
                continue;
            };
            let Ok(raw) = serde_json::from_slice::<RawClaudeProc>(&bytes) else {
                continue;
            };
            let mut pid = raw.pid.min(u32::MAX as u64) as u32;
            if pid == 0 {
                pid = path.file_stem().and_then(|stem| stem.to_str()).and_then(|stem| stem.parse().ok()).unwrap_or(0);
            }
            if pid == 0 || raw.session_id.is_empty() {
                continue;
            }
            procs.push(ClaudeProc {
                pid,
                session_id: raw.session_id.as_str().to_owned(),
                status: raw.status.as_str().to_ascii_lowercase(),
                status_updated_ms: raw.status_updated_at.0,
                updated_ms: raw.updated_at.0,
                started_ms: raw.started_at.0,
                name: raw.name.as_str().to_owned(),
                cwd: raw.cwd.as_str().to_owned(),
                kind: raw.kind.as_str().to_owned(),
                entrypoint: raw.entrypoint.as_str().to_owned(),
                version: raw.version.as_str().to_owned(),
                alive: true,
            });
        }
    }
    procs.sort_by(|a, b| a.pid.cmp(&b.pid));
    (seen, procs)
}

fn read_antigravity_hubs(roots: &Roots) -> Vec<u32> {
    let mut pids = Vec::new();
    for root in &roots.antigravity {
        let Ok(entries) = fs::read_dir(root.join("daemon")) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !(name.starts_with("hub_") && name.ends_with(".json")) {
                continue;
            }
            let Some(bytes) = small_json(&entry.path(), 64 * 1024) else {
                continue;
            };
            if let Ok(hub) = serde_json::from_slice::<RawHub>(&bytes) {
                if hub.pid > 0 && hub.pid <= u32::MAX as u64 {
                    pids.push(hub.pid as u32);
                }
            }
        }
    }
    pids.sort_unstable();
    pids.dedup();
    pids
}

impl Store {
    pub fn new(roots: Roots) -> Store {
        Store {
            roots,
            files: HashMap::new(),
            antigravity_models: Mutex::new(HashMap::new()),
            processes: Processes::default(),
            processes_at_ms: 0,
            dead_pids: HashSet::new(),
            claude_procs: Vec::new(),
            claude_registry: false,
            antigravity_hubs: Vec::new(),
            kiro_limits: HashMap::new(),
            recorded: Recorded::default(),
            baishitong: None,
            last_discover_ms: 0,
            discovers: 0,
            version: 0,
            stats: ScanStats::default(),
            restored: Vec::new(),
        }
    }

    pub fn roots(&self) -> &Roots {
        &self.roots
    }

    /// 开始读百事通（客户版）插件的对话。它们存在插件的服务端，经插件在本机开的转发口读
    /// （只连本机端口、只读不写，见 [`sources::baishitong`]）；不调用就不会发任何请求。
    pub fn watch_baishitong(&mut self) {
        if self.baishitong.is_none() {
            self.baishitong = Some(Feed::start());
        }
    }

    /// 百事通的对话读得怎么样（给界面看）；没打开这个来源时为 `None`。
    pub fn baishitong_status(&self) -> Option<crate::sources::baishitong::Status> {
        self.baishitong.as_ref().map(Feed::status)
    }

    /// 用量历史里以前的运行记下的插件卡片。
    pub fn recorded(&self) -> &Recorded {
        &self.recorded
    }

    /// 用量历史读进来之后交进来一次：卡片的原始记录不落盘，以前记下的请求照这一份算（键是卡片目录名）。
    pub fn set_recorded(&mut self, cards: HashMap<String, RecordedCard>) {
        if cards.is_empty() && self.recorded.cards.is_empty() {
            return;
        }
        self.recorded.cards = cards;
        self.recorded.rev += 1;
    }

    /// 换一组数据目录（比如运行期间新装了某个 agent）。有变化时返回 true，下一次刷新会马上重新扫一遍目录。
    pub fn set_roots(&mut self, roots: Roots) -> bool {
        if roots == self.roots {
            return false;
        }
        self.roots = roots;
        self.last_discover_ms = 0;
        true
    }

    /// 数据每变化一次加一。
    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn stats(&self) -> &ScanStats {
        &self.stats
    }

    /// 全部会话：扫描到的，加上来源已经不在、由用量历史顶上的。
    pub fn entries(&self) -> impl Iterator<Item = &FileEntry> {
        self.scanned().chain(self.restored.iter())
    }

    /// 换一批由用量历史顶上的会话。
    pub fn set_restored(&mut self, entries: Vec<FileEntry>) {
        self.restored = entries;
        self.version += 1;
        self.stats.events = self.entries().map(|entry| entry.events.len()).sum();
    }

    fn scanned(&self) -> impl Iterator<Item = &FileEntry> {
        self.files.values().flat_map(|file| {
            Some(file).filter(|f| f.agent != Agent::Cursor).into_iter().chain(file.extra.iter())
        })
    }

    pub fn file_counts(&self) -> [u32; Agent::COUNT] {
        let mut counts = [0; Agent::COUNT];
        for file in self.files.values().filter(|f| !f.gone) { counts[file.agent.index()] += 1; }
        counts
    }

    pub fn source_files(&self) -> impl Iterator<Item = &FileEntry> {
        self.files.values()
    }

    pub fn processes(&self) -> &Processes {
        &self.processes
    }

    pub fn claude_procs(&self) -> &[ClaudeProc] {
        &self.claude_procs
    }

    /// Claude 的运行登记目录是否存在（旧版本没有）。
    pub fn claude_registry(&self) -> bool {
        self.claude_registry
    }

    pub fn antigravity_hubs(&self) -> &[u32] {
        &self.antigravity_hubs
    }

    /// Kiro 账户额度的最新一份读数。
    pub fn kiro_limits(&self) -> Option<&RateLimits> {
        self.kiro_limits.values().filter_map(|(_, limits)| limits.as_ref()).max_by_key(|limits| limits.ts)
    }

    /// 重读有变化的 Kiro 客户端数据库里的额度；返回是否有变化。
    fn refresh_kiro_limits(&mut self) -> bool {
        let mut changed = false;
        let mut present = HashSet::new();
        for root in &self.roots.kiro {
            let path = root.join("state.vscdb");
            let mut wal = path.clone().into_os_string();
            wal.push("-wal");
            let Some(sig) = FileSig::of(&path).map(|sig| sig.with_aux(Path::new(&wal))) else {
                continue;
            };
            present.insert(path.clone());
            if self.kiro_limits.get(&path).is_some_and(|(known, _)| *known == sig) {
                continue;
            }
            let limits = sources::kiro::read_usage_limits(&path);
            self.kiro_limits.insert(path, (sig, limits));
            changed = true;
        }
        let before = self.kiro_limits.len();
        self.kiro_limits.retain(|path, _| present.contains(path));
        changed || self.kiro_limits.len() != before
    }

    pub fn refresh(&mut self) -> bool {
        self.refresh_at(now_ms())
    }

    /// 扫一遍：返回数据是否有变化。适合每秒调用一次，完整的目录扫描会自动降频。
    pub fn refresh_at(&mut self, now: i64) -> bool {
        let started = Instant::now();
        let mut changed = false;
        let mut work: HashMap<PathBuf, FileSig> = HashMap::new();

        let full = self.last_discover_ms == 0 || now < self.last_discover_ms || now - self.last_discover_ms >= DISCOVER_EVERY_MS;
        if full {
            self.last_discover_ms = now;
            self.discovers += 1;
            let deep = self.discovers % DEEP_EVERY == 1;
            let found = discover(&self.roots);
            let mut present: HashSet<PathBuf> = HashSet::with_capacity(found.len());
            for item in found {
                match self.files.get_mut(&item.path) {
                    None => {
                        let sig = precise_sig(item.agent, &item.path).unwrap_or(item.sig);
                        self.files.insert(item.path.clone(), FileEntry::new(item.agent, item.path.clone()));
                        work.insert(item.path.clone(), sig);
                    }
                    Some(entry) => {
                        if entry.gone {
                            entry.gone = false;
                            changed = true;
                        }
                        let due = is_due(entry, now);
                        let warm = now - entry.sig.latest_mtime() < WARM_MS;
                        if due || deep || warm || item.sig != entry.sig {
                            if let Some(sig) = precise_sig(entry.agent, &entry.path) {
                                if due || sig != entry.sig {
                                    work.insert(item.path.clone(), sig);
                                }
                            }
                        }
                    }
                }
                present.insert(item.path);
            }

            let mut lost = false;
            for (path, entry) in self.files.iter_mut() {
                if !entry.gone && !present.contains(path) {
                    entry.gone = true;
                    entry.live.turn_open = false;
                    lost = true;
                }
            }
            if lost || self.files.values().any(|entry| entry.agent == Agent::Codex && entry.gone) {
                changed |= lost;
                // Codex 归档会把记录文件挪到 archived_sessions：同名文件在别处出现时，丢掉旧位置的那份，避免重复统计
                let names: HashSet<String> = self
                    .files
                    .values()
                    .filter(|entry| entry.agent == Agent::Codex && !entry.gone)
                    .filter_map(|entry| sources::codex::rollout_name(&entry.path).map(str::to_owned))
                    .collect();
                self.files.retain(|_, entry| {
                    !(entry.agent == Agent::Codex && entry.gone && sources::codex::rollout_name(&entry.path).is_some_and(|name| names.contains(name)))
                });
            }
            self.antigravity_hubs = read_antigravity_hubs(&self.roots);
            changed |= self.refresh_kiro_limits();
        } else {
            for entry in self.files.values() {
                if entry.gone {
                    continue;
                }
                let due = is_due(entry, now);
                if !(due || entry.live.turn_open || now - entry.sig.latest_mtime() < HOT_MS) {
                    continue;
                }
                if let Some(sig) = precise_sig(entry.agent, &entry.path) {
                    if due || sig != entry.sig {
                        work.insert(entry.path.clone(), sig);
                    }
                }
            }
        }

        let parsed = work.len();
        if parsed > 0 {
            changed = true;
            let ctx = ScanCtx {
                now_ms: now,
                antigravity_models: &self.antigravity_models,
                recorded: Some(&self.recorded),
                baishitong: self.baishitong.as_ref(),
            };
            let mut jobs: Vec<(&mut FileEntry, FileSig)> =
                self.files.iter_mut().filter_map(|(path, entry)| work.get(path).map(|sig| (entry, *sig))).collect();
            // 从队尾取：大文件先解析，各线程负载更均衡
            jobs.sort_by_key(|(_, sig)| sig.size);
            let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(MAX_THREADS).min(jobs.len());
            if threads <= 1 {
                for (entry, sig) in jobs {
                    apply(entry, sig, &ctx);
                }
            } else {
                let queue = Mutex::new(jobs);
                std::thread::scope(|scope| {
                    for _ in 0..threads {
                        scope.spawn(|| loop {
                            let job = queue.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).pop();
                            let Some((entry, sig)) = job else {
                                break;
                            };
                            apply(entry, sig, &ctx);
                        });
                    }
                });
            }

            // 并行解析时，有的文件里查不到的模型名可能在别的文件里查到了：统一补上
            let names = self.antigravity_models.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if !names.is_empty() {
                for entry in self.files.values_mut().filter(|entry| entry.agent == Agent::Antigravity) {
                    sources::antigravity::rename_placeholders(&mut entry.models, &names);
                }
            }
        }

        // 运行登记与进程快照
        let (registry, mut procs) = read_claude_registry(&self.roots);
        let stale = self.processes_at_ms == 0 || now < self.processes_at_ms || now - self.processes_at_ms >= PROCESSES_EVERY_MS;
        let unseen = procs.iter().any(|proc| self.processes.alive(proc.pid) == Some(false) && !self.dead_pids.contains(&proc.pid));
        if stale || unseen {
            self.processes = Processes::snapshot();
            self.processes_at_ms = now;
            self.dead_pids = procs.iter().filter(|proc| self.processes.alive(proc.pid) == Some(false)).map(|proc| proc.pid).collect();
        }
        for proc in &mut procs {
            proc.alive = self.processes.alive(proc.pid).unwrap_or(true);
        }
        if registry != self.claude_registry || procs != self.claude_procs {
            changed = true;
            self.claude_registry = registry;
            self.claude_procs = procs;
        }

        if changed {
            self.version += 1;
        }
        if full || parsed > 0 {
            self.stats.files = self.files.len();
            self.stats.bytes = self.files.values().map(|entry| entry.sig.size + entry.sig.aux_size).sum();
            self.stats.events = self.entries().map(|entry| entry.events.len()).sum();
            self.stats.failing = self.files.values().filter(|entry| entry.errors > 0 || entry.corrupt).count();
        }
        self.stats.parsed = parsed;
        self.stats.scan_ms = started.elapsed().as_millis() as u64;
        self.stats.scans += 1;
        if self.stats.scans == 1 {
            self.stats.first_scan_ms = self.stats.scan_ms;
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("foxes-core-store-{tag}-{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    const CODEX_META: &str = r#"{"timestamp":"2026-01-01T00:00:00.000Z","type":"session_meta","payload":{"id":"s1","cwd":"C:\\work\\app","cli_version":"0.1.0","source":"vscode"}}"#;

    fn codex_record(second: u32, response: &str, input: u64, total: u64) -> String {
        format!(
            r#"{{"timestamp":"2026-01-01T00:00:{second:02}.000Z","type":"token_usage_record","payload":{{"response_id":"{response}","usage":{{"input_tokens":{input},"cached_input_tokens":0,"output_tokens":10,"reasoning_output_tokens":0,"total_tokens":{}}},"thread_token_usage":{{"total_tokens":{total}}}}}}}"#,
            input + 10
        )
    }

    #[test]
    fn same_length_rewrite_replaces_previous_usage() {
        let root = temp_root("equal-length");
        let sessions = root.join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        let path = sessions.join("test.jsonl");
        let mut store = Store::new(Roots { codex: vec![root.clone()], ..Default::default() });
        let now = now_ms();
        for (index, input) in [100, 200].into_iter().enumerate() {
            fs::write(&path, codex_record(1, "r1", input, input + 10)).unwrap();
            // 显式推进时间戳，避免测试取决于文件系统的时间精度。
            let timestamp = std::time::SystemTime::now() + std::time::Duration::from_secs(index as u64 * 5);
            fs::File::options().write(true).open(&path).unwrap().set_modified(timestamp).unwrap();
            store.refresh_at(now + index as i64 * 1000);
            let entry = store.entries().next().unwrap();
            assert_eq!(entry.events.len(), 1);
            assert_eq!(entry.events[0].usage.total(), input + 10);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn compressed_archives_replace_plain_rollouts_without_recounting() {
        let root = temp_root("codex-compression");
        let dir = root.join("sessions");
        let archive = root.join("archived_sessions");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&archive).unwrap();
        let plain = dir.join("rollout-a.jsonl");
        let packed = archive.join("rollout-a.jsonl.zst");
        let bytes = format!("{CODEX_META}\n{}", codex_record(1, "", 123, 133));
        fs::write(&plain, &bytes).unwrap();
        let mut store = Store::new(Roots { codex: vec![root.clone()], ..Roots::default() });
        let t = now_ms();
        store.refresh_at(t);
        fs::write(&packed, zstd::stream::encode_all(bytes.as_bytes(), 1).unwrap()).unwrap();
        store.refresh_at(t + 10_000);
        assert_eq!(store.stats().events, 1);
        fs::remove_file(plain).unwrap();
        store.refresh_at(t + 20_000);
        let entries: Vec<_> = store.entries().collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, packed);
        assert_eq!(entries[0].events[0].usage.total(), 133);
        assert_eq!(store.stats().failing, 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scans_incrementally_and_survives_moves() {
        let root = temp_root("codex");
        let day = root.join("sessions").join("2026").join("01").join("01");
        fs::create_dir_all(&day).unwrap();
        let file = day.join("rollout-a.jsonl");
        fs::write(&file, format!("{CODEX_META}\n{}\n", codex_record(1, "r1", 100, 110))).unwrap();

        let roots = Roots { codex: vec![root.clone()], ..Roots::default() };
        let mut store = Store::new(roots);
        let t0 = now_ms();
        assert!(store.refresh_at(t0));
        assert_eq!(store.stats().files, 1);
        assert_eq!(store.stats().events, 1);
        let version = store.version();

        // 没有变化：版本不动
        store.refresh_at(t0 + 1_000);
        assert_eq!(store.stats().parsed, 0);
        assert_eq!(store.version(), version);

        // 追加一行：只解析新增部分
        let mut handle = fs::OpenOptions::new().append(true).open(&file).unwrap();
        writeln!(handle, "{}", codex_record(2, "r2", 200, 320)).unwrap();
        drop(handle);
        assert!(store.refresh_at(t0 + 2_000));
        assert_eq!(store.stats().events, 2);
        let entry = store.entries().next().unwrap();
        assert_eq!(entry.offset, fs::metadata(&file).unwrap().len());
        assert_eq!(entry.session.id, "s1");

        // 挪到归档目录：仍然只算一份
        let archive = root.join("archived_sessions");
        fs::create_dir_all(&archive).unwrap();
        fs::rename(&file, archive.join("rollout-a.jsonl")).unwrap();
        store.refresh_at(t0 + 10_000);
        assert_eq!(store.stats().files, 1);
        assert_eq!(store.stats().events, 2);
        assert!(store.entries().all(|entry| !entry.gone));

        // 删除：数字保留，标记为已删除
        fs::remove_file(archive.join("rollout-a.jsonl")).unwrap();
        store.refresh_at(t0 + 20_000);
        assert_eq!(store.stats().events, 2);
        assert!(store.entries().all(|entry| entry.gone));

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn reads_claude_registry_without_secrets() {
        let root = temp_root("claude");
        let sessions = root.join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        let pid = std::process::id();
        fs::write(
            sessions.join(format!("{pid}.json")),
            format!(r#"{{"pid":{pid},"sessionId":"abc","status":"Busy","statusUpdatedAt":1767225600000,"cwd":"C:\\work\\app","name":"demo"}}"#),
        )
        .unwrap();
        fs::write(sessions.join("1.json"), r#"{"pid":1,"sessionId":"","status":"busy"}"#).unwrap();
        fs::write(sessions.join(".key"), "not json").unwrap();

        let (seen, procs) = read_claude_registry(&Roots { claude: vec![root.clone()], ..Roots::default() });
        assert!(seen);
        assert_eq!(procs.len(), 1);
        assert_eq!(procs[0].pid, pid);
        assert_eq!(procs[0].status, "busy");
        assert_eq!(procs[0].session_id, "abc");
        assert_eq!(procs[0].status_updated_ms, 1_767_225_600_000);

        fs::remove_dir_all(&root).ok();
    }
}
