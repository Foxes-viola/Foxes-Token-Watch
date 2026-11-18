//! Cursor IDE stores conversation metadata and message fragments in cursorDiskKV.
//! Agent conversations no longer record token counts, so usage is estimated by
//! accumulating each conversation's context (see `ContextModel`). Cards of the
//! gateway assistant extension are read by `cursor_gateway` and merged into the
//! matching conversation without counting a model call twice; conversations of
//! the 百事通 plugin come in through `baishitong` and become cards as well.
use std::collections::HashMap;
use std::path::Path;
use std::{fs, io};

use super::{
    clamp_ms, cursor_gateway, intern, push_net, push_tool, split_tool_name, touch, FileEntry, FileSig,
    ParserState, Recorded, ScanCtx,
};
use crate::estimate::{output_of, ContextModel};
use crate::lenient::text_weight;
use crate::model::{Agent, NetEvent, NetKind, Phase, ToolCall, Usage, UsageEvent};
use crate::timeutil::{fnv64, parse_iso_ms};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

/// Context window assumed when a conversation does not report one.
pub(super) const WINDOW_DEFAULT: u64 = 200_000;
pub(super) const WINDOW_MAX_MODE: u64 = 1_000_000;
/// How soon to come back when the 百事通 feed has more on its way.
const FEED_SOON_MS: i64 = 1000;

#[derive(Default, Debug)]
pub struct State {
    observer: Option<Connection>,
    observed_sig: Option<FileSig>,
    data_version: i64,
    last_poll: i64,
    native: super::cursor_host::Logs,
    gateway: cursor_gateway::Cards,
    jobs: super::cursor_executor::Jobs,
    chat_updated_at: HashMap<String, i64>,
    composers: HashMap<String, Composer>,
    pub(crate) revision: i64,
    /// Which snapshot of the usage history the cards were last reported with (see [`Recorded::rev`]).
    pub(crate) recorded_rev: u64,
}

/// What gateway cards need to know about a Cursor conversation.
#[derive(Debug, Default)]
pub(super) struct Composer {
    pub created_at: i64,
    pub cwd: String,
    /// Start times of fragments with model output, ascending.
    pub outputs: Vec<i64>,
}

impl Composer {
    fn of(chat: &Chat) -> Composer {
        let mut outputs: Vec<i64> =
            chat.fragments.iter().filter(|f| !f.user && f.ts > 0 && f.has_content()).map(|f| f.ts).collect();
        outputs.sort_unstable();
        Composer { created_at: chat.created_at, cwd: chat.cwd.clone(), outputs }
    }
}

fn string(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or("").to_owned()
}
fn number(value: &Value, key: &str) -> u64 {
    value[key].as_u64().unwrap_or(0)
}
fn timestamp(v: &Value) -> i64 {
    v.as_i64()
        .or_else(|| v.as_str().and_then(parse_iso_ms))
        .unwrap_or(0)
}
fn rows(
    conn: &Connection,
    prefix: &str,
    mut visit: impl FnMut(&str, Value),
) -> rusqlite::Result<()> {
    let mut stmt =
        conn.prepare("SELECT key,value FROM cursorDiskKV WHERE key >= ?1 AND key < ?2")?;
    let mut rows = stmt.query([format!("{prefix}:"), format!("{prefix};")])?;
    while let Some(row) = rows.next()? {
        let key: &str = row.get_ref(0)?.as_str()?;
        let raw = row.get_ref(1)?.as_bytes()?;
        if let Ok(value) = serde_json::from_slice(raw) {
            visit(&key[prefix.len() + 1..], value);
        }
    }
    Ok(())
}

#[derive(Default)]
struct Chat {
    name: String,
    model: String,
    status: String,
    cwd: String,
    created_at: i64,
    updated_at: i64,
    context: u64,
    window: u64,
    max_mode: bool,
    generating: bool,
    fragments: Vec<Fragment>,
}

impl Chat {
    fn window(&self) -> u64 {
        match self.window {
            0 if self.max_mode => WINDOW_MAX_MODE,
            0 => WINDOW_DEFAULT,
            window => window,
        }
    }
}

