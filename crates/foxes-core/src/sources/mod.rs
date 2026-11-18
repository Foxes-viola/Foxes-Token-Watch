//! 各家 agent 的记录解析。每个记录文件对应一个 [`FileEntry`]，只追加解析新增的部分。

pub mod antigravity;
pub mod baishitong;
pub mod claude;
pub mod codex;
pub mod cursor;
mod cursor_executor;
mod cursor_gateway;
mod cursor_host;
pub mod deepseek;
pub mod kiro;

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};

use crate::model::{Agent, LiveTrack, NetEvent, RateLimits, SessionMeta, ToolCall, UsageEvent};
use crate::timeutil::system_time_ms;

/// 文件的大小和修改时间，用来判断有没有新内容。`aux_*` 是伴生文件（SQLite 的 `-wal`）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSig {
    pub size: u64,
    pub mtime_ms: i64,
    pub aux_size: u64,
    pub aux_mtime_ms: i64,
}

impl FileSig {
    pub fn of(path: &Path) -> Option<FileSig> {
        let meta = fs::metadata(path).ok()?;
        Some(FileSig {
            size: meta.len(),
            mtime_ms: meta.modified().map(system_time_ms).unwrap_or(0),
            aux_size: 0,
            aux_mtime_ms: 0,
        })
    }

    pub fn with_aux(mut self, aux: &Path) -> FileSig {
        if let Some(sig) = FileSig::of(aux) {
            self.aux_size = sig.size;
            self.aux_mtime_ms = sig.mtime_ms;
        }
        self
    }

    pub fn latest_mtime(&self) -> i64 {
        self.mtime_ms.max(self.aux_mtime_ms)
    }
}

#[derive(Debug)]
pub enum ParserState {
    Claude(claude::State),
    Codex(codex::State),
    Deepseek(deepseek::State),
    Antigravity(antigravity::State),
    Kiro(kiro::State),
    Cursor(cursor::State),
    /// 旧版网关日志入口；Cursor 的 MCP 对话由 Cursor 解析器统一产出。
    None,
}

impl ParserState {
    fn new(agent: Agent) -> ParserState {
        match agent {
            Agent::Claude => ParserState::Claude(Default::default()),
            Agent::Codex => ParserState::Codex(Default::default()),
            Agent::Deepseek => ParserState::Deepseek(Default::default()),
            Agent::Antigravity => ParserState::Antigravity(Default::default()),
            Agent::Kiro => ParserState::Kiro(Default::default()),
            Agent::Cursor => ParserState::Cursor(Default::default()),
            Agent::WebMcp => ParserState::None,
        }
    }
}

/// 一个记录文件的解析结果和断点。
#[derive(Debug)]
pub struct FileEntry {
    pub agent: Agent,
    pub path: PathBuf,
    pub sig: FileSig,
    /// 已解析到的字节位置（Antigravity 不用）
    pub offset: u64,
    pub session: SessionMeta,
    pub models: Vec<String>,
    pub events: Vec<UsageEvent>,
    pub net: Vec<NetEvent>,
    pub live: LiveTrack,
    pub rate_limits: Option<RateLimits>,
    /// 工具调用记录（只保留最近一批）
    pub tools: Vec<ToolCall>,
    /// 同一个文件里的其他对话（Cursor）
    pub extra: Vec<FileEntry>,
    pub parser: ParserState,
    /// 源文件已被删除（统计数字继续保留）
    pub gone: bool,
    /// 这段会话是别的电脑记下的，由用量历史带过来：这台电脑上没有它的来源
    pub elsewhere: bool,
    /// 来自 Cursor 插件（网关）的卡片记录，而不是 Cursor 自己的对话库
    pub plugin: bool,
    /// 这张卡片所在的 Cursor 对话：那段对话自己记下的请求，卡片不再重复报
    pub host: String,
    /// 文件内容无法继续解析，等它变化后再试
    pub corrupt: bool,
    /// 非 0 时表示到这个时刻要再解析一次，即使文件没有变化（由解析器设置）
    pub recheck_at_ms: i64,
    /// 连续读取失败的次数，以及下次重试的时刻（由扫描器设置）
    pub errors: u32,
    /// 最近一次读取失败的原因；成功重试后清除，不包含对话正文。
    pub last_error: Option<String>,
    pub retry_at_ms: i64,
}

