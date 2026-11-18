//! Antigravity（反重力）：`~/.gemini/antigravity/conversations/<uuid>.db`，SQLite（WAL 模式）。
//!
//! - `steps` 表每步一行，`metadata` 列是 protobuf：字段 9 是用量，字段 1/6/7/8 是时刻，
//!   字段 4 是工具调用，字段 20 里有对应的 `gen_metadata` 行号。
//! - 步骤会被原地更新（状态、用量），所以每次都重读末尾的一段。
//! - 只读打开：没有 WAL 时用 immutable 方式，不在对方目录里产生任何文件。

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

use super::{aux_path, clamp_ms, intern, touch, FileEntry, ParserState, ScanCtx};
use crate::model::{Agent, Phase, Usage, UsageEvent};
use crate::proto::{first_bytes, first_string, first_varint, nested, timestamp_ms, Field, Reader};

const STEP_USER_INPUT: i64 = 14;
const STEP_MODEL: i64 = 15;

const STATUS_PENDING: i64 = 1;
const STATUS_RUNNING: i64 = 2;
const STATUS_GENERATING: i64 = 8;
const STATUS_WAITING: i64 = 9;
const STATUS_QUEUED: i64 = 11;

/// 最后一步结束后，等这么久还没有新步骤才认为这一轮结束（两步之间有短暂空档）。
const TURN_END_GRACE_MS: i64 = 3_000;
/// 每次重读末尾多少步（它们可能被原地更新）。
const REREAD_TAIL: i64 = 40;
/// 模型名查不到时用的占位名前缀，后面跟模型编号
const PLACEHOLDER_PREFIX: &str = "model-";
/// 一次解析里，同一个模型最多查几次名字
const MAX_NAME_LOOKUPS: u8 = 12;

#[derive(Debug)]
pub struct State {
    /// 步骤序号 → 事件下标
    by_step: HashMap<i64, u32>,
    max_idx: i64,
    last_user_ms: i64,
    /// 最后一步第一次被看到处于结束状态的时刻，0 表示不在等待收尾
    final_since_ms: i64,
    workspace_loaded: bool,
    loaded: bool,
}

impl Default for State {
    fn default() -> Self {
        State { by_step: HashMap::new(), max_idx: -1, last_user_ms: 0, final_since_ms: 0, workspace_loaded: false, loaded: false }
    }
}

fn sql_error(error: rusqlite::Error) -> io::Error {
    io::Error::new(io::ErrorKind::Other, error.to_string())
}

fn sqlite_uri(path: &Path, immutable: bool) -> String {
    let mut uri = String::from("file:");
    let text = path.to_string_lossy().replace('\\', "/");
    if !text.starts_with('/') {
        uri.push('/');
    }
    for ch in text.chars() {
        match ch {
            '%' => uri.push_str("%25"),
            '?' => uri.push_str("%3f"),
            '#' => uri.push_str("%23"),
            _ => uri.push(ch),
        }
    }
    uri.push_str(if immutable { "?mode=ro&immutable=1" } else { "?mode=ro" });
    uri
}