struct Fragment {
    id: String,
    user: bool,
    ts: i64,
    end: i64,
    /// Visible text plus tool arguments.
    text: u64,
    thinking: u64,
    think_ms: u32,
    result: u64,
    /// Shared by parallel tool calls from one model response.
    call_id: String,
    tool: String,
    status: String,
    /// The turn failed before the model ran (e.g. an account error).
    error: bool,
    measured: Usage,
}
impl Fragment {
    fn read(id: &str, v: &Value) -> Self {
        let tool = &v["toolFormerData"];
        let count = &v["tokenCount"];
        let input = number(count, "inputTokens");
        let output = number(count, "outputTokens");
        let cache = number(count, "cachedInputTokens").min(input);
        let ts = timestamp(&v["startedAtMs"]);
        Self {
            id: id.to_owned(),
            user: v["type"] == 1,
            ts: if ts > 0 {
                ts
            } else {
                timestamp(&v["createdAt"])
            },
            end: timestamp(&v["completedAtMs"]),
            text: text_weight(v["text"].as_str().unwrap_or(""))
                + text_weight(tool["rawArgs"].as_str().unwrap_or("")),
            thinking: text_weight(
                v["thinking"]["text"]
                    .as_str()
                    .or(v["thinking"].as_str())
                    .unwrap_or(""),
            ),
            think_ms: clamp_ms(number(v, "thinkingDurationMs") as i64),
            result: text_weight(tool["result"].as_str().unwrap_or("")),
            call_id: string(tool, "modelCallId"),
            tool: string(tool, "name"),
            status: string(tool, "status"),
            error: v.get("errorDetails").is_some_and(|e| !e.is_null()),
            measured: Usage {
                input: input - cache,
                cache_read: cache,
                output,
                reasoning: number(count, "reasoningTokens").min(output),
                ..Usage::default()
            },
        }
    }
    fn has_content(&self) -> bool {
        !self.error && (self.text + self.thinking > 0 || !self.tool.is_empty() || !self.measured.is_zero())
    }
}

/// The model call whose output is being collected.
struct Call {
    event: usize,
    model_call: String,
    text: u64,
    thinking: u64,
    measured: bool,
}

fn finish(call: &mut Option<Call>, events: &mut [UsageEvent]) {
    let Some(call) = call.take() else { return };
    if call.measured {
        return;
    }
    if let Some(event) = events.get_mut(call.event) {
        let (output, reasoning) = output_of(call.text, call.thinking);
        event.usage.output = output;
        event.usage.reasoning = reasoning;
        event.reasoning_estimated = reasoning > 0;
    }
}

/// One conversation's record. Each model call becomes one usage event: its
/// prompt is the whole context so far (mostly cached), its output the visible
/// text, thinking and tool arguments. Exact token counts win where Cursor kept them.
fn sessions(id: &str, mut chat: Chat, path: &Path) -> Option<FileEntry> {
    chat.fragments
        .sort_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.id.cmp(&b.id)));
    let generating = chat.generating || matches!(chat.status.as_str(), "generating" | "streaming");
    let mut entry = FileEntry::new(Agent::Cursor, path.to_path_buf());
    entry.session.id = id.to_owned();
    entry.session.title.clone_from(&chat.name);
    entry.session.cwd.clone_from(&chat.cwd);
    let mut context = ContextModel::new(chat.window());
    let mut call: Option<Call> = None;
    let mut after_tool = false;
    let mut pending_tool = false;
    let mut turn_started = 0;
    let mut seen = false;
    for b in &chat.fragments {
        if b.ts <= 0 {
            continue;
        }
        if b.user {
            finish(&mut call, &mut entry.events);
            context.add(b.text);
            after_tool = false;
            turn_started = b.ts;
        }
        // Cursor writes the user prompt and an empty assistant placeholder before
        // the first response. Keep that live session, without inventing usage.
        if b.user || !b.has_content() {
            if generating || b.error {
                touch(&mut entry.session, b.ts);
                seen = true;
            }
            continue;
        }
        seen = true;
        let tool = !b.tool.is_empty();
        // Text and thinking continue the response until a tool result comes back;
        // tool calls after a result only belong together when Cursor says so.
        let same_call = match &call {
            None => false,
            Some(_) if !after_tool => true,
            Some(current) => tool && !b.call_id.is_empty() && b.call_id == current.model_call,
        };
        if !same_call {
            finish(&mut call, &mut entry.events);
            let model = intern(&mut entry.models, &chat.model);
            entry.events.push(UsageEvent {
                ts: b.ts,
                usage: context.request(b.ts),
                model,
                estimated: true,
                key: fnv64(&[b"cursor", id.as_bytes(), b.id.as_bytes()]),
                ..UsageEvent::default()
            });
            call = Some(Call { event: entry.events.len() - 1, model_call: String::new(), text: 0, thinking: 0, measured: false });
        }
        let Some(current) = call.as_mut() else { continue };
        if tool && !b.call_id.is_empty() {
            current.model_call.clone_from(&b.call_id);
        }
        current.text += b.text;
        current.thinking += b.thinking;
        context.add(b.text + b.thinking + b.result);
        let ts = if tool { b.ts } else { b.ts.max(b.end) };
        let event = &mut entry.events[current.event];
        event.ts = event.ts.max(ts);
        event.think_ms = event.think_ms.saturating_add(b.think_ms);
        if !tool && b.end > b.ts {
            event.latency_ms = event.latency_ms.max(clamp_ms(b.end - b.ts));
        }
        if !b.measured.is_zero() {
            if !current.measured {
                current.measured = true;
                event.usage = b.measured;
                event.estimated = false;
                event.reasoning_estimated = false;
            } else if b.measured.total() > event.usage.total() {
                event.usage = b.measured;
            }
        }
        touch(&mut entry.session, b.ts);
        touch(&mut entry.session, b.ts.max(b.end));
        after_tool = tool;
        pending_tool = tool && matches!(b.status.as_str(), "loading" | "running" | "pending") && b.end == 0;
        if tool {
            let (server, name) = split_tool_name(&b.tool, &[]);
            let ok = !matches!(b.status.as_str(), "error" | "failed");
            push_tool(&mut entry.tools, ToolCall { ts, server, name, ok });
            if !ok {
                record_error(&mut entry, ts);
            }
        }
    }
    finish(&mut call, &mut entry.events);
    if !seen && generating {
        let ts = chat.updated_at.max(chat.created_at);
        if ts > 0 {
            touch(&mut entry.session, ts);
            seen = true;
        }
    }
    if !seen {
        return None;
    }
    entry.session.context_used = chat.context;
    entry.session.context_window = chat.window;
    if generating || pending_tool {
        let ts = entry.session.last_active_ms;
        entry.live.open_turn(if turn_started > 0 { turn_started } else { ts });
        entry.live.set_phase(if pending_tool { Phase::Tool } else { Phase::Generating }, ts);
        if pending_tool {
            entry.live.tool = entry.tools.last().map(|tool| tool.name.clone()).unwrap_or_default();
        }
    }
    Some(entry)
}

