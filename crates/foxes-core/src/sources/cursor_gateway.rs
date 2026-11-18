//! Cards of the gateway assistant extension (`local.bajie-compatible-native-gateway`)
//! under Cursor's globalStorage: `conversations/<instance>/sessions/<card>/`, and
//! deleted cards under `trash/`.
//!
//! - `meta.json`: card name, model, creation time, parent card
//! - `context.json`: messages sent to the assistant; user messages enter its context
//! - `process.json`: the assistant's thinking, text and tool calls with start and
//!   end times, but no token counts. The extension keeps only the newest 3000 rows
//!   and drops older ones in batches of 500. Every row seen stays known while the
//!   app runs (as times, sizes and tool names, never content), and nothing of it
//!   is written to disk: after a restart the requests already in the usage history
//!   are taken over from there as they are (see [`Cards::entries`]).
//!
//! Conversations of the 百事通 plugin are kept by its service rather than in
//! files. `baishitong` reads them, and they become cards here as well (see
//! [`Cards::sync_service`]).
//!
//! The assistant sometimes runs inside a local Cursor conversation, and Cursor's
//! own record is more complete then. A model call with output from the linked
//! Cursor conversation close by is therefore not counted again from the card.
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::baishitong::{self, Batch};
use super::cursor::{Composer, WINDOW_DEFAULT, WINDOW_MAX_MODE};
use super::{clamp_ms, intern, list_dirs, push_tool, touch, FileEntry, FileSig, Recorded};
use crate::estimate::{output_of, ContextModel};
use super::cursor_executor::{Job, Jobs};
use crate::lenient::{text_weight, u64_lenient, Flag, Text, TimeMs, Weight};
use crate::model::{Agent, Phase, ToolCall, Usage, UsageEvent};
use crate::timeutil::{fnv64, MINUTE, SECOND};

pub(super) const DIR: &str = "local.bajie-compatible-native-gateway";

/// A card and a Cursor conversation created this close together are one session.
const LINK_MS: i64 = 10 * SECOND;
/// A Cursor conversation started this long after the card may still be its run,
/// if its output lines up with the card's calls.
const LATE_LINK_MS: i64 = 30 * MINUTE;
/// Cursor output this close to a call means Cursor's record already counts it.
const COVER_MS: i64 = 90 * SECOND;
/// A reply this close to a process row was written by the run those rows record.
const REPLY_COVER_MS: i64 = MINUTE;
/// A tool call starting this soon after the previous one ended came from the same response.
const PARALLEL_MS: i64 = SECOND;
/// Without new rows for this long the assistant is idle (thinking rows can take a minute).
const IDLE_MS: i64 = 2 * MINUTE;
/// A run seen only through its replies reports back every few minutes while it works.
const REMOTE_IDLE_MS: i64 = 5 * MINUTE;
/// A tool still marked running is given up on after this long.
const TOOL_IDLE_MS: i64 = 30 * MINUTE;
/// An executor job is matched to cards once this old, after the gateway's rows for it came in.
const CLAIM_DELAY_MS: i64 = 90 * SECOND;
/// A gateway row this close to a job of the same tool and target ran that job.
const MATCH_MS: i64 = 30 * SECOND;
/// A card with gateway rows this close to a job sends its own steps.
const ROWS_NEAR_MS: i64 = 5 * MINUTE;
/// A card with a message this close to a job was running then.
const ACTIVE_NEAR_MS: i64 = 20 * MINUTE;
/// A word names a card's own work when its messages bring it up this often, and
/// this many times as often as any other card's.
const OWN_WORD_MIN: u32 = 3;
const OWN_WORD_RATIO: u32 = 4;
/// Thinking before a call that came without any: one seen only as an executor
/// job, or a step of the 百事通 service, which seldom has thinking rows. The
/// gateway's rows average about 690 per tool call (2026-11).
const INFERRED_THINKING: u64 = 600;

#[derive(Default, Debug)]
pub(super) struct Cards {
    cards: HashMap<PathBuf, Card>,
}

#[derive(Default, Debug)]
struct Card {
    sig: [Option<FileSig>; 3],
    id: String,
    parent: String,
    name: String,
    model: String,
    created_at: i64,
    deleted: bool,
    /// Every row seen so far, by row id.
    rows: BTreeMap<String, Row>,
    /// The user's messages, by message id.
    prompts: BTreeMap<String, Prompt>,
    /// The assistant's replies, by message id. A remote run whose process rows
    /// never reached this machine leaves only these behind.
    replies: BTreeMap<String, Prompt>,
    /// When the assistant (re)entered a workspace; each entry opens a Cursor conversation.
    entered: Vec<i64>,
    /// Set for a conversation of the 百事通 service, which has no files here.
    service: Option<Service>,
    steps: Vec<Step>,
    prompt_times: Vec<i64>,
    /// End of the newest row, and the tool that row is still running.
    last_ts: i64,
    running_tool: Option<String>,
    /// The newest sign of life is a reply or an executor job, not a process row.
    remote: bool,
    /// How often each word of the card's name and messages (paths, branches)
    /// comes up, to tell which card an executor job worked for.
    vocab: HashMap<String, u32>,
}

/// What the 百事通 service says of a conversation besides its rows.
#[derive(Clone, Debug, Default, PartialEq)]
struct Service {
    cwd: String,
    max_mode: bool,
    /// The assistant is at work on the conversation right now.
    busy: bool,
    /// Since when it has been thinking, if it is.
    thinking_since: Option<i64>,
    /// When its list last saw a message or the assistant: the sign of life to go
    /// by until the conversation's rows have been read.
    active_at: i64,
}

/// What is kept of a process row.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Row {
    pub(super) kind: String,
    pub(super) at: i64,
    pub(super) end: i64,
    pub(super) seq: u64,
    pub(super) duration_ms: u64,
    /// Size of the thinking or visible text.
    pub(super) text: u64,
    /// Size of a tool call's arguments and of its result.
    pub(super) args: u64,
    pub(super) result: u64,
    pub(super) tool: String,
    pub(super) server: String,
    pub(super) ok: bool,
    pub(super) running: bool,
    /// Taken from the executor's log, not sent by the gateway.
    pub(super) inferred: bool,
    /// Fingerprint of the target (see [`target_key`]), to find the step's job in that log.
    pub(super) key: u64,
}

/// A message: when it was sent and how large it is.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Prompt {
    pub(super) ts: i64,
    pub(super) weight: u64,
}

#[derive(Clone, Debug, Default)]
struct Step {
    ts: i64,
    /// Model call number; 0 for a user message.
    call: u32,
    /// Model output: visible text and tool arguments.
    text: u64,
    thinking: u64,
    think_ms: u32,
    /// Content entering the context: the user's message or a tool result.
    input: u64,
    tool: Option<ToolCall>,
}

#[derive(Deserialize)]
struct RawMeta<'a> {
    #[serde(borrow, default)]
    name: Text<'a>,
    #[serde(borrow, default)]
    model: Text<'a>,
    #[serde(rename = "createdAt", default)]
    created_at: TimeMs,
    #[serde(rename = "parentSessionId", borrow, default)]
    parent: Text<'a>,
}

#[derive(Default, Deserialize)]
struct RawContext<'a> {
    #[serde(borrow, default)]
    history: Vec<RawMessage<'a>>,
}

#[derive(Deserialize)]
struct RawMessage<'a> {
    #[serde(rename = "messageId", borrow, default)]
    id: Text<'a>,
    #[serde(borrow, default)]
    role: Text<'a>,
    #[serde(borrow, default)]
    text: Text<'a>,
    #[serde(rename = "createdAt", default)]
    created_at: TimeMs,
}

#[derive(Deserialize)]
struct RawProcess<'a> {
    #[serde(borrow, default)]
    rows: Vec<RawRow<'a>>,
}

#[derive(Deserialize)]
struct RawRow<'a> {
    #[serde(borrow, default)]
    id: Text<'a>,
    #[serde(borrow, default)]
    kind: Text<'a>,
    #[serde(default)]
    at: TimeMs,
    #[serde(rename = "endedAt", default)]
    ended_at: TimeMs,
    #[serde(default, deserialize_with = "u64_lenient")]
    seq: u64,
    #[serde(rename = "durationMs", default, deserialize_with = "u64_lenient")]
    duration_ms: u64,
    #[serde(default)]
    text: Weight,
    #[serde(borrow, default)]
    tool: Text<'a>,
    #[serde(borrow, default)]
    server: Text<'a>,
    /// What the extension shows for a step of its own (`own: true`), which has no tool name.
    #[serde(borrow, default)]
    label: Text<'a>,
    #[serde(borrow, default)]
    status: Text<'a>,
    #[serde(default)]
    ok: Option<Flag>,
    #[serde(borrow, default)]
    target: Text<'a>,
    #[serde(default)]
    patch: Weight,
    #[serde(default)]
    output: Weight,
    #[serde(default)]
    detail: Weight,
}

/// Lower-case pieces of the paths, branches and names in a text, three letters or longer.
fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|word| word.len() >= 3 && word.bytes().any(|b| b.is_ascii_alphabetic()))
        .map(str::to_ascii_lowercase)
}

pub(super) fn short(text: &str) -> String {
    text.chars().filter(|ch| !ch.is_control()).take(80).collect()
}

