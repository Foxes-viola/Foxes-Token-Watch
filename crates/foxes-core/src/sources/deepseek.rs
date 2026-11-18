//! Deepseek Harness：`<dsh-home>/sessions/<slug>/session-<uuid>/session.v4.jsonl.zstd`。
//!
//! - 文件是多个 zstd 帧首尾相接，每次追加一帧；按帧解压后是 JSONL。
//! - 第一行是会话头（字段在顶层），其余行是 `{type, seq, time, data}`。
//! - `assistant/message` 在响应结束后才写入，所以进行中的请求只能显示为“生成中”。
//! - 用量里没有思考 token，按思考文本在输出里的体量占比估算。

use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

use serde::de::{Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;

use super::{clamp_ms, each_line, intern, push_net, touch, FileEntry, ParserState, ScanCtx};
use crate::lenient::{f64_lenient, u64_lenient, NumSum, Obj, ShortCode, Text, TextStat, TimeMs};
use crate::model::{Effort, NetEvent, NetKind, Phase, Usage, UsageEvent};
use crate::timeutil::MINUTE;
use crate::zstdframes::{complete_frames, decompress};

#[derive(Debug, Default)]
pub struct State {
    /// 解压后还没凑成一整行的字节
    carry: Vec<u8>,
    model: String,
    /// 最近一次选定（或请求头里带的）思考强度
    effort: Effort,
    /// 当前这一步请求发出的时刻
    request_ms: i64,
}

#[derive(Deserialize)]
struct Line<'a> {
    #[serde(rename = "type", borrow, default)]
    kind: Text<'a>,
    #[serde(default)]
    time: TimeMs,
    #[serde(borrow, default)]
    id: Text<'a>,
    #[serde(borrow, default)]
    cwd: Text<'a>,
    #[serde(rename = "createdAt", default)]
    created_at: TimeMs,
    #[serde(rename = "delegationDepth", default, deserialize_with = "u64_lenient")]
    delegation_depth: u64,
    #[serde(borrow, default)]
    data: Obj<Data<'a>>,
}

#[derive(Deserialize)]
struct Data<'a> {
    #[serde(default)]
    usage: Obj<RawUsage>,
    #[serde(borrow, default)]
    message: Obj<Message<'a>>,
    #[serde(default)]
    stream: Stream,
    #[serde(borrow, default)]
    title: Text<'a>,
    #[serde(borrow, default)]
    name: Text<'a>,
    #[serde(default)]
    failure: Obj<Failure>,
    #[serde(rename = "delayMs", default, deserialize_with = "u64_lenient")]
    delay_ms: u64,
    #[serde(default, deserialize_with = "u64_lenient")]
    retry: u64,
    #[serde(rename = "maxRetries", default, deserialize_with = "u64_lenient")]
    max_retries: u64,
    #[serde(rename = "contextWindow", default, deserialize_with = "u64_lenient")]
    context_window: u64,
    #[serde(borrow, default)]
    model: Text<'a>,
    #[serde(rename = "reasoningEffort", borrow, default)]
    reasoning_effort: Text<'a>,
    #[serde(borrow, default)]
    header: Obj<Header<'a>>,
}

/// `request/header` 里只取思考强度这一项设置，其余内容不解析。
#[derive(Deserialize)]
struct Header<'a> {
    #[serde(borrow, default)]
    config: Obj<HeaderConfig<'a>>,
}

#[derive(Deserialize)]
struct HeaderConfig<'a> {
    #[serde(rename = "reasoningEffort", borrow, default)]
    reasoning_effort: Text<'a>,
}

#[derive(Deserialize)]
struct RawUsage {
    #[serde(rename = "inputTokens", default, deserialize_with = "u64_lenient")]
    input: u64,
    #[serde(rename = "outputTokens", default, deserialize_with = "u64_lenient")]
    output: u64,
    #[serde(rename = "cacheReadTokens", default, deserialize_with = "u64_lenient")]
    cache_read: u64,
    #[serde(rename = "cacheWriteTokens", default, deserialize_with = "u64_lenient")]
    cache_write: u64,
}

#[derive(Deserialize)]
struct Message<'a> {
    #[serde(borrow, default)]
    source: Obj<MessageSource<'a>>,
}

#[derive(Deserialize)]
struct MessageSource<'a> {
    #[serde(borrow, default)]
    model: Text<'a>,
}

#[derive(Deserialize)]
struct Failure {
    #[serde(default)]
    code: ShortCode,
    #[serde(default, deserialize_with = "u64_lenient")]
    status: u64,
}