impl FileEntry {
    pub fn new(agent: Agent, path: PathBuf) -> FileEntry {
        FileEntry {
            agent,
            path,
            sig: FileSig::default(),
            offset: 0,
            session: SessionMeta::default(),
            models: Vec::new(),
            events: Vec::new(),
            net: Vec::new(),
            live: LiveTrack::default(),
            rate_limits: None,
            tools: Vec::new(),
            extra: Vec::new(),
            parser: ParserState::new(agent),
            gone: false,
            elsewhere: false,
            plugin: false,
            host: String::new(),
            corrupt: false,
            recheck_at_ms: 0,
            errors: 0,
            last_error: None,
            retry_at_ms: 0,
        }
    }

    /// 文件被截断或重写时从头再来。
    pub fn reset(&mut self) {
        let agent = self.agent;
        let path = std::mem::take(&mut self.path);
        *self = FileEntry::new(agent, path);
    }

    pub fn model_name(&self, index: u16) -> &str {
        self.models.get(index as usize).map(String::as_str).unwrap_or("unknown")
    }
}

/// 用量历史里已经记下的一张插件卡片：只有每次请求的时刻和用量。
#[derive(Clone, Debug, Default)]
pub struct RecordedCard {
    pub models: Vec<String>,
    pub events: Vec<UsageEvent>,
    /// 以前的运行把这张卡片记到了什么时刻；这次运行才有的卡片为 0。这个时刻之前的请求照记下的算、不再重算：
    /// 算出它们的那些行没有存下来，来源也可能已经把行清掉，重算只会算少或者算重。
    pub prior_until: i64,
}

/// 用量历史交给扫描器的那一份：卡片 id → 已记下的请求。
///
/// 插件的过程记录这个软件不另存，插件自己也只留着最近的一批，所以以前算出来的用量只在这里还有。
/// 解析器把以前的运行记下的请求照原样算，只按现存的记录算那之后的：既不丢，也不会重复。
#[derive(Debug, Default)]
pub struct Recorded {
    /// 第一次交接了非空的一份时加一。解析器记下自己用的是哪一版，用量历史只收用过当前这一版的结果，
    /// 否则在交接之前扫出来的、缺了前半段的结果会把记下的那份覆盖掉。
    pub rev: u64,
    pub cards: HashMap<String, RecordedCard>,
}

/// 一轮扫描里各文件共用的上下文。
pub struct ScanCtx<'a> {
    pub now_ms: i64,
    /// Antigravity 的模型枚举 → 模型名（解析一次后共用）
    pub antigravity_models: &'a Mutex<HashMap<u64, String>>,
    /// 用量历史里已经记下的插件卡片；没有用量历史时为空
    pub recorded: Option<&'a Recorded>,
    /// 百事通插件的对话（经它在本机的转发口读到）；没有开启时为空
    pub baishitong: Option<&'a baishitong::Feed>,
}