fn digest(row: &RawRow) -> Option<(String, Row)> {
    let kind = row.kind.as_str();
    if !matches!(kind, "thinking" | "text" | "tool" | "turn") || row.at.0 <= 0 {
        return None;
    }
    let id = if row.id.is_empty() { format!("{}@{}", row.seq, row.at.0) } else { short(row.id.as_str()) };
    let status = row.status.as_str();
    let tool = short(if row.tool.is_empty() && kind == "tool" { row.label.as_str() } else { row.tool.as_str() });
    Some((
        id,
        Row {
            kind: kind.to_owned(),
            at: row.at.0,
            end: row.ended_at.0.max(row.at.0),
            seq: row.seq,
            duration_ms: row.duration_ms,
            text: row.text.0,
            args: text_weight(row.target.as_str()) + row.patch.0,
            result: row.output.0 + row.detail.0,
            tool: tool.clone(),
            server: short(row.server.as_str()),
            ok: row.ok.map_or(true, |flag| flag.0) && !matches!(status, "error" | "failed"),
            running: matches!(status, "running" | "pending" | "loading"),
            inferred: false,
            key: label_of(&tool).map_or(0, |label| target_key(label, row.target.as_str())),
        },
    ))
}

/// The executor's label for a gateway tool (its log names tools by label). The
/// extension's tools are named `workspace_…`, the 百事通 service's `baishitong_…`.
fn label_of(tool: &str) -> Option<&'static str> {
    let name = tool.strip_prefix("workspace_").or_else(|| tool.strip_prefix("baishitong_")).unwrap_or(tool);
    Some(match name {
        "exec" | "运行" => "运行",
        "read_file" | "读取" => "读取",
        "edit_file" | "修改" => "修改",
        "write_file" | "写入" => "写入",
        "read_image" | "查看图片" => "查看图片",
        "search" | "find_files" | "搜索" => "搜索",
        "list_dir" | "列目录" => "列目录",
        "delete" | "删除" => "删除",
        "read_lints" | "查诊断" => "查诊断",
        "shell_start" | "启动终端" => "启动终端",
        "shell_read" | "读终端" => "读终端",
        "shell_input" | "输入终端" => "输入终端",
        "shell_stop" | "停止终端" => "停止终端",
        "notes" => "notes",
        _ => return None,
    })
}

/// A short fingerprint of a step's target, the same for the gateway's row and
/// the executor's log line: a command's start, or a file's name.
fn target_key(label: &str, target: &str) -> u64 {
    let target = target.trim();
    let key: String = if matches!(label, "运行" | "启动终端" | "输入终端") {
        target.split_whitespace().flat_map(|word| word.chars().chain([' '])).take(32).collect()
    } else {
        target.rsplit(['/', '\\']).next().unwrap_or(target).to_lowercase()
    };
    let key = key.trim();
    if key.is_empty() { 0 } else { fnv64(&[key.as_bytes()]) }
}

/// What a step puts into the conversation when no sizes came with it: the
/// arguments the model wrote and the result it read back. Measured on the
/// gateway's rows that carry sizes (2026-11); file and image reads are never
/// kept by the gateway, so theirs are assumed.
fn typical(tool: &str) -> (u64, u64) {
    match label_of(tool).unwrap_or("") {
        "运行" => (180, 1600),
        "修改" => (1600, 10),
        "写入" => (3400, 10),
        "读取" | "查看图片" => (40, 3000),
        "启动终端" | "读终端" | "输入终端" | "停止终端" => (120, 800),
        "搜索" | "列目录" | "查诊断" => (40, 800),
        _ => (40, 100),
    }
}

impl Card {
    /// Merge the card's current files into what is already known; `None` when they
    /// cannot be read right now (e.g. halfway through a rewrite). Rows the extension
    /// has dropped from its file since they were read stay known.
    fn merge_files(&mut self, dir: &Path) -> Option<()> {
        let meta_bytes = fs::read(dir.join("meta.json")).ok()?;
        let meta: RawMeta = serde_json::from_slice(&meta_bytes).ok()?;
        let process_bytes = match fs::read(dir.join("process.json")) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => b"{}".to_vec(),
            read => read.ok()?,
        };
        let process: RawProcess = serde_json::from_slice(&process_bytes).ok()?;
        let context_bytes = fs::read(dir.join("context.json")).unwrap_or_default();
        let context: RawContext = serde_json::from_slice(&context_bytes).unwrap_or_default();

        self.id = dir.file_name()?.to_string_lossy().into_owned();
        self.parent = short(meta.parent.as_str());
        self.name = meta.name.as_str().chars().filter(|ch| !ch.is_control()).take(120).collect();
        self.model = short(meta.model.as_str());
        self.created_at = meta.created_at.0;
        self.rows.extend(process.rows.iter().filter_map(digest));
        let mut entered = Vec::new();
        let mut vocab: HashMap<String, u32> = HashMap::new();
        let texts = std::iter::once(meta.name.as_str()).chain(context.history.iter().map(|message| message.text.as_str()));
        for word in texts.flat_map(words) {
            *vocab.entry(word).or_default() += 1;
        }
        for (index, message) in context.history.iter().enumerate() {
            if message.created_at.0 <= 0 {
                continue;
            }
            let id = if message.id.is_empty() { format!("{}#{index}", message.created_at.0) } else { short(message.id.as_str()) };
            let prompt = Prompt { ts: message.created_at.0, weight: text_weight(message.text.as_str()) };
            let known = match message.role.as_str() {
                "user" => &mut self.prompts,
                // Status lines the extension writes itself, not model output.
                "assistant" if id.starts_with("workspace-") => {
                    entered.push(prompt.ts);
                    continue;
                }
                "assistant" => &mut self.replies,
                _ => continue,
            };
            known.insert(id, prompt);
        }
        entered.sort_unstable();
        self.entered = entered;
        self.vocab = vocab;
        Some(())
    }

    /// Turn the remembered rows into model calls and find the current activity.
    fn rebuild(&mut self) {
        let mut rows: Vec<&Row> = self.rows.values().collect();
        // A reply with no process rows around it came from a run whose rows never
        // arrived (e.g. the gateway lost its executor): it is still one model call.
        let lone: Vec<Row> = self
            .replies
            .values()
            .filter(|reply| !rows.iter().any(|row| row.at - REPLY_COVER_MS <= reply.ts && reply.ts <= row.end + REPLY_COVER_MS))
            .map(|reply| Row { kind: "reply".into(), at: reply.ts, end: reply.ts, text: reply.weight, ..Row::default() })
            .collect();
        rows.extend(lone.iter());
        rows.sort_by_key(|row| (row.at, row.seq));
        let mut prompts: Vec<Prompt> = self.prompts.values().copied().collect();
        prompts.sort_by_key(|prompt| prompt.ts);

        // The service's rows seldom include the thinking that went before a call.
        let bare = self.service.is_some();
        let mut steps = Vec::with_capacity(rows.len() + prompts.len());
        let mut pending = prompts.iter().copied().peekable();
        let (mut call, mut open, mut after_tool, mut tools_end) = (0u32, false, false, 0i64);
        for row in &rows {
            while let Some(prompt) = pending.next_if(|prompt| prompt.ts <= row.at) {
                steps.push(Step { ts: prompt.ts, input: prompt.weight, ..Step::default() });
                (open, after_tool) = (false, false);
            }
            match row.kind.as_str() {
                kind @ ("thinking" | "text") => {
                    if !open || after_tool {
                        call += 1;
                        open = true;
                    }
                    after_tool = false;
                    let thinking = kind == "thinking";
                    steps.push(Step {
                        ts: row.end,
                        call,
                        text: if thinking { 0 } else { row.text },
                        thinking: if thinking { row.text } else { 0 },
                        think_ms: if thinking { clamp_ms(row.duration_ms.min(i64::MAX as u64) as i64) } else { 0 },
                        ..Step::default()
                    });
                }
                "tool" => {
                    // Calls overlapping the previous tool (or right behind it) were issued together.
                    let parallel = after_tool && row.at < tools_end + PARALLEL_MS;
                    let starts = !open || (after_tool && !parallel);
                    if starts {
                        call += 1;
                        open = true;
                    }
                    tools_end = if parallel { tools_end.max(row.end) } else { row.end };
                    after_tool = true;
                    // The gateway keeps no file or image it read; the executor's log no sizes at all.
                    let (args, result) = typical(&row.tool);
                    let read = matches!(label_of(&row.tool), Some("读取" | "查看图片"));
                    steps.push(Step {
                        ts: row.at,
                        call,
                        text: if row.inferred { args } else { row.args } + row.tool.len() as u64,
                        // A call that came without its thinking still thought before the first tool.
                        thinking: if (row.inferred || bare) && starts { INFERRED_THINKING } else { 0 },
                        input: if row.inferred || (read && row.result == 0) { result } else { row.result },
                        tool: (!row.tool.is_empty()).then(|| ToolCall { ts: row.at, name: row.tool.clone(), server: row.server.clone(), ok: row.ok }),
                        ..Step::default()
                    });
                }
                "reply" => {
                    call += 1;
                    steps.push(Step { ts: row.at, call, text: row.text, ..Step::default() });
                    (open, after_tool) = (false, false);
                }
                _ => (open, after_tool) = (false, false),
            }
        }
        steps.extend(pending.map(|prompt| Step { ts: prompt.ts, input: prompt.weight, ..Step::default() }));

        self.prompt_times = prompts.iter().map(|prompt| prompt.ts).collect();
        self.last_ts = rows.iter().map(|row| row.end).max().unwrap_or(0);
        self.remote = rows.last().is_some_and(|row| row.kind == "reply" || row.inferred);
        self.running_tool = rows.last().filter(|row| row.kind == "tool" && row.running).map(|row| row.tool.clone());
        self.steps = steps;
    }

    /// Start and end of every model call, in order.
    fn calls(&self) -> Vec<(i64, i64)> {
        let mut spans: Vec<(u32, i64, i64)> = Vec::new();
        for step in self.steps.iter().filter(|step| step.call > 0) {
            match spans.last_mut() {
                Some((call, _, end)) if *call == step.call => *end = (*end).max(step.ts),
                _ => spans.push((step.call, step.ts, step.ts)),
            }
        }
        spans.into_iter().map(|(_, from, to)| (from, to)).collect()
    }
}

