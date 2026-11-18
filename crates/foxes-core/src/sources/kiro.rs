//! Kiro：`~/.kiro/sessions/<工作区哈希>/sess_<uuid>/`。
//!
//! - `messages.jsonl`：主对话的记录，每行 `{id, timestamp, payload: {type, ...}}`。
//! - `sub-executions/<uuid>.jsonl`：子代理的记录（只有 assistant / tool_call / tool_result）。
//! - `session.json`：标题、工作区、模型、状态（整个文件会被重写，每次重新读）。
//!
//! 记录里没有 token 数，只有两样硬数据：
//! - 主代理每次请求前写一条 `session_metadata/contextUsage`，百分比 × 100 万就是这次请求的精确提示词 token；
//! - 每轮结束时的 `usage_summary` 给出这一轮消耗的 credits 和请求数（含子代理）。
//!
//! 于是主代理的每次请求记一条用量：提示词用精确读数，按缓存规则拆成命中 / 未命中，输出按文本体量估算。
//! 子代理的请求没有上下文读数，按它自己的内容累计估算。credits 记在一轮的最后一条用量上。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;
use serde_json::Value;

use super::{clamp_ms, intern, push_net, push_tool, tail_lines, touch, FileEntry, ParserState, ScanCtx};
use crate::estimate::{context_growth, output_of, split_prompt, BASE_CONTEXT, CACHE_TTL_MS};
use crate::lenient::{f64_lenient, u64_lenient, visit_objects, Obj, Text, Weight};
use crate::model::{NetEvent, NetKind, Phase, RateLimits, RateWindow, ToolCall, UsageEvent};
use crate::timeutil::{parse_iso_ms, MINUTE};

/// Kiro 的模型上下文窗口（插件里 `maxInputTokens ?? 1e6`，按百分比换算也用它）。
const WINDOW: u64 = 1_000_000;

/// 一次模型调用在记录里的中间状态。
#[derive(Debug, Default, Clone, Copy)]
struct Call {
    /// 这次调用对应的事件下标
    event: u32,
    /// 输出里正文 / 工具参数、思考的体量
    text_weight: u64,
    thinking_weight: u64,
}

#[derive(Debug, Default)]
pub struct State {
    /// 是不是子代理的记录文件
    sub: bool,
    /// `session.json` 的签名（大小、修改时间），变了才重读
    meta_sig: (u64, i64),
    model: String,
    /// 最近一次上下文读数（token）和读数时刻
    context: u64,
    context_ms: i64,
    /// 上一次请求的提示词（用来判断缓存命中）和发出时刻
    last_prompt: u64,
    last_request_ms: i64,
    /// 还在写输出的这次调用
    call: Option<Call>,
    /// 子代理：没有读数，按内容累计的上下文估算
    sub_context: u64,
    /// 本轮已经记下的第一个事件（credits 和首字延迟记在本轮的事件上）
    turn_first_event: Option<u32>,
    /// 本轮等待用户确认的工具调用数
    pending: u32,
}

#[derive(Deserialize)]
struct Line<'a> {
    #[serde(borrow, default)]
    timestamp: Text<'a>,
    #[serde(borrow, default)]
    payload: Obj<Payload<'a>>,
}

#[derive(Deserialize)]
struct Payload<'a> {
    #[serde(rename = "type", borrow, default)]
    kind: Text<'a>,
    /// 正文（assistant / user / tool_result）。只统计体量，不保留内容
    #[serde(default)]
    content: Weight,
    #[serde(default)]
    args: Weight,
    #[serde(rename = "operationType", borrow, default)]
    operation_type: Text<'a>,
    #[serde(rename = "reasoningModelId", borrow, default)]
    reasoning_model_id: Text<'a>,
    #[serde(rename = "toolName", borrow, default)]
    tool_name: Text<'a>,
    #[serde(borrow, default)]
    status: Text<'a>,
    #[serde(borrow, default)]
    key: Text<'a>,
    #[serde(default)]
    value: Obj<MetaValue<'a>>,
    #[serde(rename = "stopReason", borrow, default)]
    stop_reason: Text<'a>,
    #[serde(rename = "elapsedTime", default, deserialize_with = "u64_lenient")]
    elapsed_time: u64,
    #[serde(rename = "promptTurnSummaries", default)]
    summaries: Credits,
    #[serde(rename = "requestIds", default)]
    request_ids: Count,
    #[serde(default)]
    success: Option<bool>,
    #[serde(rename = "interactionType", borrow, default)]
    interaction_type: Text<'a>,
}

