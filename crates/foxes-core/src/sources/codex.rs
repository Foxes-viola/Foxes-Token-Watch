//! Codex：`~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`。
//!
//! - 文件里绝大部分字节是工具输出，所以先看行首的 `type` / `payload.type`，只对需要的行做完整解析。
//! - `token_usage_record` 每次响应一条，带 `response_id`，优先用它；老版本只有 `token_count`，
//!   用它的 `last_token_usage`，并跳过累计值没变的重复上报。
//! - `input_tokens` 包含缓存命中部分，归一时要减掉。

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, BufRead};
use std::path::Path;

use serde::de::{Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;

use super::{clamp_ms, head_value, intern, push_net, tail_lines, touch, FileEntry, ParserState, ScanCtx};
use crate::lenient::{f64_lenient, i64_lenient, u64_lenient, Obj, Text};
use crate::model::{Effort, NetEvent, NetKind, Phase, RateLimits, RateWindow, Usage, UsageEvent};
use crate::timeutil::{fnv64, parse_iso_bytes, parse_iso_ms, MINUTE};

/// 一次响应的用量指纹（输入、缓存、输出、合计）：用来认出 `token_usage_record` 和 `token_count` 说的是同一次响应。
///
/// 不能拿两边的累计值来对：有的响应只有 record、不进 `token_count` 的累计，从那以后两边的累计值就再也对不上了。
type Fingerprint = [u64; 4];

fn fingerprint(usage: &RawUsage) -> Fingerprint {
    [usage.input_tokens, usage.cached_input_tokens, usage.output_tokens, usage.total_tokens]
}

/// 最多记住多少条还没等到 `token_count` 的 record
const MAX_PENDING_RECORDS: usize = 8;

#[derive(Debug, Default)]
pub struct State {
    model: String,
    /// 最近一条 `turn_context` 里的思考强度
    effort: Effort,
    /// 已经由 `token_usage_record` 记过、还没等到对应 `token_count` 的响应
    pending_records: VecDeque<Fingerprint>,
    /// 最近一条 `token_count` 的累计 token（累计值没变就是重复上报）
    last_count_total: u64,
    /// 由 `token_count` 生成、还没等到对应 record 的最近一个事件（之后若出现同一次响应的 record，就补上去重键而不是再记一次）
    last_count_event: Option<(u32, Fingerprint)>,
    /// 当前这次模型请求的发出时刻
    request_ms: i64,
    /// 本轮第一个事件（轮结束时把首字延迟记到它上面）
    turn_first_event: Option<u32>,
}

#[derive(Deserialize)]
struct Line<'a> {
    #[serde(borrow, default)]
    timestamp: Text<'a>,
    #[serde(rename = "type", borrow, default)]
    kind: Text<'a>,
    #[serde(borrow, default)]
    payload: Obj<Payload<'a>>,
}

#[derive(Deserialize)]
struct Payload<'a> {
    #[serde(rename = "type", borrow, default)]
    kind: Text<'a>,
    #[serde(borrow, default)]
    id: Text<'a>,
    #[serde(borrow, default)]
    cwd: Text<'a>,
    #[serde(borrow, default)]
    cli_version: Text<'a>,
    #[serde(default)]
    source: Source,
    #[serde(borrow, default)]
    forked_from_id: Text<'a>,
    #[serde(borrow, default)]
    model: Text<'a>,
    #[serde(borrow, default)]
    effort: Text<'a>,
    #[serde(default)]
    info: Obj<TokenInfo>,
    #[serde(borrow, default)]
    rate_limits: Obj<RawRateLimits<'a>>,
    #[serde(default)]
    usage: Obj<RawUsage>,
    #[serde(borrow, default)]
    response_id: Text<'a>,
    #[serde(default, deserialize_with = "u64_lenient")]
    model_context_window: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    duration_ms: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    time_to_first_token_ms: u64,
    #[serde(borrow, default)]
    thread_name: Text<'a>,
    #[serde(borrow, default)]
    codex_error_info: Text<'a>,
}

