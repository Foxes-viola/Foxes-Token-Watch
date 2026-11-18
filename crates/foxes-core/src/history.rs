//! 用量历史：只保存每次请求的时刻和 token 数（应用数据目录下的 `history.json`）。
//!
//! 各家的原始记录迟早会不在：Claude Code 隔一阵子自己清理，Cursor、Kiro 的用量是按对话内容估算出来的、
//! 记录清掉之后再也算不出来，换一台电脑更是什么都读不到。所以把每段会话算好的结果存一份，历史用量只增不减：
//!
//! - 会话还在时以实时扫描为准，并覆盖之前存的那份；
//! - 会话消失后，由存下来的那份顶上（标记为“来源已删除”）；
//! - 同一段会话只有一个标识，实时数据和历史不会重复计数；
//! - 每次请求留着来源给的去重键（[`UsageEvent::key`]），同一次请求在几段会话里都出现时仍然只算一次。
//!
//! 历史按电脑分开记：每台电脑只改自己的那一份，别的电脑的那几份原样留着，合计时加在一起。
//! 两份历史合并时，同一台电脑的记录取版次高的那份，合并本身不产生新的记录：同一份历史并多少遍、在几台
//! 电脑之间来回带，都不会重复。同一段会话在几台电脑的记录里都有时，只取最新的一份。
//!
//! 便携运行时历史在程序旁边再放一份（[`History::carry`]），跟着程序换电脑：到了另一台电脑上先把它并进来，
//! 那台电脑自己的用量再记上去；两份文件每次都写成一样的内容，哪一份丢了都能从另一份找回。
//!
//! 插件卡片（网关扩展、百事通）只留着最近的一批过程记录，这个软件也不另存它们。所以这个文件就是卡片用量
//! 唯一的一份：启动时把里面的卡片交给扫描（[`History::prime`]），以前记下的请求照原样算，只有之后的请求按
//! 现存的过程记录来算，既不会变少也不会重复。
//!
//! 不保存标题、目录、工具调用和任何对话内容。
//!
//! 旧版把这些记在 `agent-usage.sqlite` 里（只写不读，还带着工具记录）。第一次换到这个文件时把里面的会话接过来，
//! 写盘成功后删掉旧库。第 1 版的 `history.json` 不分电脑、只记估算的来源，换成现在的格式之前原样留一份
//! （`history.v1.json`）：旧版本的软件读不懂新格式，遇到了会把文件改名成 `history.unreadable.json` 放到一边、
//! 自己另起一份。下次用这个版本启动时，放到一边的那份会被接回来。
//!
//! 反过来，比这个版本更新的软件写的历史文件，这个版本不读也不写：本机的那份是这样的话，这次运行什么都不写。

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs;
use std::hash::{BuildHasher, Hasher};
use std::io;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde::de::{self, SeqAccess, Visitor};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::model::{Agent, Effort, Usage, UsageEvent};
use crate::sources::{codex, FileEntry, FileSig, ParserState, RecordedCard};
use crate::store::Store;
use crate::timeutil::{fnv64, local_offset_ms, now_ms};

const VERSION: u32 = 2;
/// 有变化时最多隔这么久写一次盘（本机的那份）。整个文件每次都重写，写得太勤一天要写出去几十 GB；
/// 没写盘的这几分钟在各 agent 自己的记录里都还在，退出时也会马上写（[`History::flush`]）
const SAVE_EVERY_MS: i64 = 3 * 60_000;
/// 跟着程序走的那份隔这么久写一次：它只在换电脑时用得上，退出时总会写
const CARRY_EVERY_MS: i64 = 10 * 60_000;
/// 每天留一份的备份放在历史文件旁边的这个目录里，留这么多天的
const BACKUP_DIR: &str = "history-backup";
const BACKUP_DAYS: usize = 7;
/// 被这个文件取代的旧版数据库
const LEGACY: &str = "agent-usage.sqlite";
/// 第 1 版的历史文件换成现在的格式之前，原样留在这里
const BACKUP_V1: &str = "history.v1.json";

const ESTIMATED: u8 = 1;
const REASONING_ESTIMATED: u8 = 2;

/// 两个历史文件在各个数组里的下标：本机数据目录里的那份，和跟着程序走的那份
const LOCAL: usize = 0;
const CARRIED: usize = 1;

/// 一次请求。存成一行：
/// `[相对会话开始的毫秒数, 输入, 缓存命中, 缓存写入, 输出, 思考, 模型下标, 标记, 思考强度, 思考毫秒, credits, "去重键"]`，
/// 第六个数之后末尾的 0 省略；去重键是十六进制的字符串（写成数字会被当成浮点数丢掉精度），没有就不写。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Row {
    dt: i64,
    usage: Usage,
    model: u16,
    flags: u8,
    effort: u8,
    think_ms: u32,
    credits: f32,
    /// 来源给的去重键（[`UsageEvent::key`]），0 表示没有
    key: u64,
}

impl Serialize for Row {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let usage = &self.usage;
        let head = [usage.input, usage.cache_read, usage.cache_write, usage.output, usage.reasoning];
        let tail = [u64::from(self.model), u64::from(self.flags), u64::from(self.effort), u64::from(self.think_ms)];
        let credits = round_credits(self.credits);
        let kept = if credits != 0.0 { tail.len() } else { tail.iter().rposition(|value| *value != 0).map_or(0, |index| index + 1) };
        let mut seq = serializer.serialize_seq(Some(1 + head.len() + kept + usize::from(credits != 0.0) + usize::from(self.key != 0)))?;
        seq.serialize_element(&self.dt)?;
        for value in head.iter().chain(&tail[..kept]) {
            seq.serialize_element(value)?;
        }
        if credits != 0.0 {
            seq.serialize_element(&credits)?;
        }
        if self.key != 0 {
            seq.serialize_element(&format_args!("{:x}", self.key))?;
        }
        seq.end()
    }
}

/// 一行里的一格：数字，或者写成字符串的去重键。
enum Cell {
    Number(f64),
    Key(u64),
}

impl<'de> Deserialize<'de> for Cell {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct CellVisitor;
        impl<'de> Visitor<'de> for CellVisitor {
            type Value = Cell;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a number or a hexadecimal string")
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Cell, E> {
                Ok(Cell::Number(value as f64))
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Cell, E> {
                Ok(Cell::Number(value as f64))
            }

            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Cell, E> {
                Ok(Cell::Number(value))
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Cell, E> {
                Ok(Cell::Key(u64::from_str_radix(value, 16).unwrap_or(0)))
            }
        }
        deserializer.deserialize_any(CellVisitor)
    }
}

impl<'de> Deserialize<'de> for Row {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct RowVisitor;
        impl<'de> Visitor<'de> for RowVisitor {
            type Value = Row;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a row of numbers")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Row, A::Error> {
                let mut values = [0.0f64; 11];
                let mut filled = 0;
                let mut key = 0;
                while let Some(cell) = seq.next_element::<Cell>()? {
                    match cell {
                        Cell::Number(value) if filled < values.len() => {
                            values[filled] = value;
                            filled += 1;
                        }
                        Cell::Number(_) => {}
                        Cell::Key(value) => key = value,
                    }
                }
                let count = |index: usize| values[index].max(0.0) as u64;
                Ok(Row {
                    dt: values[0] as i64,
                    usage: Usage { input: count(1), cache_read: count(2), cache_write: count(3), output: count(4), reasoning: count(5) },
                    model: count(6).min(u64::from(u16::MAX)) as u16,
                    flags: count(7).min(u64::from(u8::MAX)) as u8,
                    effort: count(8).min(u64::from(u8::MAX)) as u8,
                    think_ms: count(9).min(u64::from(u32::MAX)) as u32,
                    credits: values[10] as f32,
                    key,
                })
            }
        }
        deserializer.deserialize_seq(RowVisitor)
    }
}

/// credits 只留四位小数。
fn round_credits(credits: f32) -> f64 {
    (f64::from(credits) * 1e4).round() / 1e4
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// 一段会话的用量记录。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    agent: Agent,
    id: String,
    /// 来自 Cursor 插件（网关）的卡片
    #[serde(default, skip_serializing_if = "is_false")]
    plugin: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    sub: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    parent: String,
    /// 在这段 Cursor 对话里运行的插件卡片（它的记录标识）：两边记的是同一批请求
    #[serde(default, skip_serializing_if = "String::is_empty")]
    card: String,
    started: i64,
    updated: i64,
    models: Vec<String>,
    events: Vec<Row>,
}

impl Record {
    fn of(part: &FileEntry, card: String) -> Record {
        let started = part.session.started_ms;
        let events = part
            .events
            .iter()
            .map(|event| Row {
                dt: event.ts - started,
                usage: event.usage,
                model: event.model,
                flags: (u8::from(event.estimated) * ESTIMATED) | (u8::from(event.reasoning_estimated) * REASONING_ESTIMATED),
                effort: event.effort.index() as u8,
                think_ms: event.think_ms,
                credits: round_credits(event.credits) as f32,
                key: event.key,
            })
            .collect();
        Record {
            agent: part.agent,
            id: session_id(part),
            plugin: part.plugin,
            sub: part.session.sub_agent,
            parent: part.session.parent_id.clone(),
            card,
            started,
            updated: part.session.last_active_ms,
            models: part.models.clone(),
            events,
        }
    }

    /// 来源不在这台电脑上（已经消失，或者本来就是别的电脑记下的）：用存下来的数字还原成一个只读的会话，
    /// 路径记成历史文件本身。`elsewhere` 表示这份记录是别的电脑记下的。
    fn restore(&self, source: &Path, elsewhere: bool) -> FileEntry {
        let mut entry = FileEntry::new(self.agent, source.to_path_buf());
        entry.gone = true;
        entry.elsewhere = elsewhere;
        entry.plugin = self.plugin;
        entry.session.id.clone_from(&self.id);
        entry.session.sub_agent = self.sub;
        entry.session.parent_id.clone_from(&self.parent);
        entry.session.started_ms = self.started;
        entry.session.last_active_ms = self.updated;
        entry.models.clone_from(&self.models);
        entry.events = self.usage_events();
        entry
    }

    /// 存下来的请求，换回扫描用的样子。
    fn usage_events(&self) -> Vec<UsageEvent> {
        self.events
            .iter()
            .map(|row| UsageEvent {
                ts: self.started + row.dt,
                usage: row.usage,
                model: row.model,
                key: row.key,
                think_ms: row.think_ms,
                reasoning_estimated: row.flags & REASONING_ESTIMATED != 0,
                estimated: row.flags & ESTIMATED != 0,
                credits: row.credits,
                effort: Effort::from_index(row.effort as usize),
                ..UsageEvent::default()
            })
            .collect()
    }

    /// 一张插件卡片交给扫描的那一份：记下的请求，和记到了什么时刻。
    fn recorded(&self) -> RecordedCard {
        let events = self.usage_events();
        let prior_until = events.iter().map(|event| event.ts).max().unwrap_or(0);
        RecordedCard { models: self.models.clone(), events, prior_until }
    }

    /// 同一段会话的两份记录哪份更新：先看最后活动的时刻，再看记下的请求数。
    fn freshness(&self) -> (i64, usize) {
        (self.updated, self.events.len())
    }
}

/// 一台电脑记下的会话。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Section {
    /// 这台电脑每改一次自己的记录加一。两份历史合并时，同一台电脑的记录取版次高的那份
    #[serde(default)]
    rev: u64,
    /// 记录标识 → 会话
    #[serde(default)]
    sessions: BTreeMap<String, Record>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Saved {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    saved_at: i64,
    /// 写这个文件的电脑在 `devices` 里的编号
    #[serde(default)]
    device: String,
    /// 写这个文件的电脑的指纹（见 [`host_fingerprint`]）
    #[serde(default)]
    host: String,
    /// 第 1 版不分电脑，会话直接放在这里
    #[serde(default)]
    sessions: BTreeMap<String, Record>,
    /// 电脑的编号 → 它记下的会话
    #[serde(default)]
    devices: BTreeMap<String, Section>,
}