fn open(path: &Path) -> io::Result<Connection> {
    let has_wal = aux_path(Agent::Antigravity, path)
        .and_then(|wal| std::fs::metadata(wal).ok())
        .map(|meta| meta.len() > 0)
        .unwrap_or(false);
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(sqlite_uri(path, !has_wal), flags).map_err(sql_error)?;
    conn.busy_timeout(Duration::from_millis(300)).map_err(sql_error)?;
    Ok(conn)
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut pos = 0;
    while pos < bytes.len() {
        if bytes[pos] == b'%' && pos + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[pos + 1..pos + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(value) = hex {
                out.push(value);
                pos += 3;
                continue;
            }
        }
        out.push(bytes[pos]);
        pos += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `file:///c:/Users/me/app` → `c:/Users/me/app`
fn workspace_path(uri: &str) -> String {
    let decoded = percent_decode(uri);
    let path = decoded.strip_prefix("file://").unwrap_or(&decoded);
    // Windows 盘符路径前面多一个斜杠
    let bytes = path.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[2] == b':' {
        path[1..].to_owned()
    } else {
        path.to_owned()
    }
}

#[derive(Debug, Default)]
struct StepMeta {
    created_ms: i64,
    generated_ms: i64,
    latest_ms: i64,
    usage: Option<(u64, Usage)>,
    tool: String,
    gen_idx: Option<u64>,
}

fn parse_usage(buf: &[u8]) -> Option<(u64, Usage)> {
    let mut model = 0u64;
    let mut usage = Usage::default();
    let mut reasoning = 0u64;
    for (number, value) in Reader::new(buf) {
        let Field::Varint(value) = value else {
            continue;
        };
        match number {
            1 => model = value,
            2 => usage.input = value,
            3 => usage.output = value,
            4 => usage.cache_write = value,
            5 => usage.cache_read = value,
            9 => reasoning = value,
            _ => {}
        }
    }
    // 单次请求不可能有上亿 token，超出说明这段数据不是用量
    let plausible = [usage.input, usage.output, usage.cache_write, usage.cache_read].iter().all(|v| *v < 1_000_000_000);
    if usage.is_zero() || !plausible {
        return None;
    }
    usage.reasoning = reasoning.min(usage.output);
    Some((model, usage))
}

fn parse_step(metadata: &[u8]) -> StepMeta {
    let mut meta = StepMeta::default();
    for (number, value) in Reader::new(metadata) {
        let Field::Bytes(bytes) = value else {
            continue;
        };
        match number {
            1 => meta.created_ms = timestamp_ms(bytes).unwrap_or(0),
            6 => meta.generated_ms = timestamp_ms(bytes).unwrap_or(0),
            7 | 8 | 32 => {
                if let Some(ts) = timestamp_ms(bytes) {
                    meta.latest_ms = meta.latest_ms.max(ts);
                }
            }
            4 => meta.tool = first_string(bytes, 2).filter(|name| name.len() <= 64).unwrap_or_default(),
            9 => meta.usage = parse_usage(bytes),
            20 => meta.gen_idx = first_varint(bytes, 3),
            _ => {}
        }
    }
    meta.latest_ms = meta.latest_ms.max(meta.generated_ms).max(meta.created_ms);
    meta
}

/// 把占位名换成后来（可能是在别的文件里）解析出来的模型名。返回是否有改动。
pub fn rename_placeholders(models: &mut [String], names: &HashMap<u64, String>) -> bool {
    let mut renamed = false;
    for model in models.iter_mut() {
        let known = model.strip_prefix(PLACEHOLDER_PREFIX).and_then(|rest| rest.parse::<u64>().ok()).and_then(|id| names.get(&id));
        if let Some(name) = known {
            *model = name.clone();
            renamed = true;
        }
    }
    renamed
}

fn plausible_model_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':' | b'/'))
}

fn resolve_model(conn: &Connection, gen_idx: u64) -> Option<String> {
    let data: Vec<u8> = conn.query_row("SELECT data FROM gen_metadata WHERE idx = ?1", [gen_idx as i64], |row| row.get(0)).ok()?;
    let name = first_string(nested(&data, &[1])?, 19)?;
    plausible_model_name(&name).then_some(name)
}

fn load_workspace(conn: &Connection) -> Option<String> {
    let data: Vec<u8> = conn.query_row("SELECT data FROM trajectory_metadata_blob LIMIT 1", [], |row| row.get(0)).ok()?;
    let uri = first_string(first_bytes(&data, 1)?, 1)?;
    let path = workspace_path(&uri);
    (!path.is_empty()).then_some(path)
}