#[derive(Deserialize)]
struct StreamPart<'a> {
    #[serde(rename = "type", borrow, default)]
    kind: Text<'a>,
    #[serde(default, deserialize_with = "f64_lenient")]
    time: f64,
    #[serde(default, deserialize_with = "f64_lenient")]
    time0: f64,
    #[serde(default)]
    dt: NumSum,
    #[serde(default)]
    texts: TextStat,
    #[serde(default)]
    args: TextStat,
    #[serde(borrow, default)]
    name: Text<'a>,
}

/// `data.stream` 的概况：只统计时序和各类输出的体量，不保留文本。
#[derive(Debug, Default)]
struct Stream {
    first_time_ms: f64,
    think_ms: f64,
    reasoning_weight: u64,
    other_weight: u64,
}

impl Stream {
    fn add(&mut self, part: StreamPart) {
        for time in [part.time, part.time0] {
            // 只认 epoch 毫秒量级的绝对时刻
            if time > 1e12 && (self.first_time_ms == 0.0 || time < self.first_time_ms) {
                self.first_time_ms = time;
            }
        }
        match part.kind.as_str() {
            "reasoning-chunks" => {
                self.think_ms += part.dt.sum.max(0.0);
                self.reasoning_weight += part.texts.len;
            }
            "text-chunks" => self.other_weight += part.texts.len,
            "tool-call-chunks" => self.other_weight += part.args.len + part.name.as_str().len() as u64,
            _ => {}
        }
    }
}

impl<'de> Deserialize<'de> for Stream {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Stream;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("anything")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Stream, A::Error> {
                let mut stream = Stream::default();
                while let Some(part) = seq.next_element::<Obj<StreamPart>>()? {
                    if let Some(part) = part.0 {
                        stream.add(part);
                    }
                }
                Ok(stream)
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Stream, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(Stream::default())
            }
            fn visit_str<E>(self, _: &str) -> Result<Stream, E> {
                Ok(Stream::default())
            }
            fn visit_bool<E>(self, _: bool) -> Result<Stream, E> {
                Ok(Stream::default())
            }
            fn visit_i64<E>(self, _: i64) -> Result<Stream, E> {
                Ok(Stream::default())
            }
            fn visit_u64<E>(self, _: u64) -> Result<Stream, E> {
                Ok(Stream::default())
            }
            fn visit_f64<E>(self, _: f64) -> Result<Stream, E> {
                Ok(Stream::default())
            }
            fn visit_unit<E>(self) -> Result<Stream, E> {
                Ok(Stream::default())
            }
            fn visit_none<E>(self) -> Result<Stream, E> {
                Ok(Stream::default())
            }
        }
        d.deserialize_any(V)
    }
}

pub fn update(entry: &mut FileEntry, _ctx: &ScanCtx) -> io::Result<()> {
    let mut file = File::open(&entry.path)?;
    file.seek(SeekFrom::Start(entry.offset))?;
    let mut compressed = Vec::new();
    file.read_to_end(&mut compressed)?;

    let frames = complete_frames(&compressed);
    let mut carry = match &mut entry.parser {
        ParserState::Deepseek(state) => std::mem::take(&mut state.carry),
        _ => Vec::new(),
    };
    let mut failed = frames.corrupt;
    for range in &frames.ranges {
        match decompress(&compressed[range.clone()]) {
            Ok(data) => {
                carry.extend_from_slice(&data);
                let consumed = each_line(&carry, &mut |line| handle_line(entry, line));
                carry.drain(..consumed);
            }
            Err(_) => {
                failed = true;
                break;
            }
        }
    }
    // 最后一行没有换行但已经是完整 JSON 时直接处理
    if !carry.is_empty() && serde_json::from_slice::<IgnoredAny>(&carry).is_ok() {
        handle_line(entry, &carry);
        carry.clear();
    }
    if let ParserState::Deepseek(state) = &mut entry.parser {
        state.carry = carry;
    }
    entry.offset += frames.consumed as u64;
    // 遇到坏帧就停在这里，等文件再变化时从断点重试
    entry.corrupt = failed;
    Ok(())
}

fn estimate_reasoning(output: u64, stream: &Stream) -> u64 {
    let total = stream.reasoning_weight + stream.other_weight;
    if total == 0 || stream.reasoning_weight == 0 {
        return 0;
    }
    (output as u128 * stream.reasoning_weight as u128 / total as u128) as u64
}