#[derive(Deserialize)]
struct MetaValue<'a> {
    #[serde(rename = "usagePercentage", default, deserialize_with = "f64_lenient")]
    usage_percentage: f64,
    #[serde(rename = "errorType", borrow, default)]
    error_type: Text<'a>,
}

/// `promptTurnSummaries[].usage` 的合计（只算单位是 credit 的）。
#[derive(Debug, Default)]
struct Credits(f64);

#[derive(Deserialize)]
struct Summary<'a> {
    #[serde(borrow, default)]
    unit: Text<'a>,
    #[serde(default, deserialize_with = "f64_lenient")]
    usage: f64,
}

impl<'de> Deserialize<'de> for Credits {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut total = 0.0;
        visit_objects(d, |summary: Summary| {
            let unit = summary.unit.as_str();
            if (unit.is_empty() || unit.eq_ignore_ascii_case("credit")) && summary.usage.is_finite() && summary.usage > 0.0 {
                total += summary.usage;
            }
        })?;
        Ok(Credits(total))
    }
}

/// 数组的长度（不保留内容）。
#[derive(Debug, Default)]
struct Count(u32);

impl<'de> Deserialize<'de> for Count {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Count;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("anything")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Count, A::Error> {
                let mut count = 0;
                while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {
                    count += 1;
                }
                Ok(Count(count))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Count, A::Error> {
                while map.next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?.is_some() {}
                Ok(Count(0))
            }
            fn visit_str<E>(self, _: &str) -> Result<Count, E> {
                Ok(Count(0))
            }
            fn visit_bool<E>(self, _: bool) -> Result<Count, E> {
                Ok(Count(0))
            }
            fn visit_i64<E>(self, _: i64) -> Result<Count, E> {
                Ok(Count(0))
            }
            fn visit_u64<E>(self, _: u64) -> Result<Count, E> {
                Ok(Count(0))
            }
            fn visit_f64<E>(self, _: f64) -> Result<Count, E> {
                Ok(Count(0))
            }
            fn visit_unit<E>(self) -> Result<Count, E> {
                Ok(Count(0))
            }
            fn visit_none<E>(self) -> Result<Count, E> {
                Ok(Count(0))
            }
        }
        d.deserialize_any(V)
    }
}

#[derive(Deserialize)]
struct SessionFile<'a> {
    #[serde(borrow, default)]
    id: Text<'a>,
    #[serde(borrow, default)]
    title: Text<'a>,
    #[serde(rename = "modelId", borrow, default)]
    model_id: Text<'a>,
    #[serde(rename = "workspacePaths", default)]
    workspace_paths: FirstText,
    #[serde(rename = "rootPaths", default)]
    root_paths: FirstText,
    #[serde(rename = "createdAt", borrow, default)]
    created_at: Text<'a>,
}

/// 字符串数组里的第一个。
#[derive(Debug, Default)]
struct FirstText(String);

impl<'de> Deserialize<'de> for FirstText {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = FirstText;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("anything")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<FirstText, A::Error> {
                let mut first = String::new();
                while let Some(item) = seq.next_element::<Text>()? {
                    if first.is_empty() && !item.is_empty() {
                        first = item.as_str().to_owned();
                    }
                }
                Ok(FirstText(first))
            }
            fn visit_str<E>(self, v: &str) -> Result<FirstText, E> {
                Ok(FirstText(v.to_owned()))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<FirstText, A::Error> {
                while map.next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?.is_some() {}
                Ok(FirstText::default())
            }
            fn visit_bool<E>(self, _: bool) -> Result<FirstText, E> {
                Ok(FirstText::default())
            }
            fn visit_i64<E>(self, _: i64) -> Result<FirstText, E> {
                Ok(FirstText::default())
            }
            fn visit_u64<E>(self, _: u64) -> Result<FirstText, E> {
                Ok(FirstText::default())
            }
            fn visit_f64<E>(self, _: f64) -> Result<FirstText, E> {
                Ok(FirstText::default())
            }
            fn visit_unit<E>(self) -> Result<FirstText, E> {
                Ok(FirstText::default())
            }
            fn visit_none<E>(self) -> Result<FirstText, E> {
                Ok(FirstText::default())
            }
        }
        d.deserialize_any(V)
    }
}