fn covers(composer: &Composer, from: i64, to: i64) -> bool {
    let first = composer.outputs.partition_point(|ts| *ts < from - COVER_MS);
    composer.outputs.get(first).is_some_and(|ts| *ts <= to + COVER_MS)
}

/// Which Cursor conversation each card ran in, if any. Conversations created with
/// the card come first; otherwise one started a little later whose output covers
/// most of the card's calls. Parallel conversations overlap in time, so overlap
/// alone never links a conversation created before the card.
fn links<'a>(cards: &'a HashMap<PathBuf, Card>, composers: &'a HashMap<String, Composer>) -> HashMap<&'a str, (&'a str, &'a Composer)> {
    // The service's assistants run in the cloud, never in a local Cursor conversation.
    let mut ordered: Vec<&Card> = cards.values().filter(|card| !card.id.is_empty() && card.created_at > 0 && card.service.is_none()).collect();
    ordered.sort_by_key(|card| (card.created_at, card.id.as_str()));
    let mut links = HashMap::new();
    let mut taken: HashSet<&str> = HashSet::new();
    for card in &ordered {
        let found = composers
            .iter()
            .filter(|(id, composer)| composer.created_at > 0 && !taken.contains(id.as_str()))
            .map(|(id, composer)| ((composer.created_at - card.created_at).abs(), id.as_str(), composer))
            .filter(|(gap, _, _)| *gap <= LINK_MS)
            .min_by_key(|(gap, id, _)| (*gap, *id));
        if let Some((_, id, composer)) = found {
            taken.insert(id);
            links.insert(card.id.as_str(), (id, composer));
        }
    }
    for card in &ordered {
        if links.contains_key(card.id.as_str()) {
            continue;
        }
        let calls = card.calls();
        let found = composers
            .iter()
            .filter(|(id, composer)| {
                let gap = composer.created_at - card.created_at;
                !taken.contains(id.as_str()) && gap > -MINUTE && gap <= LATE_LINK_MS
            })
            .map(|(id, composer)| (calls.iter().filter(|(from, to)| covers(composer, *from, *to)).count(), id.as_str(), composer))
            .filter(|(covered, _, _)| *covered > 0 && *covered * 2 >= calls.len())
            .max_by_key(|(covered, id, _)| (*covered, std::cmp::Reverse(*id)));
        if let Some((_, id, composer)) = found {
            taken.insert(id);
            links.insert(card.id.as_str(), (id, composer));
        }
    }
    links
}

impl Cards {
    /// Re-read the cards whose files changed; returns whether anything changed.
    pub(super) fn refresh(&mut self, root: &Path) -> bool {
        let mut present = HashSet::new();
        let mut changed = false;
        for instance in list_dirs(&root.join("conversations")) {
            for (folder, deleted) in [("sessions", false), ("trash", true)] {
                for dir in list_dirs(&instance.join(folder)) {
                    let sig = ["process.json", "context.json", "meta.json"].map(|name| FileSig::of(&dir.join(name)));
                    // A card whose run happens on the gateway may never get process rows.
                    if sig[2].is_none() {
                        continue;
                    }
                    present.insert(dir.clone());
                    let card = match self.cards.entry(dir.clone()) {
                        Entry::Occupied(known) if known.get().sig == sig => continue,
                        Entry::Occupied(known) => known.into_mut(),
                        Entry::Vacant(slot) => slot.insert(Card::default()),
                    };
                    // A card being rewritten keeps its previous reading until it parses again.
                    if card.merge_files(&dir).is_none() {
                        continue;
                    }
                    card.sig = sig;
                    card.deleted = deleted;
                    card.rebuild();
                    changed = true;
                }
            }
        }
        let before = self.cards.len();
        self.cards.retain(|dir, card| card.service.is_some() || present.contains(dir));
        changed || self.cards.len() != before
    }

    /// Take in what the 百事通 feed has read since the last pass; returns whether
    /// anything changed. Its conversations have no files: each becomes a card under
    /// `<globalStorage>/local.baishitong-customer/sessions/<id>`, a path that only
    /// names it.
    pub(super) fn sync_service(&mut self, global: &Path, batch: Batch) -> bool {
        let root = global.join(baishitong::DIR).join("sessions");
        let mut changed = false;
        if let Some(list) = batch.list {
            let listed: HashSet<PathBuf> = list.iter().map(|session| root.join(&session.id)).collect();
            let before = self.cards.len();
            // A conversation the service no longer lists is gone; what it used stays in the usage history.
            self.cards.retain(|dir, card| card.service.is_none() || listed.contains(dir));
            changed |= self.cards.len() != before;
            for session in list {
                let active_at = session.active_at;
                let card = self.cards.entry(root.join(&session.id)).or_default();
                let mut service = card.service.clone().unwrap_or_default();
                (service.cwd, service.max_mode, service.busy) = (session.cwd, session.max_mode, session.busy);
                if !service.busy {
                    service.thinking_since = None;
                }
                let same = card.id == session.id
                    && card.parent == session.parent
                    && card.name == session.name
                    && card.model == session.model
                    && card.created_at == session.created_at
                    && card.service.as_ref() == Some(&service);
                if !same {
                    (card.id, card.parent, card.name, card.model) = (session.id, session.parent, session.name, session.model);
                    (card.created_at, card.service) = (session.created_at, Some(service));
                    changed = true;
                }
                // It moves with every step the assistant takes, and only the live state
                // goes by it: no change to report.
                if let Some(service) = card.service.as_mut() {
                    service.active_at = active_at;
                }
            }
        }
        for (id, page) in batch.pages {
            let Some(card) = self.cards.get_mut(&root.join(&id)) else { continue };
            let Some(service) = card.service.as_mut() else { continue };
            // An answer that says "unchanged" brings no rows and so no word on the thinking either.
            let mut fresh = !page.partial && service.thinking_since != page.thinking_since;
            if fresh {
                service.thinking_since = page.thinking_since;
            }
            for (id, row) in page.rows {
                if card.rows.get(&id) != Some(&row) {
                    card.rows.insert(id, row);
                    fresh = true;
                }
            }
            for (known, messages) in [(&mut card.prompts, page.prompts), (&mut card.replies, page.replies)] {
                for (id, message) in messages {
                    if known.get(&id) != Some(&message) {
                        known.insert(id, message);
                        fresh = true;
                    }
                }
            }
            if fresh {
                card.rebuild();
                changed = true;
            }
        }
        if !batch.alive {
            // Without word from the plugin nothing is known to be running.
            for service in self.cards.values_mut().filter_map(|card| card.service.as_mut()) {
                if service.busy || service.thinking_since.is_some() {
                    (service.busy, service.thinking_since) = (false, None);
                    changed = true;
                }
            }
        }
        changed
    }