impl<'a> ScanCtx<'a> {
    /// 不接用量历史、不读百事通的一轮扫描。
    pub fn new(now_ms: i64, antigravity_models: &'a Mutex<HashMap<u64, String>>) -> ScanCtx<'a> {
        ScanCtx { now_ms, antigravity_models, recorded: None, baishitong: None }
    }
}

/// 解析文件的新增内容。调用前 `entry.sig` 还是上次的签名，成功后由调用方更新。
pub fn update(entry: &mut FileEntry, ctx: &ScanCtx) -> io::Result<()> {
    match entry.agent {
        Agent::Claude => claude::update(entry, ctx),
        Agent::Codex => codex::update(entry, ctx),
        Agent::Deepseek => deepseek::update(entry, ctx),
        Agent::Antigravity => antigravity::update(entry, ctx),
        Agent::Kiro => kiro::update(entry, ctx),
        Agent::Cursor => cursor::update(entry, ctx),
        Agent::WebMcp => cursor::update_gateway(entry),
    }
}

/// 最多保留多少条工具调用记录（每个对话）
pub(crate) const MAX_TOOL_CALLS: usize = 4000;

pub(crate) fn push_tool(tools: &mut Vec<ToolCall>, call: ToolCall) {
    tools.push(call);
    if tools.len() > MAX_TOOL_CALLS {
        tools.drain(..MAX_TOOL_CALLS / 2);
    }
}

/// 把 `mcp-<服务>-<工具>` / `mcp_<服务>_<工具>` / `<服务>___<工具>` 这类名字拆成 (服务, 工具)。
/// 拆不出服务名时服务为空。名字只保留标识符样式的短字符串。
pub(crate) fn split_tool_name(raw: &str, servers: &[&str]) -> (String, String) {
    let clean = |text: &str| -> String { text.chars().filter(|ch| !ch.is_control()).take(64).collect() };
    if let Some(rest) = raw.strip_prefix("mcp-").or_else(|| raw.strip_prefix("mcp_")) {
        // 已知服务名里可能带连字符或空格，优先按最长匹配
        let mut known: Vec<&&str> = servers.iter().filter(|server| rest.starts_with(**server) && rest[server.len()..].starts_with(['-', '_'])).collect();
        known.sort_by_key(|server| std::cmp::Reverse(server.len()));
        if let Some(server) = known.first() {
            return (clean(server), clean(&rest[server.len() + 1..]));
        }
        if let Some((server, tool)) = rest.split_once(['-', '_']) {
            return (clean(server), clean(tool));
        }
        return (String::new(), clean(rest));
    }
    (String::new(), clean(raw))
}

pub(crate) fn intern(models: &mut Vec<String>, name: &str) -> u16 {
    let name = if name.is_empty() { "unknown" } else { name };
    if let Some(position) = models.iter().position(|model| model == name) {
        return position as u16;
    }
    models.push(name.to_owned());
    (models.len() - 1).min(u16::MAX as usize) as u16
}

pub(crate) fn touch(session: &mut SessionMeta, ts: i64) {
    if ts <= 0 {
        return;
    }
    if session.started_ms == 0 || ts < session.started_ms {
        session.started_ms = ts;
    }
    if ts > session.last_active_ms {
        session.last_active_ms = ts;
    }
}

pub(crate) fn push_net(net: &mut Vec<NetEvent>, event: NetEvent) {
    net.push(event);
    if net.len() > 400 {
        net.drain(..200);
    }
}

pub(crate) fn clamp_ms(value: i64) -> u32 {
    value.clamp(0, u32::MAX as i64) as u32
}

fn each_line(data: &[u8], on_line: &mut dyn FnMut(&[u8])) -> usize {
    let mut start = 0;
    while let Some(relative) = memchr::memchr(b'\n', &data[start..]) {
        let end = start + relative;
        let mut line = &data[start..end];
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        if !line.is_empty() {
            on_line(line);
        }
        start = end + 1;
    }
    start
}

const CHUNK: usize = 8 * 1024 * 1024;

/// 从 `offset` 读到文件末尾，逐行回调，并把 `offset` 推进到最后一个完整行之后。
///
/// 文件末尾没有换行的那一行：如果已经是完整的 JSON 就当成一行处理，否则留到下次。
pub(crate) fn tail_lines(path: &Path, offset: &mut u64, mut on_line: impl FnMut(&[u8])) -> io::Result<()> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(*offset))?;
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; CHUNK];
    loop {
        let read = file.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        pending.extend_from_slice(&chunk[..read]);
        let consumed = each_line(&pending, &mut on_line);
        *offset += consumed as u64;
        pending.drain(..consumed);
    }
    let rest = pending.strip_suffix(b"\r").unwrap_or(&pending);
    if !rest.is_empty() && serde_json::from_slice::<IgnoredAny>(rest).is_ok() {
        on_line(rest);
        *offset += pending.len() as u64;
    }
    Ok(())
}

/// 在一行开头的 `limit` 个字节里找 `needle`（形如 `"type":"`），返回紧跟其后的字符串值。
pub(crate) fn head_value<'a>(line: &'a [u8], needle: &[u8], limit: usize) -> Option<&'a [u8]> {
    let head = &line[..line.len().min(limit)];
    let start = memchr::memmem::find(head, needle)? + needle.len();
    let rest = &line[start..line.len().min(start + 96)];
    let end = memchr::memchr(b'"', rest)?;
    Some(&rest[..end])
}

/// 取路径的最后一段作为项目名。
pub fn project_name(cwd: &str) -> String {
    let trimmed = cwd.trim_end_matches(['/', '\\']);
    let name = trimmed.rsplit(['/', '\\']).next().unwrap_or("");
    if name.is_empty() || name.ends_with(':') {
        trimmed.to_owned()
    } else {
        name.to_owned()
    }
}