#[derive(Serialize)]
struct SectionRef<'a> {
    rev: u64,
    sessions: &'a BTreeMap<String, Record>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SavedRef<'a> {
    version: u32,
    saved_at: i64,
    device: &'a str,
    host: &'a str,
    devices: BTreeMap<&'a str, SectionRef<'a>>,
}

/// 读历史文件时的两种失败。
enum Unread {
    /// 这会儿读不了（没有权限，或者硬盘不在）
    Io(io::Error),
    /// 里面不是历史记录
    Garbled,
}

/// 读一个历史文件；文件不存在时是 `None`。
fn read_saved(path: &Path) -> Result<Option<Saved>, Unread> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|_| Unread::Garbled),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Unread::Io(error)),
    }
}

/// 给这台电脑的那份记录起一个随机编号。
fn new_device_id() -> String {
    // RandomState 的种子取自系统的随机数，再掺上时刻和进程号
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_i64(now_ms());
    hasher.write_u32(std::process::id());
    format!("{:016x}", hasher.finish())
}

/// 这台电脑的指纹：电脑名和用户名的散列，不存原文。本机的历史文件被整个拷到别的电脑上时靠它认出来。
fn host_fingerprint() -> String {
    let read = |name: &str| std::env::var(name).unwrap_or_default();
    let (computer, user) = if cfg!(windows) { (read("COMPUTERNAME"), read("USERNAME")) } else { (read("HOSTNAME"), read("USER")) };
    format!("{:016x}", fnv64(&[b"host", computer.as_bytes(), user.as_bytes()]))
}

/// 第 1 版的历史文件（不分电脑）被并进来时用的编号：按内容算，同一个文件并几遍都是同一份。
fn legacy_device(saved_at: i64, sessions: &BTreeMap<String, Record>) -> String {
    let mut hash = fnv64(&[b"v1", &saved_at.to_le_bytes()]);
    for (key, record) in sessions {
        hash = fnv64(&[&hash.to_le_bytes(), key.as_bytes(), &record.updated.to_le_bytes(), &(record.events.len() as u64).to_le_bytes()]);
    }
    format!("v1-{hash:016x}")
}

fn session_id(part: &FileEntry) -> String {
    if part.session.id.is_empty() {
        part.path.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default()
    } else {
        part.session.id.clone()
    }
}

/// 一段会话在历史文件里的标识。数据目录搬了家、换了电脑、记录被归档或者进了回收站，标识都不变；
/// 里面不带工作目录的名字。
fn identity(part: &FileEntry) -> String {
    let names: Vec<String> = part.path.iter().map(|name| name.to_string_lossy().into_owned()).collect();
    // 路径的最后几段
    let tail = |count: usize| names[names.len().saturating_sub(count)..].join("/");
    // 倒数第二段：文件所在的目录
    let folder = || names.iter().rev().nth(1).cloned().unwrap_or_default();
    if part.plugin {
        // 路径是 <卡片目录>/process.json
        return format!("card:{}", folder());
    }
    let name = match part.agent {
        Agent::Cursor => return format!("chat:{}", part.session.id),
        // <数据目录>/projects/<工作目录的名字>/<会话>.jsonl，子代理在 <会话>/subagents/ 下：去掉工作目录那一段
        Agent::Claude => match names.iter().rposition(|name| name == "projects").map(|at| at + 2).filter(|at| *at < names.len()) {
            Some(at) => names[at..].join("/"),
            None => tail(1),
        },
        // 压缩、归档都不改 rollout 的名字
        Agent::Codex => codex::rollout_name(&part.path).map_or_else(|| tail(1), str::to_owned),
        // <数据目录>/sessions/<工作目录的名字>/session-<uuid>/session.v4.jsonl.zstd
        Agent::Deepseek => Some(folder()).filter(|folder| !folder.is_empty()).unwrap_or_else(|| tail(1)),
        // <数据目录>/conversations/<uuid>.db
        Agent::Antigravity => part.path.file_stem().map_or_else(|| tail(1), |stem| stem.to_string_lossy().into_owned()),
        Agent::Kiro | Agent::WebMcp => tail(3),
    };
    format!("{}:{name}", part.agent.id())
}

/// 一个来源文件里的各段会话（Cursor 的数据库本身不算会话）。
fn parts(file: &FileEntry) -> impl Iterator<Item = &FileEntry> {
    Some(file).filter(|file| file.agent != Agent::Cursor).into_iter().chain(file.extra.iter())
}

/// 旧版数据库里的一次请求（`usage_json` 数组的一项）。
#[derive(Deserialize)]
struct LegacyEvent {
    ts: i64,
    #[serde(default)]
    model: String,
    #[serde(default)]
    tokens: Usage,
    #[serde(default)]
    estimated: bool,
    #[serde(default)]
    credits: f32,
}

/// 读出旧版数据库里的会话，换成现在的记录（标识 → 记录）。
///
/// 旧版按「来源文件 + 会话 id」各存一行，会话 id 变了也不清旧行：同一个标识留下过几行时取最新的那行。
fn read_legacy(path: &Path) -> Result<BTreeMap<String, Record>, Box<dyn std::error::Error>> {
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut statement = db.prepare("SELECT source, session, agent, updated_ms, usage_json FROM sessions")?;
    let mut rows = statement.query([])?;
    let mut records: BTreeMap<String, Record> = BTreeMap::new();
    // 卡片所在的 Cursor 对话 id → 卡片的记录标识：旧版里卡片和它所在的对话共用会话 id
    let mut cards: HashMap<String, String> = HashMap::new();
    while let Some(row) = rows.next()? {
        let (source, session, agent, updated, usage): (String, String, String, i64, String) = (row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?);
        let Some(agent) = Agent::from_id(&agent).filter(|agent| agent.estimates_tokens()) else {
            continue;
        };
        let events: Vec<LegacyEvent> = serde_json::from_str(&usage)?;
        let mut part = FileEntry::new(agent, PathBuf::from(source));
        part.plugin = agent == Agent::Cursor && part.path.file_name().is_some_and(|name| name == "process.json");
        part.session.id = session;
        part.session.started_ms = events.iter().map(|event| event.ts).min().unwrap_or(updated);
        part.session.last_active_ms = events.iter().map(|event| event.ts).fold(updated, i64::max);
        for event in &events {
            let model = part.models.iter().position(|name| *name == event.model).unwrap_or_else(|| {
                part.models.push(event.model.clone());
                part.models.len() - 1
            });
            part.events.push(UsageEvent {
                ts: event.ts,
                usage: event.tokens,
                model: model.min(usize::from(u16::MAX)) as u16,
                estimated: event.estimated,
                credits: event.credits,
                effort: Effort::from_model_name(&event.model),
                ..UsageEvent::default()
            });
        }
        let key = identity(&part);
        if part.plugin {
            cards.insert(part.session.id.clone(), key.clone());
        }
        // 卡片自己没有用量也留着：它说明所在的对话是插件对话
        if part.events.is_empty() && !part.plugin {
            continue;
        }
        let record = Record::of(&part, String::new());
        let newer = match records.get(&key) {
            Some(known) => (record.updated, record.events.len()) > (known.updated, known.events.len()),
            None => true,
        };
        if newer {
            records.insert(key, record);
        }
    }
    for record in records.values_mut().filter(|record| record.agent == Agent::Cursor && !record.plugin) {
        if let Some(card) = cards.get(&record.id) {
            record.card.clone_from(card);
        }
    }
    Ok(records)
}

/// 给界面看的概况。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryInfo {
    /// 本机记下的会话数
    pub sessions: usize,
    /// 历史里还有几台别的电脑的记录（记过会话的才算）
    pub devices: usize,
    /// 现在算进总数的会话里，有多少段是别的电脑记下的
    pub elsewhere: usize,
    /// 跟着程序走的那份历史放在哪里；没有就是空
    pub carried: String,
}

pub struct History {
    /// 本机的历史文件；没有可写的数据目录时是 `None`
    path: Option<PathBuf>,
    /// 跟着程序走的那一份（便携运行时放在程序旁边）；不用时是 `None`
    carried: Option<PathBuf>,
    loaded: bool,
    /// 旧版数据库里的会话已经接过来（或者本来就没有旧库），写盘之后可以删掉它
    legacy_done: bool,
    /// 这台电脑的指纹（见 [`host_fingerprint`]）
    host: String,
    /// 本机的那份记录在历史文件里的编号，和它的版次
    device: String,
    rev: u64,
    /// 本机记下的会话（记录标识 → 会话）
    sessions: BTreeMap<String, Record>,
    /// 别的电脑记下的会话（电脑的编号 → 它的那一份）。只读：原样留着，原样写回去
    others: BTreeMap<String, Section>,
    /// 各来源文件上次比对时的戳，没变就不用再比
    stamps: HashMap<PathBuf, FileSig>,
    /// 当前由历史顶上的会话：（电脑的编号，本机记下的为空；记录标识）
    restored: Vec<(String, String)>,
    /// 上次比对时扫描结果的版本
    synced: Option<u64>,
    /// 读进来时各张插件卡片记到的时刻（记录标识 → 最后一次请求的时刻）。这之前的请求卡片不再重算
    prior_until: HashMap<String, i64>,
    /// 交给扫描的那份卡片记录的版本（[`crate::sources::Recorded::rev`]）：照别的版本算出来的扫描结果先不收
    recorded_rev: u64,
    /// 从文件里并进来的记录换过了：由历史顶上的会话要重新算一遍
    refill: bool,
    /// 本机的会话有改动，还没写盘（写的时候版次加一）
    dirty: bool,
    /// 两个文件（[`LOCAL`]、[`CARRIED`]）各自要不要重写：内容落后于内存里的这份时为真
    pending: [bool; 2],
    /// 两个文件上次读写之后的大小和修改时间；对不上说明别处改过，写之前先并进来
    seen: [Option<FileSig>; 2],
    /// 文件是更新版本的软件写的：不读也不写
    newer: [bool; 2],
    /// 本机的文件还是第 1 版的格式：重写之前先原样留一份
    upgrading: bool,
    /// 被旧版本的软件放到一边的那份历史已经接回来：本机的文件写好之后删掉它
    rescued: bool,
    written_at_ms: i64,
    /// 上次写（或者试着写）跟着程序走的那份的时间
    carried_at_ms: i64,
    error: Option<String>,
}

impl History {
    /// `path` 是历史文件的位置（应用数据目录下的 `history.json`），[`History::prime`] 时才读它。
    pub fn new(path: Option<PathBuf>) -> History {
        History {
            path,
            carried: None,
            loaded: false,
            legacy_done: false,
            host: host_fingerprint(),
            device: new_device_id(),
            rev: 0,
            sessions: BTreeMap::new(),
            others: BTreeMap::new(),
            stamps: HashMap::new(),
            restored: Vec::new(),
            synced: None,
            prior_until: HashMap::new(),
            recorded_rev: 0,
            refill: false,
            dirty: false,
            pending: [false; 2],
            seen: [None; 2],
            newer: [false; 2],
            upgrading: false,
            rescued: false,
            written_at_ms: 0,
            carried_at_ms: 0,
            error: None,
        }
    }

    /// 便携运行时在程序旁边再放一份历史（`path`），跟着程序换电脑。要在 [`History::prime`] 之前调用。
    pub fn carry(mut self, path: Option<PathBuf>) -> History {
        self.carried = path;
        self
    }

    /// 读入历史文件，并把里面的插件卡片交给 `store`：卡片的过程记录不另存，以前记下的请求就靠这一份接着算。
    ///
    /// 在第一次扫描之前调用；没调的话第一次 [`History::sync`] 会补上，只是那之前扫出来的卡片要等下一轮才收。
    /// 返回读没读成；没读成时原因见 [`History::error`]，下次再试。
    pub fn prime(&mut self, store: &mut Store) -> bool {
        if self.loaded {
            return true;
        }
        if let Err(error) = self.load() {
            self.error = Some(error);
            return false;
        }
        (self.loaded, self.error) = (true, None);
        self.legacy_done = self.import_legacy();
        let cards = self.cards();
        self.prior_until = cards.iter().map(|(card, known)| (format!("card:{card}"), known.prior_until)).collect();
        store.set_recorded(cards);
        self.recorded_rev = store.recorded().rev;
        true
    }