#[derive(Deserialize)]
struct TokenInfo {
    #[serde(default)]
    last_token_usage: Obj<RawUsage>,
    #[serde(default)]
    total_token_usage: Obj<RawUsage>,
    #[serde(default, deserialize_with = "u64_lenient")]
    model_context_window: u64,
}

#[derive(Clone, Copy, Deserialize)]
struct RawUsage {
    #[serde(default, deserialize_with = "u64_lenient")]
    input_tokens: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    cached_input_tokens: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    cache_write_input_tokens: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    output_tokens: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    reasoning_output_tokens: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    total_tokens: u64,
}

#[derive(Deserialize)]
struct RawRateLimits<'a> {
    #[serde(default)]
    primary: Obj<RawWindow>,
    #[serde(default)]
    secondary: Obj<RawWindow>,
    #[serde(borrow, default)]
    plan_type: Text<'a>,
}

#[derive(Deserialize)]
struct RawWindow {
    #[serde(default, deserialize_with = "f64_lenient")]
    used_percent: f64,
    #[serde(default, deserialize_with = "u64_lenient")]
    window_minutes: u64,
    #[serde(default, deserialize_with = "i64_lenient")]
    resets_at: i64,
    #[serde(default, deserialize_with = "i64_lenient")]
    resets_in_seconds: i64,
}

/// `session_meta.source`：字符串（cli / vscode …）或对象（`{"subagent": …}`）。
#[derive(Debug, Default)]
struct Source {
    sub_agent: bool,
}

impl<'de> Deserialize<'de> for Source {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Source;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("anything")
            }
            fn visit_str<E>(self, v: &str) -> Result<Source, E> {
                Ok(Source { sub_agent: v.contains("subagent") || v.contains("sub_agent") })
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Source, A::Error> {
                let mut source = Source::default();
                while let Some(key) = map.next_key::<Text>()? {
                    if matches!(key.as_str(), "subagent" | "sub_agent" | "subAgent") {
                        source.sub_agent = true;
                    }
                    map.next_value::<IgnoredAny>()?;
                }
                Ok(source)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Source, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(Source::default())
            }
            fn visit_bool<E>(self, _: bool) -> Result<Source, E> {
                Ok(Source::default())
            }
            fn visit_i64<E>(self, _: i64) -> Result<Source, E> {
                Ok(Source::default())
            }
            fn visit_u64<E>(self, _: u64) -> Result<Source, E> {
                Ok(Source::default())
            }
            fn visit_f64<E>(self, _: f64) -> Result<Source, E> {
                Ok(Source::default())
            }
            fn visit_unit<E>(self) -> Result<Source, E> {
                Ok(Source::default())
            }
            fn visit_none<E>(self) -> Result<Source, E> {
                Ok(Source::default())
            }
        }
        d.deserialize_any(V)
    }
}

fn normalize(raw: &RawUsage) -> Usage {
    let cache_read = raw.cached_input_tokens.min(raw.input_tokens);
    let cache_write = raw.cache_write_input_tokens.min(raw.input_tokens - cache_read);
    Usage {
        input: raw.input_tokens - cache_read - cache_write,
        cache_read,
        cache_write,
        output: raw.output_tokens,
        reasoning: raw.reasoning_output_tokens.min(raw.output_tokens),
    }
}

fn window(raw: &RawWindow, ts: i64) -> RateWindow {
    let resets_at_ms = if raw.resets_at > 0 {
        // 新版是 epoch 秒；万一已经是毫秒就不再乘
        if raw.resets_at < 100_000_000_000 {
            raw.resets_at * 1_000
        } else {
            raw.resets_at
        }
    } else if raw.resets_in_seconds > 0 && ts > 0 {
        ts + raw.resets_in_seconds * 1_000
    } else {
        0
    };
    RateWindow { used_percent: raw.used_percent, window_minutes: raw.window_minutes, resets_at_ms, ..RateWindow::default() }
}