pub fn update(entry: &mut FileEntry, ctx: &ScanCtx) -> io::Result<()> {
    let conn = open(&entry.path)?;
    let FileEntry { parser, session, models, events, live, path, recheck_at_ms, .. } = entry;
    let ParserState::Antigravity(state) = parser else {
        return Ok(());
    };

    if session.id.is_empty() {
        session.id = path.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default();
    }
    if !state.workspace_loaded {
        if let Some(workspace) = load_workspace(&conn) {
            session.cwd = workspace;
            state.workspace_loaded = true;
        }
    }

    let from_idx = (state.max_idx - REREAD_TAIL).max(0);
    let mut statement = conn.prepare("SELECT idx, step_type, status, metadata FROM steps WHERE idx >= ?1 ORDER BY idx").map_err(sql_error)?;
    let rows = statement
        .query_map([from_idx], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?, row.get::<_, Option<Vec<u8>>>(3)?))
        })
        .map_err(sql_error)?;

    // 同一个模型在这个文件里查不到名字的次数：查够几次就不再查，等别的文件解析出来后统一补名字
    let mut misses: HashMap<u64, u8> = HashMap::new();
    let mut last: Option<(i64, i64, StepMeta)> = None;
    for row in rows {
        let (idx, step_type, status, metadata) = row.map_err(sql_error)?;
        let meta = parse_step(metadata.as_deref().unwrap_or(&[]));
        touch(session, meta.created_ms);
        touch(session, meta.latest_ms);
        if step_type == STEP_USER_INPUT && meta.created_ms > state.last_user_ms {
            state.last_user_ms = meta.created_ms;
        }

        if let Some((model_enum, usage)) = meta.usage {
            let known = ctx.antigravity_models.lock().unwrap_or_else(|e| e.into_inner()).get(&model_enum).cloned();
            let name = match known {
                Some(name) => name,
                None => {
                    let tried = misses.get(&model_enum).copied().unwrap_or(0);
                    let resolved = if tried >= MAX_NAME_LOOKUPS { None } else { meta.gen_idx.and_then(|gen_idx| resolve_model(&conn, gen_idx)) };
                    match resolved {
                        Some(name) => {
                            ctx.antigravity_models.lock().unwrap_or_else(|e| e.into_inner()).insert(model_enum, name.clone());
                            name
                        }
                        None => {
                            misses.insert(model_enum, tried.saturating_add(1));
                            format!("{PLACEHOLDER_PREFIX}{model_enum}")
                        }
                    }
                }
            };
            let ts = [meta.generated_ms, meta.latest_ms, meta.created_ms].into_iter().find(|ts| *ts > 0).unwrap_or(0);
            let latency_ms = if meta.created_ms > 0 && meta.generated_ms >= meta.created_ms { clamp_ms(meta.generated_ms - meta.created_ms) } else { 0 };
            let event = UsageEvent {
                ts,
                usage,
                model: intern(models, &name),
                key: 0,
                latency_ms,
                ttft_ms: 0,
                think_ms: 0,
                reasoning_estimated: false,
                ..UsageEvent::default()
            };
            let existing = state.by_step.get(&idx).copied().filter(|index| (*index as usize) < events.len());
            match existing {
                Some(index) => events[index as usize] = event,
                None => {
                    events.push(event);
                    state.by_step.insert(idx, (events.len() - 1) as u32);
                }
            }
            session.context_used = usage.prompt();
        }
        state.max_idx = state.max_idx.max(idx);
        last = Some((step_type, status, meta));
    }

    *recheck_at_ms = 0;
    if let Some((step_type, status, meta)) = last {
        let running = matches!(status, STATUS_PENDING | STATUS_RUNNING | STATUS_GENERATING | STATUS_WAITING | STATUS_QUEUED);
        if running {
            state.final_since_ms = 0;
            let started = if state.last_user_ms > 0 { state.last_user_ms } else { meta.created_ms };
            live.open_turn(started);
            let since = meta.latest_ms;
            match status {
                STATUS_GENERATING => live.set_phase(Phase::Generating, since),
                STATUS_WAITING => live.set_phase(Phase::Waiting, since),
                STATUS_PENDING | STATUS_QUEUED => live.set_phase(Phase::Requesting, since),
                _ if step_type == STEP_MODEL => live.set_phase(Phase::Generating, since),
                _ => {
                    live.set_phase(Phase::Tool, since);
                    live.tool = meta.tool.clone();
                }
            }
        } else if live.turn_open {
            if !state.loaded {
                // 首次读入的历史会话：不当作“刚刚跑完一轮”
                live.turn_open = false;
                live.set_phase(Phase::Idle, meta.latest_ms);
            } else if state.final_since_ms == 0 {
                state.final_since_ms = ctx.now_ms;
                *recheck_at_ms = ctx.now_ms + TURN_END_GRACE_MS;
            } else if ctx.now_ms - state.final_since_ms >= TURN_END_GRACE_MS - 200 {
                live.close_turn(meta.latest_ms.max(live.turn_started_ms), None);
                state.final_since_ms = 0;
            } else {
                *recheck_at_ms = state.final_since_ms + TURN_END_GRACE_MS;
            }
        }
    }
    state.loaded = true;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn varint(mut value: u64, out: &mut Vec<u8>) {
        while value >= 0x80 {
            out.push((value as u8 & 0x7f) | 0x80);
            value >>= 7;
        }
        out.push(value as u8);
    }

    fn field_varint(number: u32, value: u64, out: &mut Vec<u8>) {
        varint((number as u64) << 3, out);
        varint(value, out);
    }

    fn field_bytes(number: u32, bytes: &[u8], out: &mut Vec<u8>) {
        varint(((number as u64) << 3) | 2, out);
        varint(bytes.len() as u64, out);
        out.extend_from_slice(bytes);
    }

    fn time(seconds: u64) -> Vec<u8> {
        let mut out = Vec::new();
        field_varint(1, seconds, &mut out);
        field_varint(2, 500_000_000, &mut out);
        out
    }

    fn model_step(created: u64, generated: u64, gen_idx: u64, usage: &[(u32, u64)]) -> Vec<u8> {
        let mut usage_bytes = Vec::new();
        for (number, value) in usage {
            field_varint(*number, *value, &mut usage_bytes);
        }
        let mut link = Vec::new();
        field_varint(3, gen_idx, &mut link);
        let mut out = Vec::new();
        field_bytes(1, &time(created), &mut out);
        field_bytes(6, &time(generated), &mut out);
        field_bytes(9, &usage_bytes, &mut out);
        field_bytes(20, &link, &mut out);
        out
    }

    fn tool_step(created: u64, name: &str) -> Vec<u8> {
        let mut call = Vec::new();
        field_bytes(1, b"call_1", &mut call);
        field_bytes(2, name.as_bytes(), &mut call);
        let mut out = Vec::new();
        field_bytes(1, &time(created), &mut out);
        field_bytes(4, &call, &mut out);
        out
    }

    fn gen_row(model: &str) -> Vec<u8> {
        let mut inner = Vec::new();
        field_bytes(1, b"system prompt text", &mut inner);
        field_varint(3, 1319, &mut inner);
        field_bytes(19, model.as_bytes(), &mut inner);
        let mut out = Vec::new();
        field_bytes(1, &inner, &mut out);
        out
    }

    fn workspace_row(uri: &str) -> Vec<u8> {
        let mut inner = Vec::new();
        field_bytes(1, uri.as_bytes(), &mut inner);
        let mut out = Vec::new();
        field_bytes(1, &inner, &mut out);
        out
    }

    #[test]
    fn decodes_workspace_uri() {
        assert_eq!(workspace_path("file:///c%3A/Users/me/my%20app"), "c:/Users/me/my app");
        assert_eq!(workspace_path("file:///home/me/app"), "/home/me/app");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz%41"), "%zzA");
    }

    #[test]
    fn reads_usage_and_live_state_from_database() {
        let dir = std::env::temp_dir().join(format!("foxes-core-ag-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("11111111-2222-3333-4444-555555555555.db");
        std::fs::remove_file(&path).ok();
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE steps(idx INTEGER PRIMARY KEY, step_type INTEGER, status INTEGER, metadata BLOB);
                 CREATE TABLE gen_metadata(idx INTEGER PRIMARY KEY, data BLOB);
                 CREATE TABLE trajectory_metadata_blob(id INTEGER PRIMARY KEY, data BLOB);",
            )
            .unwrap();
            conn.execute("INSERT INTO trajectory_metadata_blob(id, data) VALUES (1, ?1)", [workspace_row("file:///c%3A/work/app")]).unwrap();
            conn.execute("INSERT INTO gen_metadata(idx, data) VALUES (0, ?1)", [gen_row("gemini-x")]).unwrap();
            let user = {
                let mut out = Vec::new();
                field_bytes(1, &time(1_793_684_000), &mut out);
                out
            };
            conn.execute("INSERT INTO steps VALUES (0, 14, 3, ?1)", [user]).unwrap();
            let usage = [(1, 1319), (2, 3335), (3, 1418), (5, 31369), (6, 24), (9, 144), (10, 1274)];
            conn.execute("INSERT INTO steps VALUES (1, 15, 3, ?1)", [model_step(1_793_684_001, 1_793_684_021, 0, &usage)]).unwrap();
            conn.execute("INSERT INTO steps VALUES (2, 132, 2, ?1)", [tool_step(1_793_684_021, "run_command")]).unwrap();
        }

        let models = Mutex::new(HashMap::new());
        let mut entry = FileEntry::new(Agent::Antigravity, path.clone());
        update(&mut entry, &ScanCtx::new(1_793_684_030_000, &models)).unwrap();

        assert_eq!(entry.session.id, "11111111-2222-3333-4444-555555555555");
        assert_eq!(entry.session.cwd, "c:/work/app");
        assert_eq!(entry.events.len(), 1);
        let event = &entry.events[0];
        assert_eq!(event.usage, Usage { input: 3335, cache_read: 31369, cache_write: 0, output: 1418, reasoning: 144 });
        assert_eq!(event.ts, 1_793_684_021_500);
        assert_eq!(event.latency_ms, 20_000);
        assert_eq!(entry.models, vec!["gemini-x"]);
        assert_eq!(models.lock().unwrap().get(&1319).map(String::as_str), Some("gemini-x"));
        assert!(entry.live.turn_open);
        assert_eq!(entry.live.phase, Phase::Tool);
        assert_eq!(entry.live.tool, "run_command");
        assert_eq!(entry.live.turn_started_ms, 1_793_684_000_500);

        // 工具步结束：先进入收尾等待，过了宽限期还没有新步骤才算这一轮结束；重读不会重复计数
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute("UPDATE steps SET status = 3 WHERE idx = 2", []).unwrap();
        }
        update(&mut entry, &ScanCtx::new(1_793_684_031_000, &models)).unwrap();
        assert!(entry.live.turn_open);
        assert_eq!(entry.recheck_at_ms, 1_793_684_034_000);
        update(&mut entry, &ScanCtx::new(1_793_684_034_100, &models)).unwrap();
        assert!(!entry.live.turn_open);
        assert_eq!(entry.live.turns, 1);
        assert_eq!(entry.events.len(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }
}
