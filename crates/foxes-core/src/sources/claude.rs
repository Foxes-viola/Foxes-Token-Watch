//! Claude Code：`~/.claude/projects/<slug>/**/*.jsonl`。
//!
//! - `assistant` 行每个内容块写一行，同一条消息（`message.id`）会重复出现，用量以最后一行为准。
//! - 内容块是在写完之后才落盘的，所以“刚写完思考块”意味着模型正在写正文。
//! - `api-request` 行是请求发出的时刻；`system` + `api_error` 是一次失败重试。

use std::collections::HashMap;
use std::fmt;
use std::io;

use serde::de::{Deserializer, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;

use super::{clamp_ms, intern, push_net, tail_lines, touch, FileEntry, ParserState, ScanCtx};
use crate::lenient::{text_weight, u64_lenient, Flag, Obj, Text, TextStat, Weight};
use crate::model::{Effort, NetEvent, NetKind, Phase, Usage, UsageEvent};
use crate::timeutil::{fnv64, parse_iso_ms, MINUTE};

#[derive(Debug, Default)]
struct MessageTrack {
    event: u32,
    request_ms: i64,
    thinking_weight: u64,
    other_weight: u64,
    exact_reasoning: bool,
}

#[derive(Debug, Default)]
pub struct State {
    /// message id 的哈希 → 该消息对应的事件
    messages: HashMap<u64, MessageTrack>,
    last_request_ms: i64,
    main_lines: u32,
    sidechain_lines: u32,
}

#[derive(Deserialize)]
struct Line<'a> {
    #[serde(rename = "type", borrow, default)]
    kind: Text<'a>,
    #[serde(borrow, default)]
    subtype: Text<'a>,
    #[serde(borrow, default)]
    timestamp: Text<'a>,
    #[serde(rename = "sessionId", borrow, default)]
    session_id: Text<'a>,
    #[serde(borrow, default)]
    cwd: Text<'a>,
    #[serde(borrow, default)]
    version: Text<'a>,
    #[serde(rename = "isSidechain", default)]
    is_sidechain: Flag,
    #[serde(borrow, default)]
    message: Obj<Message<'a>>,
    #[serde(rename = "customTitle", borrow, default)]
    custom_title: Text<'a>,
    #[serde(rename = "agentName", borrow, default)]
    agent_name: Text<'a>,
    #[serde(borrow, default)]
    summary: Text<'a>,
    #[serde(borrow, default)]
    error: Obj<ApiError<'a>>,
    #[serde(rename = "retryAttempt", default, deserialize_with = "u64_lenient")]
    retry_attempt: u64,
    #[serde(rename = "maxRetries", default, deserialize_with = "u64_lenient")]
    max_retries: u64,
    #[serde(rename = "retryInMs", default, deserialize_with = "u64_lenient")]
    retry_in_ms: u64,
    #[serde(rename = "thinkingDurationMs", default, deserialize_with = "u64_lenient")]
    thinking_duration_ms: u64,
    /// 这次请求设定的思考强度（`medium` / `xhigh` / `max` …）
    #[serde(borrow, default)]
    effort: Text<'a>,
}

#[derive(Deserialize)]
struct Message<'a> {
    #[serde(borrow, default)]
    id: Text<'a>,
    #[serde(borrow, default)]
    model: Text<'a>,
    #[serde(borrow, default)]
    stop_reason: Text<'a>,
    #[serde(default)]
    usage: Obj<RawUsage>,
    #[serde(default)]
    content: Content,
}

#[derive(Deserialize)]
struct RawUsage {
    #[serde(default, deserialize_with = "u64_lenient")]
    input_tokens: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    output_tokens: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    cache_creation_input_tokens: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    cache_read_input_tokens: u64,
    #[serde(default)]
    output_tokens_details: Obj<OutputDetails>,
}

#[derive(Deserialize)]
struct OutputDetails {
    #[serde(default, alias = "reasoning_tokens", deserialize_with = "u64_lenient")]
    thinking_tokens: u64,
}

#[derive(Deserialize)]
struct ApiError<'a> {
    #[serde(default, deserialize_with = "u64_lenient")]
    status: u64,
    #[serde(borrow, default)]
    connection: Obj<Connection<'a>>,
    #[serde(rename = "isNetworkDown", default)]
    is_network_down: Flag,
}