    /// 历史里记下的插件卡片（卡片目录名 → 记下的请求）。本机没记过的卡片，用别的电脑的记录里最新的那份。
    fn cards(&self) -> HashMap<String, RecordedCard> {
        let mut best: HashMap<&str, &Record> = HashMap::new();
        let elsewhere = self.others.values().flat_map(|section| &section.sessions);
        for (key, record) in elsewhere.filter(|(_, record)| record.plugin) {
            let Some(card) = key.strip_prefix("card:") else { continue };
            if best.get(card).is_none_or(|known| record.freshness() > known.freshness()) {
                best.insert(card, record);
            }
        }
        for (key, record) in self.sessions.iter().filter(|(_, record)| record.plugin) {
            if let Some(card) = key.strip_prefix("card:") {
                best.insert(card, record);
            }
        }
        best.into_iter().map(|(card, record)| (card.to_owned(), record.recorded())).collect()
    }

    /// 读入本机的历史文件，再把跟着程序走的那份并进来。文件不存在时从空白开始；读不懂的文件改名留存，不覆盖；
    /// 更新版本的软件写的文件原样留着，不读也不写。
    fn load(&mut self) -> Result<(), String> {
        let path = self.path.clone().ok_or("找不到应用数据目录")?;
        (self.sessions, self.others, self.rev) = (BTreeMap::new(), BTreeMap::new(), 0);
        (self.dirty, self.pending, self.newer, self.upgrading, self.refill, self.rescued) = (false, [false; 2], [false; 2], false, false, false);
        // 只认第 1 版的旧软件遇到现在这种格式的文件，会把它改名放到一边、自己另起一份：放到一边的那份要接回来
        let aside = path.with_extension("unreadable.json");
        let set_aside = read_saved(&aside).ok().flatten().filter(|saved| (2..=VERSION).contains(&saved.version));
        let mut kept = true;
        let mut settled = false;
        match read_saved(&path) {
            // 更新版本的软件写的：这个版本不认识里面的内容，不读也不写，免得写坏
            Ok(Some(saved)) if saved.version > VERSION => self.newer[LOCAL] = true,
            Ok(Some(saved)) => settled = self.adopt(saved),
            Err(Unread::Garbled) => {
                // 改名会顶掉原来放在一边的那份（它的内容上面已经读进来了），之后留在那里的是这个读不懂的文件
                let _ = fs::rename(&path, &aside);
                kept = false;
            }
            Ok(None) => {}
            Err(Unread::Io(error)) => return Err(error.to_string()),
        }
        self.seen[LOCAL] = FileSig::of(&path);
        if let Some(saved) = set_aside.filter(|_| !self.newer[LOCAL]) {
            self.take_back(saved, settled);
            self.rescued = kept;
        }
        if let Some(carried) = self.carried.clone() {
            self.take_in(CARRIED, &carried);
        }
        Ok(())
    }

    /// 接手本机的历史文件。返回里面有没有这台电脑自己的编号（第 1 版的没有；从别的电脑拷来的不算）。
    fn adopt(&mut self, saved: Saved) -> bool {
        if saved.version < 2 {
            // 第 1 版不分电脑：里面的会话都算本机的。写成新格式之前先把原来的文件留一份
            self.sessions = saved.sessions;
            (self.upgrading, self.dirty) = (true, true);
            return false;
        }
        let mut devices = saved.devices;
        let own = !saved.device.is_empty() && saved.host == self.host;
        if own {
            if let Some(section) = devices.remove(&saved.device) {
                (self.sessions, self.rev) = (section.sessions, section.rev);
            }
            self.device = saved.device;
        } else {
            // 指纹对不上：这个文件是从别的电脑整个拷过来的。里面的每一份都算别的电脑的，本机另起一份
            self.pending[LOCAL] = true;
        }
        self.refill = !devices.is_empty();
        self.others = devices;
        own
    }

    /// 接回被旧版本的软件放到一边的那份历史（见 [`History::load`]）。`settled` 是本机现在的文件里有没有这台电脑自己的编号。
    fn take_back(&mut self, saved: Saved, settled: bool) {
        if !settled && self.others.is_empty() && !saved.device.is_empty() && saved.host == self.host {
            // 那一份就是这台电脑自己的：编号和记录接着用，旧软件后来另记的那些（第 1 版）盖在上面
            let later = std::mem::take(&mut self.sessions);
            self.adopt(saved);
            for (key, record) in later {
                if self.sessions.get(&key).is_none_or(|known| record.freshness() > known.freshness()) {
                    self.sessions.insert(key, record);
                    self.dirty = true;
                }
            }
        } else {
            // 其余的情形当成另一份历史并进来
            self.merge(saved);
        }
        // 接回来的内容要写进本机的文件
        self.pending = [true; 2];
    }

    /// 把一个历史文件并进来（见 [`History::merge`]），记下它之后要不要重写。返回这个文件现在能不能写。
    fn take_in(&mut self, which: usize, path: &Path) -> bool {
        self.newer[which] = false;
        match read_saved(path) {
            // 更新版本的软件写的：这个版本不认识里面的内容，不读也不写，免得写坏
            Ok(Some(saved)) if saved.version > VERSION => self.newer[which] = true,
            Ok(Some(mut saved)) => {
                if which == LOCAL {
                    // 本机的文件里要是第 1 版的内容，是旧版本的软件写的：那些会话本机都扫得到，不当成另一台电脑的记录
                    saved.sessions.clear();
                }
                let (changed, behind) = self.merge(saved);
                if changed {
                    // 内存里的内容变了：别的文件都落后了
                    self.pending = [true; 2];
                }
                self.pending[which] = behind;
            }
            Ok(None) => self.pending[which] = true,
            Err(Unread::Garbled) => {
                let _ = fs::rename(path, path.with_extension("unreadable.json"));
                self.pending[which] = true;
            }
            // 这会儿读不了：先不写它，下次写盘前再试
            Err(Unread::Io(_)) => {
                self.seen[which] = None;
                return false;
            }
        }
        self.seen[which] = FileSig::of(path);
        !self.newer[which]
    }

    /// 把另一份历史并进来。别的电脑的记录取版次高的那份；本机的记录只在对方那份版次更高时才换成它
    /// （本机的文件丢过或者上次没写成，另一份里留着更新的）。合并不产生新的记录，同一份并几遍结果都一样。
    ///
    /// 返回（内存里的内容变没变，对方那份是不是落后于内存里的）。
    fn merge(&mut self, saved: Saved) -> (bool, bool) {
        let mut theirs = saved.devices;
        if saved.version < 2 && !saved.sessions.is_empty() {
            // 第 1 版的文件不分电脑：当成一台不知名的电脑记下的
            theirs.insert(legacy_device(saved.saved_at, &saved.sessions), Section { rev: 1, sessions: saved.sessions });
        }
        let ids: HashSet<String> = theirs.keys().cloned().collect();
        let mut changed = false;
        let mut behind = saved.version < VERSION;
        for (id, section) in theirs {
            if id == self.device {
                match section.rev.cmp(&self.rev) {
                    Ordering::Greater => {
                        (self.sessions, self.rev) = (section.sessions, section.rev);
                        // 本机的记录换过了：所有来源重新比对一遍
                        self.stamps.clear();
                        self.synced = None;
                        (self.refill, changed) = (true, true);
                    }
                    Ordering::Less => behind = true,
                    Ordering::Equal => {}
                }
                continue;
            }
            match self.others.get(&id).map(|known| known.rev.cmp(&section.rev)) {
                Some(Ordering::Greater) => behind = true,
                Some(Ordering::Equal) => {}
                Some(Ordering::Less) | None => {
                    self.others.insert(id, section);
                    (self.refill, changed) = (true, true);
                }
            }
        }
        // 内存里有、对方没有的
        behind |= self.others.keys().any(|id| !ids.contains(id));
        behind |= !ids.contains(&self.device) && (self.rev > 0 || !self.sessions.is_empty());
        (changed, behind)
    }

    /// 把旧版数据库里的会话接过来：来源还在的随后会被实时扫描覆盖，来源已经没了的（比如旧插件的记录被清掉）靠这一份留住。
    /// 历史文件里已经有的会话不动。
    ///
    /// 返回旧库是不是可以删了；读不出来就留着它，下次启动再试。
    fn import_legacy(&mut self) -> bool {
        let Some(path) = self.path.as_ref().map(|path| path.with_file_name(LEGACY)) else {
            return false;
        };
        if !path.exists() {
            return true;
        }
        let Ok(records) = read_legacy(&path) else {
            return false;
        };
        for (key, record) in records {
            self.sessions.entry(key).or_insert(record);
        }
        // 写一次盘：写成功之后旧库才会被删掉
        self.dirty = true;
        true
    }

    /// 存了多少段会话。
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// 把扫描到的会话记进历史，让已经消失的会话从历史里顶上，并按需写盘。每轮刷新后调用一次。
    ///
    /// 返回 `store` 里由历史顶上的会话有没有变（变了就要重新汇总）。
    pub fn sync(&mut self, store: &mut Store, now: i64) -> bool {
        if !self.prime(store) {
            return false;
        }
        let mut changed = false;
        if self.synced != Some(store.version()) || self.refill {
            self.absorb(store.source_files());
            let missing = self.missing(store.source_files());
            if missing != self.restored || self.refill {
                let source = self.path.clone().unwrap_or_default();
                let entries = missing.iter().filter_map(|(device, key)| Some(self.record(device, key)?.restore(&source, !device.is_empty()))).collect();
                store.set_restored(entries);
                self.restored = missing;
                changed = true;
            }
            self.refill = false;
            self.synced = Some(store.version());
        }
        if self.unsaved() && (self.written_at_ms == 0 || now < self.written_at_ms || now - self.written_at_ms >= SAVE_EVERY_MS) {
            self.written_at_ms = now.max(1);
            // 跟着程序走的那份不用写得这么勤；上一回没写成的话两份都再试
            let carry = self.error.is_some() || self.carried_at_ms == 0 || now < self.carried_at_ms || now - self.carried_at_ms >= CARRY_EVERY_MS;
            self.error = self.write_to(now, carry).err();
        }
        changed
    }

    /// 有没写盘的改动就马上写，不等到点（退出之前调用）。
    pub fn flush(&mut self, now: i64) {
        if self.loaded && self.unsaved() {
            self.written_at_ms = now.max(1);
            self.error = self.write(now).err();
        }
    }

    /// 有没有还没写进文件的内容。
    fn unsaved(&self) -> bool {
        // 本机的文件是更新版本的软件写的：这台电脑的编号和记录都在里面、读不出来，这次运行什么都不写
        if self.newer[LOCAL] {
            return false;
        }
        self.dirty || self.pending[LOCAL] || (self.pending[CARRIED] && !self.newer[CARRIED] && self.carried.is_some())
    }

    /// 历史文件读不出来或写不进去时的原因；之后会重试，成功后清除。
    pub fn error(&self) -> Option<&str> {
        match &self.error {
            Some(error) => Some(error),
            None if self.newer.contains(&true) => Some("有一份历史文件是更新版本的软件写的，这个版本不读写它，请换用新版本"),
            None => None,
        }
    }

    /// 给界面看的概况。
    pub fn info(&self) -> HistoryInfo {
        HistoryInfo {
            sessions: self.sessions.len(),
            // 一段会话都没记的电脑（比如只是在上面打开看了看）不算
            devices: self.others.values().filter(|section| !section.sessions.is_empty()).count(),
            elsewhere: self.restored.iter().filter(|(device, _)| !device.is_empty()).count(),
            carried: self.carried.as_ref().map(|path| path.display().to_string()).unwrap_or_default(),
        }
    }