    /// One entry per card. A card that ran in a Cursor conversation takes that
    /// conversation's id, so both records show up as one session.
    ///
    /// `recorded` is what the usage history already holds of each card. What earlier
    /// runs of the app put there stays as it is: the rows it was worked out from are
    /// not kept, and the source may have dropped them since, so working it out again
    /// could lose requests or count them twice. Rows only give what came after it.
    pub(super) fn entries(&self, composers: &HashMap<String, Composer>, recorded: &Recorded, now: i64) -> Vec<FileEntry> {
        let links = links(&self.cards, composers);
        let session_of = |card: &str| match links.get(card) {
            Some((id, _)) => (*id).to_owned(),
            None => format!("gateway/{card}"),
        };
        // A conversation of the service named like a card on disk would be recorded as that card.
        let on_disk: HashSet<&str> = self.cards.values().filter(|card| card.service.is_none()).map(|card| card.id.as_str()).collect();
        let mut entries = Vec::with_capacity(self.cards.len());
        for (dir, card) in self.cards.iter().filter(|(_, card)| !card.id.is_empty()) {
            if card.service.is_some() && on_disk.contains(card.id.as_str()) {
                continue;
            }
            let linked = links.get(card.id.as_str()).copied();
            let mut entry = FileEntry::new(Agent::Cursor, dir.join("process.json"));
            entry.plugin = true;
            if let Some((id, _)) = linked {
                entry.host = id.to_owned();
            }
            entry.gone = card.deleted;
            if card.parent.is_empty() {
                entry.session.id = session_of(&card.id);
            } else {
                entry.session.sub_agent = true;
                entry.session.parent_id = session_of(&card.parent);
                entry.session.id.clone_from(&entry.session.parent_id);
            }
            entry.session.title.clone_from(&card.name);
            // The conversation the card runs in now: the linked one, or the one
            // Cursor opened when the assistant last entered a workspace.
            let host = linked.map(|(_, composer)| composer).or_else(|| entered_in(card, composers));
            // Only its folder: that conversation's model relays to the gateway,
            // which runs a model of its own.
            if let Some(composer) = host {
                entry.session.cwd.clone_from(&composer.cwd);
            } else if let Some(service) = &card.service {
                entry.session.cwd.clone_from(&service.cwd);
            }
            touch(&mut entry.session, card.created_at);

            let max_mode = card.service.as_ref().is_some_and(|service| service.max_mode) || card.model.to_ascii_lowercase().contains("max");
            let mut context = ContextModel::new(if max_mode { WINDOW_MAX_MODE } else { WINDOW_DEFAULT });
            let covered = |from: i64, to: i64| linked.is_some_and(|(_, composer)| covers(composer, from, to));

            // The stretch earlier runs recorded, taken over as it is.
            let known = recorded.cards.get(card.id.as_str());
            let frozen_until = known.map_or(0, |known| known.prior_until);
            let mut frozen: Vec<&UsageEvent> =
                known.map_or(&[][..], |known| known.events.as_slice()).iter().filter(|event| event.ts <= frozen_until).collect();
            frozen.sort_by_key(|event| event.ts);
            // The context goes on from the size the last of them was sent with.
            let resumed_at = frozen.last().map_or(i64::MIN, |event| event.ts);
            if let Some(last) = frozen.last() {
                context.resume(last.usage.input + last.usage.cache_read + last.usage.cache_write, last.ts);
            }
            for event in frozen {
                let name = known.and_then(|known| known.models.get(usize::from(event.model))).map_or(card.model.as_str(), String::as_str);
                let model = intern(&mut entry.models, name);
                touch(&mut entry.session, event.ts);
                entry.events.push(UsageEvent { model, ..event.clone() });
            }

            let mut index = 0;
            while index < card.steps.len() {
                let call = card.steps[index].call;
                let end = card.steps[index..].iter().position(|step| step.call != call).map_or(card.steps.len(), |n| index + n);
                let group = &card.steps[index..if call == 0 { index + 1 } else { end }];
                index += group.len();
                if call == 0 {
                    // A message from before the context was resumed is part of it already.
                    if group[0].ts > resumed_at {
                        context.add(group[0].input);
                        touch(&mut entry.session, group[0].ts);
                    }
                    continue;
                }
                let from = group[0].ts;
                let to = group.iter().map(|step| step.ts).max().unwrap_or(from);
                if from <= frozen_until {
                    // Counted as recorded. The history keeps no tool names; the rows still here do.
                    if !covered(from, to) {
                        for tool in group.iter().filter_map(|step| step.tool.clone()) {
                            push_tool(&mut entry.tools, tool);
                        }
                    }
                    // The last recorded request: what it wrote and read came into the context after it was sent.
                    if to >= resumed_at {
                        for step in group {
                            context.add(step.text + step.thinking + step.input);
                        }
                    }
                    continue;
                }
                let usage = context.request(from);
                let (mut text, mut thinking, mut think_ms) = (0, 0, 0u32);
                for step in group {
                    text += step.text;
                    thinking += step.thinking;
                    think_ms = think_ms.saturating_add(step.think_ms);
                    context.add(step.text + step.thinking + step.input);
                    touch(&mut entry.session, step.ts);
                }
                if covered(from, to) {
                    continue;
                }
                let (output, reasoning) = output_of(text, thinking);
                let model = intern(&mut entry.models, &card.model);
                entry.events.push(UsageEvent {
                    ts: to,
                    usage: Usage { output, reasoning, ..usage },
                    model,
                    key: fnv64(&[b"cursor-gateway", card.id.as_bytes(), &from.to_le_bytes()]),
                    think_ms,
                    reasoning_estimated: reasoning > 0,
                    estimated: true,
                    ..UsageEvent::default()
                });
                for tool in group.iter().filter_map(|step| step.tool.clone()) {
                    push_tool(&mut entry.tools, tool);
                }
            }

            // The newest sign of life. A conversation of the service whose rows have not
            // been read yet (the app has just started) goes by what the list says of it.
            let listed_at = card.service.as_ref().map_or(0, |service| service.active_at);
            let seen = if card.last_ts > 0 { card.last_ts } else { listed_at };
            let unread = card.last_ts == 0 && seen > 0;
            let turn_start = card.prompt_times.iter().rev().find(|ts| **ts <= seen).copied().unwrap_or(if unread { seen } else { card.created_at });
            let quiet = now - seen;
            let active = match &card.service {
                // The service says whether an assistant is at work; its rows come in up to a minute late.
                Some(service) => service.busy && (service.thinking_since.is_some() || (seen > 0 && quiet <= TOOL_IDLE_MS)),
                None => {
                    !card.deleted
                        && card.last_ts > 0
                        && (quiet <= IDLE_MS
                            || (card.remote && quiet <= REMOTE_IDLE_MS)
                            || (card.running_tool.is_some() && quiet <= TOOL_IDLE_MS))
                }
            };
            // The service's assistant is thinking, and nothing has come back since it began.
            let thinking = card.service.as_ref().and_then(|service| service.thinking_since).filter(|since| active && *since >= card.last_ts);
            // A message just sent waits for the gateway before any row comes back.
            let asked = card.prompt_times.last().copied().filter(|ts| *ts > card.last_ts && now - *ts <= IDLE_MS);
            if let Some(since) = thinking {
                entry.live.open_turn(card.prompt_times.iter().rev().find(|ts| **ts <= since).copied().unwrap_or(turn_start));
                entry.live.set_phase(Phase::Thinking, since);
            } else if let (Some(ts), false) = (asked, card.deleted) {
                entry.live.open_turn(ts);
                entry.live.set_phase(Phase::Requesting, ts);
            } else if active {
                entry.live.open_turn(turn_start);
                entry.live.set_phase(if card.running_tool.is_some() { Phase::Tool } else { Phase::Generating }, seen);
                entry.live.tool = card.running_tool.clone().unwrap_or_default();
            } else if card.last_ts > 0 {
                entry.live.turns = card.prompt_times.len().max(1) as u32;
                entry.live.last_turn_end_ms = card.last_ts;
                entry.live.last_turn_ms = (card.last_ts - turn_start).max(0) as u64;
            }
            entries.push(entry);
        }
        entries
    }
}

impl Cards {
    /// Give the executor's jobs that no gateway row accounts for to the cards
    /// they most likely served (see [`owner`]); whether any card changed.
    pub(super) fn adopt(&mut self, jobs: &Jobs, now: i64) -> bool {
        let until = now - CLAIM_DELAY_MS;
        // Event time is not an ingestion cursor: channels, lines and card files
        // can become readable after later jobs have already been processed.
        // Replay retained jobs by identity, retrying those with no owner yet.
        let mut fresh: Vec<&Job> = jobs.all().filter(|job| job.ts <= until).collect();
        if fresh.is_empty() {
            return false;
        }
        fresh.sort_by(|a, b| (a.ts, a.label, &a.target, a.ms, a.ok).cmp(&(b.ts, b.label, &b.target, b.ms, b.ok)));
        let mut adopted: Vec<(PathBuf, String, Row)> = Vec::new();
        let mut replaced: Vec<(PathBuf, String)> = Vec::new();
        {
            // The executor runs the extension's steps; the service's assistants never go through it.
            let mut views: Vec<View> =
                self.cards.iter().filter(|(_, card)| !card.id.is_empty() && card.service.is_none()).map(|(dir, card)| view(dir, card)).collect();
            views.sort_by(|a, b| a.card.id.cmp(&b.card.id));
            // The gateway's own steps around these jobs; each accounts for one job.
            let (low, high) = (fresh[0].ts - MATCH_MS, until + MATCH_MS);
            let mut sent: Vec<(i64, &'static str, u64)> = views
                .iter()
                .flat_map(|view| view.card.rows.values())
                .filter(|row| !row.inferred && row.kind == "tool" && (low..=high).contains(&row.at))
                .filter_map(|row| Some((row.at, label_of(&row.tool)?, row.key)))
                .collect();
            sent.sort_unstable();
            let mut used = vec![false; sent.len()];
            let known: HashMap<&str, &Path> = views.iter().flat_map(|view| {
                view.card.rows.iter().filter(|(_, row)| row.inferred).map(|(id, _)| (id.as_str(), view.dir))
            }).collect();
            let mut repeats: HashMap<String, u32> = HashMap::new();
            for job in fresh {
                let key = target_key(job.label, &job.target);
                let mut id = format!("x:{}:{:x}", job.ts, fnv64(&[job.label.as_bytes(), job.target.as_bytes()]));
                let seen = repeats.entry(id.clone()).or_default();
                *seen += 1;
                if *seen > 1 {
                    id = format!("{id}:{seen}");
                }
                let first = sent.partition_point(|(at, _, _)| *at < job.ts - MATCH_MS);
                let claim = (first..sent.len())
                    .take_while(|&i| sent[i].0 <= job.ts + MATCH_MS)
                    .filter(|&i| !used[i] && sent[i].1 == job.label && (sent[i].2 == 0 || key == 0 || sent[i].2 == key))
                    .min_by_key(|&i| (sent[i].0 - job.ts).abs());
                if let Some(i) = claim {
                    used[i] = true;
                    // A delayed official row supersedes its earlier estimate,
                    // including when the job was initially assigned elsewhere.
                    if let Some(dir) = known.get(id.as_str()) {
                        replaced.push((dir.to_path_buf(), id));
                    }
                    continue;
                }
                if known.contains_key(id.as_str()) {
                    continue;
                }
                let Some(i) = owner(&views, job) else { continue };
                views[i].held += 1;
                let row = Row { kind: "tool".into(), at: job.ts, end: job.ts + job.ms.max(0), tool: job.label.into(), server: "executor".into(), ok: job.ok, inferred: true, key, ..Row::default() };
                adopted.push((views[i].dir.to_path_buf(), id, row));
            }
        }
        let mut touched = HashSet::new();
        for (dir, id) in replaced {
            if let Some(card) = self.cards.get_mut(&dir) {
                card.rows.remove(&id);
                touched.insert(dir);
            }
        }
        for (dir, id, row) in adopted {
            if let Some(card) = self.cards.get_mut(&dir) {
                card.rows.insert(id, row);
                touched.insert(dir);
            }
        }
        for dir in &touched {
            if let Some(card) = self.cards.get_mut(dir) {
                card.rebuild();
            }
        }
        !touched.is_empty()
    }

    /// Cursor conversations opened only to relay a card into a workspace.
    pub(super) fn relays<'a>(&self, composers: &'a HashMap<String, Composer>) -> HashSet<&'a str> {
        let entered: Vec<i64> = self.cards.values().flat_map(|card| card.entered.iter().copied()).collect();
        composers
            .iter()
            .filter(|(_, composer)| composer.created_at > 0 && entered.iter().any(|at| (composer.created_at - at).abs() <= LINK_MS))
            .map(|(id, _)| id.as_str())
            .collect()
    }
}