/// 压缩、解压或归档不改变 rollout 名称。
pub(crate) fn rollout_name(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?;
    Some(name.strip_suffix(".zst").unwrap_or(name))
}

pub fn update(entry: &mut FileEntry, _ctx: &ScanCtx) -> io::Result<()> {
    if entry.path.extension().is_some_and(|ext| ext == "zst") {
        // 已封存的文件只在签名改变时重新读取。流式解压，不在磁盘上展开对话；
        // 读完整个流后才替换旧结果，避免半个压缩帧在重试时计两遍。
        let file = std::fs::File::open(&entry.path)?;
        let size = file.metadata()?.len();
        let mut reader = io::BufReader::new(zstd::stream::read::Decoder::new(file)?);
        let mut parsed = FileEntry::new(entry.agent, entry.path.clone());
        let mut line = Vec::new();
        while reader.read_until(b'\n', &mut line)? != 0 {
            handle_line(&mut parsed, &line);
            line.clear();
        }
        parsed.offset = size;
        *entry = parsed;
        return Ok(());
    }
    let path = entry.path.clone();
    let mut offset = entry.offset;
    let result = tail_lines(&path, &mut offset, |line| handle_line(entry, line));
    entry.offset = offset;
    result
}

const TYPE_NEEDLE: &[u8] = b"\"type\":\"";
const PAYLOAD_TYPE_NEEDLE: &[u8] = b"\"payload\":{\"type\":\"";
const TIMESTAMP_NEEDLE: &[u8] = b"\"timestamp\":\"";

fn wants_full_parse(kind: &[u8], payload_kind: &[u8]) -> bool {
    match kind {
        b"session_meta" | b"turn_context" | b"token_usage_record" => true,
        b"event_msg" => matches!(
            payload_kind,
            b"token_count" | b"task_started" | b"task_complete" | b"turn_aborted" | b"stream_error" | b"error" | b"thread_name_updated"
        ),
        _ => false,
    }
}

fn handle_line(entry: &mut FileEntry, raw: &[u8]) {
    let kind = head_value(raw, TYPE_NEEDLE, 120);
    let payload_kind = head_value(raw, PAYLOAD_TYPE_NEEDLE, 240).unwrap_or(b"");
    match kind {
        Some(b"response_item") => handle_response_item(entry, raw, payload_kind),
        Some(kind) if !wants_full_parse(kind, payload_kind) => {}
        // 行首没找到 type（格式和预期不同）时退回到完整解析
        _ => handle_parsed(entry, raw),
    }
}

/// `response_item` 只用来推断实时状态，只看行首，不解析正文。
fn handle_response_item(entry: &mut FileEntry, raw: &[u8], payload_kind: &[u8]) {
    let ts = head_value(raw, TIMESTAMP_NEEDLE, 48).and_then(parse_iso_bytes).unwrap_or(0);
    let FileEntry { parser, session, live, .. } = entry;
    let ParserState::Codex(state) = parser else {
        return;
    };
    touch(session, ts);
    if !live.turn_open {
        return;
    }
    match payload_kind {
        b"reasoning" => live.set_phase(Phase::Writing, ts),
        b"message" => {
            if head_value(raw, b"\"role\":\"", 800) == Some(&b"assistant"[..]) {
                live.set_phase(Phase::Writing, ts);
            }
        }
        b"function_call" | b"custom_tool_call" | b"local_shell_call" | b"web_search_call" | b"tool_search_call" | b"image_generation_call" => {
            live.set_phase(Phase::Tool, ts);
            live.tool = head_value(raw, b"\"name\":\"", 800)
                .and_then(|name| std::str::from_utf8(name).ok())
                .unwrap_or("")
                .to_owned();
        }
        b"function_call_output" | b"custom_tool_call_output" | b"tool_search_output" => {
            live.set_phase(Phase::Thinking, ts);
            state.request_ms = ts;
        }
        _ => {}
    }
}