    /// 一份记录；`device` 为空时是本机记下的。
    fn record(&self, device: &str, key: &str) -> Option<&Record> {
        if device.is_empty() {
            self.sessions.get(key)
        } else {
            self.others.get(device)?.sessions.get(key)
        }
    }

    /// 用扫描到的会话覆盖历史里的同一段会话。读不出来或已经消失的文件不动它之前的记录。
    fn absorb<'a>(&mut self, files: impl Iterator<Item = &'a FileEntry>) {
        for file in files {
            let stamp = match &file.parser {
                // 卡片是照另一份记录算出来的（历史刚读进来，还没重新扫过）：收了会把以前记下的请求盖掉，等下一轮
                ParserState::Cursor(state) if state.recorded_rev != self.recorded_rev => continue,
                ParserState::Cursor(state) => FileSig { mtime_ms: state.revision, ..FileSig::default() },
                _ => file.sig,
            };
            if file.gone || file.errors != 0 || file.corrupt || self.stamps.get(&file.path) == Some(&stamp) {
                continue;
            }
            // 卡片所在的 Cursor 对话 id → 卡片的记录标识
            let cards: HashMap<&str, String> =
                parts(file).filter(|part| part.plugin && !part.host.is_empty()).map(|part| (part.host.as_str(), identity(part))).collect();
            let mut present = HashSet::new();
            for part in parts(file) {
                let key = identity(part);
                // 卡片的请求全都记在它所在的对话里时，它自己没有用量，但说明那段对话是插件对话：也留着
                if part.events.is_empty() && !part.plugin {
                    self.dirty |= self.sessions.remove(&key).is_some();
                } else {
                    let card = if part.plugin { String::new() } else { cards.get(part.session.id.as_str()).cloned().unwrap_or_default() };
                    let record = Record::of(part, card);
                    if self.sessions.get(&key) != Some(&record) {
                        self.sessions.insert(key.clone(), record);
                        self.dirty = true;
                    }
                }
                present.insert(key);
            }
            // 对话被删掉后，它的卡片会自己报出那些请求；对话那份记录再留着，以后就会和卡片重复。
            // 不过卡片只重算以前的运行没记过的那一段：更早的那一段卡片里没有这些请求，对话这边要留着
            let orphaned: Vec<String> = self
                .sessions
                .iter()
                .filter(|(key, record)| !record.card.is_empty() && !present.contains(*key) && present.contains(&record.card))
                .map(|(key, _)| key.clone())
                .collect();
            for key in orphaned {
                let Some(mut record) = self.sessions.remove(&key) else { continue };
                let until = self.prior_until.get(&record.card).copied().unwrap_or(0);
                let started = record.started;
                record.events.retain(|row| started + row.dt <= until);
                record.card.clear();
                if !record.events.is_empty() {
                    self.sessions.insert(key, record);
                }
                self.dirty = true;
            }
            self.stamps.insert(file.path.clone(), stamp);
        }
    }

    /// 历史里有、这台电脑上扫不到的会话：（电脑的编号，本机记下的为空；记录标识），按标识排序。
    /// 同一段会话在几台电脑的记录里都有时只取最新的一份，一样新就用本机的。
    fn missing<'a>(&self, files: impl Iterator<Item = &'a FileEntry>) -> Vec<(String, String)> {
        let live: HashSet<String> = files.flat_map(parts).map(identity).collect();
        // 对话被删掉、它的卡片还在时，卡片会自己报出全部请求，再还原对话就重复了
        let absent = |key: &String, record: &Record| !live.contains(key) && (record.card.is_empty() || !live.contains(&record.card));
        let mut best: BTreeMap<&String, (&str, &Record)> = BTreeMap::new();
        for (key, record) in self.sessions.iter().filter(|&(key, record)| absent(key, record)) {
            best.insert(key, ("", record));
        }
        for (device, section) in &self.others {
            for (key, record) in section.sessions.iter().filter(|&(key, record)| absent(key, record)) {
                if best.get(key).is_none_or(|(_, known)| record.freshness() > known.freshness()) {
                    best.insert(key, (device.as_str(), record));
                }
            }
        }
        best.into_iter().map(|(key, (device, _))| (device.to_owned(), key.clone())).collect()
    }

    /// 内存里的全部内容，照文件的样子。
    fn saved(&self, now: i64) -> SavedRef<'_> {
        let mut devices: BTreeMap<&str, SectionRef> =
            self.others.iter().map(|(id, section)| (id.as_str(), SectionRef { rev: section.rev, sessions: &section.sessions })).collect();
        devices.insert(&self.device, SectionRef { rev: self.rev, sessions: &self.sessions });
        SavedRef { version: VERSION, saved_at: now, device: &self.device, host: &self.host, devices }
    }

    /// 把内存里的内容写进历史文件（本机的，和跟着程序走的）。别处改过的文件先并进来再写，免得把那边新记的盖掉。
    fn write(&mut self, now: i64) -> Result<(), String> {
        self.write_to(now, true)
    }

    /// 同 [`History::write`]；`carry` 为否时这一回不写跟着程序走的那份，留到下一回。
    fn write_to(&mut self, now: i64, carry: bool) -> Result<(), String> {
        let Some(path) = self.path.clone() else {
            (self.dirty, self.pending) = (false, [false; 2]);
            return Ok(());
        };
        let files = [Some(path), self.carried.clone()];
        let mut writable = [false; 2];
        for (which, file) in files.iter().enumerate() {
            if let Some(file) = file {
                writable[which] = if FileSig::of(file) == self.seen[which] { !self.newer[which] } else { self.take_in(which, file) };
            }
        }
        if self.dirty {
            self.rev += 1;
            (self.dirty, self.pending) = (false, [true; 2]);
        }
        if carry {
            self.carried_at_ms = now.max(1);
        }
        let due: Vec<usize> =
            (0..files.len()).filter(|&which| files[which].is_some() && self.pending[which] && !self.newer[which] && (carry || which == LOCAL)).collect();
        if due.is_empty() {
            return Ok(());
        }
        let bytes = serde_json::to_vec(&self.saved(now)).map_err(|error| error.to_string())?;
        let mut failure = None;
        for which in due {
            let Some(file) = &files[which] else { continue };
            let written = if writable[which] {
                if which == LOCAL {
                    keep_daily(file, now);
                }
                self.replace(which, file, &bytes)
            } else {
                Err(io::Error::other("文件这会儿读不出来"))
            };
            if let Err(error) = written {
                let name = if which == CARRIED { "程序旁边的那份历史" } else { "本机的历史文件" };
                failure.get_or_insert(format!("{name}：{error}"));
            }
        }
        failure.map_or(Ok(()), Err)
    }

    /// 换掉一个历史文件：先写临时文件再改名，中途退出也不会留下半截文件。
    fn replace(&mut self, which: usize, path: &Path, bytes: &[u8]) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        if which == LOCAL && self.upgrading {
            // 旧版本的软件读不懂新格式：第 1 版的文件原样留一份
            let backup = path.with_file_name(BACKUP_V1);
            if !backup.exists() {
                match fs::copy(path, &backup) {
                    Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
                    _ => {}
                }
            }
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, bytes)?;
        fs::rename(&tmp, path)?;
        self.seen[which] = FileSig::of(path);
        self.pending[which] = false;
        if which == LOCAL {
            self.upgrading = false;
            if self.rescued {
                // 被旧版本的软件放到一边的那份已经接回来、现在也写进了这个文件
                let _ = fs::remove_file(path.with_extension("unreadable.json"));
                self.rescued = false;
            }
            // 旧版的数据库只写不读，里面的会话启动时已经接过来、现在也写进了这个文件
            if self.legacy_done {
                for suffix in ["", "-wal", "-shm", "-journal"] {
                    let _ = fs::remove_file(path.with_file_name(format!("{LEGACY}{suffix}")));
                }
            }
        }
        Ok(())
    }
}

/// 每天头一回改写本机那份历史之前，把现有的文件原样留一份（旁边的 `history-backup` 目录里，留最近 7 天的）：
/// 万一哪次合并或升级出了岔子，还能拿回前一天的。留不成不算错，下一回再试。
fn keep_daily(path: &Path, now: i64) {
    let Some(dir) = path.parent().map(|dir| dir.join(BACKUP_DIR)) else { return };
    let target = dir.join(format!("history.{}.json", day_label(now)));
    if target.exists() || !path.exists() || fs::create_dir_all(&dir).is_err() {
        return;
    }
    // 先用临时名字，抄完再改名，免得留下半截的备份
    let tmp = target.with_extension("json.tmp");
    if fs::copy(path, &tmp).is_err() || fs::rename(&tmp, &target).is_err() {
        let _ = fs::remove_file(&tmp);
        return;
    }
    let mut kept: Vec<PathBuf> = fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|file| file.file_name().and_then(|name| name.to_str()).is_some_and(is_daily_backup))
        .collect();
    kept.sort();
    let extra = kept.len().saturating_sub(BACKUP_DAYS);
    for old in kept.into_iter().take(extra) {
        let _ = fs::remove_file(old);
    }
}

fn is_daily_backup(name: &str) -> bool {
    name.strip_prefix("history.").and_then(|rest| rest.strip_suffix(".json")).is_some_and(|day| day.len() == 8 && day.bytes().all(|byte| byte.is_ascii_digit()))
}

/// 本地日期，写成 20261118 的样子。
fn day_label(now: i64) -> String {
    let (year, month, day) = civil((now + local_offset_ms()).div_euclid(86_400_000));
    format!("{year:04}{month:02}{day:02}")
}