/// 各 agent 的数据目录。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Roots {
    pub claude: Vec<PathBuf>,
    pub codex: Vec<PathBuf>,
    pub deepseek: Vec<PathBuf>,
    pub antigravity: Vec<PathBuf>,
    pub kiro: Vec<PathBuf>,
    pub cursor: Vec<PathBuf>,
    /// 兼容旧配置；自动发现不再扫描 Cursor 的重复网关日志。
    pub web_mcp: Vec<PathBuf>,
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).filter(|value| !value.is_empty()).map(PathBuf::from)
}

impl Roots {
    /// 按默认位置和环境变量找出本机存在的数据目录。
    pub fn detect() -> Roots {
        let home = env_path("USERPROFILE").or_else(|| env_path("HOME")).unwrap_or_default();
        let mut roots = Roots::default();

        let claude = env_path("CLAUDE_CONFIG_DIR").unwrap_or_else(|| home.join(".claude"));
        if claude.is_dir() {
            roots.claude.push(claude);
        }

        let codex = env_path("CODEX_HOME").unwrap_or_else(|| home.join(".codex"));
        if codex.is_dir() {
            roots.codex.push(codex);
        }

        let mut deepseek: Vec<PathBuf> = Vec::new();
        if let Some(dir) = env_path("DSH_HOME") {
            deepseek.push(dir);
        }
        deepseek.push(home.join(".dsh"));
        if let Some(local) = env_path("LOCALAPPDATA") {
            for app in list_dirs(&local) {
                let name = app.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
                if !(name.contains("deepseek") || name.contains("dsh")) {
                    continue;
                }
                for environment in list_dirs(&app.join("dpx").join("dsh-environments")) {
                    deepseek.push(environment.join("dsh-home"));
                }
            }
        }
        for dir in deepseek {
            if dir.join("sessions").is_dir() && !roots.deepseek.contains(&dir) {
                roots.deepseek.push(dir);
            }
        }

        let antigravity = home.join(".gemini").join("antigravity");
        if antigravity.is_dir() {
            roots.antigravity.push(antigravity);
        }
        let roaming = env_path("APPDATA").unwrap_or_else(|| home.join("AppData/Roaming"));
        let kiro = home.join(".kiro");
        if kiro.join("sessions").is_dir() { roots.kiro.push(kiro); }
        // Kiro 客户端自己的数据目录：账户额度缓存在这里的 state.vscdb
        let kiro_app = roaming.join("Kiro/User/globalStorage");
        if kiro_app.join("state.vscdb").is_file() { roots.kiro.push(kiro_app); }
        let cursor = roaming.join("Cursor/User/globalStorage");
        if cursor.is_dir() {
            // Cursor's database already includes its MCP bridge messages and
            // tools. Do not scan the gateway's duplicate activity mirrors.
            roots.cursor.push(cursor);
        }
        roots
    }

    pub fn of(&self, agent: Agent) -> &[PathBuf] {
        match agent {
            Agent::Claude => &self.claude,
            Agent::Codex => &self.codex,
            Agent::Deepseek => &self.deepseek,
            Agent::Antigravity => &self.antigravity,
            Agent::Kiro => &self.kiro,
            Agent::Cursor => &self.cursor,
            Agent::WebMcp => &self.web_mcp,
        }
    }
}

fn list_dirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
        .map(|entry| entry.path())
        .collect()
}

/// 发现到的一个记录文件。
#[derive(Clone, Debug)]
pub struct Found {
    pub agent: Agent,
    pub path: PathBuf,
    pub sig: FileSig,
}

fn walk(dir: &Path, depth: usize, accept: &dyn Fn(&str) -> bool, out: &mut Vec<(PathBuf, FileSig)>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            if depth > 0 {
                walk(&entry.path(), depth - 1, accept, out);
            }
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !accept(name) {
            continue;
        }
        // 目录项里的元数据对正在写入的文件可能滞后，这里只用来发现文件；活跃文件由调用方再精确查一次
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let sig = FileSig {
            size: meta.len(),
            mtime_ms: meta.modified().map(system_time_ms).unwrap_or(0),
            aux_size: 0,
            aux_mtime_ms: 0,
        };
        out.push((entry.path(), sig));
    }
}