/// 会话目录：主记录在 `<会话>/messages.jsonl`，子代理在 `<会话>/sub-executions/<id>.jsonl`。
fn session_dir(path: &Path) -> Option<PathBuf> {
    let parent = path.parent()?;
    if parent.file_name().is_some_and(|name| name == "sub-executions") {
        parent.parent().map(Path::to_path_buf)
    } else {
        Some(parent.to_path_buf())
    }
}

/// 模型名去掉 `qdev::` 这类前缀。
fn model_name(raw: &str) -> &str {
    raw.rsplit("::").next().unwrap_or(raw)
}

fn load_session_file(entry: &mut FileEntry) {
    let Some(dir) = session_dir(&entry.path) else {
        return;
    };
    let path = dir.join("session.json");
    let Ok(meta) = fs::metadata(&path) else {
        return;
    };
    let sig = (meta.len(), meta.modified().map(crate::timeutil::system_time_ms).unwrap_or(0));
    let FileEntry { parser, session, .. } = entry;
    let ParserState::Kiro(state) = parser else {
        return;
    };
    if sig == state.meta_sig || meta.len() > 1024 * 1024 {
        return;
    }
    state.meta_sig = sig;
    let Ok(bytes) = fs::read(&path) else {
        return;
    };
    let Ok(file) = serde_json::from_slice::<SessionFile>(&bytes) else {
        return;
    };
    let session_id = file.id.as_str().strip_prefix("sess_").unwrap_or(file.id.as_str());
    if !state.sub && !session_id.is_empty() {
        session.id = session_id.to_owned();
    }
    if !file.title.is_empty() && !state.sub {
        session.title = file.title.as_str().to_owned();
    }
    let cwd = if file.workspace_paths.0.is_empty() { &file.root_paths.0 } else { &file.workspace_paths.0 };
    if !cwd.is_empty() {
        session.cwd = cwd.clone();
    }
    if !file.model_id.is_empty() {
        state.model = model_name(file.model_id.as_str()).to_owned();
    }
    touch(session, parse_iso_ms(file.created_at.as_str()).unwrap_or(0));
}

pub fn update(entry: &mut FileEntry, _ctx: &ScanCtx) -> io::Result<()> {
    if let ParserState::Kiro(state) = &mut entry.parser {
        state.sub = entry.path.parent().and_then(Path::file_name).is_some_and(|name| name == "sub-executions");
    }
    if entry.session.id.is_empty() {
        if let ParserState::Kiro(state) = &entry.parser {
            if state.sub {
                // 子代理归到所属会话下：会话 id 取目录名
                let parent = session_dir(&entry.path).and_then(|dir| dir.file_name().map(|name| name.to_string_lossy().into_owned())).unwrap_or_default();
                entry.session.id = parent.strip_prefix("sess_").unwrap_or(&parent).to_owned();
                entry.session.sub_agent = true;
            }
        }
    }
    load_session_file(entry);
    let path = entry.path.clone();
    let mut offset = entry.offset;
    let result = tail_lines(&path, &mut offset, |line| handle_line(entry, line));
    entry.offset = offset;
    result
}

/// 把还在进行的调用收尾：按已经看到的输出体量估算输出 token。
fn finish_call(state: &mut State, events: &mut [UsageEvent]) {
    let Some(call) = state.call.take() else {
        return;
    };
    let (output, reasoning) = output_of(call.text_weight, call.thinking_weight);
    if let Some(event) = events.get_mut(call.event as usize) {
        event.usage.output = output;
        event.usage.reasoning = reasoning;
        event.reasoning_estimated = reasoning > 0;
    }
}