/// What `adopt` needs to know of a card, apart from the card.
struct View<'a> {
    dir: &'a Path,
    card: &'a Card,
    /// Start times of the gateway's rows and of the card's messages, ascending.
    rows: Vec<i64>,
    messages: Vec<i64>,
    /// Jobs the card already holds, to keep an even split where nothing tells them apart.
    held: usize,
}

fn view<'a>(dir: &'a Path, card: &'a Card) -> View<'a> {
    let mut rows: Vec<i64> = card.rows.values().filter(|row| !row.inferred).map(|row| row.at).collect();
    rows.sort_unstable();
    let mut messages: Vec<i64> =
        card.prompts.values().chain(card.replies.values()).map(|message| message.ts).chain(card.entered.iter().copied()).collect();
    messages.sort_unstable();
    let held = card.rows.values().filter(|row| row.inferred).count();
    View { dir, card, rows, messages, held }
}

fn near(times: &[i64], ts: i64, within: i64) -> bool {
    let first = times.partition_point(|at| *at < ts - within);
    times.get(first).is_some_and(|at| *at <= ts + within)
}

/// The card most likely served by a job none of the gateway's rows account for:
/// one that was running then without sending rows, preferring the card whose
/// messages name the job's files or commands.
fn owner(views: &[View], job: &Job) -> Option<usize> {
    let candidates: Vec<usize> = (0..views.len())
        .filter(|&i| {
            let view = &views[i];
            !view.card.deleted
                && view.card.created_at <= job.ts + LINK_MS
                && !near(&view.rows, job.ts, ROWS_NEAR_MS)
                && near(&view.messages, job.ts, ACTIVE_NEAR_MS)
        })
        .collect();
    if candidates.len() <= 1 {
        return candidates.first().copied();
    }
    let count = |i: usize, word: &str| views[i].card.vocab.get(word).copied().unwrap_or(0);
    // A word of the job counts for the card that brings it up far more often than
    // every other candidate (a branch or folder name); shared words tell nothing.
    let dominant = |i: usize, word: &str| {
        let own = count(i, word);
        own >= OWN_WORD_MIN && candidates.iter().all(|&c| c == i || count(c, word) * OWN_WORD_RATIO <= own)
    };
    let words: Vec<String> = words(&job.target).collect();
    let score = |i: usize| words.iter().filter(|word| dominant(i, word)).count();
    // When no word tells them apart: the card that spoke closest to the job, then
    // the one holding fewer jobs.
    let gap = |i: usize| {
        let messages = &views[i].messages;
        let next = messages.partition_point(|at| *at < job.ts);
        let before = next.checked_sub(1).map(|k| job.ts - messages[k]);
        let after = messages.get(next).map(|at| at - job.ts);
        before.into_iter().chain(after).min().unwrap_or(i64::MAX) / MINUTE
    };
    candidates.iter().copied().max_by_key(|&i| {
        (score(i), std::cmp::Reverse(gap(i)), std::cmp::Reverse(views[i].held), std::cmp::Reverse(views[i].card.id.as_str()))
    })
}

/// The Cursor conversation opened when the card last entered a workspace.
fn entered_in<'a>(card: &Card, composers: &'a HashMap<String, Composer>) -> Option<&'a Composer> {
    card.entered.iter().rev().find_map(|at| {
        composers
            .iter()
            .filter(|(_, composer)| composer.created_at > 0 && (composer.created_at - at).abs() <= LINK_MS)
            .min_by_key(|(id, composer)| ((composer.created_at - at).abs(), id.as_str()))
            .map(|(_, composer)| composer)
    })
}

#[cfg(test)]
mod tests {
    use super::super::RecordedCard;
    use super::*;
    use serde_json::{json, Value};