fn handle_line(entry: &mut FileEntry, raw: &[u8]) {
    let Ok(line) = serde_json::from_slice::<Line>(raw) else {
        return;
    };
    let FileEntry { parser, session, models, events, net, live, .. } = entry;
    let ParserState::Deepseek(state) = parser else {
        return;
    };
    let ts = line.time.0;
    let kind = line.kind.as_str();

    if kind == "session" {
        if !line.id.is_empty() {
            session.id = line.id.as_str().to_owned();
        }
        if !line.cwd.is_empty() {
            session.cwd = line.cwd.as_str().to_owned();
        }
        session.sub_agent = line.delegation_depth > 0;
        touch(session, line.created_at.0);
        return;
    }

    touch(session, ts);
    let data = line.data.0;
    match kind {
        "turn/start" => {
            live.turn_open = false;
            live.open_turn(ts);
            live.set_phase(Phase::Generating, ts);
        }
        "step/start" => {
            state.request_ms = ts;
            live.open_turn(ts);
            live.set_phase(Phase::Generating, ts);
        }
        "assistant/message" => {
            let Some(data) = data else {
                return;
            };
            let Some(raw_usage) = data.usage.0 else {
                return;
            };
            let reasoning = estimate_reasoning(raw_usage.output, &data.stream);
            let usage = Usage {
                input: raw_usage.input,
                cache_read: raw_usage.cache_read,
                cache_write: raw_usage.cache_write,
                output: raw_usage.output,
                reasoning,
            };
            if usage.is_zero() {
                return;
            }
            let model = data.message.get().and_then(|m| m.source.get()).map(|s| s.model.as_str()).filter(|m| !m.is_empty()).unwrap_or(&state.model);
            let in_window = |start: i64, end: i64| start > 0 && end >= start && end - start < 30 * MINUTE;
            let latency_ms = if in_window(state.request_ms, ts) { clamp_ms(ts - state.request_ms) } else { 0 };
            let first_ms = data.stream.first_time_ms as i64;
            let ttft_ms = if in_window(state.request_ms, first_ms) { clamp_ms(first_ms - state.request_ms) } else { 0 };
            events.push(UsageEvent {
                ts,
                usage,
                model: intern(models, model),
                key: 0,
                latency_ms,
                ttft_ms,
                think_ms: clamp_ms(data.stream.think_ms as i64),
                reasoning_estimated: reasoning > 0,
                effort: state.effort,
                ..UsageEvent::default()
            });
            session.context_used = usage.prompt();
            state.request_ms = 0;
        }
        "tool/call" => {
            live.open_turn(ts);
            live.set_phase(Phase::Tool, ts);
            live.tool = data.map(|d| d.name.as_str().to_owned()).unwrap_or_default();
        }
        "tool/result" => {
            if live.turn_open {
                live.set_phase(Phase::Generating, ts);
            }
        }
        "llm/retry" => {
            let Some(data) = data else {
                return;
            };
            let code = match data.failure.get() {
                Some(failure) if !failure.code.0.is_empty() => failure.code.0.clone(),
                Some(failure) if failure.status > 0 => format!("HTTP {}", failure.status),
                _ => "retry".to_owned(),
            };
            push_net(
                net,
                NetEvent {
                    ts,
                    kind: NetKind::Retry,
                    code,
                    attempt: data.retry as u32,
                    max_attempts: data.max_retries as u32,
                    delay_ms: clamp_ms(data.delay_ms as i64),
                },
            );
            if live.turn_open {
                live.set_phase(Phase::Retrying, ts);
            }
        }
        "llm/retry-started" => {
            if live.turn_open {
                live.set_phase(Phase::Generating, ts);
            }
        }
        "turn/end" => live.close_turn(ts, None),
        "session/title" => {
            if let Some(title) = data.map(|d| d.title.as_str().to_owned()).filter(|t| !t.is_empty()) {
                session.title = title;
            }
        }
        "request/context" => {
            if let Some(data) = data {
                if data.context_window > 0 {
                    session.context_window = data.context_window;
                }
                if !data.model.is_empty() {
                    state.model = data.model.as_str().to_owned();
                }
            }
        }
        "model/selection" => {
            if let Some(effort) = data.as_ref().map(|d| d.reasoning_effort.as_str()).filter(|effort| !effort.is_empty()) {
                state.effort = Effort::parse(effort);
            }
        }
        "request/header" => {
            let config = data.as_ref().and_then(|d| d.header.get()).and_then(|header| header.config.get());
            if let Some(effort) = config.map(|config| config.reasoning_effort.as_str()).filter(|effort| !effort.is_empty()) {
                state.effort = Effort::parse(effort);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Agent;
    use std::io::Write;

    const LINES: &[&str] = &[
        r#"{"type":"session","version":4,"id":"sess-1","createdAt":1767225600000,"cwd":"C:\\work\\app","delegationDepth":0}"#,
        r#"{"type":"turn/start","seq":1,"time":1767225601000,"data":{"turn":1}}"#,
        r#"{"type":"request/context","seq":2,"time":1767225601100,"data":{"contextWindow":1000000,"model":"deepseek-x"}}"#,
        r#"{"type":"step/start","seq":3,"time":1767225602000,"data":{"turn":1,"step":1}}"#,
        r#"{"type":"llm/retry","seq":4,"time":1767225603000,"data":{"failure":{"code":"ETIMEDOUT","message":"long text that must not be kept"},"delayMs":500,"retry":1,"maxRetries":5}}"#,
        r#"{"type":"assistant/message","seq":5,"time":1767225605000,"data":{"usage":{"inputTokens":100,"outputTokens":60,"cacheReadTokens":900,"cacheWriteTokens":10,"totalTokens":1070},"message":{"source":{"model":"deepseek-flash"},"content":[]},"stream":[{"type":"chunk","time":1767225602800,"chunk":{}},{"type":"reasoning-chunks","time0":1767225602900,"dt":[100,200,50],"texts":["aaaa","bbbb","cc"]},{"type":"text-chunks","time0":1767225603400,"dt":[10],"texts":["dddddddddd"]},{"type":"tool-call-chunks","time0":1767225603500,"dt":[5],"name":"bash","args":["123456"]}]}}"#,
        r#"{"type":"tool/call","seq":6,"time":1767225605100,"data":{"callId":"c1","name":"bash"}}"#,
    ];

    fn write_frames(path: &std::path::Path, chunks: &[&[&str]], append: bool) {
        let mut file = std::fs::OpenOptions::new().create(true).write(true).append(append).truncate(!append).open(path).unwrap();
        for chunk in chunks {
            let mut text = String::new();
            for line in *chunk {
                text.push_str(line);
                text.push('\n');
            }
            file.write_all(&zstd::stream::encode_all(text.as_bytes(), 3).unwrap()).unwrap();
        }
    }

    #[test]
    fn parses_frames_incrementally() {
        let dir = std::env::temp_dir().join(format!("foxes-core-dsh-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.v4.jsonl.zstd");
        write_frames(&path, &[&LINES[..2], &LINES[2..6]], false);

        let models = std::sync::Mutex::new(std::collections::HashMap::new());
        let ctx = ScanCtx::new(0, &models);
        let mut entry = FileEntry::new(Agent::Deepseek, path.clone());
        update(&mut entry, &ctx).unwrap();

        assert_eq!(entry.session.id, "sess-1");
        assert_eq!(entry.session.started_ms, 1_767_225_600_000);
        assert_eq!(entry.session.context_window, 1_000_000);
        assert_eq!(entry.events.len(), 1);
        let event = &entry.events[0];
        assert_eq!(event.usage.total(), 1070);
        assert_eq!(event.latency_ms, 3000);
        assert_eq!(event.ttft_ms, 800);
        assert_eq!(event.think_ms, 350);
        // 思考体量 10，其余 = 正文 10 + 参数 6 + 工具名 4 = 20
        assert_eq!(event.usage.reasoning, 60 * 10 / 30);
        assert!(event.reasoning_estimated);
        assert_eq!(entry.models, vec!["deepseek-flash"]);
        assert_eq!(entry.net.len(), 1);
        assert_eq!(entry.net[0].code, "ETIMEDOUT");
        assert!(entry.live.turn_open);
        assert_eq!(entry.offset, std::fs::metadata(&path).unwrap().len());

        // 追加一帧：只解析新增部分
        write_frames(&path, &[&LINES[6..]], true);
        update(&mut entry, &ctx).unwrap();
        assert_eq!(entry.events.len(), 1);
        assert_eq!(entry.live.phase, Phase::Tool);
        assert_eq!(entry.live.tool, "bash");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reads_reasoning_effort() {
        let mut entry = FileEntry::new(Agent::Deepseek, std::path::PathBuf::from("session.v4.jsonl.zstd"));
        let lines = [
            LINES[5],
            r#"{"type":"model/selection","seq":6,"time":1767225606000,"data":{"model":"deepseek-x","reasoningEffort":"high"}}"#,
            LINES[5],
            r#"{"type":"request/header","seq":8,"time":1767225607000,"data":{"header":{"config":{"reasoningEffort":"off","other":{"nested":true}}}}}"#,
            LINES[5],
        ];
        for line in lines {
            handle_line(&mut entry, line.as_bytes());
        }
        let efforts: Vec<Effort> = entry.events.iter().map(|event| event.effort).collect();
        assert_eq!(efforts, vec![Effort::Unknown, Effort::High, Effort::Off]);
    }
}