fn handle_line(entry: &mut FileEntry, raw: &[u8]) {
    let Ok(line) = serde_json::from_slice::<Line>(raw) else {
        return;
    };
    let Some(payload) = line.payload.0 else {
        return;
    };
    let ts = parse_iso_ms(line.timestamp.as_str()).unwrap_or(0);
    let FileEntry { parser, session, models, events, net, live, tools, .. } = entry;
    let ParserState::Kiro(state) = parser else {
        return;
    };
    touch(session, ts);
    let kind = payload.kind.as_str();

    match kind {
        "turn_start" => {
            finish_call(state, events);
            // 上一轮没有正常收尾（进程退出等）时，直接从这一轮重新计时
            live.turn_open = false;
            live.open_turn(ts);
            live.set_phase(Phase::Requesting, ts);
            state.turn_first_event = None;
            state.pending = 0;
        }
        "user" => {
            if !live.turn_open {
                live.open_turn(ts);
                live.set_phase(Phase::Requesting, ts);
            }
            // 用户输入进入上下文（主代理下次请求会有精确读数，这里只管子代理）
            state.sub_context += context_growth(payload.content.0);
        }
        "session_metadata" => {
            let Some(value) = payload.value.0 else {
                return;
            };
            match payload.key.as_str() {
                "contextUsage" if value.usage_percentage > 0.0 => {
                    // 主代理每次请求前都会写一条：这就是一次新的模型调用
                    let prompt = ((value.usage_percentage / 100.0) * WINDOW as f64).round().max(0.0) as u64;
                    state.context = prompt;
                    state.context_ms = ts;
                    session.context_used = prompt;
                    session.context_window = WINDOW;
                }
                "displayError" => {
                    let code = value.error_type.as_str();
                    let code = if let Some(status) = code.strip_prefix("HttpError") {
                        format!("HTTP {status}")
                    } else if crate::lenient::is_short_code(code) {
                        code.to_owned()
                    } else {
                        "error".to_owned()
                    };
                    push_net(net, NetEvent { ts, kind: NetKind::Error, code, attempt: 0, max_attempts: 0, delay_ms: 0 });
                    if live.turn_open {
                        live.set_phase(Phase::Retrying, ts);
                    }
                }
                _ => {}
            }
        }
        "assistant" => {
            let model = if payload.reasoning_model_id.is_empty() { state.model.clone() } else { model_name(payload.reasoning_model_id.as_str()).to_owned() };
            let thinking = payload.operation_type.as_str() == "Reasoning";
            let weight = payload.content.0;
            // 主代理：有新的上下文读数就是一次新调用；子代理：上一次调用之后又有工具结果就是新调用
            let fresh_reading = !state.sub && state.context_ms > 0 && state.context_ms > state.last_request_ms;
            let new_call = state.call.is_none() || fresh_reading;
            if new_call {
                finish_call(state, events);
                let prompt = if state.sub {
                    if state.sub_context == 0 {
                        state.sub_context = BASE_CONTEXT;
                    }
                    state.sub_context
                } else if state.context > 0 {
                    state.context
                } else {
                    BASE_CONTEXT
                };
                let request_ms = if state.sub { ts } else { state.context_ms.max(ts - MINUTE).min(ts) };
                let cache_alive = state.last_request_ms > 0 && request_ms - state.last_request_ms < CACHE_TTL_MS;
                let usage = split_prompt(prompt, state.last_prompt, cache_alive);
                let latency_ms = if !state.sub && state.context_ms > 0 && ts >= state.context_ms { clamp_ms(ts - state.context_ms) } else { 0 };
                events.push(UsageEvent {
                    ts,
                    usage,
                    model: intern(models, &model),
                    latency_ms,
                    estimated: true,
                    ..UsageEvent::default()
                });
                let index = (events.len() - 1) as u32;
                if state.turn_first_event.is_none() {
                    state.turn_first_event = Some(index);
                }
                state.call = Some(Call { event: index, ..Call::default() });
                state.last_prompt = prompt;
                state.last_request_ms = request_ms.max(1);
            }
            if let Some(call) = state.call.as_mut() {
                if thinking {
                    call.thinking_weight += weight;
                } else {
                    call.text_weight += weight;
                }
                if let Some(event) = events.get_mut(call.event as usize) {
                    event.ts = event.ts.max(ts);
                }
            }
            if state.sub {
                state.sub_context += context_growth(weight);
            }
            live.open_turn(ts);
            live.set_phase(if thinking { Phase::Thinking } else { Phase::Writing }, ts);
        }
        "tool_call" => {
            if let Some(call) = state.call.as_mut() {
                call.text_weight += payload.args.0 + payload.tool_name.as_str().len() as u64;
            }
            if state.sub {
                state.sub_context += context_growth(payload.args.0);
            }
            let name = payload.tool_name.as_str();
            match payload.status.as_str() {
                "completed" | "failed" | "denied" => {
                    if !name.is_empty() {
                        push_tool(
                            tools,
                            ToolCall { ts, name: name.chars().take(64).collect(), server: String::new(), ok: payload.status.as_str() == "completed" },
                        );
                    }
                    if live.turn_open {
                        live.set_phase(Phase::Thinking, ts);
                    }
                }
                _ => {
                    live.open_turn(ts);
                    live.set_phase(Phase::Tool, ts);
                    live.tool = name.chars().take(64).collect();
                }
            }
        }
        "tool_result" => {
            // 工具结果回到上下文，模型会再请求一次
            finish_call(state, events);
            if state.sub {
                state.sub_context += context_growth(payload.content.0);
            }
            if payload.success == Some(false) {
                if let Some(last) = tools.last_mut() {
                    if last.ts <= ts && ts - last.ts < MINUTE {
                        last.ok = false;
                    }
                }
            }
            if live.turn_open {
                live.set_phase(Phase::Requesting, ts);
            }
        }
        "pending_interaction" => {
            if payload.interaction_type.is_empty() || payload.interaction_type.as_str() == "tool_approval" {
                state.pending += 1;
                live.open_turn(ts);
                live.set_phase(Phase::Waiting, ts);
            }
        }
        "interaction_resolved" => {
            state.pending = state.pending.saturating_sub(1);
            if live.turn_open && state.pending == 0 {
                live.set_phase(Phase::Tool, ts);
            }
        }
        "usage_summary" => {
            finish_call(state, events);
            let credits = payload.summaries.0;
            if credits > 0.0 {
                // credits 记在这一轮的第一个事件上；这一轮一次请求都没记下时补一条只有 credits 的事件
                match state.turn_first_event.and_then(|index| events.get_mut(index as usize)) {
                    Some(event) => event.credits += credits as f32,
                    None => {
                        events.push(UsageEvent { ts, model: intern(models, &state.model), estimated: true, credits: credits as f32, ..UsageEvent::default() });
                        state.turn_first_event = Some((events.len() - 1) as u32);
                    }
                }
            }
            let _ = payload.request_ids.0;
            if payload.status.as_str() == "failed" {
                push_net(net, NetEvent { ts, kind: NetKind::Error, code: "turn_failed".into(), attempt: 0, max_attempts: 0, delay_ms: 0 });
            }
            if payload.elapsed_time > 0 {
                live.close_turn(ts, Some(payload.elapsed_time));
            }
        }
        "turn_end" => {
            finish_call(state, events);
            let _ = payload.stop_reason.as_str();
            live.close_turn(ts, None);
            state.pending = 0;
        }
        _ => {}
    }
}