fn record_error(entry: &mut FileEntry, ts: i64) {
    push_net(
        &mut entry.net,
        NetEvent {
            ts,
            kind: NetKind::Error,
            code: "tool_error".into(),
            attempt: 0,
            max_attempts: 0,
            delay_ms: 0,
        },
    );
}
pub fn update(file: &mut FileEntry, ctx: &ScanCtx) -> io::Result<()> {
    update_database(file, ctx)?;
    let Some(global) = file
        .path
        .parent()
        .filter(|p| p.file_name().is_some_and(|n| n == "globalStorage"))
        .map(Path::to_path_buf)
    else {
        return Ok(());
    };
    // Gateway cards are reported anew on every pass.
    let source = file.path.clone();
    file.extra.retain(|e| e.path == source);
    let ParserState::Cursor(state) = &mut file.parser else {
        return Ok(());
    };
    // New native Agent windows can keep running while composerData still says
    // "aborted" with no generating bubble. Read their host activity even when
    // SQLite's data_version did not change.
    let cursor_root = global.parent().and_then(Path::parent).map(Path::to_path_buf);
    let activities = match &cursor_root {
        Some(cursor_root) => state.native.refresh(&cursor_root.join("logs"), ctx.now_ms)?,
        None => Vec::new(),
    };
    if let Some(cursor_root) = &cursor_root {
        state.jobs.refresh(&cursor_root.join("logs"), ctx.now_ms);
    }
    for activity in activities {
        if activity.ts < *state.chat_updated_at.get(&activity.id).unwrap_or(&0) {
            continue;
        }
        let index = match file.extra.iter().position(|e| e.session.id == activity.id) {
            Some(index) => index,
            None if activity.active && ctx.now_ms - activity.ts <= 30 * 60_000 => {
                let mut entry = FileEntry::new(Agent::Cursor, file.path.clone());
                entry.session.id.clone_from(&activity.id);
                entry.session.title = "Cursor Agent".into();
                file.extra.push(entry);
                file.extra.len() - 1
            }
            None => continue,
        };
        let entry = &mut file.extra[index];
        // Prefer newer database phase information, but do not let an old
        // composer status hide a still-active native turn.
        if activity.active {
            entry.live.open_turn(activity.started);
            if entry.live.phase_since_ms <= activity.ts {
                entry.live.set_phase(activity.phase, activity.ts);
                entry.live.tool.clone_from(&activity.tool);
            }
        } else if activity.ts >= entry.session.last_active_ms {
            entry.live.close_turn(activity.ts, None);
        }
        touch(&mut entry.session, activity.ts);
    }
    if state.gateway.refresh(&global.join(cursor_gateway::DIR)) {
        state.revision += 1;
    }
    // Conversations of the 百事通 plugin: its service keeps them, a feed reads them through the plugin's local port.
    if let Some(feed) = ctx.baishitong {
        let batch = feed.take(&global, ctx.now_ms, ctx.recorded);
        // The feed has just started, or reads its first round: what it finds next
        // should not wait for the database's next poll.
        if batch.soon {
            let soon = ctx.now_ms + FEED_SOON_MS;
            if file.recheck_at_ms == 0 || file.recheck_at_ms > soon {
                file.recheck_at_ms = soon;
            }
        }
        if state.gateway.sync_service(&global, batch) {
            state.revision += 1;
        }
    }
    // Steps of gateway runs that sent no process rows, from the executor's log.
    if state.gateway.adopt(&state.jobs, ctx.now_ms) {
        state.revision += 1;
    }
    // What the usage history already holds of the cards. Once it is loaded they report differently.
    let unrecorded = Recorded::default();
    let recorded = ctx.recorded.unwrap_or(&unrecorded);
    if state.recorded_rev != recorded.rev {
        state.recorded_rev = recorded.rev;
        state.revision += 1;
    }
    let cards = state.gateway.entries(&state.composers, recorded, ctx.now_ms);
    // A relay conversation with nothing of its own is shown as its card.
    let relays = state.gateway.relays(&state.composers);
    let linked: std::collections::HashSet<&str> = cards.iter().map(|card| card.session.id.as_str()).collect();
    file.extra.retain(|e| !relays.contains(e.session.id.as_str()) || linked.contains(e.session.id.as_str()) || !e.events.is_empty());
    // A card's name is what its user knows the conversation by.
    for card in cards.iter().filter(|card| !card.session.sub_agent && !card.session.title.is_empty()) {
        if let Some(chat) = file.extra.iter_mut().find(|e| e.session.id == card.session.id) {
            chat.session.title.clone_from(&card.session.title);
        }
    }
    file.extra.extend(cards);
    Ok(())
}