#[derive(Deserialize)]
struct Connection<'a> {
    #[serde(borrow, default)]
    code: Text<'a>,
}

#[derive(Deserialize)]
struct Block<'a> {
    #[serde(rename = "type", borrow, default)]
    kind: Text<'a>,
    #[serde(borrow, default)]
    name: Text<'a>,
    #[serde(default)]
    text: TextStat,
    #[serde(default)]
    thinking: TextStat,
    #[serde(default)]
    input: Weight,
}

/// 内容块的概况：有哪几类块、各自的体量。不保留任何正文。
#[derive(Debug, Default)]
struct Content {
    has_thinking: bool,
    has_text: bool,
    has_tool_use: bool,
    has_tool_result: bool,
    /// 用户直接输入的内容（文本 / 图片），不含工具结果
    has_prompt: bool,
    interrupted: bool,
    tool: String,
    thinking_weight: u64,
    other_weight: u64,
}

impl Content {
    fn add(&mut self, block: Block) {
        match block.kind.as_str() {
            "thinking" | "redacted_thinking" => {
                self.has_thinking = true;
                self.thinking_weight += block.thinking.len;
            }
            "text" => {
                self.has_text = true;
                self.has_prompt = true;
                self.other_weight += block.text.len;
                if block.text.interrupted {
                    self.interrupted = true;
                }
            }
            "tool_use" | "server_tool_use" | "mcp_tool_use" => {
                self.has_tool_use = true;
                self.other_weight += block.input.0 + block.name.as_str().len() as u64;
                self.tool = block.name.as_str().to_owned();
            }
            "tool_result" => self.has_tool_result = true,
            "image" | "document" => self.has_prompt = true,
            _ => {}
        }
    }
}

impl<'de> Deserialize<'de> for Content {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Content;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("anything")
            }
            fn visit_str<E>(self, v: &str) -> Result<Content, E> {
                Ok(Content {
                    has_text: true,
                    has_prompt: true,
                    interrupted: v.starts_with("[Request interrupted"),
                    other_weight: text_weight(v),
                    ..Content::default()
                })
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Content, A::Error> {
                let mut content = Content::default();
                while let Some(block) = seq.next_element::<Obj<Block>>()? {
                    if let Some(block) = block.0 {
                        content.add(block);
                    }
                }
                Ok(content)
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Content, A::Error> {
                while map.next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?.is_some() {}
                Ok(Content::default())
            }
            fn visit_bool<E>(self, _: bool) -> Result<Content, E> {
                Ok(Content::default())
            }
            fn visit_i64<E>(self, _: i64) -> Result<Content, E> {
                Ok(Content::default())
            }
            fn visit_u64<E>(self, _: u64) -> Result<Content, E> {
                Ok(Content::default())
            }
            fn visit_f64<E>(self, _: f64) -> Result<Content, E> {
                Ok(Content::default())
            }
            fn visit_unit<E>(self) -> Result<Content, E> {
                Ok(Content::default())
            }
            fn visit_none<E>(self) -> Result<Content, E> {
                Ok(Content::default())
            }
        }
        d.deserialize_any(V)
    }
}

pub fn update(entry: &mut FileEntry, _ctx: &ScanCtx) -> io::Result<()> {
    let path = entry.path.clone();
    let mut offset = entry.offset;
    let result = tail_lines(&path, &mut offset, |line| handle_line(entry, line));
    entry.offset = offset;
    result
}

/// 按块的体量占比估算思考 token（记录里没有精确值时用）。
fn estimate_reasoning(output: u64, thinking_weight: u64, other_weight: u64) -> u64 {
    let total = thinking_weight + other_weight;
    if total == 0 || thinking_weight == 0 {
        return 0;
    }
    (output as u128 * thinking_weight as u128 / total as u128) as u64
}

fn error_code(error: Option<&ApiError>) -> String {
    let Some(error) = error else {
        return "api_error".to_owned();
    };
    if let Some(code) = error.connection.get().map(|c| c.code.as_str()).filter(|code| !code.is_empty()) {
        // 错误码都是 ECONNRESET 这类短标识；万一不是，就不保留原文
        if code.len() <= 40 && code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
            return code.to_owned();
        }
    }
    if error.status > 0 {
        return format!("HTTP {}", error.status);
    }
    if error.is_network_down.0 {
        return "NETWORK_DOWN".to_owned();
    }
    "api_error".to_owned()
}