/// 1970-01-01 起的第几天 → 公历的年、月、日。
fn civil(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let doe = shifted.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daily_backup_names_and_dates() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(19_782), (2024, 2, 29));
        assert_eq!(civil(20_775), (2026, 11, 18));
        assert!(is_daily_backup("history.20261118.json"));
        assert!(!is_daily_backup("history.20261118.json.tmp"));
        assert!(!is_daily_backup("history.v1.json"));
    }

    fn event(ts: i64, input: u64, output: u64) -> UsageEvent {
        UsageEvent { ts, usage: Usage { input, cache_read: input * 10, cache_write: 0, output, reasoning: output / 2 }, estimated: true, ..UsageEvent::default() }
    }

    fn part(agent: Agent, path: &str, id: &str, events: Vec<UsageEvent>) -> FileEntry {
        let mut entry = FileEntry::new(agent, PathBuf::from(path));
        entry.session.id = id.to_owned();
        entry.session.started_ms = events.iter().map(|event| event.ts).min().unwrap_or(0);
        entry.session.last_active_ms = events.iter().map(|event| event.ts).max().unwrap_or(0);
        entry.models.push("model-high".to_owned());
        entry.events = events;
        entry
    }

    fn empty() -> History {
        History::new(None)
    }

    fn bump(file: &mut FileEntry) {
        if let ParserState::Cursor(state) = &mut file.parser {
            state.revision += 1;
        }
    }

    /// 由历史顶上的会话都是本机记下的：只留标识。
    fn own(missing: Vec<(String, String)>) -> Vec<String> {
        missing
            .into_iter()
            .map(|(device, key)| {
                assert_eq!(device, "", "{key} was recorded by this computer");
                key
            })
            .collect()
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("foxes-core-history-{name}-{}-{}", std::process::id(), now_ms()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 名叫 `name` 的电脑：数据目录在 `dir` 下，`carried` 是插在它上面的那份跟着程序走的历史。
    fn computer(dir: &Path, name: &str, carried: Option<&Path>) -> History {
        let mut history = History::new(Some(dir.join(name).join("history.json"))).carry(carried.map(Path::to_path_buf));
        history.host = format!("host-{name}");
        history
    }

    /// 这台电脑上算进总数的请求数：扫描到的，加上由历史顶上的。
    fn requests(history: &History, live: &[&FileEntry]) -> usize {
        let scanned: usize = live.iter().copied().flat_map(parts).map(|part| part.events.len()).sum();
        let restored: usize = history.missing(live.iter().copied()).iter().map(|(device, key)| history.record(device, key).unwrap().events.len()).sum();
        scanned + restored
    }

    fn read(path: &Path) -> Saved {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn rows_keep_only_what_is_set() {
        let row = Row { dt: 1500, usage: Usage { input: 10, cache_read: 200, cache_write: 0, output: 30, reasoning: 5 }, ..Row::default() };
        assert_eq!(serde_json::to_string(&row).unwrap(), "[1500,10,200,0,30,5]");
        assert_eq!(serde_json::from_str::<Row>("[1500,10,200,0,30,5]").unwrap(), row);

        let flagged = Row { flags: ESTIMATED | REASONING_ESTIMATED, ..row };
        assert_eq!(serde_json::to_string(&flagged).unwrap(), "[1500,10,200,0,30,5,0,3]");

        let full = Row { model: 2, flags: ESTIMATED, effort: Effort::High.index() as u8, think_ms: 40, credits: 0.05, ..row };
        let text = serde_json::to_string(&full).unwrap();
        assert_eq!(text, "[1500,10,200,0,30,5,2,1,5,40,0.05]");
        assert_eq!(serde_json::from_str::<Row>(&text).unwrap(), full);

        // 去重键写成十六进制的字符串，放在最后；没有键的行和第 1 版写的一样
        let keyed = Row { key: 0xfedc_ba98_7654_3210, ..row };
        let text = serde_json::to_string(&keyed).unwrap();
        assert_eq!(text, "[1500,10,200,0,30,5,\"fedcba9876543210\"]");
        assert_eq!(serde_json::from_str::<Row>(&text).unwrap(), keyed);
        let keyed = Row { key: 0x1f, ..full };
        let text = serde_json::to_string(&keyed).unwrap();
        assert_eq!(text, "[1500,10,200,0,30,5,2,1,5,40,0.05,\"1f\"]");
        assert_eq!(serde_json::from_str::<Row>(&text).unwrap(), keyed);
    }

    #[test]
    fn keeps_one_copy_per_session_and_restores_what_disappeared() {
        let t = 1_767_225_600_000;
        let kiro = part(Agent::Kiro, "C:/u/.kiro/sessions/ws/sess_1/messages.jsonl", "sess_1", vec![event(t, 100, 10), event(t + 5_000, 200, 20)]);
        // 不靠估算的来源也进历史：换一台电脑就读不到它们的记录了
        let codex = part(Agent::Codex, "C:/u/.codex/sessions/a.jsonl", "a", vec![event(t, 1, 1)]);

        let mut history = empty();
        history.absorb([&kiro, &codex].into_iter());
        assert_eq!(history.len(), 2);
        assert!(history.dirty);
        // 会话还在：不还原，实时数据和历史不会叠加
        assert!(history.missing([&kiro, &codex].into_iter()).is_empty());

        // 会话文件没了（比如重启后扫不到）：由历史顶上
        let missing = own(history.missing([&codex].into_iter()));
        assert_eq!(missing, ["kiro:ws/sess_1/messages.jsonl"]);
        let restored = history.sessions[&missing[0]].restore(Path::new("history.json"), false);
        assert!(restored.gone && !restored.elsewhere);
        assert_eq!(restored.path, Path::new("history.json"));
        assert_eq!(restored.session.id, "sess_1");
        assert_eq!(restored.models, ["model-high"]);
        assert_eq!(restored.events.iter().map(|event| event.ts).collect::<Vec<_>>(), [t, t + 5_000]);
        assert_eq!(restored.events.iter().map(|event| event.usage.total()).sum::<u64>(), 1100 + 10 + 2200 + 20);
        assert!(restored.events.iter().all(|event| event.estimated));

        // 存盘再读回来是同一份，记在这台电脑名下
        let text = serde_json::to_string(&history.saved(t)).unwrap();
        assert!(!text.contains("title") && !text.contains("cwd"));
        let saved: Saved = serde_json::from_str(&text).unwrap();
        assert_eq!((saved.version, &saved.device, &saved.host), (VERSION, &history.device, &history.host));
        assert_eq!(saved.devices.keys().collect::<Vec<_>>(), [&history.device]);
        assert_eq!(saved.devices[&history.device].sessions, history.sessions);

        // 同一个文件没变化时不重新比对；变成空的（比如被改写）就以实时扫描为准
        let mut rewritten = part(Agent::Kiro, "C:/u/.kiro/sessions/ws/sess_1/messages.jsonl", "sess_1", Vec::new());
        rewritten.sig.size = 1;
        history.absorb([&rewritten].into_iter());
        assert_eq!(history.sessions.keys().collect::<Vec<_>>(), ["codex:a.jsonl"]);
    }

    #[test]
    fn plugin_cards_keep_their_identity_and_do_not_double_count() {
        let t = 1_767_225_600_000;
        let mut database = FileEntry::new(Agent::Cursor, PathBuf::from("C:/u/Cursor/User/globalStorage/state.vscdb"));
        let chat = part(Agent::Cursor, "C:/u/Cursor/User/globalStorage/state.vscdb", "c-1", vec![event(t, 100, 10)]);
        let native = part(Agent::Cursor, "C:/u/Cursor/User/globalStorage/state.vscdb", "c-2", vec![event(t + 1, 50, 5)]);
        let mut card = part(Agent::Cursor, "C:/u/Cursor/x/conversations/local/sessions/card-9/process.json", "c-1", vec![event(t + 2, 30, 3)]);
        card.plugin = true;
        card.host = "c-1".to_owned();
        database.extra = vec![chat, native, card];

        let mut history = empty();
        history.absorb([&database].into_iter());
        assert_eq!(history.sessions.keys().collect::<Vec<_>>(), ["card:card-9", "chat:c-1", "chat:c-2"]);
        assert_eq!(history.sessions["chat:c-1"].card, "card:card-9");
        assert!(history.sessions["card:card-9"].plugin);
        assert!(history.missing([&database].into_iter()).is_empty());

        // 卡片被移进回收站，它所在的 Cursor 对话被删掉：卡片还是同一条记录并报出全部请求，对话那份不再保留
        let mut moved = part(Agent::Cursor, "C:/u/Cursor/x/conversations/local/trash/card-9/process.json", "gateway/card-9", vec![event(t, 100, 10), event(t + 2, 30, 3)]);
        moved.plugin = true;
        let native = database.extra.remove(1);
        database.extra = vec![native, moved];
        bump(&mut database);
        history.absorb([&database].into_iter());
        assert_eq!(history.sessions.keys().collect::<Vec<_>>(), ["card:card-9", "chat:c-2"]);
        assert_eq!(history.sessions["card:card-9"].events.len(), 2);
        assert!(history.missing([&database].into_iter()).is_empty());

        // 插件的存储目录也没了：只有卡片那一份由历史顶上
        database.extra.truncate(1);
        assert_eq!(own(history.missing([&database].into_iter())), ["card:card-9"]);
        let restored = history.sessions["card:card-9"].restore(Path::new("history.json"), false);
        assert!(restored.plugin && restored.gone);
        assert_eq!(restored.session.id, "gateway/card-9");
    }

    #[test]
    fn restores_a_card_next_to_its_conversation() {
        let t = 1_767_225_600_000;
        let mut database = FileEntry::new(Agent::Cursor, PathBuf::from("C:/u/Cursor/User/globalStorage/state.vscdb"));
        let chat = part(Agent::Cursor, "C:/u/Cursor/User/globalStorage/state.vscdb", "c-1", vec![event(t, 100, 10)]);
        let mut card = part(Agent::Cursor, "C:/u/Cursor/x/conversations/local/sessions/card-9/process.json", "c-1", vec![event(t + 2, 30, 3)]);
        card.plugin = true;
        card.host = "c-1".to_owned();
        database.extra = vec![chat, card];
        let mut history = empty();
        history.absorb([&database].into_iter());

        // 插件的存储目录被清掉、对话还在：卡片记的是对话没记到的请求，由历史顶上
        database.extra.truncate(1);
        bump(&mut database);
        history.absorb([&database].into_iter());
        assert_eq!(own(history.missing([&database].into_iter())), ["card:card-9"]);

        // 对话后来也被删掉：两份各管各的请求，都顶上
        database.extra.clear();
        bump(&mut database);
        history.absorb([&database].into_iter());
        assert_eq!(own(history.missing([&database].into_iter())), ["card:card-9", "chat:c-1"]);
        let total: u64 = history.sessions.values().flat_map(|record| &record.events).map(|row| row.usage.total()).sum();
        assert_eq!(total, (100 + 1000 + 10) + (30 + 300 + 3));
    }

    #[test]
    fn keeps_cards_that_only_mark_a_plugin_conversation() {
        let t = 1_767_225_600_000;
        let mut database = FileEntry::new(Agent::Cursor, PathBuf::from("C:/u/Cursor/User/globalStorage/state.vscdb"));
        let chat = part(Agent::Cursor, "C:/u/Cursor/User/globalStorage/state.vscdb", "c-1", vec![event(t, 100, 10)]);
        // 卡片的请求全都已经记在对话里：它自己没有用量
        let mut card = part(Agent::Cursor, "C:/u/Cursor/x/conversations/local/sessions/card-9/process.json", "c-1", Vec::new());
        card.plugin = true;
        card.host = "c-1".to_owned();
        database.extra = vec![chat, card];
        let mut history = empty();
        history.absorb([&database].into_iter());
        assert_eq!(history.sessions.keys().collect::<Vec<_>>(), ["card:card-9", "chat:c-1"]);
        assert!(history.missing([&database].into_iter()).is_empty());

        // 插件的存储目录被清掉：卡片由历史顶上，和对话还是同一个会话 id，对话仍然算插件对话
        database.extra.truncate(1);
        bump(&mut database);
        history.absorb([&database].into_iter());
        assert_eq!(own(history.missing([&database].into_iter())), ["card:card-9"]);
        let restored = history.sessions["card:card-9"].restore(Path::new("history.json"), false);
        assert!(restored.plugin && restored.gone && restored.events.is_empty());
        assert_eq!(restored.session.id, "c-1");

        // 非卡片的会话没有用量时不留
        let blank = part(Agent::Cursor, "C:/u/Cursor/User/globalStorage/state.vscdb", "c-3", Vec::new());
        database.extra.push(blank);
        bump(&mut database);
        history.absorb([&database].into_iter());
        assert_eq!(history.sessions.keys().collect::<Vec<_>>(), ["card:card-9", "chat:c-1"]);
    }

    #[test]
    fn takes_over_what_the_old_database_kept() {
        let t = 1_767_225_600_000;
        let dir = std::env::temp_dir().join(format!("foxes-core-history-legacy-{}-{}", std::process::id(), crate::timeutil::now_ms()));
        fs::create_dir_all(&dir).unwrap();
        let legacy = dir.join(LEGACY);
        let saved = dir.join("history.json");
        {
            let db = Connection::open(&legacy).unwrap();
            db.execute_batch(
                "CREATE TABLE sessions (source TEXT NOT NULL, session TEXT NOT NULL, agent TEXT NOT NULL, updated_ms INTEGER NOT NULL,
                 total_tokens INTEGER NOT NULL, estimated_tokens INTEGER NOT NULL, credits REAL NOT NULL,
                 usage_json TEXT NOT NULL, tools_json TEXT NOT NULL, PRIMARY KEY(source, session, agent))",
            )
            .unwrap();
            let usage = |ts: i64, input: u64, output: u64| {
                let tokens = Usage { input, output, ..Usage::default() };
                serde_json::json!([{ "ts": ts, "model": "claude-opus-5-5-max", "tokens": tokens, "total": input + output, "estimated": true, "credits": 0.5 }]).to_string()
            };
            let insert = |source: &str, session: &str, agent: &str, updated: i64, usage: String| {
                db.execute("INSERT INTO sessions VALUES (?1,?2,?3,?4,0,0,0.0,?5,'[{\"name\":\"shell\"}]')", rusqlite::params![source, session, agent, updated, usage]).unwrap();
            };
            // 来源已经不在了的 Kiro 会话
            insert("C:/u/.kiro/sessions/ws/sess_1/messages.jsonl", "sess_1", "kiro", t + 10, usage(t, 100, 10));
            // 一张卡片和它所在的对话共用会话 id；这张卡片还留着一行更早的
            insert("C:/u/Cursor/x/conversations/local/sessions/card-9/process.json", "c-1", "cursor", t + 20, usage(t + 2, 30, 3));
            insert("C:/u/Cursor/x/conversations/local/sessions/card-9/process.json", "gateway/card-9", "cursor", t + 5, usage(t + 2, 7, 0));
            insert("C:/u/Cursor/User/globalStorage/state.vscdb", "c-1", "cursor", t + 20, usage(t, 200, 20));
            // 请求全都记在对话里的卡片：自己没有用量，但说明 c-2 是插件对话
            insert("C:/u/Cursor/x/conversations/local/sessions/card-7/process.json", "c-2", "cursor", t + 30, "[]".to_owned());
            insert("C:/u/Cursor/User/globalStorage/state.vscdb", "c-2", "cursor", t + 30, usage(t + 1, 50, 5));
            // 不靠估算的来源不接
            insert("C:/u/.codex/sessions/a.jsonl", "a", "codex", t, usage(t, 1, 1));
        }

        let mut history = History::new(Some(saved.clone()));
        history.load().unwrap();
        history.legacy_done = history.import_legacy();
        assert!(history.legacy_done && history.dirty);
        assert_eq!(history.sessions.keys().collect::<Vec<_>>(), ["card:card-7", "card:card-9", "chat:c-1", "chat:c-2", "kiro:ws/sess_1/messages.jsonl"]);
        // 没有用量的卡片也接过来，和它所在的对话共用会话 id
        assert!(history.sessions["card:card-7"].plugin && history.sessions["card:card-7"].events.is_empty());
        assert_eq!(history.sessions["card:card-7"].id, "c-2");
        assert_eq!(history.sessions["chat:c-2"].card, "card:card-7");
        // 同一张卡片取最新的一行；对话记下在它里面运行的卡片
        assert_eq!(history.sessions["card:card-9"].id, "c-1");
        assert!(history.sessions["card:card-9"].plugin);
        assert_eq!(history.sessions["chat:c-1"].card, "card:card-9");
        let kiro = history.sessions["kiro:ws/sess_1/messages.jsonl"].restore(&saved, false);
        assert_eq!(kiro.session.id, "sess_1");
        assert_eq!(kiro.models, ["claude-opus-5-5-max"]);
        assert_eq!((kiro.events[0].ts, kiro.events[0].usage.total(), kiro.events[0].estimated), (t, 110, true));
        assert_eq!(kiro.events[0].effort, Effort::from_model_name("claude-opus-5-5-max"));
        assert_eq!(kiro.events[0].credits, 0.5);

        // 对话已经被删掉、卡片还在并报出全部请求：卡片以实时扫描为准，对话那份不再接着算
        let mut database = FileEntry::new(Agent::Cursor, PathBuf::from("C:/u/Cursor/User/globalStorage/state.vscdb"));
        let mut card = part(Agent::Cursor, "C:/u/Cursor/x/conversations/local/sessions/card-9/process.json", "gateway/card-9", vec![event(t, 200, 20), event(t + 2, 30, 3)]);
        card.plugin = true;
        database.extra = vec![card];
        history.absorb([&database].into_iter());
        assert_eq!(history.sessions.keys().collect::<Vec<_>>(), ["card:card-7", "card:card-9", "chat:c-2", "kiro:ws/sess_1/messages.jsonl"]);
        assert_eq!(history.sessions["card:card-9"].events.len(), 2);
        // 另一段对话和它的卡片都扫不到了：两份一起由历史顶上，对话仍然算插件对话
        assert_eq!(own(history.missing([&database].into_iter())), ["card:card-7", "chat:c-2", "kiro:ws/sess_1/messages.jsonl"]);

        // 写盘成功后旧库删掉，工具记录不跟过来；再启动时历史里的会话都在，还是记在同一台电脑名下
        history.write(t + 100).unwrap();
        assert!(saved.exists() && !legacy.exists());
        assert!(!history.unsaved());
        assert!(!fs::read_to_string(&saved).unwrap().contains("shell"));
        let mut again = History::new(Some(saved.clone()));
        again.load().unwrap();
        assert!(again.import_legacy() && !again.dirty);
        assert_eq!(again.sessions, history.sessions);
        assert_eq!((&again.device, again.rev), (&history.device, 1));
        assert!(again.others.is_empty() && !again.unsaved());

        // 读不懂的旧库留着不删，等下次启动再试
        fs::write(&legacy, b"not a database").unwrap();
        let mut broken = History::new(Some(saved.clone()));
        broken.load().unwrap();
        broken.legacy_done = broken.import_legacy();
        assert!(!broken.legacy_done);
        broken.dirty = true;
        broken.write(t + 200).unwrap();
        assert!(legacy.exists());
        assert_eq!(read(&saved).devices[&history.device].rev, 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn hands_the_recorded_cards_to_the_scan_before_it_starts() {
        let t = 1_767_225_600_000;
        let dir = std::env::temp_dir().join(format!("foxes-core-history-prime-{}-{}", std::process::id(), crate::timeutil::now_ms()));
        fs::create_dir_all(&dir).unwrap();
        let saved = dir.join("history.json");
        {
            let mut database = FileEntry::new(Agent::Cursor, PathBuf::from("C:/u/Cursor/User/globalStorage/state.vscdb"));
            let chat = part(Agent::Cursor, "C:/u/Cursor/User/globalStorage/state.vscdb", "c-1", vec![event(t, 100, 10)]);
            let mut card = part(Agent::Cursor, "C:/u/Cursor/x/local.baishitong-customer/sessions/card-9/process.json", "card-9", vec![event(t + 2_000, 30, 3), event(t + 9_000, 40, 4)]);
            card.plugin = true;
            database.extra = vec![chat, card];
            let mut history = History::new(Some(saved.clone()));
            history.absorb([&database].into_iter());
            history.write(t + 10_000).unwrap();
        }

        let mut store = Store::new(crate::sources::Roots::default());
        let mut history = History::new(Some(saved.clone()));
        assert!(history.prime(&mut store));
        let recorded = store.recorded();
        // 只交出插件卡片，对话不交
        assert_eq!(recorded.cards.keys().collect::<Vec<_>>(), ["card-9"]);
        let known = &recorded.cards["card-9"];
        assert_eq!(known.events.iter().map(|event| event.ts).collect::<Vec<_>>(), [t + 2_000, t + 9_000]);
        assert_eq!(known.events[1].usage, event(t + 9_000, 40, 4).usage);
        assert!(known.events.iter().all(|event| event.estimated && event.key == 0));
        assert_eq!((known.prior_until, known.models.as_slice()), (t + 9_000, &["model-high".to_owned()][..]));
        assert_eq!((recorded.rev, history.recorded_rev), (1, 1));
        assert_eq!(history.prior_until["card:card-9"], t + 9_000);
        // 只交一次
        assert!(history.prime(&mut store));
        assert_eq!(store.recorded().rev, 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn waits_for_a_scan_that_used_the_recorded_cards() {
        let t = 1_767_225_600_000;
        let path = "C:/u/Cursor/x/local.baishitong-customer/sessions/card-9/process.json";
        let mut database = FileEntry::new(Agent::Cursor, PathBuf::from("C:/u/Cursor/User/globalStorage/state.vscdb"));
        let mut card = part(Agent::Cursor, path, "card-9", vec![event(t, 30, 3), event(t + 5_000, 40, 4)]);
        card.plugin = true;
        database.extra = vec![card];
        let mut history = empty();
        history.absorb([&database].into_iter());
        assert_eq!(history.sessions["card:card-9"].events.len(), 2);

        // 重启之后：历史读进来了，但这一轮扫描开始得更早，卡片只照现存的过程记录算出了一次请求
        history.recorded_rev = 1;
        history.stamps.clear();
        let mut early = part(Agent::Cursor, path, "card-9", vec![event(t + 5_000, 40, 4)]);
        early.plugin = true;
        database.extra = vec![early];
        history.absorb([&database].into_iter());
        assert_eq!(history.sessions["card:card-9"].events.len(), 2, "what was recorded stands until the scan has seen it");

        // 下一轮扫描照着记下的那份算：收下
        let mut merged = part(Agent::Cursor, path, "card-9", vec![event(t, 30, 3), event(t + 5_000, 40, 4), event(t + 9_000, 50, 5)]);
        merged.plugin = true;
        database.extra = vec![merged];
        if let ParserState::Cursor(state) = &mut database.parser {
            state.recorded_rev = 1;
        }
        history.absorb([&database].into_iter());
        assert_eq!(history.sessions["card:card-9"].events.len(), 3);
    }

    #[test]
    fn keeps_what_a_deleted_conversation_recorded_before_its_card_took_over() {
        let t = 1_767_225_600_000;
        let path = "C:/u/Cursor/x/conversations/local/sessions/card-9/process.json";
        let mut database = FileEntry::new(Agent::Cursor, PathBuf::from("C:/u/Cursor/User/globalStorage/state.vscdb"));
        let chat = part(Agent::Cursor, "C:/u/Cursor/User/globalStorage/state.vscdb", "c-1", vec![event(t, 100, 10), event(t + 10_000, 200, 20), event(t + 20_000, 300, 30)]);
        // 卡片自己只报对话没记到的请求
        let mut card = part(Agent::Cursor, path, "c-1", vec![event(t + 5_000, 30, 3), event(t + 12_000, 40, 4)]);
        card.plugin = true;
        card.host = "c-1".to_owned();
        database.extra = vec![chat, card];
        let mut history = empty();
        history.absorb([&database].into_iter());
        // 重启：卡片在这之前记下的请求不再重算
        history.prior_until.insert("card:card-9".to_owned(), t + 12_000);

        // 对话被删掉：那之后的请求卡片自己报出来，之前的那些只有对话这份记录里有
        let mut alone = part(Agent::Cursor, path, "gateway/card-9", vec![event(t + 5_000, 30, 3), event(t + 12_000, 40, 4), event(t + 20_000, 300, 30)]);
        alone.plugin = true;
        database.extra = vec![alone];
        bump(&mut database);
        history.absorb([&database].into_iter());
        assert_eq!(history.sessions.keys().collect::<Vec<_>>(), ["card:card-9", "chat:c-1"]);
        let kept = history.sessions["chat:c-1"].restore(Path::new("history.json"), false);
        assert_eq!(kept.events.iter().map(|event| event.ts).collect::<Vec<_>>(), [t, t + 10_000]);
        assert!(history.sessions["chat:c-1"].card.is_empty());
        // 留下的那一段由历史顶上，和卡片不重复
        assert_eq!(own(history.missing([&database].into_iter())), ["chat:c-1"]);
        let stamps = |history: &History| -> Vec<i64> {
            let mut all: Vec<i64> = history.sessions.values().flat_map(|record| record.events.iter().map(|row| record.started + row.dt)).collect();
            all.sort_unstable();
            all
        };
        assert_eq!(stamps(&history), [t, t + 5_000, t + 10_000, t + 12_000, t + 20_000]);

        // 之后的扫描不再动它
        bump(&mut database);
        history.absorb([&database].into_iter());
        assert_eq!(stamps(&history), [t, t + 5_000, t + 10_000, t + 12_000, t + 20_000]);
    }

    #[test]
    fn identities_do_not_depend_on_the_computer_or_the_working_directory() {
        let identity_of = |agent: Agent, path: &str| identity(&part(agent, path, "id", Vec::new()));
        // 同一段会话在两台电脑上（数据目录、工作目录都不一样）是同一个标识，标识里没有工作目录的名字
        assert_eq!(identity_of(Agent::Claude, "C:/Users/a/.claude/projects/D--work-secret/0f1e.jsonl"), "claude:0f1e.jsonl");
        assert_eq!(identity_of(Agent::Claude, "/home/b/.claude/projects/-srv-other/0f1e.jsonl"), "claude:0f1e.jsonl");
        assert_eq!(identity_of(Agent::Claude, "C:/Users/a/.claude/projects/D--work-secret/0f1e/subagents/agent-7.jsonl"), "claude:0f1e/subagents/agent-7.jsonl");
        assert_eq!(identity_of(Agent::Codex, "C:/Users/a/.codex/sessions/2026/11/17/rollout-x.jsonl"), "codex:rollout-x.jsonl");
        assert_eq!(identity_of(Agent::Codex, "D:/codex/archived_sessions/rollout-x.jsonl.zst"), "codex:rollout-x.jsonl");
        assert_eq!(identity_of(Agent::Deepseek, "C:/Users/a/.dsh/sessions/D--work-secret/session-9/session.v4.jsonl.zstd"), "deepseek:session-9");
        assert_eq!(identity_of(Agent::Antigravity, "C:/Users/a/.gemini/antigravity/conversations/ab12.db"), "antigravity:ab12");
        assert_eq!(identity_of(Agent::Kiro, "C:/Users/a/.kiro/sessions/5d41/sess_1/messages.jsonl"), "kiro:5d41/sess_1/messages.jsonl");
    }

    #[test]
    fn keeps_the_request_keys() {
        let t = 1_767_225_600_000;
        let keyed = |ts: i64, key: u64| UsageEvent { key, ..event(ts, 100, 10) };
        let session = part(Agent::Claude, "C:/Users/a/.claude/projects/D--work/s1.jsonl", "s1", vec![keyed(t, 0xaa), keyed(t + 1_000, u64::MAX), event(t + 2_000, 1, 1)]);
        let mut history = empty();
        history.absorb([&session].into_iter());
        let text = serde_json::to_string(&history.saved(t)).unwrap();
        assert!(text.contains(",\"aa\"]") && text.contains(",\"ffffffffffffffff\"]"));
        let saved: Saved = serde_json::from_str(&text).unwrap();
        let restored = saved.devices[&history.device].sessions["claude:s1.jsonl"].restore(Path::new("history.json"), true);
        assert_eq!(restored.events.iter().map(|event| event.key).collect::<Vec<_>>(), [0xaa, u64::MAX, 0]);
    }

    #[test]
    fn adds_up_what_each_computer_recorded_without_counting_twice() {
        let t = 1_767_225_600_000;
        let dir = temp("carried");
        let carried = dir.join("disk/Foxes-watch-data/history.json");

        // 第一台电脑：两段会话记进本机的历史，也写进跟着程序走的那份
        let mut claude = part(Agent::Claude, "C:/Users/a/.claude/projects/D--work/s1.jsonl", "s1", vec![event(t, 100, 10), event(t + 1_000, 200, 20)]);
        let kiro = part(Agent::Kiro, "C:/Users/a/.kiro/sessions/ws/sess_1/messages.jsonl", "sess_1", vec![event(t + 2_000, 300, 30)]);
        let mut first = computer(&dir, "a", Some(&carried));
        first.load().unwrap();
        first.absorb([&claude, &kiro].into_iter());
        first.write(t + 10_000).unwrap();
        assert_eq!(requests(&first, &[&claude, &kiro]), 3);
        assert_eq!(fs::read(&carried).unwrap(), fs::read(dir.join("a/history.json")).unwrap());

        // 换到第二台电脑：先把带过来的并进来，再加上这台电脑自己的
        let codex = part(Agent::Codex, "/home/b/.codex/sessions/2026/01/01/rollout-b1.jsonl", "b1", vec![event(t + 60_000, 400, 40), event(t + 61_000, 500, 50)]);
        let mut second = computer(&dir, "b", Some(&carried));
        second.load().unwrap();
        assert_eq!((second.sessions.len(), second.others.len()), (0, 1));
        assert_ne!(second.device, first.device);
        second.absorb([&codex].into_iter());
        assert_eq!(requests(&second, &[&codex]), 5);
        // 第一台电脑记下的两段由历史顶上，标成别的电脑记下的
        let missing = second.missing([&codex].into_iter());
        assert_eq!(missing, [(first.device.clone(), "claude:s1.jsonl".to_owned()), (first.device.clone(), "kiro:ws/sess_1/messages.jsonl".to_owned())]);
        let restored = second.record(&missing[0].0, &missing[0].1).unwrap().restore(&carried, true);
        assert!(restored.gone && restored.elsewhere);
        second.write(t + 70_000).unwrap();
        assert_eq!(fs::read(&carried).unwrap(), fs::read(dir.join("b/history.json")).unwrap());

        // 回到第一台电脑：它自己的会话多了一次请求，第二台记下的也算进来
        claude.events.push(event(t + 90_000, 600, 60));
        claude.session.last_active_ms = t + 90_000;
        let mut first = computer(&dir, "a", Some(&carried));
        first.load().unwrap();
        assert_eq!((first.sessions.len(), first.others.len(), first.rev), (2, 1, 1));
        first.absorb([&claude, &kiro].into_iter());
        assert_eq!(requests(&first, &[&claude, &kiro]), 6);
        first.write(t + 100_000).unwrap();

        // 同一份历史在两台电脑之间来回带、并上好几遍，数还是那么多
        for round in 0..3 {
            let mut second = computer(&dir, "b", Some(&carried));
            second.load().unwrap();
            second.absorb([&codex].into_iter());
            assert_eq!(requests(&second, &[&codex]), 6, "round {round}");
            second.write(t + 110_000 + round).unwrap();
            let mut first = computer(&dir, "a", Some(&carried));
            first.load().unwrap();
            first.absorb([&claude, &kiro].into_iter());
            assert_eq!(requests(&first, &[&claude, &kiro]), 6, "round {round}");
            first.write(t + 120_000 + round).unwrap();
            let saved = read(&carried);
            assert_eq!(saved.devices.len(), 2);
            assert_eq!(saved.devices.values().map(|section| section.sessions.values().map(|record| record.events.len()).sum::<usize>()).sum::<usize>(), 6);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn counts_a_request_two_computers_both_recorded_once() {
        let t = 1_767_225_600_000;
        let dir = temp("totals");
        let carried = dir.join("disk/Foxes-watch-data/history.json");
        let keyed = |ts: i64, key: u64| UsageEvent { key, ..event(ts, 100, 10) };
        // 第二台电脑上接着第一台的会话干：新的会话文件里带着前面那两次请求
        let original = part(Agent::Claude, "C:/Users/a/.claude/projects/D--work/s1.jsonl", "s1", vec![keyed(t, 1), keyed(t + 1_000, 2)]);
        let resumed = part(Agent::Claude, "/home/b/.claude/projects/-srv-work/s2.jsonl", "s2", vec![keyed(t, 1), keyed(t + 1_000, 2), keyed(t + 60_000, 3)]);
        let mut first = computer(&dir, "a", Some(&carried));
        first.load().unwrap();
        first.absorb([&original].into_iter());
        first.write(t + 10_000).unwrap();
        let mut second = computer(&dir, "b", Some(&carried));
        second.load().unwrap();
        second.absorb([&resumed].into_iter());
        second.write(t + 70_000).unwrap();

        // 第三台电脑上什么来源都没有：两段会话都由历史顶上，重复的那两次请求只算一次
        let mut store = Store::new(crate::sources::Roots::default());
        let mut third = computer(&dir, "c", Some(&carried));
        assert!(third.sync(&mut store, t + 80_000));
        assert_eq!(third.error(), None);
        assert!(store.entries().all(|entry| entry.gone && entry.elsewhere));
        assert_eq!(store.entries().map(|entry| entry.events.len()).sum::<usize>(), 5);
        let analysis = crate::analyze(&store, t + 80_000);
        let all = analysis.snapshot.ranges.iter().find(|range| range.id == "all").unwrap();
        assert_eq!(all.totals.total, 3 * event(t, 100, 10).usage.total());
        assert_eq!(third.info(), HistoryInfo { sessions: 0, devices: 2, elsewhere: 2, carried: carried.display().to_string() });
        // 这台电脑只是看了一眼：它的数据目录里留一份，带着走的那份不用动；再同步也不会多出什么
        assert!(dir.join("c/history.json").exists());
        assert_eq!(read(&carried).devices.len(), 2);
        assert!(!third.sync(&mut store, t + 81_000));
        assert_eq!(store.entries().count(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_session_both_computers_recorded_counts_once() {
        let t = 1_767_225_600_000;
        let dir = temp("same-session");
        let carried = dir.join("disk/Foxes-watch-data/history.json");
        // 同一段会话的记录被拷到另一台电脑上接着用：第一台记到两次请求，第二台记到三次
        let earlier = part(Agent::Claude, "C:/Users/a/.claude/projects/D--work/s1.jsonl", "s1", vec![event(t, 100, 10), event(t + 1_000, 200, 20)]);
        let later = part(Agent::Claude, "/home/b/.claude/projects/-srv-work/s1.jsonl", "s1", vec![event(t, 100, 10), event(t + 1_000, 200, 20), event(t + 2_000, 300, 30)]);
        let mut first = computer(&dir, "a", Some(&carried));
        first.load().unwrap();
        first.absorb([&earlier].into_iter());
        first.write(t + 10_000).unwrap();

        // 来源还在这台电脑上：以实时扫描为准，带过来的那份不算
        let mut second = computer(&dir, "b", Some(&carried));
        second.load().unwrap();
        second.absorb([&later].into_iter());
        assert!(second.missing([&later].into_iter()).is_empty());
        assert_eq!(requests(&second, &[&later]), 3);
        second.write(t + 20_000).unwrap();

        // 来源没了：两台电脑的记录里都有它，只取更新的那一份
        assert_eq!(second.missing(std::iter::empty()), [(String::new(), "claude:s1.jsonl".to_owned())]);
        assert_eq!(requests(&second, &[]), 3);
        // 回到第一台电脑：它自己那份旧，来源不在时用第二台记下的；来源还在时照旧以这台电脑上的为准
        let mut first = computer(&dir, "a", Some(&carried));
        first.load().unwrap();
        assert_eq!(first.missing(std::iter::empty()), [(second.device.clone(), "claude:s1.jsonl".to_owned())]);
        assert_eq!(requests(&first, &[]), 3);
        assert_eq!(requests(&first, &[&earlier]), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_copy_left_behind_does_not_bring_back_what_its_computer_dropped() {
        let t = 1_767_225_600_000;
        let dir = temp("stale");
        let carried = dir.join("disk/Foxes-watch-data/history.json");
        let path = "C:/Users/a/.kiro/sessions/ws/sess_1/messages.jsonl";
        let kept = part(Agent::Kiro, "C:/Users/a/.kiro/sessions/ws/sess_2/messages.jsonl", "sess_2", vec![event(t, 100, 10)]);
        let session = part(Agent::Kiro, path, "sess_1", vec![event(t + 1_000, 200, 20)]);
        let mut first = computer(&dir, "a", Some(&carried));
        first.load().unwrap();
        first.absorb([&kept, &session].into_iter());
        first.write(t + 10_000).unwrap();

        // 硬盘不在的时候（装在这台电脑上的那份在跑）：一段会话被改写成空的，本机的历史里不再有它
        let mut rewritten = part(Agent::Kiro, path, "sess_1", Vec::new());
        rewritten.sig.size = 1;
        let mut installed = computer(&dir, "a", None);
        installed.load().unwrap();
        installed.absorb([&kept, &rewritten].into_iter());
        installed.write(t + 20_000).unwrap();
        assert_eq!(read(&carried).devices[&first.device].sessions.len(), 2);

        // 硬盘插回来：程序旁边那份还是旧的，以本机的为准，再把它写成一样的
        let mut again = computer(&dir, "a", Some(&carried));
        again.load().unwrap();
        assert_eq!(again.sessions.keys().collect::<Vec<_>>(), ["kiro:ws/sess_2/messages.jsonl"]);
        assert!(again.unsaved());
        again.write(t + 30_000).unwrap();
        let saved = read(&carried);
        assert_eq!((saved.devices.len(), saved.devices[&first.device].rev), (1, 2));
        assert_eq!(saved.devices[&first.device].sessions.keys().collect::<Vec<_>>(), ["kiro:ws/sess_2/messages.jsonl"]);
        // 另一台电脑看到的也是这样
        let mut other = computer(&dir, "b", Some(&carried));
        other.load().unwrap();
        assert_eq!(requests(&other, &[]), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_history_file_copied_from_another_computer_stays_that_computers() {
        let t = 1_767_225_600_000;
        let dir = temp("copied");
        let kiro = part(Agent::Kiro, "C:/Users/a/.kiro/sessions/ws/sess_1/messages.jsonl", "sess_1", vec![event(t, 100, 10)]);
        let mut first = computer(&dir, "a", None);
        first.load().unwrap();
        first.absorb([&kiro].into_iter());
        first.write(t + 10_000).unwrap();

        // 把整个文件拷进另一台电脑的数据目录
        fs::create_dir_all(dir.join("b")).unwrap();
        fs::copy(dir.join("a/history.json"), dir.join("b/history.json")).unwrap();
        let codex = part(Agent::Codex, "/home/b/.codex/sessions/2026/01/01/rollout-b1.jsonl", "b1", vec![event(t + 60_000, 400, 40)]);
        let mut second = computer(&dir, "b", None);
        second.load().unwrap();
        // 里面的会话还是第一台电脑的，这台电脑另起一份：以后两台各记各的，谁也不会盖掉谁
        assert_ne!(second.device, first.device);
        assert_eq!((second.sessions.len(), second.others[&first.device].sessions.len()), (0, 1));
        assert!(second.unsaved());
        second.absorb([&codex].into_iter());
        assert_eq!(requests(&second, &[&codex]), 2);
        second.write(t + 70_000).unwrap();
        let saved = read(&dir.join("b/history.json"));
        assert_eq!((saved.device.as_str(), saved.host.as_str(), saved.devices.len()), (second.device.as_str(), "host-b", 2));
        assert_eq!(saved.devices[&first.device], Section { rev: 1, sessions: first.sessions.clone() });
        // 下次启动认得出这是自己写的
        let mut again = computer(&dir, "b", None);
        again.load().unwrap();
        assert_eq!((&again.device, again.rev, again.sessions.len(), again.others.len()), (&second.device, 1, 1, 1));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn upgrades_the_first_format_and_keeps_the_old_file() {
        let t = 1_767_225_600_000;
        let dir = temp("upgrade");
        let local = dir.join("a/history.json");
        let kiro = part(Agent::Kiro, "C:/Users/a/.kiro/sessions/ws/sess_1/messages.jsonl", "sess_1", vec![event(t, 100, 10)]);
        let record = Record::of(&kiro, String::new());
        let old = serde_json::json!({ "version": 1, "savedAt": t, "sessions": { "kiro:ws/sess_1/messages.jsonl": record } }).to_string();
        fs::create_dir_all(dir.join("a")).unwrap();
        fs::write(&local, &old).unwrap();

        // 第 1 版的会话都算这台电脑的
        let mut history = computer(&dir, "a", None);
        history.load().unwrap();
        assert_eq!(history.sessions.keys().collect::<Vec<_>>(), ["kiro:ws/sess_1/messages.jsonl"]);
        assert!(history.others.is_empty() && history.unsaved());
        history.write(t + 10_000).unwrap();
        // 原来的文件原样留一份（旧版本的软件读不懂新格式），新文件按电脑分开记
        assert_eq!(fs::read_to_string(dir.join("a").join(BACKUP_V1)).unwrap(), old);
        let saved = read(&local);
        assert_eq!((saved.version, saved.sessions.len(), saved.devices.len()), (VERSION, 0, 1));
        assert_eq!(saved.devices[&history.device].sessions["kiro:ws/sess_1/messages.jsonl"], record);
        assert_eq!(saved.devices[&history.device].rev, 1);

        // 第 1 版的文件被直接放到程序旁边带去另一台电脑：里面的会话当成一台不知名的电脑记下的，并几遍都只有一份
        let carried = dir.join("disk/Foxes-watch-data/history.json");
        fs::create_dir_all(carried.parent().unwrap()).unwrap();
        for _ in 0..2 {
            fs::write(&carried, &old).unwrap();
            let mut second = computer(&dir, "b", Some(&carried));
            second.load().unwrap();
            assert_eq!((second.sessions.len(), second.others.len()), (0, 1));
            assert_eq!(requests(&second, &[]), 1);
            second.write(t + 20_000).unwrap();
            assert_eq!((read(&carried).version, read(&carried).devices.len()), (VERSION, 2));
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn merges_what_changed_on_disk_before_writing_over_it() {
        let t = 1_767_225_600_000;
        let dir = temp("replug");
        let carried = dir.join("disk/Foxes-watch-data/history.json");
        let mut kiro = part(Agent::Kiro, "C:/Users/a/.kiro/sessions/ws/sess_1/messages.jsonl", "sess_1", vec![event(t, 100, 10)]);
        let mut first = computer(&dir, "a", Some(&carried));
        first.load().unwrap();
        first.absorb([&kiro].into_iter());
        first.write(t + 10_000).unwrap();

        // 第一台电脑上的程序没退出（比如休眠了），硬盘被拿到第二台电脑上用了一阵
        let codex = part(Agent::Codex, "/home/b/.codex/sessions/2026/01/01/rollout-b1.jsonl", "b1", vec![event(t + 60_000, 400, 40)]);
        let mut second = computer(&dir, "b", Some(&carried));
        second.load().unwrap();
        second.absorb([&codex].into_iter());
        second.write(t + 70_000).unwrap();

        // 回来之后第一台接着记：写之前先把第二台记下的并进来，而不是拿内存里的旧内容盖掉它
        kiro.events.push(event(t + 80_000, 200, 20));
        kiro.sig.size = 1;
        first.absorb([&kiro].into_iter());
        first.write(t + 90_000).unwrap();
        assert!(first.refill && first.others.contains_key(&second.device));
        assert_eq!(requests(&first, &[&kiro]), 3);
        let saved = read(&carried);
        assert_eq!(saved.devices.len(), 2);
        assert_eq!((saved.devices[&first.device].rev, saved.devices[&first.device].sessions["kiro:ws/sess_1/messages.jsonl"].events.len()), (2, 2));
        assert_eq!(saved.devices[&second.device].sessions.len(), 1);
        assert_eq!(fs::read(&carried).unwrap(), fs::read(dir.join("a/history.json")).unwrap());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn leaves_a_file_from_a_newer_version_alone_and_puts_an_unreadable_one_aside() {
        let t = 1_767_225_600_000;
        let dir = temp("newer");
        let carried = dir.join("disk/Foxes-watch-data/history.json");
        fs::create_dir_all(carried.parent().unwrap()).unwrap();
        let newer = format!("{{\"version\":{},\"devices\":{{\"x\":{{\"rev\":4,\"sessions\":{{}},\"extra\":true}}}}}}", VERSION + 1);
        fs::write(&carried, &newer).unwrap();
        let kiro = part(Agent::Kiro, "C:/Users/a/.kiro/sessions/ws/sess_1/messages.jsonl", "sess_1", vec![event(t, 100, 10)]);

        // 更新版本的软件写的：不读也不写，本机的照常记
        let mut history = computer(&dir, "a", Some(&carried));
        history.load().unwrap();
        assert!(history.others.is_empty());
        assert!(history.error().is_some_and(|error| error.contains("更新版本")));
        history.absorb([&kiro].into_iter());
        history.write(t + 10_000).unwrap();
        assert_eq!(read(&dir.join("a/history.json")).devices[&history.device].sessions.len(), 1);
        assert_eq!(fs::read_to_string(&carried).unwrap(), newer);
        assert!(!history.unsaved());

        // 读不懂的：改名留着，再写一份新的
        fs::write(&carried, b"{ not a history").unwrap();
        let mut history = computer(&dir, "a", Some(&carried));
        history.load().unwrap();
        assert_eq!(history.error(), None);
        assert!(history.unsaved());
        history.write(t + 20_000).unwrap();
        assert_eq!(fs::read(carried.with_extension("unreadable.json")).unwrap(), b"{ not a history");
        assert_eq!(read(&carried).devices[&history.device].sessions.len(), 1);

        // 本机的那份是更新版本的软件写的：原样留在原处，这次运行两份都不写（这台电脑的编号读不出来），程序旁边那份照常读
        let local = dir.join("b/history.json");
        fs::create_dir_all(local.parent().unwrap()).unwrap();
        fs::write(&local, &newer).unwrap();
        let before = fs::read(&carried).unwrap();
        let mut history = computer(&dir, "b", Some(&carried));
        history.load().unwrap();
        assert!(history.error().is_some_and(|error| error.contains("更新版本")));
        assert_eq!(requests(&history, &[]), 1);
        history.absorb([&kiro].into_iter());
        assert!(!history.unsaved());
        history.flush(t + 30_000);
        assert_eq!(fs::read_to_string(&local).unwrap(), newer);
        assert!(!local.with_extension("unreadable.json").exists());
        assert_eq!(fs::read(&carried).unwrap(), before);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn takes_back_what_an_older_version_put_aside() {
        let t = 1_767_225_600_000;
        let dir = temp("aside");
        let local = dir.join("a/history.json");
        let aside = dir.join("a/history.unreadable.json");
        let kiro = part(Agent::Kiro, "C:/Users/a/.kiro/sessions/ws/sess_1/messages.jsonl", "sess_1", vec![event(t, 100, 10)]);
        let codex = part(Agent::Codex, "C:/Users/a/.codex/sessions/2026/01/01/rollout-a1.jsonl", "a1", vec![event(t, 300, 30)]);
        let mut history = computer(&dir, "a", None);
        history.load().unwrap();
        history.absorb([&kiro, &codex].into_iter());
        history.write(t + 10_000).unwrap();
        let (device, rev) = (history.device.clone(), history.rev);

        // 旧版本的软件读不懂这个文件：把它改名放到一边，自己另记一份第 1 版的（只有估算的那几种），这期间又多了一次请求
        fs::rename(&local, &aside).unwrap();
        let grown = part(Agent::Kiro, "C:/Users/a/.kiro/sessions/ws/sess_1/messages.jsonl", "sess_1", vec![event(t, 100, 10), event(t + 60_000, 200, 20)]);
        let old = serde_json::json!({ "version": 1, "savedAt": t + 70_000, "sessions": { "kiro:ws/sess_1/messages.jsonl": Record::of(&grown, String::new()) } }).to_string();
        fs::write(&local, &old).unwrap();

        // 再用这个版本启动：放到一边的那份接回来，编号接着用，旧软件后来记的盖在上面；来源都不在了也一次不少、一次不重
        let mut history = computer(&dir, "a", None);
        history.load().unwrap();
        assert_eq!((&history.device, history.rev, history.sessions.len(), history.others.len()), (&device, rev, 2, 0));
        assert_eq!(history.sessions["kiro:ws/sess_1/messages.jsonl"].events.len(), 2);
        assert_eq!(requests(&history, &[]), 3);
        history.write(t + 80_000).unwrap();
        assert!(!aside.exists() && !history.unsaved());
        let saved = read(&local);
        assert_eq!((saved.version, saved.devices.len(), saved.devices[&device].rev, saved.devices[&device].sessions.len()), (VERSION, 1, rev + 1, 2));
        // 旧软件写的那份照例原样留一份
        assert_eq!(fs::read_to_string(dir.join("a").join(BACKUP_V1)).unwrap(), old);

        // 别的电脑上被放到一边的文件拷到这里：当成另一份历史并进来，那台电脑的记录还是它的
        fs::create_dir_all(dir.join("b")).unwrap();
        fs::copy(&local, dir.join("b/history.unreadable.json")).unwrap();
        let mut second = computer(&dir, "b", None);
        second.load().unwrap();
        assert_ne!(second.device, device);
        assert_eq!((second.sessions.len(), second.others.len(), requests(&second, &[])), (0, 1, 3));
        second.write(t + 90_000).unwrap();
        assert!(!dir.join("b/history.unreadable.json").exists());
        assert_eq!(read(&dir.join("b/history.json")).devices.len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }
}