fn update_database(file: &mut FileEntry, ctx: &ScanCtx) -> io::Result<()> {
    let ParserState::Cursor(state) = &mut file.parser else {
        return Ok(());
    };
    if ctx.now_ms < state.last_poll + 3000 {
        file.recheck_at_ms = state.last_poll + 3000;
        return Ok(());
    }
    // On Windows the WAL can be overwritten without a size or mtime change.
    // data_version changes after committed writes from other connections.
    file.recheck_at_ms = ctx.now_ms + 5000;
    state.last_poll = ctx.now_ms;
    // data_version belongs to a connection, not to the path. A restored or
    // rewritten database can have the same version and change counter while
    // this connection still holds cached pages from the old (possibly empty)
    // database. Reopen when the main file changes; WAL-only writes keep using
    // data_version so they are detected even if the WAL size/mtime is unchanged.
    let signature = FileSig::of(&file.path);
    if state.observer.is_some() && state.observed_sig != signature {
        state.observer = None;
        state.data_version = 0;
    }
    if state.observer.is_none() {
        let conn = Connection::open_with_flags(&file.path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(io::Error::other)?;
        conn.busy_timeout(std::time::Duration::from_millis(300))
            .map_err(io::Error::other)?;
        state.observer = Some(conn);
        state.observed_sig = signature;
    }
    let conn = state.observer.as_mut().unwrap();
    let version: i64 = conn
        .query_row("PRAGMA data_version", [], |r| r.get(0))
        .map_err(io::Error::other)?;
    if state.data_version == version {
        return Ok(());
    }
    let tx = conn.transaction().map_err(io::Error::other)?;
    let mut chats: HashMap<String, Chat> = HashMap::new();
    rows(&tx, "composerData", |id, v| {
        chats.insert(
            id.to_owned(),
            Chat {
                name: string(&v, "name"),
                model: string(&v["modelConfig"], "modelName"),
                max_mode: v["modelConfig"]["maxMode"].as_bool().unwrap_or(false),
                cwd: string(&v["workspaceIdentifier"]["uri"], "fsPath"),
                created_at: timestamp(&v["createdAt"]),
                updated_at: timestamp(&v["lastUpdatedAt"]),
                context: number(&v, "contextTokensUsed"),
                window: number(&v, "contextTokenLimit"),
                status: string(&v, "status"),
                generating: v["generatingBubbleIds"]
                    .as_array()
                    .is_some_and(|a| !a.is_empty()),
                ..Chat::default()
            },
        );
    })
    .map_err(io::Error::other)?;
    rows(&tx, "bubbleId", |key, v| {
        if let Some((id, bubble)) = key.split_once(':') {
            chats
                .entry(id.to_owned())
                .or_default()
                .fragments
                .push(Fragment::read(bubble, &v));
        }
    })
    .map_err(io::Error::other)?;
    tx.commit().map_err(io::Error::other)?;
    state.data_version = version;
    state.chat_updated_at = chats
        .iter()
        .map(|(id, chat)| (id.clone(), chat.updated_at.max(chat.created_at)))
        .collect();
    state.composers = chats.iter().map(|(id, chat)| (id.clone(), Composer::of(chat))).collect();
    state.revision += 1;
    let path = file.path.clone();
    file.extra = chats
        .into_iter()
        .filter_map(|(id, chat)| sessions(&id, chat, &path))
        .collect();
    Ok(())
}

pub fn update_gateway(file: &mut FileEntry) -> io::Result<()> {
    let log: Value = serde_json::from_slice(&fs::read(&file.path)?)?;
    let rows = log["rows"]
        .as_array()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing gateway rows"))?;
    let mut fresh = FileEntry::new(Agent::WebMcp, file.path.clone());
    let dir = file.path.parent().unwrap_or(&file.path);
    fresh.session.id = format!(
        "gateway/{}",
        dir.file_name().unwrap_or_default().to_string_lossy()
    );
    let meta: Value = fs::read(dir.join("meta.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    fresh.session.title = string(&meta, "name");
    let mut last = (0, String::new(), String::new());
    for row in rows {
        let ts = timestamp(&row["at"]).max(timestamp(&row["endedAt"]));
        if ts <= 0 {
            continue;
        }
        touch(&mut fresh.session, ts);
        if ts >= last.0 {
            last = (ts, string(row, "status"), string(row, "tool"));
        }
        if row["kind"] == "tool" {
            let ok = row["ok"] != false && row["status"] != "error";
            push_tool(
                &mut fresh.tools,
                ToolCall {
                    ts,
                    name: string(row, "tool").chars().take(80).collect(),
                    server: string(row, "server").chars().take(80).collect(),
                    ok,
                },
            );
            if !ok {
                record_error(&mut fresh, ts);
            }
        }
    }
    if matches!(last.1.as_str(), "running" | "pending" | "loading") {
        fresh.live.open_turn(last.0);
        fresh.live.set_phase(Phase::Tool, last.0);
        fresh.live.tool = last.2;
    }
    *file = fresh;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::History;
    use crate::{analyze, Roots, Store};
    use serde_json::json;

    /// Token totals per session this computer saved in a `history.json`.
    fn saved_totals(path: &std::path::Path) -> std::collections::HashMap<String, u64> {
        let saved: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        let sessions = saved["devices"][saved["device"].as_str().unwrap()]["sessions"].as_object().unwrap();
        sessions
            .iter()
            .map(|(key, record)| {
                let rows = record["events"].as_array().unwrap();
                (key.clone(), rows.iter().map(|row| (1..=4).map(|index| row[index].as_u64().unwrap_or(0)).sum::<u64>()).sum())
            })
            .collect()
    }

    #[test]
    fn late_executor_log_updates_usage_and_history_without_a_database_write() {
        use chrono::{Local, TimeZone};
        use std::io::Write;
        let now = crate::timeutil::now_ms();
        let job_at = now - 120_000;
        let local = Local.timestamp_millis_opt(job_at).unwrap();
        let root = std::env::temp_dir().join(format!("foxes-cursor-late-log-{}-{now}", std::process::id()));
        let global = root.join("User/globalStorage");
        let output = root.join("logs/startup/window2/exthost").join(local.format("output_logging_%Y%m%dT%H%M%S_test").to_string());
        let card = global.join(cursor_gateway::DIR).join("conversations/local/sessions/card-late");
        fs::create_dir_all(&card).unwrap();
        fs::create_dir_all(&output).unwrap();
        let db = global.join("state.vscdb");
        Connection::open(&db).unwrap().execute_batch("CREATE TABLE cursorDiskKV(key TEXT PRIMARY KEY,value TEXT)").unwrap();
        fs::write(card.join("meta.json"), json!({"name":"late log","model":"model · Max","createdAt":job_at-1000}).to_string()).unwrap();
        fs::write(card.join("context.json"), json!({"history":[{"messageId":"u","role":"user","text":"read a.rs","createdAt":job_at-1000}]}).to_string()).unwrap();
        let log = output.join("1-harness网关 agent.log");
        fs::write(&log, "").unwrap();
        let mut store = Store::new(Roots { cursor: vec![global], ..Roots::default() });
        let history_path = root.join("history.json");
        let mut history = History::new(Some(history_path.clone()));
        store.refresh_at(now);
        history.sync(&mut store, now);
        assert_eq!(store.entries().map(|e| e.events.len()).sum::<usize>(), 0);
        let db_sig = FileSig::of(&db);

        // The completed job predates the previous scan, but was flushed later.
        writeln!(fs::OpenOptions::new().append(true).open(&log).unwrap(), "[{}] 读取 a.rs（25 ms）", local.format("%H:%M:%S")).unwrap();
        store.refresh_at(now + 6000);
        history.sync(&mut store, now + 6000);
        assert_eq!(FileSig::of(&db), db_sig);
        let entries: Vec<_> = store.entries().collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].events.len(), 1);
        let total = entries[0].events[0].usage.total();
        assert!(total > 20_000);
        assert_eq!(history.error(), None);
        // The card was saved on the first pass, before it had any usage; saving is throttled, so the next save carries the change.
        assert_eq!(saved_totals(&history_path)["card:card-late"], 0);
        history.sync(&mut store, now + 186_000);
        assert_eq!(history.error(), None);
        assert_eq!(saved_totals(&history_path)["card:card-late"], total, "log-only changes must also reach the usage history");
        store.refresh_at(now + 12_000);
        assert_eq!(store.entries().map(|e| e.events.len()).sum::<usize>(), 1);
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_host_activity_updates_stale_composer_without_usage_duplication() {
        use chrono::Local;
        use std::io::Write;
        let root = std::env::temp_dir().join(format!(
            "foxes-cursor-native-{}-{}",
            std::process::id(),
            crate::timeutil::now_ms()
        ));
        let global = root.join("User/globalStorage");
        let logs = root.join("logs/startup/window2/exthost/anysphere.cursor-agent-host");
        fs::create_dir_all(&global).unwrap();
        fs::create_dir_all(&logs).unwrap();
        let now = crate::timeutil::now_ms();
        let time = |ms: i64| {
            chrono::DateTime::from_timestamp_millis(ms)
                .unwrap()
                .with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S%.3f")
                .to_string()
        };
        let log = logs.join("Cursor Agent Host.log");
        let writer = Connection::open(global.join("state.vscdb")).unwrap();
        writer
            .execute_batch("CREATE TABLE cursorDiskKV(key TEXT PRIMARY KEY,value TEXT)")
            .unwrap();
        for (key, v) in [
            (
                "composerData:c",
                json!({"name":"P21 native","status":"aborted","lastUpdatedAt":now-3*60*60_000}),
            ),
            (
                "bubbleId:c:turn/s",
                json!({"type":2,"text":"hello","startedAtMs":now-3*60*60_000,"tokenCount":{"inputTokens":100,"outputTokens":7}}),
            ),
        ] {
            writer
                .execute(
                    "INSERT INTO cursorDiskKV VALUES(?1,?2)",
                    [key, &v.to_string()],
                )
                .unwrap();
        }
        fs::write(&log, format!(
            "{} [info] Selected Agent Host turn runtime {{\"conversationId\":\"c\",\"actionCase\":\"userMessageAction\"}}\n{} [info] Running step {{}}\n",
            time(now-2*60*60_000), time(now-1000)
        )).unwrap();
        let mut store = Store::new(Roots {
            cursor: vec![global],
            ..Roots::default()
        });
        store.refresh_at(now);
        let analysis = analyze(&store, now);
        let entry = store.entries().next().unwrap();
        assert_eq!(entry.session.title, "P21 native");
        assert_eq!(entry.events.len(), 1);
        assert!(entry.live.turn_open);
        assert!(now - entry.live.turn_started_ms >= 2 * 60 * 60_000);
        assert_eq!(
            analysis.snapshot.live.agents[Agent::Cursor.index()].running,
            1
        );
        assert_eq!(analysis.snapshot.ranges.last().unwrap().totals.total, 107);

        // SQLite stays untouched: a new host event still reaches the UI.
        writeln!(
            fs::OpenOptions::new().append(true).open(&log).unwrap(),
            "{} [info] nal.tool_call.start {{\"toolCallId\":\"t\",\"toolName\":\"shell\"}}",
            time(now + 5000)
        )
        .unwrap();
        store.refresh_at(now + 6000);
        let entry = store.entries().next().unwrap();
        assert_eq!(entry.live.phase, Phase::Tool);
        assert_eq!(entry.live.tool, "shell");
        assert_eq!(entry.events.len(), 1);
        writeln!(
            fs::OpenOptions::new().append(true).open(&log).unwrap(),
            "{} [info] Turn terminal blob reads {{\"sessionId\":\"c\"}}",
            time(now + 10000)
        )
        .unwrap();
        store.refresh_at(now + 12000);
        assert_eq!(
            analyze(&store, now + 12000).snapshot.live.agents[Agent::Cursor.index()].running,
            0
        );
        drop(store);
        drop(writer);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reopens_rewritten_database_even_when_sqlite_version_is_unchanged() {
        let root = std::env::temp_dir().join(format!(
            "foxes-cursor-rewrite-{}-{}",
            std::process::id(),
            crate::timeutil::now_ms()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("state.vscdb");
        let replacement = root.join("replacement.db");
        let now = crate::timeutil::now_ms();
        for (db, text) in [(&path, ""), (&replacement, "hello")] {
            let writer = Connection::open(db).unwrap();
            writer
                .execute_batch("CREATE TABLE cursorDiskKV(key TEXT PRIMARY KEY,value TEXT)")
                .unwrap();
            for (key, v) in [
                ("composerData:c", json!({"status":"completed"})),
                (
                    "bubbleId:c:turn/s",
                    json!({"type":2,"text":text,"startedAtMs":now}),
                ),
            ] {
                writer
                    .execute(
                        "INSERT INTO cursorDiskKV VALUES(?1,?2)",
                        [key, &v.to_string()],
                    )
                    .unwrap();
            }
        }
        let mut store = Store::new(Roots {
            cursor: vec![root.clone()],
            ..Roots::default()
        });
        store.refresh_at(now);
        assert_eq!(store.entries().count(), 0);
        fs::copy(&replacement, &path).unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(10))
            .unwrap();
        store.refresh_at(now + 6000);
        assert_eq!(store.stats().failing, 0);
        let entries: Vec<_> = store.entries().collect();
        assert_eq!(
            entries.len(),
            1,
            "a rewritten Cursor database must not stay empty"
        );
        assert!(!entries[0].events.is_empty());
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn keeps_generating_prompts_and_empty_placeholders_without_counting_usage() {
        let now = crate::timeutil::now_ms();
        let chat = Chat {
            name: "waiting for first response".into(),
            generating: true,
            fragments: vec![
                Fragment::read("turn/u", &json!({"type":1,"text":"hello","createdAt":now})),
                Fragment::read("turn/s", &json!({"type":2,"startedAtMs":now+10,"text":""})),
            ],
            ..Chat::default()
        };
        let entry = sessions("c", chat, Path::new("state.vscdb")).unwrap();
        assert_eq!(entry.session.id, "c");
        assert_eq!(entry.session.last_active_ms, now + 10);
        assert!(entry.live.turn_open);
        assert_eq!(entry.live.turn_started_ms, now);
        assert_eq!(entry.live.phase, Phase::Generating);
        assert!(entry.events.is_empty());
    }

    #[test]
    fn keeps_metadata_only_generation_and_ignores_empty_drafts() {
        let source = Path::new("state.vscdb");
        let chat = Chat { status: "streaming".into(), created_at: 1000, updated_at: 2000, ..Chat::default() };
        let entry = sessions("c", chat, source).unwrap();
        assert_eq!(entry.session.last_active_ms, 2000);
        assert!(entry.live.turn_open);
        assert!(entry.events.is_empty());
        assert!(sessions("draft", Chat::default(), source).is_none());
    }

    #[test]
    fn estimates_resent_context_and_joins_tool_calls_to_their_response() {
        let at = 1_800_000_000_000i64;
        let bubble = |id: &str, v: Value| Fragment::read(id, &v);
        let chat = Chat {
            model: "claude-opus-5-5".into(),
            max_mode: true,
            fragments: vec![
                bubble("t/u", json!({"type":1,"text":"fix the bug","createdAt":at})),
                bubble("t/a", json!({"type":2,"startedAtMs":at+1000,"thinking":{"text":"look at the file"},"thinkingDurationMs":800})),
                bubble("t/b", json!({"type":2,"startedAtMs":at+2000,"toolFormerData":{"name":"read_file","modelCallId":"m1","rawArgs":"{\"path\":\"a.rs\"}","result":"x".repeat(20_000),"status":"completed"}})),
                bubble("t/c", json!({"type":2,"startedAtMs":at+2100,"toolFormerData":{"name":"read_file","modelCallId":"m1","rawArgs":"{\"path\":\"b.rs\"}","result":"y".repeat(20_000),"status":"completed"}})),
                bubble("t/d", json!({"type":2,"startedAtMs":at+5000,"text":"done"})),
                bubble("t/e", json!({"type":2,"startedAtMs":at+6000,"text":"","errorDetails":{"error":"unpaid"}})),
            ],
            ..Chat::default()
        };
        let entry = sessions("c", chat, Path::new("state.vscdb")).unwrap();
        // thinking + two parallel reads are one call; the answer after the results is the second
        assert_eq!(entry.events.len(), 2);
        let (first, second) = (&entry.events[0], &entry.events[1]);
        assert!(first.estimated && first.usage.reasoning > 0);
        assert_eq!(first.think_ms, 800);
        assert_eq!(first.usage.cache_read, 0);
        // the second request resends everything, with the earlier prompt served from cache
        assert_eq!(second.usage.cache_read, first.usage.prompt());
        assert!(second.usage.input >= crate::estimate::context_growth(40_000));
        assert_eq!(entry.tools.len(), 2);
        assert_eq!(entry.session.last_active_ms, at + 6000);
    }

    #[test]
    fn shows_read_failure_then_recovers_on_wal_update() {
        let root = std::env::temp_dir().join(format!(
            "foxes-cursor-recover-{}-{}",
            std::process::id(),
            crate::timeutil::now_ms()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("state.vscdb");
        let writer = Connection::open(&path).unwrap();
        writer
            .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE ItemTable(key TEXT,value TEXT)")
            .unwrap();
        let mut store = Store::new(Roots {
            cursor: vec![root.clone()],
            ..Roots::default()
        });
        let now = crate::timeutil::now_ms();
        store.refresh_at(now);
        let failed = analyze(&store, now);
        let cursor = &failed.snapshot.live.agents[Agent::Cursor.index()];
        assert!(cursor.detected);
        assert_eq!(cursor.files, 1);
        assert!(cursor
            .read_error
            .as_deref()
            .unwrap()
            .contains("cursorDiskKV"));
        assert_eq!(failed.snapshot.scan.failing, 1);

        writer
            .execute_batch("CREATE TABLE cursorDiskKV(key TEXT PRIMARY KEY,value TEXT)")
            .unwrap();
        let put = |key: &str, v: Value| {
            writer
                .execute(
                    "INSERT OR REPLACE INTO cursorDiskKV VALUES(?1,?2)",
                    [key, &v.to_string()],
                )
                .unwrap();
        };
        put(
            "composerData:c",
            json!({"name":"active","createdAt":now,"generatingBubbleIds":["turn/s"]}),
        );
        put(
            "bubbleId:c:turn/u",
            json!({"type":1,"text":"hello","createdAt":now}),
        );
        put(
            "bubbleId:c:turn/s",
            json!({"type":2,"text":"","startedAtMs":now+1}),
        );
        store.refresh_at(now + 6000);
        assert_eq!(store.stats().failing, 0);
        let source = store.source_files().next().unwrap();
        assert!(source.last_error.is_none());
        let ParserState::Cursor(state) = &source.parser else {
            panic!("cursor parser")
        };
        assert!(state
            .observer
            .as_ref()
            .unwrap()
            .is_readonly(rusqlite::DatabaseName::Main)
            .unwrap());
        assert_eq!(source.extra.len(), 1);
        assert!(source.extra[0].live.turn_open);
        assert!(source.extra[0].events.is_empty());

        put(
            "composerData:c",
            json!({"name":"active","status":"completed"}),
        );
        put(
            "bubbleId:c:turn/s",
            json!({"type":2,"text":"response","startedAtMs":now+1,"completedAtMs":now+7000,"tokenCount":{"inputTokens":10,"outputTokens":3}}),
        );
        store.refresh_at(now + 12000);
        let recovered = analyze(&store, now + 12000);
        assert!(recovered.snapshot.live.agents[Agent::Cursor.index()]
            .read_error
            .is_none());
        let entries: Vec<_> = store.entries().collect();
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].live.turn_open);
        assert_eq!(entries[0].events.len(), 1);
        assert_eq!(entries[0].events[0].usage.total(), 13);
        drop(store);
        drop(writer);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tracks_wal_writes_keeps_bridge_in_cursor_and_one_disk_record() {
        let root = std::env::temp_dir().join(format!(
            "foxes-cursor-watch-{}-{}",
            std::process::id(),
            crate::timeutil::now_ms()
        ));
        fs::create_dir_all(&root).unwrap();
        let writer = Connection::open(root.join("state.vscdb")).unwrap();
        writer.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE cursorDiskKV(key TEXT PRIMARY KEY,value TEXT)").unwrap();
        let now = crate::timeutil::now_ms();
        let put = |key: &str, v: Value| {
            writer
                .execute(
                    "INSERT OR REPLACE INTO cursorDiskKV VALUES(?1,?2)",
                    [key, &v.to_string()],
                )
                .unwrap()
        };
        put(
            "composerData:c",
            json!({"name":"test","status":"completed"}),
        );
        put(
            "bubbleId:c:a/s:one",
            json!({"type":2,"text":"hello","startedAtMs":now,"tokenCount":{"inputTokens":100,"outputTokens":7}}),
        );
        let mut store = Store::new(Roots {
            cursor: vec![root.clone()],
            ..Roots::default()
        });
        let history_path = root.join("history.json");
        let mut history = History::new(Some(history_path.clone()));
        store.refresh_at(now);
        history.sync(&mut store, now);
        let first = analyze(&store, now);
        let all = first
            .snapshot
            .ranges
            .iter()
            .find(|r| r.id == "all")
            .unwrap();
        assert_eq!(all.by_agent[Agent::Cursor.index()].totals.total, 107);
        put(
            "bubbleId:c:a/t:bridge",
            json!({"type":2,"startedAtMs":now+10,"toolFormerData":{"name":"mcp-workspace-workspace_reply","status":"completed","rawArgs":"acknowledge"}}),
        );
        put(
            "bubbleId:c:b/s:normal",
            json!({"type":2,"text":"some visible output","startedAtMs":now+15,"tokenCount":{"inputTokens":3,"outputTokens":5}}),
        );
        store.refresh_at(now + 6000);
        history.sync(&mut store, now + 6000);
        let check = analyze(&store, now + 6000);
        let all = check
            .snapshot
            .ranges
            .iter()
            .find(|r| r.id == "all")
            .unwrap();
        assert_eq!(all.by_agent[Agent::Cursor.index()].totals.total, 115);
        assert_eq!(all.by_agent[Agent::Cursor.index()].sessions, 1);
        assert_eq!(all.by_agent[Agent::WebMcp.index()].totals.total, 0);
        assert_eq!(all.totals.total, 115);
        // The history file is written at most once every three minutes; it then holds the same single record.
        store.refresh_at(now + 181_000);
        assert!(!history.sync(&mut store, now + 181_000));
        assert_eq!(history.error(), None);
        assert_eq!(saved_totals(&history_path), std::collections::HashMap::from([("chat:c".to_owned(), 115)]));
        drop(store);

        // After the conversation is deleted from Cursor, a restart restores it from the history: same total, counted once.
        writer.execute("DELETE FROM cursorDiskKV", []).unwrap();
        let mut store = Store::new(Roots {
            cursor: vec![root.clone()],
            ..Roots::default()
        });
        let mut history = History::new(Some(history_path.clone()));
        store.refresh_at(now + 62_000);
        assert_eq!(store.entries().count(), 0);
        assert!(history.sync(&mut store, now + 62_000));
        let later = analyze(&store, now + 62_000);
        let all = later
            .snapshot
            .ranges
            .iter()
            .find(|r| r.id == "all")
            .unwrap();
        assert_eq!(all.by_agent[Agent::Cursor.index()].totals.total, 115);
        assert_eq!(all.by_agent[Agent::Cursor.index()].sessions, 1);
        assert_eq!(all.totals.total, 115);
        store.refresh_at(now + 63_000);
        assert!(!history.sync(&mut store, now + 63_000));
        assert_eq!(store.entries().count(), 1);
        assert_eq!(saved_totals(&history_path), std::collections::HashMap::from([("chat:c".to_owned(), 115)]));
        drop(store);
        drop(writer);
        fs::remove_dir_all(root).unwrap();
    }
}