/// 伴生文件路径（只有 Antigravity 的 SQLite 库有）。
pub fn aux_path(agent: Agent, path: &Path) -> Option<PathBuf> {
    if agent == Agent::Kiro && path.file_name().is_some_and(|name| name == "messages.jsonl") {
        return Some(path.with_file_name("session.json"));
    }
    if !matches!(agent, Agent::Antigravity | Agent::Cursor) {
        return None;
    }
    let mut name = path.as_os_str().to_owned();
    name.push("-wal");
    Some(PathBuf::from(name))
}

/// 列出所有数据目录下的记录文件。
pub fn discover(roots: &Roots) -> Vec<Found> {
    let mut found = Vec::new();
    let mut collect = |agent: Agent, dir: PathBuf, depth: usize, accept: &dyn Fn(&str) -> bool| {
        let mut files = Vec::new();
        walk(&dir, depth, accept, &mut files);
        for (path, sig) in files {
            let sig = match aux_path(agent, &path) {
                Some(aux) => sig.with_aux(&aux),
                None => sig,
            };
            found.push(Found { agent, path, sig });
        }
    };
    for root in &roots.claude {
        collect(Agent::Claude, root.join("projects"), 5, &|name| name.ends_with(".jsonl"));
    }
    for root in &roots.codex {
        let rollout = |name: &str| name.ends_with(".jsonl") || name.ends_with(".jsonl.zst");
        collect(Agent::Codex, root.join("sessions"), 5, &rollout);
        collect(Agent::Codex, root.join("archived_sessions"), 5, &rollout);
    }
    for root in &roots.deepseek {
        collect(Agent::Deepseek, root.join("sessions"), 5, &|name| {
            name.starts_with("session.") && name.ends_with(".jsonl.zstd")
        });
    }
    for root in &roots.antigravity {
        collect(Agent::Antigravity, root.join("conversations"), 0, &|name| name.ends_with(".db"));
    }
    for root in &roots.kiro {
        for workspace in list_dirs(&root.join("sessions")) {
            for session in list_dirs(&workspace) {
                if session.file_name().is_some_and(|s| s.to_string_lossy().starts_with("sess_")) {
                    collect(Agent::Kiro, session.clone(), 0, &|n| n == "messages.jsonl");
                    collect(Agent::Kiro, session.join("sub-executions"), 0, &|n| n.ends_with(".jsonl"));
                }
            }
        }
    }
    for root in &roots.cursor {
        collect(Agent::Cursor, root.clone(), 0, &|n| n == "state.vscdb");
    }
    for root in &roots.web_mcp {
        for session in list_dirs(root) {
            collect(Agent::WebMcp, session, 0, &|n| n == "process.json");
        }
    }
    // 压缩迁移可能短暂保留两份。优先使用可继续追加的原始文件。
    found.sort_by(|a, b| {
        let compressed = |f: &Found| f.path.extension().is_some_and(|ext| ext == "zst");
        compressed(a).cmp(&compressed(b)).then_with(|| b.sig.mtime_ms.cmp(&a.sig.mtime_ms)).then_with(|| a.path.cmp(&b.path))
    });
    let mut rollouts = std::collections::HashSet::new();
    found.retain(|f| f.agent != Agent::Codex || codex::rollout_name(&f.path).is_none_or(|name| rollouts.insert(name.to_owned())));
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_name_takes_last_segment() {
        assert_eq!(project_name(r"C:\Users\me\Desktop\Foxes-watch"), "Foxes-watch");
        assert_eq!(project_name("/home/me/work/app/"), "app");
        assert_eq!(project_name(r"D:\"), "D:");
        assert_eq!(project_name(""), "");
    }

    #[test]
    fn head_value_reads_string_after_needle() {
        let line = br#"{"timestamp":"2026-01-01T00:00:00.000Z","type":"event_msg","payload":{"type":"token_count"}}"#;
        assert_eq!(head_value(line, b"\"type\":\"", 96), Some(&b"event_msg"[..]));
        assert_eq!(head_value(line, b"\"payload\":{\"type\":\"", 200), Some(&b"token_count"[..]));
        assert_eq!(head_value(line, b"\"missing\":\"", 200), None);
    }

    #[test]
    fn each_line_stops_before_partial_line() {
        let mut lines = Vec::new();
        let consumed = each_line(b"one\r\ntwo\n\nthr", &mut |line| lines.push(line.to_vec()));
        assert_eq!(lines, vec![b"one".to_vec(), b"two".to_vec()]);
        assert_eq!(consumed, 10);
    }
}