    fn write_card(root: &Path, name: &str, created: i64, rows: Value, history: Value) -> PathBuf {
        let dir = root.join("conversations/local/sessions").join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("meta.json"), json!({"name": "地图1", "model": "claude-opus-5-5-max-fast", "createdAt": created}).to_string()).unwrap();
        fs::write(dir.join("process.json"), json!({"schemaVersion": 1, "rows": rows}).to_string()).unwrap();
        fs::write(dir.join("context.json"), json!({"history": history}).to_string()).unwrap();
        dir
    }

    fn temp_root(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("foxes-gateway-{tag}-{}-{}", std::process::id(), crate::timeutil::now_ms()))
    }

    #[test]
    fn splits_rows_into_calls_and_skips_calls_cursor_already_recorded() {
        let root = temp_root("calls");
        let t = 1_800_000_000_000i64;
        let rows = json!([
            {"id":"r1","kind":"thinking","seq":1,"at":t+1000,"endedAt":t+3000,"durationMs":2000,"text":"plan the change"},
            {"id":"r2","kind":"tool","seq":2,"at":t+3500,"endedAt":t+4000,"tool":"workspace_exec","server":"workspace","status":"completed","ok":true,"target":"cargo test","output":"ok".repeat(500)},
            {"id":"r3","kind":"tool","seq":3,"at":t+3600,"endedAt":t+4100,"tool":"workspace_read_file","server":"workspace","status":"completed","ok":true,"target":"a.rs"},
            {"id":"r4","kind":"tool","seq":4,"at":t+9000,"endedAt":t+9500,"tool":"workspace_exec","server":"workspace","status":"completed","ok":false,"target":"cargo build"},
            {"id":"r5","kind":"text","seq":5,"at":t+12000,"endedAt":t+12100,"text":"done"},
            {"id":"r6","kind":"note","seq":6,"at":t+12200,"endedAt":t+12200,"text":"stopped"},
            {"id":"r7","kind":"thinking","seq":7,"at":t+600_000,"endedAt":t+601_000,"text":"next"},
            {"id":"r8","kind":"tool","seq":8,"at":t+602_000,"tool":"workspace_exec","server":"workspace","status":"running","target":"npm run build"}
        ]);
        let history = json!([
            {"messageId":"m1","role":"user","text":"fix it","createdAt":t},
            {"messageId":"m2","role":"assistant","text":"ok","createdAt":t+12000},
            {"messageId":"m3","role":"user","text":"and build","createdAt":t+599_000}
        ]);
        write_card(&root, "card-1", t, rows, history);
        let mut cards = Cards::default();
        assert!(cards.refresh(&root));
        assert!(!cards.refresh(&root), "unchanged files are not re-read");

        let entries = cards.entries(&HashMap::new(), &Recorded::default(), t + 603_000);
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.session.id, "gateway/card-1");
        assert_eq!(entry.session.title, "地图1");
        // thinking + two overlapping tools | a later tool on its own | the answer | the second turn
        assert_eq!(entry.events.len(), 4);
        assert!(entry.events.iter().all(|event| event.estimated && event.usage.output > 0));
        assert_eq!(entry.events[0].think_ms, 2000);
        assert!(entry.events[1].usage.cache_read > 0 && entry.events[1].usage.input > 0);
        assert_eq!(entry.tools.len(), 4);
        assert!(!entry.tools[2].ok);
        assert!(entry.live.turn_open);
        assert_eq!(entry.live.turn_started_ms, t + 599_000);
        assert_eq!(entry.live.phase, Phase::Tool);
        assert_eq!(entry.live.tool, "workspace_exec");

        // The same card driven through a Cursor conversation: only the calls Cursor did not record count.
        let composers = HashMap::from([(
            "c1".to_owned(),
            Composer { created_at: t + 150, cwd: r"H:\work".into(), outputs: vec![t + 1000, t + 3500, t + 9000, t + 12000], ..Composer::default() },
        )]);
        let entries = cards.entries(&composers, &Recorded::default(), t + 602_000 + TOOL_IDLE_MS + 1);
        let entry = &entries[0];
        assert_eq!(entry.session.id, "c1");
        assert_eq!(entry.session.cwd, r"H:\work");
        assert_eq!(entry.events.len(), 1);
        assert!(entry.events[0].ts >= t + 600_000);
        assert!(!entry.live.turn_open, "a tool stuck as running is given up on");
        assert_eq!(entry.live.last_turn_end_ms, t + 602_000);

        // A conversation started minutes later that did the same work is linked too;
        // an earlier one running in parallel is not.
        let composers = HashMap::from([
            ("late".to_owned(), Composer { created_at: t + 5 * MINUTE, outputs: vec![t + 1000, t + 9000, t + 12000, t + 601_000], ..Composer::default() }),
            ("parallel".to_owned(), Composer { created_at: t - 2 * MINUTE, outputs: vec![t + 1000, t + 9000, t + 12000, t + 601_000], ..Composer::default() }),
        ]);
        let entries = cards.entries(&composers, &Recorded::default(), t + 603_000);
        assert_eq!(entries[0].session.id, "late");
        assert!(entries[0].events.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn counts_remote_replies_without_rows_and_takes_the_workspace_conversation() {
        let root = temp_root("remote");
        let t = 1_800_000_000_000i64;
        let rows = json!([
            {"id":"r1","kind":"thinking","seq":1,"at":t+1000,"endedAt":t+2000,"durationMs":1000,"text":"read the plan"},
            {"id":"r2","kind":"tool","seq":2,"at":t+2500,"endedAt":t+60_000,"label":"等后台任务","status":"completed","ok":true,"own":true},
            {"id":"r3","kind":"text","seq":3,"at":t+61_000,"endedAt":t+61_100,"text":"done"}
        ]);
        let later = t + 3_600_000;
        let history = json!([
            {"messageId":"u1","role":"user","text":"go on","createdAt":t},
            {"messageId":"a1","role":"assistant","text":"done","createdAt":t+61_100},
            {"messageId":"workspace-enter-x","role":"assistant","text":"已进入当前工作区","createdAt":later},
            {"messageId":"u2","role":"user","text":"go on","createdAt":later+5000},
            {"messageId":"a2","role":"assistant","text":"收到，我先读文档。".repeat(20),"createdAt":later+40_000},
            {"messageId":"a3","role":"assistant","text":"卡住了：执行端离线。","createdAt":later+200_000}
        ]);
        let dir = write_card(&root, "card-3", t, rows, history);
        fs::write(dir.join("meta.json"), json!({"name": "迭代", "model": "", "createdAt": t}).to_string()).unwrap();
        let mut cards = Cards::default();
        cards.refresh(&root);
        let composers = HashMap::from([(
            "relay".to_owned(),
            Composer { created_at: later + 300, cwd: r"H:\game".into(), outputs: vec![] },
        )]);
        let entries = cards.entries(&composers, &Recorded::default(), later + 210_000);
        let entry = &entries[0];
        assert_eq!(entry.session.id, "gateway/card-3", "a conversation opened later does not take the card's id");
        assert_eq!(entry.session.cwd, r"H:\game");
        // thinking | the own step and the answer | two replies with no rows behind them
        assert_eq!(entry.events.len(), 4);
        assert!(entry.events[2].usage.output > entry.events[3].usage.output);
        assert_eq!(entry.tools[0].name, "等后台任务");
        assert!(entry.live.turn_open, "a reply just came back");
        assert_eq!(cards.relays(&composers), HashSet::from(["relay"]));

        // A message just sent shows the gateway being asked.
        let mut history = serde_json::from_slice::<Value>(&fs::read(dir.join("context.json")).unwrap()).unwrap();
        history["history"].as_array_mut().unwrap().push(json!({"messageId":"u3","role":"user","text":"retry","createdAt":later+400_000}));
        fs::write(dir.join("context.json"), history.to_string()).unwrap();
        cards.refresh(&root);
        let entries = cards.entries(&composers, &Recorded::default(), later + 410_000);
        assert_eq!(entries[0].live.phase, Phase::Requesting);
        assert_eq!(entries[0].live.turn_started_ms, later + 400_000);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn gives_executor_jobs_to_the_cards_that_sent_no_rows() {
        let root = temp_root("adopt");
        let t = 1_800_000_000_000i64;
        // One card sends its steps; two others only reply now and then.
        write_card(&root, "card-10", t, json!([
            {"id":"s1","kind":"thinking","seq":1,"at":t+1000,"endedAt":t+2000,"text":"look"},
            {"id":"s2","kind":"tool","seq":2,"at":t+60_000,"endedAt":t+61_000,"tool":"workspace_read_file","status":"completed","ok":true,"target":"maps/coolant.gd"}
        ]), json!([{"messageId":"u","role":"user","text":"go","createdAt":t}]));
        for (name, branch) in [("card-11", "21-G-gpu"), ("card-12", "21-G-port")] {
            let dir = write_card(&root, name, t, json!([]), json!([
                {"messageId":"u","role":"user","text":format!("worktree .claude/worktrees/{branch}, branch codex/{branch}, {branch} again"),"createdAt":t},
                {"messageId":"a","role":"assistant","text":"收到。","createdAt":t+30 * MINUTE}
            ]));
            fs::remove_file(dir.join("process.json")).unwrap();
        }
        let job = |at: i64, label: &'static str, target: &str| Job { ts: t + at, ms: 500, label, target: target.into(), ok: true };
        let jobs = Jobs::of(vec![
            job(60_500, "读取", ".claude/worktrees/21-G-coolant/maps/coolant.gd"),
            job(20 * MINUTE, "读取", ".claude/worktrees/21-G-gpu/maps/gpu.gd"),
            job(21 * MINUTE, "修改", ".claude/worktrees/21-G-port/maps/port.gd"),
            job(22 * MINUTE, "运行", "git status"),
            job(23 * MINUTE, "运行", "git status"),
        ]);
        let now = t + 24 * MINUTE + CLAIM_DELAY_MS;
        let mut cards = Cards::default();
        cards.refresh(&root);
        assert!(cards.adopt(&jobs, now));
        assert!(!cards.adopt(&jobs, now + SECOND), "each job is taken once");
        let held = |name: &str| {
            let card = cards.cards.values().find(|card| card.id == name).unwrap();
            card.rows.values().filter(|row| row.inferred).map(|row| row.tool.as_str()).collect::<Vec<_>>()
        };
        assert!(held("card-10").is_empty(), "its own step was sent as a row");
        assert_eq!(held("card-11").len() + held("card-12").len(), 4);
        assert!(held("card-11").contains(&"读取") && held("card-12").contains(&"修改"));
        assert_eq!((held("card-11").len(), held("card-12").len()), (2, 2), "the rest is split evenly");
        let entries = cards.entries(&HashMap::new(), &Recorded::default(), now);
        let gpu = entries.iter().find(|entry| entry.session.id == "gateway/card-11").unwrap();
        // two jobs and the reply, each a model call with the whole context resent
        assert_eq!(gpu.events.len(), 3);
        assert!(gpu.events.iter().all(|event| event.usage.input + event.usage.cache_read > 20_000));
        assert!(gpu.live.turn_open);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retries_jobs_when_logs_or_their_card_arrive_late() {
        let root = temp_root("late-jobs");
        let t = 1_800_000_000_000i64;
        let now = t + 10 * MINUTE;
        let jobs = Jobs::of(vec![Job { ts: t + MINUTE, ms: 500, label: "读取", target: "maps/gpu.gd".into(), ok: true }]);
        let mut cards = Cards::default();
        // The collector can poll before an executor channel or a card is readable.
        assert!(!cards.adopt(&Jobs::default(), now));
        assert!(!cards.adopt(&jobs, now + SECOND));
        write_card(&root, "card-late", t, json!([]), json!([
            {"messageId":"u","role":"user","text":"work on maps/gpu.gd","createdAt":t}
        ]));
        cards.refresh(&root);
        assert!(cards.adopt(&jobs, now + 2 * SECOND));
        let entries = cards.entries(&HashMap::new(), &Recorded::default(), now + 2 * SECOND);
        assert_eq!(entries[0].events.len(), 1);
        assert!(entries[0].events[0].usage.total() > 20_000);
        assert!(!cards.adopt(&jobs, now + 3 * SECOND), "replaying a job must not count it twice");

        // A channel discovered after that successful pass can contain older jobs.
        let more = Jobs::of(vec![
            jobs.all().next().unwrap().clone(),
            Job { ts: t + 2 * MINUTE, ms: 500, label: "运行", target: "cargo test".into(), ok: true },
        ]);
        assert!(cards.adopt(&more, now + 4 * SECOND));
        assert_eq!(cards.entries(&HashMap::new(), &Recorded::default(), now + 4 * SECOND)[0].events.len(), 2);
        assert!(!cards.adopt(&more, now + 5 * SECOND));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replaces_inferred_jobs_when_process_rows_arrive_late() {
        let root = temp_root("late-rows");
        let t = 1_800_000_000_000i64;
        let now = t + 5 * MINUTE;
        let history = json!([{"messageId":"u","role":"user","text":"fix it","createdAt":t}]);
        write_card(&root, "card-late-rows", t, json!([]), history.clone());
        let jobs = Jobs::of(vec![Job { ts: t + MINUTE, ms: 500, label: "运行", target: "cargo test".into(), ok: true }]);
        let mut cards = Cards::default();
        cards.refresh(&root);
        assert!(cards.adopt(&jobs, now));
        assert_eq!(cards.entries(&HashMap::new(), &Recorded::default(), now)[0].events.len(), 1);

        write_card(&root, "card-late-rows", t, json!([
            {"id":"official","kind":"tool","at":t+MINUTE-5000,"endedAt":t+MINUTE,
             "tool":"workspace_exec","target":"cargo test","status":"completed","ok":true}
        ]), history);
        cards.refresh(&root);
        assert!(cards.adopt(&jobs, now + SECOND));
        let entries = cards.entries(&HashMap::new(), &Recorded::default(), now + SECOND);
        assert_eq!(entries[0].events.len(), 1);
        assert_eq!(entries[0].tools.len(), 1);
        assert!(cards.cards.values().all(|card| card.rows.values().all(|row| !row.inferred)));
        assert!(!cards.adopt(&jobs, now + 2 * SECOND));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn display_name_max_uses_the_full_context_window() {
        let t = 1_800_000_000_000i64;
        let mut card = Card { id: "card-max".into(), created_at: t, ..Card::default() };
        for i in 0..180 {
            card.rows.insert(format!("r{i}"), Row {
                kind: "tool".into(), at: t + i * 5000, end: t + i * 5000 + 100,
                tool: "workspace_read_file".into(), ok: true, ..Row::default()
            });
        }
        card.rebuild();
        let mut cards = Cards { cards: HashMap::from([(PathBuf::from("card-max"), card)]) };
        let usage = |cards: &Cards| cards.entries(&HashMap::new(), &Recorded::default(), t + 20 * MINUTE)[0]
            .events.iter().map(|event| event.usage).collect::<Vec<_>>();
        cards.cards.values_mut().next().unwrap().model = "claude-fable-5-1 · max".into();
        let lowercase = usage(&cards);
        cards.cards.values_mut().next().unwrap().model = "claude-fable-5-1 · Max".into();
        assert_eq!(usage(&cards), lowercase, "the gateway stores Max as a display name, with a capital M");
        cards.cards.values_mut().next().unwrap().model = "claude-fable-5-1".into();
        let standard = usage(&cards);
        assert!(lowercase.iter().map(Usage::total).sum::<u64>() > standard.iter().map(Usage::total).sum::<u64>());
    }

    #[test]
    fn reads_a_card_that_has_no_process_file_yet() {
        let root = temp_root("noprocess");
        let t = 1_800_000_000_000i64;
        let dir = write_card(&root, "card-4", t, json!([]), json!([
            {"messageId":"u1","role":"user","text":"你负责地图任务","createdAt":t+500},
            {"messageId":"workspace-enter-y","role":"assistant","text":"已进入当前工作区","createdAt":t+900},
            {"messageId":"a1","role":"assistant","text":"收到，开始做。","createdAt":t+30_000}
        ]));
        fs::remove_file(dir.join("process.json")).unwrap();
        let mut cards = Cards::default();
        assert!(cards.refresh(&root));
        let entries = cards.entries(&HashMap::new(), &Recorded::default(), t + 200_000);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].events.len(), 1);
        assert!(entries[0].live.turn_open, "replies every few minutes keep a remote run open");
        assert!(!cards.entries(&HashMap::new(), &Recorded::default(), t + 30_000 + REMOTE_IDLE_MS + 1)[0].live.turn_open);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn keeps_rows_the_extension_dropped_while_the_app_runs() {
        let root = temp_root("fold");
        let t = 1_800_000_000_000i64;
        let row = |id: u64, at: i64| json!({"id": format!("r{id}"), "kind":"text","seq":id,"at":at,"endedAt":at+100,"text":"some output"});
        let first: Vec<Value> = (0..4).map(|i| row(i, t + i as i64 * 60_000)).collect();
        write_card(&root, "card-2", t, Value::Array(first), json!([]));
        let mut cards = Cards::default();
        cards.refresh(&root);
        let count = |cards: &Cards| {
            cards.entries(&HashMap::new(), &Recorded::default(), t + crate::timeutil::DAY).iter().map(|entry| entry.events.len()).sum::<usize>()
        };
        assert_eq!(count(&cards), 1, "text rows without tools in between form one call");

        // The extension folds the oldest rows away and a tool splits the newer ones off.
        let mut later: Vec<Value> = vec![json!({"id":"t1","kind":"tool","seq":10,"at":t+300_000,"endedAt":t+301_000,"tool":"workspace_exec","status":"completed","ok":true})];
        later.extend((20..22).map(|i| row(i, t + 400_000 + i as i64 * 1000)));
        write_card(&root, "card-2", t, Value::Array(later), json!([]));
        cards.refresh(&root);
        assert_eq!(count(&cards), 2);
        assert_eq!(cards.cards.values().next().unwrap().rows.len(), 7);
        fs::remove_dir_all(root).unwrap();
    }

    /// What the usage history holds of these cards. With `prior` it is as the next
    /// start of the app finds it; without, as a later pass of the same run does.
    fn recorded_from(entries: &[FileEntry], prior: bool) -> Recorded {
        let mut recorded = Recorded::default();
        for entry in entries {
            let card = entry.path.parent().and_then(Path::file_name).unwrap().to_string_lossy().into_owned();
            let prior_until = if prior { entry.events.iter().map(|event| event.ts).max().unwrap_or(0) } else { 0 };
            recorded.cards.insert(card, RecordedCard { models: entry.models.clone(), events: entry.events.clone(), prior_until });
        }
        recorded
    }

    fn usage_of(entry: &FileEntry) -> Vec<(i64, Usage)> {
        entry.events.iter().map(|event| (event.ts, event.usage)).collect()
    }

    #[test]
    fn takes_over_what_earlier_runs_recorded_after_a_restart() {
        let root = temp_root("restart");
        let t = 1_800_000_000_000i64;
        let day = t + crate::timeutil::DAY;
        // A call every minute, each with a result to read back.
        let tool = |id: i64| {
            json!({"id": format!("r{id}"), "kind":"tool","seq":id,"at":t+id*60_000,"endedAt":t+id*60_000+500,
                "tool":"workspace_exec","server":"workspace","status":"completed","ok":true,"target":"cargo test","output":"ok".repeat(2000)})
        };
        let history = json!([{"messageId":"u1","role":"user","text":"fix it","createdAt":t}]);
        write_card(&root, "card-7", t, Value::Array((1..=6).map(tool).collect()), history.clone());
        let mut first = Cards::default();
        first.refresh(&root);
        let before = first.entries(&HashMap::new(), &Recorded::default(), day);
        assert_eq!(before[0].events.len(), 6);

        // The app is closed. The extension drops the four oldest rows and the assistant makes two more calls.
        write_card(&root, "card-7", t, Value::Array((5..=8).map(tool).collect()), history);
        let recorded = recorded_from(&before, true);
        let mut second = Cards::default();
        second.refresh(&root);
        let after = second.entries(&HashMap::new(), &recorded, day);
        assert_eq!(after[0].events.len(), 8, "six recorded calls and two new ones, none of them twice");
        assert_eq!(usage_of(&after[0])[..6], usage_of(&before[0])[..], "what was recorded stays as it was");
        // The new calls go on from the recorded context, not from an empty one.
        let prompt = |event: &UsageEvent| event.usage.input + event.usage.cache_read + event.usage.cache_write;
        assert!(prompt(&after[0].events[6]) > prompt(&after[0].events[5]));
        assert_eq!(after[0].tools.len(), 4, "tool names come from the rows still there");
        // Without the history only the rows still there would count.
        assert_eq!(second.entries(&HashMap::new(), &Recorded::default(), day)[0].events.len(), 4);

        // Later passes of the same run find their own requests in the history: worked out again, not added.
        let mut again = recorded_from(&after, false);
        again.cards.get_mut("card-7").unwrap().prior_until = recorded.cards["card-7"].prior_until;
        assert_eq!(usage_of(&second.entries(&HashMap::new(), &again, day)[0]), usage_of(&after[0]));
        fs::remove_dir_all(root).unwrap();
    }

    fn service_row(tool: &str, at: i64, result: u64) -> Row {
        Row { kind: "tool".into(), at, end: at + 500, args: 100, result, tool: tool.into(), server: "baishitong-workspace".into(), ok: true, ..Row::default() }
    }

    #[test]
    fn conversations_of_the_service_become_cards() {
        let t = 1_800_000_000_000i64;
        let global = PathBuf::from("globalStorage");
        let session = baishitong::Session {
            id: "card-9".into(),
            name: "检查项目".into(),
            model: "claude-opus-5-5".into(),
            created_at: t,
            cwd: r"F:\work".into(),
            busy: true,
            ..baishitong::Session::default()
        };
        let listed = || Batch { list: Some(vec![session.clone()]), pages: Vec::new(), alive: true, soon: false };
        let mut cards = Cards::default();
        assert!(cards.sync_service(&global, listed()));
        assert!(!cards.sync_service(&global, listed()), "the same list changes nothing");
        let page = baishitong::Page {
            rows: vec![
                ("r1".into(), service_row("baishitong_read_file", t + 10_000, 3000)),
                ("r2".into(), service_row("baishitong_exec", t + 20_000, 2000)),
                ("r3".into(), Row { kind: "thinking".into(), at: t + 25_000, end: t + 29_000, duration_ms: 4000, text: 2000, ..Row::default() }),
                ("r4".into(), service_row("baishitong_reply", t + 30_000, 500)),
            ],
            prompts: vec![("u1".into(), Prompt { ts: t + 1000, weight: 400 })],
            replies: vec![("a1".into(), Prompt { ts: t + 30_000, weight: 1500 })],
            thinking_since: Some(t + 31_000),
            ..baishitong::Page::default()
        };
        let read = |page: &baishitong::Page| Batch { list: None, pages: vec![("card-9".into(), page.clone())], alive: true, soon: false };
        assert!(cards.sync_service(&global, read(&page)));
        assert!(!cards.sync_service(&global, read(&page)), "the same rows change nothing");
        // Cards on disk come and go without touching it.
        assert!(!cards.refresh(&temp_root("service-none")));

        // A Cursor conversation created with it is not its host: the assistant runs in the cloud.
        let composers = HashMap::from([(
            "c1".to_owned(),
            Composer { created_at: t + 100, cwd: r"H:\other".into(), outputs: vec![t + 10_000, t + 20_000], ..Composer::default() },
        )]);
        let entries = cards.entries(&composers, &Recorded::default(), t + 40_000);
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert!(entry.plugin && entry.host.is_empty());
        assert_eq!(entry.session.id, "gateway/card-9");
        assert_eq!(entry.path, global.join(baishitong::DIR).join("sessions").join("card-9").join("process.json"));
        assert_eq!(entry.session.cwd, r"F:\work");
        // the read | the command | thinking and the reply
        assert_eq!(entry.events.len(), 3);
        assert!(entry.events[0].usage.reasoning > 0, "a step that came without its thinking is given some");
        assert_eq!(entry.events[2].think_ms, 4000);
        assert_eq!(entry.tools.len(), 3);
        assert!(entry.live.turn_open);
        assert_eq!(entry.live.phase, Phase::Thinking);
        assert_eq!(entry.live.turn_started_ms, t + 1000);

        // The plugin stops answering: nothing is known to be running any more.
        assert!(cards.sync_service(&global, Batch::default()));
        assert!(!cards.entries(&composers, &Recorded::default(), t + 40_000)[0].live.turn_open);
        // The service no longer lists the conversation.
        assert!(cards.sync_service(&global, Batch { list: Some(Vec::new()), pages: Vec::new(), alive: true, soon: false }));
        assert!(cards.entries(&composers, &Recorded::default(), t + 40_000).is_empty());
    }

    #[test]
    fn a_conversation_at_work_shows_before_its_rows_are_read() {
        let t = 1_800_000_000_000i64;
        let global = PathBuf::from("globalStorage");
        let session = |busy: bool, active_at: i64| baishitong::Session {
            id: "card-3".into(),
            model: "claude-opus-5-5".into(),
            created_at: t - crate::timeutil::DAY,
            busy,
            active_at,
            ..baishitong::Session::default()
        };
        let listed = |session: baishitong::Session| Batch { list: Some(vec![session]), alive: true, ..Batch::default() };
        let live_of = |cards: &Cards| cards.entries(&HashMap::new(), &Recorded::default(), t)[0].live.clone();
        let mut cards = Cards::default();
        // The app has just started: the list says an assistant is at work, the rows are still to be read.
        assert!(cards.sync_service(&global, listed(session(true, t - 40_000))));
        let live = live_of(&cards);
        assert!(live.turn_open, "the list is enough to tell that it runs");
        assert_eq!((live.phase, live.phase_since_ms, live.turn_started_ms), (Phase::Generating, t - 40_000, t - 40_000));
        // The assistant takes another step: nothing for the usage history, but the live state follows.
        assert!(!cards.sync_service(&global, listed(session(true, t - 5000))));
        assert_eq!(live_of(&cards).phase_since_ms, t - 5000);
        // A flag left on by a conversation that has long been quiet is not believed.
        assert!(!cards.sync_service(&global, listed(session(true, t - TOOL_IDLE_MS - 1))));
        assert!(!live_of(&cards).turn_open);
        // Once the rows are read, they tell when the turn began and what it is doing.
        cards.sync_service(&global, listed(session(true, t - 5000)));
        let page = baishitong::Page {
            rows: vec![("r1".into(), service_row("baishitong_exec", t - 20_000, 2000))],
            prompts: vec![("u1".into(), Prompt { ts: t - 60_000, weight: 400 })],
            ..baishitong::Page::default()
        };
        assert!(cards.sync_service(&global, Batch { pages: vec![("card-3".into(), page)], alive: true, ..Batch::default() }));
        let live = live_of(&cards);
        assert_eq!((live.turn_open, live.turn_started_ms, live.phase_since_ms), (true, t - 60_000, t - 19_500));
        // Nobody at work: the time of the last message alone starts nothing.
        assert!(cards.sync_service(&global, listed(session(false, t - 1000))));
        assert!(!live_of(&cards).turn_open);
    }

    #[test]
    fn a_recorded_conversation_counts_without_its_rows() {
        let t = 1_800_000_000_000i64;
        let day = t + crate::timeutil::DAY;
        let global = PathBuf::from("globalStorage");
        let session = baishitong::Session { id: "card-8".into(), model: "claude-opus-5-5".into(), created_at: t, ..baishitong::Session::default() };
        let mut cards = Cards::default();
        cards.sync_service(&global, Batch { list: Some(vec![session]), pages: Vec::new(), alive: true, soon: false });
        let event = |ts: i64| UsageEvent {
            ts,
            usage: Usage { input: 1000, cache_read: 50_000, output: 300, ..Usage::default() },
            estimated: true,
            ..UsageEvent::default()
        };
        let mut recorded = Recorded::default();
        let known = RecordedCard { models: vec!["claude-opus-5-5".into()], events: vec![event(t + 1000), event(t + 2000)], prior_until: t + 2000 };
        recorded.cards.insert("card-8".into(), known.clone());
        let kept: Vec<(i64, Usage)> = known.events.iter().map(|event| (event.ts, event.usage)).collect();
        // An old conversation the history covers is not read again: its record stands.
        let entries = cards.entries(&HashMap::new(), &recorded, day);
        assert_eq!(usage_of(&entries[0]), kept);
        assert_eq!(entries[0].models, ["claude-opus-5-5"]);
        assert!(entries[0].events.iter().all(|event| event.estimated));

        // It is read after all, and one more step was made since: only that step is added.
        let rows = vec![
            ("r1".to_owned(), service_row("baishitong_exec", t + 900, 2000)),
            ("r2".to_owned(), service_row("baishitong_exec", t + 1900, 2000)),
            ("r3".to_owned(), service_row("baishitong_exec", t + 60_000, 2000)),
        ];
        cards.sync_service(
            &global,
            Batch { list: None, pages: vec![("card-8".into(), baishitong::Page { rows, ..baishitong::Page::default() })], alive: true, soon: false },
        );
        let entries = cards.entries(&HashMap::new(), &recorded, day);
        assert_eq!(entries[0].events.len(), 3);
        assert_eq!(usage_of(&entries[0])[..2], kept[..]);
        assert_eq!(entries[0].events[2].ts, t + 60_000);
        assert!(entries[0].events[2].usage.cache_read >= 51_000, "the context goes on from the recorded size");
        assert_eq!(entries[0].tools.len(), 3);
    }

    #[test]
    fn a_conversation_named_like_a_card_on_disk_is_not_counted_twice() {
        let root = temp_root("same-name");
        let t = 1_800_000_000_000i64;
        write_card(&root, "card-5", t, json!([{"id":"r1","kind":"text","seq":1,"at":t+1000,"endedAt":t+1100,"text":"done"}]), json!([]));
        let mut cards = Cards::default();
        cards.refresh(&root);
        let session = baishitong::Session { id: "card-5".into(), created_at: t, ..baishitong::Session::default() };
        let rows = vec![("s1".to_owned(), service_row("baishitong_exec", t + 5000, 2000))];
        let page = baishitong::Page { rows, ..baishitong::Page::default() };
        cards.sync_service(&root, Batch { list: Some(vec![session]), pages: vec![("card-5".into(), page)], alive: true, soon: false });
        let entries = cards.entries(&HashMap::new(), &Recorded::default(), t + crate::timeutil::DAY);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].path.starts_with(root.join("conversations")));
        assert_eq!(entries[0].events.len(), 1);
        fs::remove_dir_all(root).unwrap();
    }
}