/// Kiro 客户端缓存的账户额度：`<Kiro 用户目录>/User/globalStorage/state.vscdb` 里 `kiro.kiroAgent` 的
/// `kiro.resourceNotifications.usageState`，Kiro 运行时向服务端查询后写入（`timestamp` 是查询时刻）。
/// 主额度是按月重置的 credits；试用等赠送额度合在一起，到期时间取最早的那份。
pub fn read_usage_limits(db: &Path) -> Option<RateLimits> {
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).ok()?;
    conn.busy_timeout(Duration::from_millis(300)).ok()?;
    let raw = conn
        .query_row("SELECT value FROM ItemTable WHERE key = 'kiro.kiroAgent'", [], |row| {
            Ok(match row.get_ref(0)? {
                ValueRef::Text(bytes) | ValueRef::Blob(bytes) => bytes.to_vec(),
                _ => Vec::new(),
            })
        })
        .ok()?;
    let value: Value = serde_json::from_slice(&raw).ok()?;
    let state = &value["kiro.resourceNotifications.usageState"];
    let breakdowns = state["usageBreakdowns"].as_array()?;
    let credit = breakdowns
        .iter()
        .find(|item| item["type"].as_str().is_some_and(|kind| kind.eq_ignore_ascii_case("credit")))
        .or_else(|| breakdowns.first())?;
    let amount = |value: &Value| value.as_f64().filter(|n| n.is_finite() && *n >= 0.0).unwrap_or(0.0);
    let unit = credit["displayNamePlural"].as_str().filter(|unit| !unit.is_empty()).unwrap_or("credits").to_lowercase();

    let (used, limit) = (amount(&credit["currentUsage"]), amount(&credit["usageLimit"]));
    let monthly = RateWindow {
        used_percent: credit["percentageUsed"].as_f64().filter(|p| p.is_finite()).unwrap_or(if limit > 0.0 { used / limit * 100.0 } else { 0.0 }),
        resets_at_ms: credit["resetDate"].as_str().and_then(parse_iso_ms).unwrap_or(0),
        used,
        limit,
        unit: unit.clone(),
        label: "每月额度".into(),
        ..RateWindow::default()
    };
    let mut bonus = RateWindow { unit, label: "赠送额度".into(), expires: true, ..RateWindow::default() };
    for item in credit["bonuses"].as_array().into_iter().flatten() {
        if item["status"].as_str().is_some_and(|status| !status.eq_ignore_ascii_case("active")) {
            continue;
        }
        bonus.used += amount(&item["currentUsage"]);
        bonus.limit += amount(&item["usageLimit"]);
        if let Some(at) = item["expiryDate"].as_str().and_then(parse_iso_ms) {
            if bonus.resets_at_ms == 0 || at < bonus.resets_at_ms {
                bonus.resets_at_ms = at;
            }
        }
    }
    bonus.used_percent = if bonus.limit > 0.0 { bonus.used / bonus.limit * 100.0 } else { 0.0 };
    Some(RateLimits {
        ts: state["timestamp"].as_i64().unwrap_or(0),
        primary: Some(monthly),
        secondary: (bonus.limit > 0.0).then_some(bonus),
        plan: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Agent;
    use std::sync::Mutex;

    fn ctx() -> (Mutex<std::collections::HashMap<u64, String>>,) {
        (Mutex::new(std::collections::HashMap::new()),)
    }

    const MAIN: &[&str] = &[
        r#"{"id":"u1","timestamp":"2026-11-05T13:04:50.251Z","payload":{"type":"user","content":"hello there","documents":[],"images":[]}}"#,
        r#"{"id":"t1","timestamp":"2026-11-05T13:04:50.266Z","payload":{"type":"turn_start","executionId":"e1","turnId":"e1"}}"#,
        r#"{"id":"m1","timestamp":"2026-11-05T13:04:50.300Z","payload":{"type":"session_metadata","key":"contextUsage","value":{"usagePercentage":3.1846},"executionId":"e1"}}"#,
        r#"{"id":"a1-say","timestamp":"2026-11-05T13:05:01.316Z","payload":{"type":"assistant","content":"thinking about it","operationType":"Reasoning","reasoningModelId":"qdev::claude-opus-5.5","executionId":"e1","turnId":"e1"}}"#,
        r#"{"id":"c1","timestamp":"2026-11-05T13:05:01.400Z","payload":{"type":"tool_call","toolName":"execute_pwsh","status":"approved","args":{"command":"dir"},"toolCallId":"x1","turnId":"e1"}}"#,
        r#"{"id":"p1","timestamp":"2026-11-05T13:05:01.500Z","payload":{"type":"pending_interaction","interactionType":"tool_approval","toolCallId":"x1"}}"#,
    ];

    const REST: &[&str] = &[
        r#"{"id":"r1","timestamp":"2026-11-05T13:05:03.000Z","payload":{"type":"interaction_resolved","outcome":"selected","toolCallId":"x1"}}"#,
        r#"{"id":"c1","timestamp":"2026-11-05T13:05:04.000Z","payload":{"type":"tool_call","toolName":"execute_pwsh","status":"completed","toolCallId":"x1","turnId":"e1"}}"#,
        r#"{"id":"c1-result","timestamp":"2026-11-05T13:05:04.000Z","payload":{"type":"tool_result","content":"output text","success":true,"toolCallId":"x1","turnId":"e1"}}"#,
        r#"{"id":"m2","timestamp":"2026-11-05T13:05:04.100Z","payload":{"type":"session_metadata","key":"contextUsage","value":{"usagePercentage":3.2348},"executionId":"e1"}}"#,
        r#"{"id":"a2-say","timestamp":"2026-11-05T13:05:09.100Z","payload":{"type":"assistant","content":"done","operationType":"Say","executionId":"e1","turnId":"e1"}}"#,
        r#"{"id":"m3","timestamp":"2026-11-05T13:05:09.200Z","payload":{"type":"session_metadata","key":"displayError","value":{"errorType":"HttpError502","message":"Bad gateway: secret details"}}}"#,
        r#"{"id":"us","timestamp":"2026-11-05T13:05:10.000Z","payload":{"type":"usage_summary","promptTurnSummaries":[{"unit":"credit","unitPlural":"credits","usage":1.25,"usedTools":["a"]}],"elapsedTime":19734,"status":"success","requestIds":["q1","q2"]}}"#,
        r#"{"id":"te","timestamp":"2026-11-05T13:05:10.000Z","payload":{"type":"turn_end","stopReason":"end_turn","executionId":"e1","turnId":"e1"}}"#,
    ];

    fn feed(entry: &mut FileEntry, lines: &[&str]) {
        for line in lines {
            handle_line(entry, line.as_bytes());
        }
    }

    #[test]
    fn turns_context_readings_into_requests() {
        let _ = ctx();
        let mut entry = FileEntry::new(Agent::Kiro, PathBuf::from("sess_1/messages.jsonl"));
        feed(&mut entry, MAIN);
        assert!(entry.live.turn_open);
        assert_eq!(entry.live.phase, Phase::Waiting);
        assert_eq!(entry.events.len(), 1);
        // 第一次请求：精确的提示词，没有缓存
        assert_eq!(entry.events[0].usage.input, 31_846);
        assert_eq!(entry.events[0].usage.cache_read, 0);
        assert!(entry.events[0].estimated);
        assert_eq!(entry.models, vec!["claude-opus-5.5"]);
        assert_eq!(entry.session.context_used, 31_846);

        feed(&mut entry, REST);
        assert_eq!(entry.events.len(), 2);
        let second = &entry.events[1];
        // 第二次：上一次的提示词命中缓存，新增部分未命中
        assert_eq!(second.usage.cache_read, 31_846);
        assert_eq!(second.usage.input, 32_348 - 31_846);
        assert_eq!(second.latency_ms, 5_000);
        assert!(entry.events[0].usage.output > 0);
        assert!(entry.events[0].usage.reasoning > 0);
        // credits 记在本轮第一条上
        assert!((entry.events[0].credits - 1.25).abs() < 1e-6);
        assert_eq!(entry.events[1].credits, 0.0);
        assert!(!entry.live.turn_open);
        assert_eq!(entry.live.turns, 1);
        assert_eq!(entry.live.last_turn_ms, 19_734);
        assert_eq!(entry.net.len(), 1);
        assert_eq!(entry.net[0].code, "HTTP 502");
        assert_eq!(entry.tools.len(), 1);
        assert_eq!(entry.tools[0].name, "execute_pwsh");
        assert!(entry.tools[0].ok);
    }

    #[test]
    fn sub_agents_estimate_their_own_context() {
        let mut entry = FileEntry::new(Agent::Kiro, PathBuf::from("sess_9/sub-executions/abc.jsonl"));
        if let ParserState::Kiro(state) = &mut entry.parser {
            state.sub = true;
        }
        feed(
            &mut entry,
            &[
                r#"{"timestamp":"2026-11-05T13:12:09.000Z","payload":{"type":"assistant","content":"plan","operationType":"Reasoning","reasoningModelId":"qdev::claude-opus-5.5"}}"#,
                r#"{"timestamp":"2026-11-05T13:12:10.000Z","payload":{"type":"tool_call","toolName":"read_file","status":"completed","args":{"path":"a.txt"}}}"#,
                r#"{"timestamp":"2026-11-05T13:12:10.000Z","payload":{"type":"tool_result","content":"0123456789012345678901234567890123456789","success":true}}"#,
                r#"{"timestamp":"2026-11-05T13:12:15.000Z","payload":{"type":"assistant","content":"next","operationType":"Say"}}"#,
            ],
        );
        assert_eq!(entry.events.len(), 2);
        assert_eq!(entry.events[0].usage.prompt(), BASE_CONTEXT);
        assert!(entry.events[1].usage.prompt() > BASE_CONTEXT);
        assert_eq!(entry.events[1].usage.cache_read, BASE_CONTEXT);
    }

    #[test]
    fn reads_account_credits_cached_by_the_client() {
        let dir = std::env::temp_dir().join(format!("foxes-kiro-usage-{}-{}", std::process::id(), crate::timeutil::now_ms()));
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("state.vscdb");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)").unwrap();
        let state = serde_json::json!({"kiro.resourceNotifications.usageState": {"timestamp": 1793883791544i64, "usageBreakdowns": [{
            "type": "CREDIT", "displayNamePlural": "Credits", "currentUsage": 37.5, "usageLimit": 50, "percentageUsed": 75,
            "resetDate": "2026-12-01T00:00:00.000Z",
            "bonuses": [
                {"currentUsage": 20, "usageLimit": 500, "expiryDate": "2027-11-04T09:09:26.176Z", "status": "ACTIVE"},
                {"currentUsage": 100, "usageLimit": 100, "expiryDate": "2026-01-01T00:00:00.000Z", "status": "EXPIRED"}
            ]}]}, "lastSelectedModelId": "auto"});
        conn.execute("INSERT INTO ItemTable VALUES ('kiro.kiroAgent', ?1)", [state.to_string()]).unwrap();
        drop(conn);

        let limits = read_usage_limits(&db).unwrap();
        assert_eq!(limits.ts, 1_793_883_791_544);
        let monthly = limits.primary.unwrap();
        assert_eq!((monthly.used, monthly.limit, monthly.used_percent), (37.5, 50.0, 75.0));
        assert_eq!(monthly.unit, "credits");
        assert_eq!(monthly.resets_at_ms, parse_iso_ms("2026-12-01T00:00:00.000Z").unwrap());
        assert!(!monthly.expires);
        let bonus = limits.secondary.unwrap();
        assert_eq!((bonus.used, bonus.limit, bonus.used_percent), (20.0, 500.0, 4.0));
        assert!(bonus.expires);
        assert!(read_usage_limits(&dir.join("missing.vscdb")).is_none());
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn reads_session_file_for_title_and_workspace() {
        let dir = std::env::temp_dir().join(format!("foxes-kiro-{}", std::process::id())).join("sess_abc");
        fs::create_dir_all(dir.join("sub-executions")).unwrap();
        fs::write(
            dir.join("session.json"),
            r#"{"id":"sess_abc","title":"Fix login","modelId":"claude-opus-5.5","workspacePaths":["C:\\work\\app"],"createdAt":"2026-11-05T13:01:39.112Z","status":"idle"}"#,
        )
        .unwrap();
        fs::write(dir.join("messages.jsonl"), format!("{}\n", MAIN[0])).unwrap();
        fs::write(dir.join("sub-executions").join("s1.jsonl"), "").unwrap();
        let models = Mutex::new(std::collections::HashMap::new());
        let scan = ScanCtx::new(0, &models);

        let mut main = FileEntry::new(Agent::Kiro, dir.join("messages.jsonl"));
        update(&mut main, &scan).unwrap();
        assert_eq!(main.session.id, "abc");
        assert_eq!(main.session.title, "Fix login");
        assert_eq!(main.session.cwd, r"C:\work\app");
        assert!(!main.session.sub_agent);

        let mut sub = FileEntry::new(Agent::Kiro, dir.join("sub-executions").join("s1.jsonl"));
        update(&mut sub, &scan).unwrap();
        assert_eq!(sub.session.id, "abc");
        assert!(sub.session.sub_agent);
        assert_eq!(sub.session.cwd, r"C:\work\app");
        fs::remove_dir_all(dir.parent().unwrap()).ok();
    }
}