fn handle_parsed(entry: &mut FileEntry, raw: &[u8]) {
    let Ok(line) = serde_json::from_slice::<Line>(raw) else {
        return;
    };
    let Some(payload) = line.payload.0 else {
        return;
    };
    let ts = parse_iso_ms(line.timestamp.as_str()).unwrap_or(0);
    let FileEntry { parser, session, models, events, net, live, rate_limits, .. } = entry;
    let ParserState::Codex(state) = parser else {
        return;
    };
    touch(session, ts);

    let mut push_event = |state: &mut State, raw_usage: &RawUsage, key: u64| -> u32 {
        let latency_ms = if state.request_ms > 0 && ts >= state.request_ms && ts - state.request_ms < 60 * MINUTE {
            clamp_ms(ts - state.request_ms)
        } else {
            0
        };
        events.push(UsageEvent {
            ts,
            usage: normalize(raw_usage),
            model: intern(models, &state.model),
            key,
            latency_ms,
            ttft_ms: 0,
            think_ms: 0,
            reasoning_estimated: false,
            effort: state.effort,
            ..UsageEvent::default()
        });
        let index = (events.len() - 1) as u32;
        if state.turn_first_event.is_none() {
            state.turn_first_event = Some(index);
        }
        state.request_ms = ts;
        index
    };

    match (line.kind.as_str(), payload.kind.as_str()) {
        ("session_meta", _) => {
            if !payload.id.is_empty() {
                session.id = payload.id.as_str().to_owned();
            }
            if !payload.cwd.is_empty() {
                session.cwd = payload.cwd.as_str().to_owned();
            }
            session.app_version = payload.cli_version.as_str().to_owned();
            session.sub_agent = payload.source.sub_agent;
            session.parent_id = payload.forked_from_id.as_str().to_owned();
        }
        ("turn_context", _) => {
            if !payload.model.is_empty() {
                state.model = payload.model.as_str().to_owned();
            }
            if !payload.effort.is_empty() {
                state.effort = Effort::parse(payload.effort.as_str());
            }
            if session.cwd.is_empty() && !payload.cwd.is_empty() {
                session.cwd = payload.cwd.as_str().to_owned();
            }
        }
        ("token_usage_record", _) => {
            let Some(raw_usage) = payload.usage.0 else {
                return;
            };
            let key = if payload.response_id.is_empty() { 0 } else { fnv64(&[b"codex", payload.response_id.as_str().as_bytes()]) };
            let print = fingerprint(&raw_usage);
            match state.last_count_event.take() {
                // 同一次响应的 token_count 先到了：只补去重键，不再记一次
                Some((index, counted)) if counted == print => {
                    if let Some(event) = events.get_mut(index as usize) {
                        event.key = key;
                    }
                }
                _ => {
                    push_event(&mut *state, &raw_usage, key);
                    if state.pending_records.len() >= MAX_PENDING_RECORDS {
                        state.pending_records.pop_front();
                    }
                    state.pending_records.push_back(print);
                }
            }
            session.context_used = raw_usage.input_tokens;
        }
        ("event_msg", "token_count") => {
            if let Some(info) = payload.info.0 {
                if info.model_context_window > 0 {
                    session.context_window = info.model_context_window;
                }
                let total = info.total_token_usage.get().map(|u| u.total_tokens).unwrap_or(0);
                if let Some(last) = info.last_token_usage.0 {
                    session.context_used = last.input_tokens;
                    if total != state.last_count_total {
                        state.last_count_total = total;
                        let print = fingerprint(&last);
                        let recorded = state.pending_records.iter().position(|pending| *pending == print);
                        state.last_count_event = match recorded {
                            // 同一次响应已经由 token_usage_record 记过；排在它前面的 record 不会再有 token_count 了
                            Some(position) => {
                                state.pending_records.drain(..=position);
                                None
                            }
                            None if last.total_tokens > 0 => Some((push_event(&mut *state, &last, 0), print)),
                            None => None,
                        };
                    }
                }
            }
            if let Some(raw_limits) = payload.rate_limits.0 {
                let limits = RateLimits {
                    ts,
                    primary: raw_limits.primary.get().map(|w| window(w, ts)),
                    secondary: raw_limits.secondary.get().map(|w| window(w, ts)),
                    plan: raw_limits.plan_type.as_str().to_owned(),
                };
                if limits.primary.is_some() || limits.secondary.is_some() {
                    *rate_limits = Some(limits);
                }
            }
        }
        ("event_msg", "task_started") => {
            // 上一轮没有正常收尾（进程退出等）时，直接从这一轮重新计时
            live.turn_open = false;
            live.open_turn(ts);
            live.set_phase(Phase::Thinking, ts);
            state.request_ms = ts;
            state.turn_first_event = None;
            if payload.model_context_window > 0 {
                session.context_window = payload.model_context_window;
            }
        }
        ("event_msg", "task_complete") => {
            if payload.time_to_first_token_ms > 0 {
                if let Some(event) = state.turn_first_event.and_then(|index| events.get_mut(index as usize)) {
                    event.ttft_ms = clamp_ms(payload.time_to_first_token_ms as i64);
                }
            }
            let duration = (payload.duration_ms > 0).then_some(payload.duration_ms);
            live.close_turn(ts, duration);
            state.turn_first_event = None;
        }
        ("event_msg", "turn_aborted") => {
            live.close_turn(ts, None);
            state.turn_first_event = None;
        }
        ("event_msg", "stream_error") | ("event_msg", "error") => {
            let is_retry = payload.kind.as_str() == "stream_error";
            let info = payload.codex_error_info.as_str();
            let code = if !info.is_empty() && info.len() <= 48 && info.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
                info.to_owned()
            } else if is_retry {
                "stream_error".to_owned()
            } else {
                "error".to_owned()
            };
            push_net(
                net,
                NetEvent { ts, kind: if is_retry { NetKind::Retry } else { NetKind::Error }, code, attempt: 0, max_attempts: 0, delay_ms: 0 },
            );
            if is_retry && live.turn_open {
                live.set_phase(Phase::Retrying, ts);
            }
        }
        ("event_msg", "thread_name_updated") => {
            if !payload.thread_name.is_empty() {
                session.title = payload.thread_name.as_str().to_owned();
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Agent;
    use std::path::PathBuf;

    fn feed(entry: &mut FileEntry, lines: &[&str]) {
        for line in lines {
            handle_line(entry, line.as_bytes());
        }
    }

    fn entry() -> FileEntry {
        FileEntry::new(Agent::Codex, PathBuf::from("rollout.jsonl"))
    }

    const META: &str = r#"{"timestamp":"2026-01-01T00:00:00.000Z","type":"session_meta","ordinal":0,"payload":{"id":"thread-1","cwd":"C:\\work\\app","cli_version":"0.1.0","source":"vscode"}}"#;
    const STARTED: &str = r#"{"timestamp":"2026-01-01T00:00:01.000Z","type":"event_msg","ordinal":1,"payload":{"type":"task_started","model_context_window":400000}}"#;
    const CONTEXT: &str = r#"{"timestamp":"2026-01-01T00:00:01.100Z","type":"turn_context","ordinal":2,"payload":{"model":"gpt-x"}}"#;

    #[test]
    fn record_then_count_is_counted_once() {
        let mut entry = entry();
        feed(
            &mut entry,
            &[
                META,
                STARTED,
                CONTEXT,
                r#"{"timestamp":"2026-01-01T00:00:05.000Z","ordinal":3,"type":"token_usage_record","payload":{"response_id":"resp_1","usage":{"input_tokens":1000,"cached_input_tokens":800,"output_tokens":50,"reasoning_output_tokens":20,"total_tokens":1050},"thread_token_usage":{"total_tokens":1050}}}"#,
                r#"{"timestamp":"2026-01-01T00:00:05.100Z","type":"event_msg","ordinal":4,"payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":800,"output_tokens":50,"total_tokens":1050},"last_token_usage":{"input_tokens":1000,"cached_input_tokens":800,"output_tokens":50,"total_tokens":1050},"model_context_window":400000},"rate_limits":{"primary":{"used_percent":12.5,"window_minutes":300,"resets_at":1767230000},"secondary":null,"plan_type":"pro"}}}"#,
                r#"{"timestamp":"2026-01-01T00:00:05.200Z","type":"event_msg","ordinal":5,"payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":1050},"last_token_usage":{"input_tokens":1000,"output_tokens":50,"total_tokens":1050}}}}"#,
                r#"{"timestamp":"2026-01-01T00:00:09.000Z","type":"event_msg","ordinal":6,"payload":{"type":"task_complete","duration_ms":8000,"time_to_first_token_ms":1500}}"#,
            ],
        );
        assert_eq!(entry.events.len(), 1);
        let event = &entry.events[0];
        assert_eq!(event.usage, Usage { input: 200, cache_read: 800, cache_write: 0, output: 50, reasoning: 20 });
        assert_eq!(event.usage.total(), 1050);
        assert_ne!(event.key, 0);
        assert_eq!(event.latency_ms, 4000);
        assert_eq!(event.ttft_ms, 1500);
        assert_eq!(entry.models, vec!["gpt-x"]);
        assert_eq!(entry.session.id, "thread-1");
        assert_eq!(entry.session.context_window, 400000);
        assert!(!entry.session.sub_agent);
        assert!(!entry.live.turn_open);
        assert_eq!(entry.live.last_turn_ms, 8000);
        let limits = entry.rate_limits.as_ref().unwrap();
        assert_eq!(limits.primary.as_ref().unwrap().used_percent, 12.5);
        assert_eq!(limits.primary.as_ref().unwrap().resets_at_ms, 1_767_230_000_000);
        assert!(limits.secondary.is_none());
    }

    #[test]
    fn records_are_counted_once_even_when_totals_drift() {
        let mut entry = entry();
        feed(
            &mut entry,
            &[
                STARTED,
                CONTEXT,
                r#"{"timestamp":"2026-01-01T00:00:05.000Z","type":"token_usage_record","payload":{"response_id":"resp_1","usage":{"input_tokens":1000,"cached_input_tokens":800,"output_tokens":50,"total_tokens":1050},"thread_token_usage":{"total_tokens":1050}}}"#,
                r#"{"timestamp":"2026-01-01T00:00:05.500Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":1050},"last_token_usage":{"input_tokens":1000,"cached_input_tokens":800,"output_tokens":50,"total_tokens":1050}}}}"#,
                // 这次响应只有 record，token_count 的累计值没有算上它
                r#"{"timestamp":"2026-01-01T00:00:10.000Z","type":"token_usage_record","payload":{"response_id":"resp_2","usage":{"input_tokens":2000,"cached_input_tokens":1500,"output_tokens":100,"total_tokens":2100},"thread_token_usage":{"total_tokens":3150}}}"#,
                r#"{"timestamp":"2026-01-01T00:00:10.500Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":1050},"last_token_usage":{"input_tokens":1000,"cached_input_tokens":800,"output_tokens":50,"total_tokens":1050}}}}"#,
                r#"{"timestamp":"2026-01-01T00:00:15.000Z","type":"token_usage_record","payload":{"response_id":"resp_3","usage":{"input_tokens":1200,"cached_input_tokens":1000,"output_tokens":60,"total_tokens":1260},"thread_token_usage":{"total_tokens":4410}}}"#,
                r#"{"timestamp":"2026-01-01T00:00:15.500Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":2310},"last_token_usage":{"input_tokens":1200,"cached_input_tokens":1000,"output_tokens":60,"total_tokens":1260}}}}"#,
                // token_count 先到、record 后到
                r#"{"timestamp":"2026-01-01T00:00:20.500Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":3000},"last_token_usage":{"input_tokens":600,"cached_input_tokens":500,"output_tokens":90,"total_tokens":690}}}}"#,
                r#"{"timestamp":"2026-01-01T00:00:21.000Z","type":"token_usage_record","payload":{"response_id":"resp_4","usage":{"input_tokens":600,"cached_input_tokens":500,"output_tokens":90,"total_tokens":690},"thread_token_usage":{"total_tokens":5100}}}"#,
            ],
        );
        assert_eq!(entry.events.len(), 4);
        assert_eq!(entry.events.iter().map(|event| event.usage.total()).sum::<u64>(), 1050 + 2100 + 1260 + 690);
        assert!(entry.events.iter().all(|event| event.key != 0));
    }

    #[test]
    fn count_only_files_use_last_usage_and_skip_repeats() {
        let mut entry = entry();
        feed(
            &mut entry,
            &[
                STARTED,
                CONTEXT,
                r#"{"timestamp":"2026-01-01T00:00:05.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":300},"last_token_usage":{"input_tokens":250,"cached_input_tokens":100,"output_tokens":50,"total_tokens":300}}}}"#,
                r#"{"timestamp":"2026-01-01T00:00:06.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":300},"last_token_usage":{"input_tokens":250,"cached_input_tokens":100,"output_tokens":50,"total_tokens":300}}}}"#,
                r#"{"timestamp":"2026-01-01T00:00:08.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":700},"last_token_usage":{"input_tokens":350,"cached_input_tokens":300,"output_tokens":50,"total_tokens":400}}}}"#,
                r#"{"timestamp":"2026-01-01T00:00:08.500Z","type":"event_msg","payload":{"type":"token_count","info":null}}"#,
            ],
        );
        assert_eq!(entry.events.len(), 2);
        assert_eq!(entry.events[0].usage, Usage { input: 150, cache_read: 100, cache_write: 0, output: 50, reasoning: 0 });
        assert_eq!(entry.events[1].usage.total(), 400);
        assert_eq!(entry.events[1].key, 0);
        assert!(entry.live.turn_open);
    }

    #[test]
    fn response_items_drive_phase_without_full_parse() {
        let mut entry = entry();
        feed(
            &mut entry,
            &[
                STARTED,
                r#"{"timestamp":"2026-01-01T00:00:02.000Z","type":"response_item","ordinal":3,"payload":{"type":"reasoning","summary":[]}}"#,
            ],
        );
        assert_eq!(entry.live.phase, Phase::Writing);
        feed(
            &mut entry,
            &[r#"{"timestamp":"2026-01-01T00:00:03.000Z","type":"response_item","ordinal":4,"payload":{"type":"function_call","name":"shell","arguments":"{}"}}"#],
        );
        assert_eq!(entry.live.phase, Phase::Tool);
        assert_eq!(entry.live.tool, "shell");
        feed(
            &mut entry,
            &[r#"{"timestamp":"2026-01-01T00:00:04.000Z","type":"response_item","ordinal":5,"payload":{"type":"function_call_output","output":"\"type\":\"token_count\""}}"#],
        );
        assert_eq!(entry.live.phase, Phase::Thinking);
        assert!(entry.events.is_empty());
        feed(
            &mut entry,
            &[r#"{"timestamp":"2026-01-01T00:00:05.000Z","type":"event_msg","ordinal":6,"payload":{"type":"turn_aborted","reason":"interrupted"}}"#],
        );
        assert!(!entry.live.turn_open);
        assert_eq!(entry.live.phase, Phase::Idle);
    }

    #[test]
    fn sub_agent_source_is_detected() {
        let mut entry = entry();
        feed(
            &mut entry,
            &[r#"{"timestamp":"2026-01-01T00:00:00.000Z","type":"session_meta","payload":{"id":"t2","source":{"subagent":{"other":"worker"}}}}"#],
        );
        assert!(entry.session.sub_agent);
    }
}