fn handle_line(entry: &mut FileEntry, raw: &[u8]) {
    let Ok(line) = serde_json::from_slice::<Line>(raw) else {
        return;
    };
    let FileEntry { parser, session, models, events, net, live, .. } = entry;
    let ParserState::Claude(state) = parser else {
        return;
    };
    let ts = parse_iso_ms(line.timestamp.as_str()).unwrap_or(0);

    if session.id.is_empty() && !line.session_id.is_empty() {
        session.id = line.session_id.as_str().to_owned();
    }
    if session.cwd.is_empty() && !line.cwd.is_empty() {
        session.cwd = line.cwd.as_str().to_owned();
    }
    if !line.version.is_empty() && session.app_version != line.version.as_str() {
        session.app_version = line.version.as_str().to_owned();
    }

    match line.kind.as_str() {
        "assistant" | "user" => {
            if line.is_sidechain.0 {
                state.sidechain_lines += 1;
            } else {
                state.main_lines += 1;
            }
            session.sub_agent = state.sidechain_lines > 0 && state.main_lines == 0;
            touch(session, ts);
        }
        "api-request" | "system" => touch(session, ts),
        _ => {}
    }

    match line.kind.as_str() {
        "assistant" => {
            let Some(message) = line.message.0 else {
                return;
            };
            let content = &message.content;

            live.open_turn(ts);
            if content.has_tool_use {
                live.set_phase(Phase::Tool, ts);
                live.tool = content.tool.clone();
            } else if content.has_text || content.has_thinking {
                live.set_phase(Phase::Writing, ts);
            }

            if let Some(raw_usage) = message.usage.0 {
                let exact_reasoning = raw_usage.output_tokens_details.get().map(|d| d.thinking_tokens).unwrap_or(0);
                let usage = Usage {
                    input: raw_usage.input_tokens,
                    cache_read: raw_usage.cache_read_input_tokens,
                    cache_write: raw_usage.cache_creation_input_tokens,
                    output: raw_usage.output_tokens,
                    reasoning: exact_reasoning.min(raw_usage.output_tokens),
                };
                // 报错时写入的占位消息用量全为 0，不算一次响应
                if !usage.is_zero() {
                    let key = if message.id.is_empty() { 0 } else { fnv64(&[b"claude", message.id.as_str().as_bytes()]) };
                    let think_ms = clamp_ms(line.thinking_duration_ms as i64);
                    let effort = Effort::parse(line.effort.as_str());
                    let known = if key != 0 { state.messages.get_mut(&key) } else { None };
                    match known {
                        Some(track) => {
                            track.thinking_weight += content.thinking_weight;
                            track.other_weight += content.other_weight;
                            track.exact_reasoning |= exact_reasoning > 0;
                            if let Some(event) = events.get_mut(track.event as usize) {
                                // 之前如果是估算值就不参与比较，只保留记录里给出的精确值
                                let previous_exact = if event.reasoning_estimated { 0 } else { event.usage.reasoning };
                                if usage.output >= event.usage.output {
                                    event.usage = usage;
                                }
                                if track.exact_reasoning {
                                    event.usage.reasoning = usage.reasoning.max(previous_exact).min(event.usage.output);
                                    event.reasoning_estimated = false;
                                } else {
                                    event.usage.reasoning = estimate_reasoning(event.usage.output, track.thinking_weight, track.other_weight);
                                    event.reasoning_estimated = event.usage.reasoning > 0;
                                }
                                if ts > event.ts {
                                    event.ts = ts;
                                }
                                if track.request_ms > 0 && ts >= track.request_ms {
                                    event.latency_ms = clamp_ms(ts - track.request_ms);
                                }
                                event.think_ms = event.think_ms.saturating_add(think_ms);
                                if effort != Effort::Unknown {
                                    event.effort = effort;
                                }
                            }
                        }
                        None => {
                            let request_ms = if state.last_request_ms > 0 && ts >= state.last_request_ms && ts - state.last_request_ms < 30 * MINUTE {
                                state.last_request_ms
                            } else {
                                0
                            };
                            state.last_request_ms = 0;
                            let first_block_ms = if request_ms > 0 { clamp_ms(ts - request_ms) } else { 0 };
                            // 记录里只有“内容块写完”的时刻。第一块是思考且记了思考时长时，写完时刻减去思考时长
                            // 就是开始出字的时刻；其他情况推不出首字延迟，记 0。
                            let ttft_ms = if content.has_thinking && think_ms > 0 && first_block_ms > think_ms { first_block_ms - think_ms } else { 0 };
                            let mut usage = usage;
                            let mut estimated = false;
                            if exact_reasoning == 0 {
                                usage.reasoning = estimate_reasoning(usage.output, content.thinking_weight, content.other_weight);
                                estimated = usage.reasoning > 0;
                            }
                            events.push(UsageEvent {
                                ts,
                                usage,
                                model: intern(models, message.model.as_str()),
                                key,
                                latency_ms: first_block_ms,
                                ttft_ms,
                                think_ms,
                                reasoning_estimated: estimated,
                                effort,
                                ..UsageEvent::default()
                            });
                            if key != 0 {
                                state.messages.insert(
                                    key,
                                    MessageTrack {
                                        event: (events.len() - 1) as u32,
                                        request_ms,
                                        thinking_weight: content.thinking_weight,
                                        other_weight: content.other_weight,
                                        exact_reasoning: exact_reasoning > 0,
                                    },
                                );
                            }
                        }
                    }
                    session.context_used = usage.prompt();
                }
            }

            let stop = message.stop_reason.as_str();
            if !content.has_tool_use && matches!(stop, "end_turn" | "stop_sequence" | "max_tokens" | "refusal") {
                live.close_turn(ts, None);
            }
        }
        "user" => {
            let Some(message) = line.message.0 else {
                return;
            };
            let content = &message.content;
            if content.interrupted {
                live.close_turn(ts, None);
            } else if content.has_tool_result {
                live.open_turn(ts);
                live.set_phase(Phase::Thinking, ts);
            } else if content.has_prompt {
                // 新的一轮：上一轮如果没有正常收尾（比如进程被杀），这里直接重新计时
                if live.turn_open && !line.is_sidechain.0 {
                    live.turn_open = false;
                }
                live.open_turn(ts);
                live.set_phase(Phase::Thinking, ts);
            }
        }
        "api-request" => {
            state.last_request_ms = ts;
            if live.turn_open {
                live.set_phase(Phase::Thinking, ts);
            }
        }
        "system" if line.subtype.as_str() == "api_error" => {
            push_net(
                net,
                NetEvent {
                    ts,
                    kind: NetKind::Retry,
                    code: error_code(line.error.get()),
                    attempt: line.retry_attempt as u32,
                    max_attempts: line.max_retries as u32,
                    delay_ms: clamp_ms(line.retry_in_ms as i64),
                },
            );
            if live.turn_open {
                live.set_phase(Phase::Retrying, ts);
            }
        }
        "custom-title" => {
            if !line.custom_title.is_empty() {
                session.title = line.custom_title.as_str().to_owned();
            }
        }
        "agent-name" => {
            if session.title.is_empty() && !line.agent_name.is_empty() {
                session.title = line.agent_name.as_str().to_owned();
            }
        }
        "summary" => {
            if session.title.is_empty() && !line.summary.is_empty() {
                session.title = line.summary.as_str().to_owned();
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
        FileEntry::new(Agent::Claude, PathBuf::from("test.jsonl"))
    }

    #[test]
    fn merges_lines_of_one_message() {
        let mut entry = entry();
        feed(
            &mut entry,
            &[
                r#"{"type":"user","timestamp":"2026-01-01T00:00:00.000Z","sessionId":"s1","cwd":"C:\\work\\app","message":{"role":"user","content":"hi"}}"#,
                r#"{"type":"api-request","timestamp":"2026-01-01T00:00:01.000Z","sessionId":"s1"}"#,
                r#"{"type":"assistant","timestamp":"2026-01-01T00:00:03.000Z","sessionId":"s1","thinkingDurationMs":1500,"message":{"id":"msg_1","model":"claude-x","stop_reason":null,"content":[{"type":"thinking","thinking":"aaaaaaaaaa"}],"usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":100,"cache_creation_input_tokens":20}}}"#,
                r#"{"type":"assistant","timestamp":"2026-01-01T00:00:05.000Z","sessionId":"s1","message":{"id":"msg_1","model":"claude-x","stop_reason":"tool_use","content":[{"type":"tool_use","name":"Bash","input":{"command":"0123456789"}}],"usage":{"input_tokens":10,"output_tokens":40,"cache_read_input_tokens":100,"cache_creation_input_tokens":20}}}"#,
            ],
        );
        assert_eq!(entry.events.len(), 1);
        let event = &entry.events[0];
        assert_eq!(event.usage.input, 10);
        assert_eq!(event.usage.cache_read, 100);
        assert_eq!(event.usage.cache_write, 20);
        assert_eq!(event.usage.output, 40);
        // 第一块（思考）在请求后 2 秒写完，思考用了 1.5 秒，所以 0.5 秒开始出字
        assert_eq!(event.ttft_ms, 500);
        assert_eq!(event.latency_ms, 4000);
        assert_eq!(event.think_ms, 1500);
        // 思考体量 10，其余体量 = 参数 ("command" 7 + 值 10) + 工具名 4 = 21
        assert!(event.reasoning_estimated);
        assert_eq!(event.usage.reasoning, 40 * 10 / 31);
        assert_eq!(entry.session.id, "s1");
        assert_eq!(entry.session.cwd, r"C:\work\app");
        assert!(entry.live.turn_open);
        assert_eq!(entry.live.phase, Phase::Tool);
        assert_eq!(entry.live.tool, "Bash");
    }

    #[test]
    fn exact_thinking_tokens_win_over_estimate() {
        let mut entry = entry();
        feed(
            &mut entry,
            &[
                r#"{"type":"assistant","timestamp":"2026-01-01T00:00:03.000Z","message":{"id":"msg_1","model":"m","content":[{"type":"thinking","thinking":"aaaa"}],"usage":{"output_tokens":5}}}"#,
                r#"{"type":"assistant","timestamp":"2026-01-01T00:00:05.000Z","message":{"id":"msg_1","model":"m","stop_reason":"end_turn","content":[{"type":"text","text":"bbbb"}],"usage":{"output_tokens":100,"output_tokens_details":{"thinking_tokens":70}}}}"#,
            ],
        );
        assert_eq!(entry.events.len(), 1);
        assert_eq!(entry.events[0].usage.reasoning, 70);
        assert!(!entry.events[0].reasoning_estimated);
        assert!(!entry.live.turn_open);
        assert_eq!(entry.live.turns, 1);
        assert_eq!(entry.live.phase, Phase::Idle);
    }

    #[test]
    fn records_retries_and_titles_and_skips_empty_usage() {
        let mut entry = entry();
        feed(
            &mut entry,
            &[
                r#"{"type":"user","timestamp":"2026-01-01T00:00:00.000Z","message":{"content":[{"type":"text","text":"go"}]}}"#,
                r#"{"type":"system","subtype":"api_error","timestamp":"2026-01-01T00:00:02.000Z","error":{"connection":{"code":"ECONNRESET"}},"retryAttempt":2,"maxRetries":10,"retryInMs":1200}"#,
                r#"{"type":"assistant","timestamp":"2026-01-01T00:00:03.000Z","message":{"id":"x","model":"<synthetic>","content":[{"type":"text","text":"error"}],"usage":{"input_tokens":0,"output_tokens":0}}}"#,
                r#"{"type":"custom-title","customTitle":"my session"}"#,
                r#"{"type":"user","timestamp":"2026-01-01T00:00:09.000Z","message":{"content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#,
                "not json at all",
            ],
        );
        assert!(entry.events.is_empty());
        assert_eq!(entry.net.len(), 1);
        assert_eq!(entry.net[0].code, "ECONNRESET");
        assert_eq!(entry.net[0].attempt, 2);
        assert_eq!(entry.net[0].max_attempts, 10);
        assert_eq!(entry.session.title, "my session");
        assert!(!entry.live.turn_open);
    }

    #[test]
    fn sidechain_only_file_is_sub_agent() {
        let mut entry = entry();
        feed(
            &mut entry,
            &[r#"{"type":"user","isSidechain":true,"timestamp":"2026-01-01T00:00:00.000Z","message":{"content":"task"}}"#],
        );
        assert!(entry.session.sub_agent);
    }
}
