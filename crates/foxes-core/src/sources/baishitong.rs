//! 百事通（客户版）插件的对话。
//!
//! 这一版插件把对话放在它的服务端，本机不落任何记录文件。它在本机开了一个对话页转发口
//! （`127.0.0.1:9791`，只接受本机连接），对话页就是经它读服务端的；这里也经这个口读，只发 GET，
//! 只取两样东西：
//!
//! - `/api/sessions`：有哪些对话，各自的名称、模型、创建时间、是否正在运行；
//! - `/api/process?session=<id>`：一段对话里助手每一步工具调用的时刻、工具名和大小，偶尔还有
//!   云端同步回来的思考块（没有 token 数）。
//!
//! 读到的内容当场折成时刻、体量和工具名，对话正文不保留。登录用的是转发口给本机页面的自动登录，
//! 换来的 Cookie 只放在内存里，不写盘、不进日志。除了这个本机端口，不连任何别的地址。
//!
//! 服务端每次都回整段过程记录，没有增量接口，所以读得很省：会话列表带着上次的 ETag 去问，
//! 过程记录带着上次的 `rev` 去问，没变化时一次只有几百字节；正在运行的对话最多每分钟读一次，
//! 记录越长隔得越久；不在运行的只在列表显示它有动静时读一次。
//!
//! 只有刚启动的那一遍读得紧：列表一到，正在运行的对话就先按列表的说法报出去，从没读过的对话
//! 一段接一段地读（先读正在运行的），不等整整一拍；读过一遍之后回到上面的节奏。
//!
//! 几百 KB 的过程记录时不时传到一半就断（2026-11 实测：八次里断了两次）。断了的那一段过一会儿再读，
//! 一次比一次隔得久，别的对话照常读；连着出错、什么都读不到的时候才整个停下来，一分钟问一次。

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::cursor_gateway::{short, Prompt, Row};
use super::Recorded;
use crate::lenient::{text_weight, u64_lenient, Flag, Obj, ShortCode, Text, TimeMs, Weight};
use crate::timeutil::{fnv64, now_ms, DAY, MINUTE, SECOND};

/// 插件在 Cursor 的 globalStorage 里的名字。卡片条目的路径挂在它下面，只是个标识，磁盘上没有这些文件。
pub(super) const DIR: &str = "local.baishitong-customer";

/// 客户版对话页转发口的端口（插件里写死的）和它发的 Cookie 名。
const PORT: u16 = 9791;
const COOKIE: &str = "mcprust_cust";

const TICK: Duration = Duration::from_secs(2);
/// 还有从没读过的对话等着读的时候（刚启动的那一遍），两次读取之间只歇这么久，不等整整一拍
const HURRY: Duration = Duration::from_millis(400);
/// 刚启动、解析器还没来取过的时候，隔一小会儿看一眼它来了没有；最多看这么多次，之后一拍看一次
const EARLY_CHECKS: u32 = 50;
const CONNECT_TIMEOUT: Duration = Duration::from_millis(1500);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_HEAD: usize = 64 * 1024;
const MAX_BODY: usize = 64 * 1024 * 1024;
const MAX_REDIRECTS: usize = 6;

/// 解析器这么久没来取，就当没人在看，停止轮询
const WANTED_MS: i64 = 2 * MINUTE;
const LIST_EVERY_MS: i64 = 20 * SECOND;
/// 正在运行的对话隔多久读一次过程记录：上次读到的越大隔得越久（见 [`poll_every`]）
const BUSY_EVERY_MS: i64 = MINUTE;
const BUSY_EVERY_MAX_MS: i64 = 5 * MINUTE;
/// 同一段对话两次读取之间至少隔这么久；上次没读成的隔得更久（见 [`Poll::gap`]）
const SESSION_GAP_MS: i64 = 15 * SECOND;
const OFFLINE_RETRY_MS: i64 = 30 * SECOND;
/// 连着出错时整个停这么久再问；头两次停得短（见 [`error_pause`]）
const ERROR_RETRY_MS: i64 = MINUTE;
/// 插件的自动登录每分钟只有几次，而且和用户自己打开的对话页共用：两次登录至少隔这么久；
/// 只有刚开始没登录上的头几次隔得短些（见 [`login_wait`]）
const LOGIN_EVERY_MS: i64 = 5 * MINUTE;
/// 转发口这么久没有回应，就不再相信“正在运行”的说法
const ALIVE_MS: i64 = 2 * MINUTE;
/// 最近有消息的对话，开机后不管用量历史里记到哪都读一遍：上次关掉软件之后它可能又跑过几步、没再回话
const RECENT_MS: i64 = 7 * DAY;
/// 用量历史里最后一次请求比对话最后一条消息早不过这么多，就算已经记全了
const COVERED_MS: i64 = 2 * MINUTE;

/// 一次工具结果进上下文的上限：网关会把更长的结果截掉
const RESULT_CAP: u64 = 33_000;
/// 模型一次写文件最多算这么多
const WRITE_CAP: u64 = 200_000;
/// 服务端留的目标预览最多这么多字；占满了就是被截过
const TARGET_CAP: usize = 200;
/// 用户的一条消息、助手的一条回复按多大算。过程记录里只有消息的时刻，正文这里不读，所以是粗估。
const USER_MESSAGE: u64 = 400;
const REPLY: u64 = 1500;
/// 一步工具调用最长算这么久。记录里个别行的结束时刻比实际晚得多（2026-11 实测：进入工作区的
/// 那一行会晚几十分钟），照单全收的话，它后面的调用都会被当成和它同时发出的。
const MAX_TOOL_MS: i64 = 2 * MINUTE;
const MAX_THINKING_MS: i64 = 15 * MINUTE;

/// 服务端列出的一段对话。
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Session {
    pub id: String,
    pub parent: String,
    pub name: String,
    pub model: String,
    pub max_mode: bool,
    pub created_at: i64,
    pub cwd: String,
    /// 助手正在处理这段对话
    pub busy: bool,
    /// 最后一条消息、或者助手最后一次露面的时刻；列表里没有时为 0
    pub active_at: i64,
}

/// 一段对话的过程记录读到的一次结果，已经折成时刻、体量和工具名。
#[derive(Clone, Debug, Default)]
pub(super) struct Page {
    /// 过程记录没变：这一页只有消息的时刻
    pub partial: bool,
    pub rows: Vec<(String, Row)>,
    /// 用户的消息和助手的回复，按消息 id
    pub prompts: Vec<(String, Prompt)>,
    pub replies: Vec<(String, Prompt)>,
    /// 助手正在思考的话，从什么时候开始
    pub thinking_since: Option<i64>,
}

impl Page {
    /// 上一页还没被取走时，把新读到的一页并进来。
    fn merge(&mut self, newer: Page) {
        if newer.partial {
            self.prompts.extend(newer.prompts);
            self.replies.extend(newer.replies);
        } else {
            *self = newer;
        }
    }
}

/// 解析器一次取走的东西。
#[derive(Default)]
pub(super) struct Batch {
    /// 会话列表；和上次取走的一样时为空
    pub list: Option<Vec<Session>>,
    pub pages: Vec<(String, Page)>,
    /// 转发口最近有回应
    pub alive: bool,
    /// 马上还有新的要交：刚启动还没问到，或者正一段接一段地读第一遍。解析器过一秒就该再来取
    pub soon: bool,
}

/// 和转发口连得怎么样。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Link {
    /// 还没问过
    #[default]
    Unknown,
    /// 读得到
    Online,
    /// 端口连不上：Cursor 没开，或者插件没在运行
    Offline,
    /// 端口上是别的程序，不是百事通客户版的转发
    Foreign,
    /// 没登录上：插件自己还没连上服务端，或者自动登录的次数一时用完了
    Denied,
    /// 服务端让等一等
    Busy,
    /// 连着出错：服务端报错、回应读不懂、读到一半断了
    Failing,
}

/// 读百事通的对话读得怎么样，给界面看：只有状态和几个数。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub link: Link,
    /// 服务端列出的对话数，其中正在运行的，和还从没读过过程记录的
    pub sessions: u32,
    pub busy: u32,
    pub pending: u32,
    /// 上一次读成的时刻；还没读成过为 0
    pub ok_at: i64,
    /// 出了错在等的话，下一次什么时候再试；没在等为 0
    pub retry_at: i64,
}

#[derive(Default)]
struct Shared {
    /// 解析器上次来取的时刻
    wanted_ms: i64,
    /// 还没被取走的会话列表，和各段对话最新的一页过程记录
    list: Option<Vec<Session>>,
    pages: HashMap<String, Page>,
    alive: bool,
    soon: bool,
    status: Status,
    /// 用量历史把各段对话记到了什么时刻
    recorded_until: HashMap<String, i64>,
    /// 第一个来取的那份 Cursor 数据
    owner: Option<PathBuf>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 百事通对话的来源：后台线程轮询，Cursor 的解析器每轮来取一次新读到的东西。
pub struct Feed {
    shared: Arc<Mutex<Shared>>,
}

impl Feed {
    /// 起后台线程。线程只拿着弱引用，`Feed` 被丢掉后自己退出；解析器不来取的时候它不发任何请求。
    pub fn start() -> Feed {
        // 线程一起来就会去问，第一份结果马上就到
        let shared = Arc::new(Mutex::new(Shared { soon: true, ..Shared::default() }));
        let weak = Arc::downgrade(&shared);
        // 线程起不来就当没有这个来源
        if thread::Builder::new().name("foxes-baishitong".into()).spawn(move || run(weak, Client::new(PORT))).is_err() {
            lock(&shared).soon = false;
        }
        Feed { shared }
    }

    /// 现在读得怎么样（给界面看）。
    pub fn status(&self) -> Status {
        lock(&self.shared).status.clone()
    }

    /// 取走上次之后新读到的东西。`root` 是来取的那份 Cursor 数据的目录：机器上有几份 Cursor 数据时
    /// 只交给第一个来取的，免得同一段对话在两处各算一遍。`recorded` 是用量历史里已经记下的卡片：
    /// 记全了的旧对话不用再读。
    pub(super) fn take(&self, root: &Path, now: i64, recorded: Option<&Recorded>) -> Batch {
        let mut shared = lock(&self.shared);
        if shared.owner.get_or_insert_with(|| root.to_path_buf()).as_path() != root {
            return Batch::default();
        }
        shared.wanted_ms = now;
        if let Some(recorded) = recorded.filter(|recorded| recorded.cards.len() != shared.recorded_until.len()) {
            shared.recorded_until =
                recorded.cards.iter().map(|(id, card)| (id.clone(), card.events.iter().map(|event| event.ts).max().unwrap_or(0))).collect();
        }
        Batch { list: shared.list.take(), pages: shared.pages.drain().collect(), alive: shared.alive, soon: shared.soon }
    }
}

fn run(shared: Weak<Mutex<Shared>>, mut client: Client) {
    // 第一拍不等整整两秒：解析器一来取就开始问
    let mut pause = HURRY;
    let mut early = EARLY_CHECKS;
    loop {
        thread::sleep(pause);
        let Some(shared) = shared.upgrade() else {
            return;
        };
        let now = now_ms();
        let wanted = lock(&shared).wanted_ms;
        pause = if within(now, wanted, WANTED_MS) {
            early = 0;
            if client.step(now, &shared) {
                HURRY
            } else {
                TICK
            }
        } else if wanted == 0 && early > 0 {
            early -= 1;
            HURRY
        } else {
            TICK
        };
    }
}

/// `since` 之后还不到 `gap`（时钟被往回拨过也不会一直等下去）。
fn within(now: i64, since: i64, gap: i64) -> bool {
    since > 0 && (now - since).abs() < gap
}

/// 正在运行的对话隔多久读一次：每 256 KB 一分钟，一分钟到五分钟之间。
fn poll_every(bytes: usize) -> i64 {
    ((bytes / (256 * 1024)) as i64 * MINUTE).clamp(BUSY_EVERY_MS, BUSY_EVERY_MAX_MS)
}

/// 连着出了 `errors` 次错之后整个停多久。回应传到一半断掉是常事，停几秒换一段对话接着读就行；
/// 连着出错多半是服务端真有事，这才一分钟问一次。
fn error_pause(errors: u32) -> i64 {
    match errors {
        0 | 1 => 5 * SECOND,
        2 => 20 * SECOND,
        _ => ERROR_RETRY_MS,
    }
}

/// 连着 `fails` 次没登录上之后，隔多久再登录。头几次隔半分钟、一分钟、两分钟：多半只是插件自己还没
/// 连上服务端，或者登录的那次回应断了。再往后，以及登录成功过、Cookie 后来失效的，都隔五分钟。
fn login_wait(fails: u32) -> i64 {
    match fails {
        1 => 30 * SECOND,
        2 => MINUTE,
        3 => 2 * MINUTE,
        _ => LOGIN_EVERY_MS,
    }
}

fn clean(text: &str, limit: usize) -> String {
    text.chars().filter(|ch| !ch.is_control()).take(limit).collect()
}

/// 对话 id 形如 `card-<毫秒>-<十六进制>`。它会进请求地址和条目路径，别的样子一律不收。
fn safe_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 96 && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

/// 请求地址只能是本站的路径，不带空白和控制字符。
fn safe_path(path: &str) -> bool {
    path.starts_with('/') && !path.starts_with("//") && path.len() <= 1024 && path.bytes().all(|b| b.is_ascii_graphic())
}

#[derive(Deserialize)]
struct RawProbe<'a> {
    #[serde(borrow, default)]
    proxy: Text<'a>,
    #[serde(default)]
    customer: Flag,
}

#[derive(Deserialize)]
struct RawList<'a> {
    #[serde(default)]
    ok: Option<Flag>,
    #[serde(borrow, default)]
    sessions: Option<Vec<Obj<RawSession<'a>>>>,
}

#[derive(Deserialize)]
struct RawSession<'a> {
    #[serde(rename = "sessionId", borrow, default)]
    id: Text<'a>,
    #[serde(rename = "parentSessionId", borrow, default)]
    parent: Text<'a>,
    #[serde(borrow, default)]
    name: Text<'a>,
    #[serde(borrow, default)]
    model: Text<'a>,
    #[serde(rename = "modelId", borrow, default)]
    model_id: Text<'a>,
    #[serde(rename = "maxMode", default)]
    max_mode: Flag,
    #[serde(rename = "createdAt", default)]
    created_at: TimeMs,
    #[serde(rename = "workspaceRoot", borrow, default)]
    cwd: Text<'a>,
    #[serde(borrow, default)]
    state: Text<'a>,
    #[serde(rename = "agentPhase", borrow, default)]
    agent_phase: Text<'a>,
    #[serde(default)]
    ended: Flag,
    #[serde(rename = "lastMessageAt", default)]
    last_message_at: TimeMs,
    #[serde(rename = "lastAgentAt", default)]
    last_agent_at: TimeMs,
    #[serde(rename = "historyCount", default, deserialize_with = "u64_lenient")]
    history_count: u64,
    #[serde(rename = "contextBytes", default, deserialize_with = "u64_lenient")]
    context_bytes: u64,
}

#[derive(Deserialize)]
struct RawPage<'a> {
    #[serde(default)]
    ok: Option<Flag>,
    #[serde(default)]
    unchanged: Flag,
    #[serde(default)]
    rev: ShortCode,
    #[serde(borrow, default)]
    rows: Vec<Obj<RawRow<'a>>>,
    #[serde(borrow, default)]
    anchors: Vec<Obj<RawAnchor<'a>>>,
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
    #[serde(borrow, default)]
    tool: Text<'a>,
    #[serde(borrow, default)]
    server: Text<'a>,
    #[serde(borrow, default)]
    label: Text<'a>,
    #[serde(borrow, default)]
    status: Text<'a>,
    #[serde(default)]
    ok: Option<Flag>,
    #[serde(borrow, default)]
    target: Text<'a>,
    #[serde(default)]
    output: Weight,
    #[serde(borrow, default)]
    detail: Text<'a>,
    /// 思考行和正文行的用时和内容（内容只取大小）
    #[serde(rename = "durationMs", default, deserialize_with = "u64_lenient")]
    duration_ms: u64,
    #[serde(default)]
    text: Weight,
}

#[derive(Deserialize)]
struct RawAnchor<'a> {
    #[serde(borrow, default)]
    id: Text<'a>,
    #[serde(borrow, default)]
    role: Text<'a>,
    #[serde(default)]
    time: TimeMs,
}

/// 列表里的一段对话，连同轮询要看的指纹：列表里它有任何动静就变。
struct Listed {
    session: Session,
    stamp: u64,
}

/// 解析会话列表；读不懂返回 `None`。
fn sessions_of(body: &[u8]) -> Option<Vec<Listed>> {
    let raw: RawList = serde_json::from_slice(body).ok()?;
    if raw.ok.is_some_and(|ok| !ok.0) {
        return None;
    }
    let mut listed = Vec::new();
    for item in raw.sessions?.iter().filter_map(Obj::get) {
        let (id, parent, phase) = (item.id.as_str(), item.parent.as_str(), item.agent_phase.as_str());
        if !safe_id(id) {
            continue;
        }
        // 助手干活的时候，这个状态在 busy（有调用在跑）和 working（两次调用之间）之间来回变（2026-11 实测），
        // 停下来是 idle。指纹里只记折算后的“在不在干活”：来回变不算动静，不用为它重读过程记录
        let busy = (phase.eq_ignore_ascii_case("busy") || phase.eq_ignore_ascii_case("working")) && !item.ended.0;
        let stamp = fnv64(&[
            item.state.as_str().as_bytes(),
            b"|",
            &[u8::from(busy)],
            &[u8::from(item.ended.0)],
            &item.last_message_at.0.to_le_bytes(),
            &item.last_agent_at.0.to_le_bytes(),
            &item.history_count.to_le_bytes(),
            &item.context_bytes.to_le_bytes(),
        ]);
        listed.push(Listed {
            session: Session {
                id: id.to_owned(),
                parent: if safe_id(parent) { parent.to_owned() } else { String::new() },
                name: clean(item.name.as_str(), 120),
                // 模型的标识比显示名稳定；没有标识时才用显示名
                model: short(if item.model_id.is_empty() { item.model.as_str() } else { item.model_id.as_str() }),
                max_mode: item.max_mode.0,
                created_at: item.created_at.0,
                cwd: clean(item.cwd.as_str(), 260),
                busy,
                active_at: item.last_message_at.0.max(item.last_agent_at.0),
            },
            stamp,
        });
    }
    Some(listed)
}

/// 说明里的第一个数（「657 行」「新建 3245 字节」）。
fn number_in(text: &str) -> Option<u64> {
    let digits = &text[text.find(|ch: char| ch.is_ascii_digit())?..];
    let end = digits.find(|ch: char| !ch.is_ascii_digit()).unwrap_or(digits.len());
    digits[..end].parse().ok()
}

/// 一步工具调用进入上下文的体量：`(模型写出的参数, 读回的结果)`。
///
/// 服务端只留着每一步的目标预览（最多 200 字）、命令输出的开头（最多 6000 字）和一句结果说明
/// （「657 行」「31 处匹配」「新建 3245 字节」「替换 2 处」），没有完整的参数和结果，只能按
/// 工具的种类折算；每行、每处匹配、每处替换按多大算都是粗估。
fn sizes(tool: &str, target: &str, output: u64, detail: &str) -> (u64, u64) {
    let clipped = target.chars().count() >= TARGET_CAP;
    let target = text_weight(target);
    let count = number_in(detail);
    let scaled = |each: u64, extra: u64| count.map(|n| n.saturating_mul(each).min(RESULT_CAP) + extra);
    match tool.strip_prefix("baishitong_").unwrap_or(tool) {
        // 结果里带着命令原文和输出；预览占满时命令原文更长
        "exec" => (target + if clipped { 300 } else { 60 }, output + target + 120),
        "shell_start" => (target + if clipped { 300 } else { 60 }, 300),
        "shell_read" | "shell_input" | "shell_stop" => (60, output + 150),
        // 没有行数时留 0，由卡片按读文件的典型大小补
        "read_file" => (target + 60, scaled(60, 300).unwrap_or(0)),
        "read_image" | "read_attachment" | "browser_take_screenshot" | "computer_screenshot" | "computer_zoom" => (target + 60, 3000),
        "search" => (target + 80, scaled(150, 300).unwrap_or(800)),
        "list_dir" => (target + 40, scaled(60, 200).unwrap_or(800)),
        // 说明里是写入的字节数、替换的处数
        "write_file" => (target + count.unwrap_or(3200).min(WRITE_CAP), 200),
        "edit_file" => (target + 1600 * count.unwrap_or(1).clamp(1, 4), 200),
        // 回复只留了开头一小段：预览没占满就是全文，占满了按一条回复的粗估算
        "reply" => (if clipped { target.max(REPLY) } else { target }, 500),
        "checkpoint" => (target.max(8000), 200),
        // 进入工作区、取项目上下文时读回整份规则和上下文
        "switch_workspace" | "project_context" => (120, 30_000),
        "read_history" | "search_history" => (60, 8000),
        "browser_snapshot" => (60, 6000),
        _ => (target + 40, output + 100),
    }
}

/// 工具调用、思考和正文各算一步；其余的行是插件自己的提示（接入、排队）。
fn digest(row: &RawRow) -> Option<(String, Row)> {
    let kind = row.kind.as_str();
    if !matches!(kind, "tool" | "thinking" | "text") || row.at.0 <= 0 {
        return None;
    }
    let id = if row.id.is_empty() { format!("{}@{}", row.seq, row.at.0) } else { short(row.id.as_str()) };
    let (at, ended) = (row.at.0, row.ended_at.0.max(row.at.0));
    if kind != "tool" {
        let end = ended.min(at + MAX_THINKING_MS);
        return Some((id, Row { kind: kind.to_owned(), at, end, seq: row.seq, duration_ms: row.duration_ms, text: row.text.0, ..Row::default() }));
    }
    let tool = short(if row.tool.is_empty() { row.label.as_str() } else { row.tool.as_str() });
    let status = row.status.as_str();
    let (args, result) = sizes(&tool, row.target.as_str(), row.output.0, row.detail.as_str());
    // 等活干的两个调用会一直挂到有消息为止，挂着的时候模型什么也没做
    let waits = matches!(tool.strip_prefix("baishitong_").unwrap_or(&tool), "switch_workspace" | "wait_for_user");
    Some((
        id,
        Row {
            kind: "tool".into(),
            at,
            end: if waits { at } else { ended.min(at + MAX_TOOL_MS) },
            seq: row.seq,
            args,
            result,
            server: short(row.server.as_str()),
            ok: row.ok.map_or(true, |flag| flag.0) && !matches!(status, "error" | "failed"),
            running: !waits && matches!(status, "running" | "pending" | "loading"),
            tool,
            ..Row::default()
        },
    ))
}

/// 解析一次过程记录，连同它的版本号；读不懂返回 `None`。
fn page_of(body: &[u8]) -> Option<(Page, String)> {
    let raw: RawPage = serde_json::from_slice(body).ok()?;
    if raw.ok.is_some_and(|ok| !ok.0) {
        return None;
    }
    let mut page = Page { partial: raw.unchanged.0, ..Page::default() };
    for row in raw.rows.iter().filter_map(Obj::get) {
        // 占位行（没有工具名的工具行）：助手还在想，没有可记的一步
        let placeholder = row.id.as_str().ends_with("-thinking") || row.label.as_str().starts_with("助手思考中");
        if row.kind.as_str() == "tool" && row.tool.is_empty() && placeholder {
            if row.at.0 > 0 && matches!(row.status.as_str(), "running" | "pending" | "loading") {
                page.thinking_since = Some(row.at.0);
            }
            continue;
        }
        page.rows.extend(digest(row));
    }
    for (index, anchor) in raw.anchors.iter().filter_map(Obj::get).enumerate() {
        if anchor.time.0 <= 0 {
            continue;
        }
        let id = if anchor.id.is_empty() { format!("{}#{index}", anchor.time.0) } else { short(anchor.id.as_str()) };
        match anchor.role.as_str() {
            "user" => page.prompts.push((id, Prompt { ts: anchor.time.0, weight: USER_MESSAGE })),
            "agent" | "assistant" => page.replies.push((id, Prompt { ts: anchor.time.0, weight: REPLY })),
            _ => {}
        }
    }
    Some((page, raw.rev.0))
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Fault {
    /// 连不上：Cursor 没开，或者插件没在运行
    Offline,
    /// 端口上不是百事通客户版的转发
    Foreign,
    /// 没登录上，或者登录失效了
    Denied,
    /// 服务端嫌问得太勤，或者一时顾不上（429、503）
    Busy,
    /// 别的错：服务端报错、回应读不懂、读到一半断了
    Other,
}

/// 一段对话的轮询状态。
#[derive(Default)]
struct Poll {
    stamp: u64,
    active_at: i64,
    busy: bool,
    /// 上次去读过程记录的时刻（读没读成都算），和连着没读成的次数
    tried_at: i64,
    failed: u32,
    /// 上次读成的时刻、当时的指纹，和读到的版本号、大小
    read_at: i64,
    read_stamp: Option<u64>,
    rev: String,
    bytes: usize,
}

impl Poll {
    /// 离上次去读至少隔多久才再读：平时十五秒；上次没读成的，每多一次翻一倍，最多五分钟。
    fn gap(&self) -> i64 {
        (SESSION_GAP_MS << self.failed.min(5)).min(BUSY_EVERY_MAX_MS)
    }
}

struct Client {
    port: u16,
    /// 确认过端口上是百事通客户版的转发
    checked: bool,
    cookie: Option<String>,
    /// 上次登录的时刻，和连着没登录上的次数
    login_at: i64,
    login_fails: u32,
    /// 出错后停一阵：从什么时候起、停多久；连着出了几次错
    paused: (i64, i64),
    errors: u32,
    list_at: i64,
    list_tag: Option<String>,
    /// 上一次请求成功的时刻；连不上之后清零，不再相信“正在运行”的说法
    ok_at: i64,
    /// 上一次请求成功的时刻（连不上之后也留着），和现在连得怎么样：给界面看
    last_ok: i64,
    link: Link,
    polls: HashMap<String, Poll>,
}

impl Client {
    fn new(port: u16) -> Client {
        Client {
            port,
            checked: false,
            cookie: None,
            login_at: 0,
            login_fails: 0,
            paused: (0, 0),
            errors: 0,
            list_at: 0,
            list_tag: None,
            ok_at: 0,
            last_ok: 0,
            link: Link::Unknown,
            polls: HashMap::new(),
        }
    }

    /// 轮询一步：必要时登录，到点了读会话列表，再读一段到期的对话。还有从没读过的对话现在就能读时
    /// 返回 true：下一步不用等整整一拍。
    fn step(&mut self, now: i64, shared: &Mutex<Shared>) -> bool {
        if !within(now, self.paused.0, self.paused.1) {
            if let Err(fault) = self.poll(now, shared) {
                let pause = match fault {
                    Fault::Offline => {
                        (self.checked, self.ok_at) = (false, 0);
                        OFFLINE_RETRY_MS
                    }
                    Fault::Foreign => {
                        self.ok_at = 0;
                        LOGIN_EVERY_MS
                    }
                    // 下次什么时候再登录由 login_at 管
                    Fault::Denied => {
                        self.cookie = None;
                        0
                    }
                    Fault::Busy => ERROR_RETRY_MS,
                    Fault::Other => {
                        self.errors = self.errors.saturating_add(1);
                        error_pause(self.errors)
                    }
                };
                self.paused = (now, pause);
                self.link = match fault {
                    Fault::Offline => Link::Offline,
                    Fault::Foreign => Link::Foreign,
                    Fault::Denied => Link::Denied,
                    Fault::Busy => Link::Busy,
                    // 回应传到一半断掉是常事：读得好好的时候断一次不算出错
                    Fault::Other if self.link == Link::Online && self.errors < 2 => Link::Online,
                    Fault::Other => Link::Failing,
                };
            }
        }
        self.report(now, &mut lock(shared))
    }

    /// 把现在的情况交给解析器和界面；返回值同 `hurried`。
    fn report(&mut self, now: i64, shared: &mut Shared) -> bool {
        let alive = within(now, self.ok_at, ALIVE_MS);
        if !alive {
            // 解析器这时会把各段对话都当成没在运行；再连上时要一份完整的列表，好把“正在运行”补回来
            self.list_tag = None;
        }
        let hurry = self.hurried(now);
        (shared.alive, shared.soon, shared.status) = (alive, hurry, self.status(now));
        hurry
    }

    /// 和服务端说上话了。
    fn spoke(&mut self, now: i64) {
        (self.ok_at, self.last_ok, self.errors, self.link) = (now, now, 0, Link::Online);
    }

    /// 还有从没读过的对话现在就能读：刚启动的那一遍，或者列表里新冒出来的。读过一遍之后的重读不算，
    /// 照常一拍读一段。
    fn hurried(&self, now: i64) -> bool {
        !within(now, self.paused.0, self.paused.1)
            && self.cookie.is_some()
            && self.polls.values().any(|poll| poll.read_stamp.is_none() && !within(now, poll.tried_at, poll.gap()))
    }

    fn status(&self, now: i64) -> Status {
        let wait = login_wait(self.login_fails);
        let retry_at = if within(now, self.paused.0, self.paused.1) {
            self.paused.0 + self.paused.1
        } else if self.cookie.is_none() && within(now, self.login_at, wait) {
            self.login_at + wait
        } else {
            0
        };
        Status {
            link: self.link,
            sessions: self.polls.len() as u32,
            busy: self.polls.values().filter(|poll| poll.busy).count() as u32,
            pending: self.polls.values().filter(|poll| poll.read_stamp.is_none()).count() as u32,
            ok_at: self.last_ok,
            retry_at,
        }
    }

    fn poll(&mut self, now: i64, shared: &Mutex<Shared>) -> Result<(), Fault> {
        if !self.checked {
            let reply = request(self.port, "/__baishitong-chat-proxy", None, None)?;
            let probe: Option<RawProbe> = serde_json::from_slice(&reply.body).ok();
            // 端口上是别的程序：不登录，也不再发别的请求
            if reply.status != 200 || !probe.is_some_and(|probe| probe.proxy.as_str() == "baishitong-chat" && probe.customer.0) {
                return Err(Fault::Foreign);
            }
            self.checked = true;
        }
        if self.cookie.is_none() {
            if within(now, self.login_at, login_wait(self.login_fails)) {
                return Ok(());
            }
            self.login_at = now;
            match self.login() {
                Ok(cookie) => (self.cookie, self.login_fails) = (Some(cookie), 0),
                Err(fault) => {
                    self.login_fails = self.login_fails.saturating_add(1);
                    return Err(fault);
                }
            }
        }
        if !within(now, self.list_at, LIST_EVERY_MS) {
            self.list_at = now;
            self.read_list(now, shared)?;
        }
        let due = {
            let shared = lock(shared);
            self.skip_recorded(now, &shared.recorded_until);
            self.due(now)
        };
        if let Some(id) = due {
            self.read_page(&id, now, shared)?;
            self.spoke(now);
        }
        Ok(())
    }

    /// 借转发口给本机页面的自动登录换一个 Cookie：打开对话页，跟着它的跳转走到发 Cookie 的那一步。
    fn login(&self) -> Result<String, Fault> {
        let mut path = "/chat".to_owned();
        for _ in 0..MAX_REDIRECTS {
            let reply = request(self.port, &path, None, None)?;
            if let Some(cookie) = reply.cookie {
                return Ok(cookie);
            }
            match reply.location.as_deref().and_then(|location| local_path(location, self.port)) {
                Some(next) if (300..400).contains(&reply.status) => path = next,
                _ => break,
            }
        }
        Err(Fault::Denied)
    }

    fn read_list(&mut self, now: i64, shared: &Mutex<Shared>) -> Result<(), Fault> {
        let reply = request(self.port, "/api/sessions", self.cookie.as_deref(), self.list_tag.as_deref())?;
        let listed = match reply.status {
            200 => Some(sessions_of(&reply.body).ok_or(Fault::Other)?),
            // 和上次一样
            304 => None,
            401 | 403 => return Err(Fault::Denied),
            429 | 503 => return Err(Fault::Busy),
            _ => return Err(Fault::Other),
        };
        let sessions = listed.map(|listed| {
            self.list_tag = reply.etag;
            self.polls.retain(|id, _| listed.iter().any(|item| item.session.id == *id));
            let mut sessions = Vec::with_capacity(listed.len());
            for item in listed {
                let poll = self.polls.entry(item.session.id.clone()).or_default();
                (poll.stamp, poll.active_at, poll.busy) = (item.stamp, item.session.active_at, item.session.busy);
                sessions.push(item.session);
            }
            sessions
        });
        self.spoke(now);
        let mut shared = lock(shared);
        if let Some(sessions) = sessions {
            shared.pages.retain(|id, _| self.polls.contains_key(id));
            shared.list = Some(sessions);
        }
        // 列表要和“连得上”一起交出去，不等这一步后面那段过程记录读完：只交了列表的话，解析器这时
        // 来取，会把刚列出来的对话全当成没在运行，要到列表下次有变化才改得回来
        self.report(now, &mut shared);
        Ok(())
    }

    /// 开机后的第一遍：用量历史已经记到最后一条消息、又很久没动静的旧对话不用再读，当它读过。
    /// （这样的对话如果在软件没开的时候又跑了几步、却没再回话，那几步要等它下次有动静才读得到。）
    fn skip_recorded(&mut self, now: i64, recorded_until: &HashMap<String, i64>) {
        for (id, poll) in self.polls.iter_mut().filter(|(_, poll)| poll.read_stamp.is_none() && !poll.busy) {
            let recorded = recorded_until.get(id).is_some_and(|until| *until + COVERED_MS >= poll.active_at);
            if recorded && now - poll.active_at > RECENT_MS {
                poll.read_stamp = Some(poll.stamp);
            }
        }
    }

    /// 现在该读哪一段对话：列表里有新动静的，或者正在运行、到了间隔的；最久没读的优先。一样久的
    /// （刚启动时都还没读过）先读正在运行的，再读最近有动静的。
    fn due(&self, now: i64) -> Option<String> {
        self.polls
            .iter()
            .filter(|(_, poll)| !within(now, poll.tried_at, poll.gap()))
            .filter(|(_, poll)| poll.read_stamp != Some(poll.stamp) || (poll.busy && !within(now, poll.read_at, poll_every(poll.bytes))))
            .min_by_key(|&(id, poll)| (poll.tried_at, !poll.busy, std::cmp::Reverse(poll.active_at), id))
            .map(|(id, _)| id.clone())
    }

    /// 读一段对话的过程记录，交给解析器。
    fn read_page(&mut self, id: &str, now: i64, shared: &Mutex<Shared>) -> Result<(), Fault> {
        let result = self.fetch_page(id, now, shared);
        if let Some(poll) = self.polls.get_mut(id) {
            match result {
                Ok(()) => poll.failed = 0,
                // 这一段没读成，下次隔得更久再试
                Err(Fault::Other) => poll.failed = poll.failed.saturating_add(1),
                // 连不上、登录失效、服务端让等一等，都不是这一段的毛病
                Err(_) => {}
            }
        }
        result
    }

    fn fetch_page(&mut self, id: &str, now: i64, shared: &Mutex<Shared>) -> Result<(), Fault> {
        let Some(poll) = self.polls.get_mut(id) else {
            return Ok(());
        };
        // 读没读成都算去读过一次：出错的对话不会被连着重试
        poll.tried_at = now;
        let stamp = poll.stamp;
        let mut path = format!("/api/process?session={id}");
        if !poll.rev.is_empty() {
            path.push_str("&rev=");
            path.push_str(&poll.rev);
        }
        let reply = request(self.port, &path, self.cookie.as_deref(), None)?;
        let Some(poll) = self.polls.get_mut(id) else {
            return Ok(());
        };
        match reply.status {
            200 => {}
            401 | 403 => return Err(Fault::Denied),
            // 对话已经不在了：等列表把它去掉
            404 | 410 => {
                (poll.read_at, poll.read_stamp) = (now, Some(stamp));
                return Ok(());
            }
            429 | 503 => return Err(Fault::Busy),
            _ => return Err(Fault::Other),
        }
        let (page, rev) = page_of(&reply.body).ok_or(Fault::Other)?;
        (poll.read_at, poll.read_stamp) = (now, Some(stamp));
        if !page.partial {
            (poll.rev, poll.bytes) = (rev, reply.body.len());
        }
        let mut shared = lock(shared);
        match shared.pages.get_mut(id) {
            Some(known) => known.merge(page),
            None => {
                shared.pages.insert(id.to_owned(), page);
            }
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
struct Reply {
    status: u16,
    location: Option<String>,
    /// 转发口发的登录 Cookie（`名字=值`）
    cookie: Option<String>,
    etag: Option<String>,
    body: Vec<u8>,
}

fn bad(text: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, text)
}

/// 发一个 GET。只连本机回环地址上的这一个端口。
fn request(port: u16, path: &str, cookie: Option<&str>, tag: Option<&str>) -> Result<Reply, Fault> {
    if !safe_path(path) {
        return Err(Fault::Other);
    }
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT).map_err(|_| Fault::Offline)?;
    exchange(&mut stream, port, path, cookie, tag).map_err(|_| Fault::Other)
}

fn exchange(stream: &mut TcpStream, port: u16, path: &str, cookie: Option<&str>, tag: Option<&str>) -> io::Result<Reply> {
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    let mut head = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: application/json, text/html;q=0.9\r\nAccept-Encoding: gzip\r\nUser-Agent: Foxes-Token-Watch\r\nConnection: close\r\n"
    );
    for (name, value) in [("Cookie", cookie), ("If-None-Match", tag)] {
        if let Some(value) = value {
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    read_reply(stream)
}

fn fill(stream: &mut impl Read, data: &mut Vec<u8>) -> io::Result<usize> {
    let mut chunk = [0u8; 32 * 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(read) => {
                data.extend_from_slice(&chunk[..read]);
                return Ok(read);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

/// 转发口发的登录 Cookie：只认它那一个名字，只留 `名字=值`。
fn cookie_pair(header: &str) -> Option<String> {
    let pair = header.split(';').next()?.trim();
    let (name, value) = pair.split_once('=')?;
    (name == COOKIE && !value.is_empty() && pair.len() <= 1024 && pair.bytes().all(|b| b.is_ascii_graphic())).then(|| pair.to_owned())
}

/// 跳转的去向：只跟本站的路径，指向别处的一律不跟。
fn local_path(location: &str, port: u16) -> Option<String> {
    let path = [format!("http://127.0.0.1:{port}"), format!("http://localhost:{port}")]
        .iter()
        .find_map(|origin| location.strip_prefix(origin.as_str()))
        .unwrap_or(location);
    safe_path(path).then(|| path.to_owned())
}

fn read_reply(stream: &mut impl Read) -> io::Result<Reply> {
    let mut data: Vec<u8> = Vec::with_capacity(16 * 1024);
    let head_end = loop {
        if let Some(at) = memchr::memmem::find(&data, b"\r\n\r\n") {
            break at;
        }
        if data.len() > MAX_HEAD {
            return Err(bad("response head too long"));
        }
        if fill(stream, &mut data)? == 0 {
            return Err(bad("connection closed early"));
        }
    };
    let head = std::str::from_utf8(&data[..head_end]).map_err(|_| bad("response head is not text"))?;
    let mut lines = head.split("\r\n");
    let status = lines.next().and_then(|line| line.split(' ').nth(1)).and_then(|code| code.parse::<u16>().ok()).ok_or_else(|| bad("no status line"))?;
    let mut reply = Reply { status, ..Reply::default() };
    let (mut length, mut chunked, mut gzip) = (None, false, false);
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        let is = |wanted: &str| name.eq_ignore_ascii_case(wanted);
        if is("content-length") {
            length = value.parse::<usize>().ok();
        } else if is("transfer-encoding") {
            chunked = value.to_ascii_lowercase().contains("chunked");
        } else if is("content-encoding") {
            match value.to_ascii_lowercase().as_str() {
                "gzip" | "x-gzip" => gzip = true,
                "" | "identity" => {}
                _ => return Err(bad("unsupported content encoding")),
            }
        } else if is("location") {
            reply.location = Some(value.to_owned());
        } else if is("etag") {
            reply.etag = (value.len() <= 200 && value.bytes().all(|b| b.is_ascii_graphic())).then(|| value.to_owned());
        } else if is("set-cookie") {
            if let Some(pair) = cookie_pair(value) {
                reply.cookie = Some(pair);
            }
        }
    }
    let mut body = data.split_off(head_end + 4);
    if status == 204 || status == 304 || (100..200).contains(&status) {
        body.clear();
    } else if chunked {
        body = unchunk(stream, body)?;
    } else if let Some(length) = length {
        if length > MAX_BODY {
            return Err(bad("response too large"));
        }
        while body.len() < length {
            if fill(stream, &mut body)? == 0 {
                return Err(bad("connection closed early"));
            }
        }
        body.truncate(length);
    } else {
        while fill(stream, &mut body)? > 0 {
            if body.len() > MAX_BODY {
                return Err(bad("response too large"));
            }
        }
    }
    if gzip && !body.is_empty() {
        let mut plain = Vec::with_capacity(body.len().saturating_mul(4).min(MAX_BODY));
        flate2::read::GzDecoder::new(body.as_slice()).take(MAX_BODY as u64 + 1).read_to_end(&mut plain)?;
        if plain.len() > MAX_BODY {
            return Err(bad("response too large"));
        }
        body = plain;
    }
    reply.body = body;
    Ok(reply)
}

/// 把分块传输的正文接起来。`raw` 是跟着响应头已经读到的部分。
fn unchunk(stream: &mut impl Read, mut raw: Vec<u8>) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let line_end = loop {
            if let Some(at) = memchr::memmem::find(&raw, b"\r\n") {
                break at;
            }
            if raw.len() > 1024 {
                return Err(bad("bad chunk size"));
            }
            if fill(stream, &mut raw)? == 0 {
                return Err(bad("connection closed early"));
            }
        };
        let size = std::str::from_utf8(&raw[..line_end])
            .ok()
            .and_then(|line| line.split(';').next())
            .and_then(|size| usize::from_str_radix(size.trim(), 16).ok())
            .ok_or_else(|| bad("bad chunk size"))?;
        if size == 0 {
            // 结尾块；后面的尾部字段用不着
            return Ok(body);
        }
        if size > MAX_BODY || body.len() + size > MAX_BODY {
            return Err(bad("response too large"));
        }
        let (from, to) = (line_end + 2, line_end + 2 + size);
        while raw.len() < to + 2 {
            if fill(stream, &mut raw)? == 0 {
                return Err(bad("connection closed early"));
            }
        }
        body.extend_from_slice(&raw[from..to]);
        raw.drain(..to + 2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::BufRead;
    use std::net::TcpListener;

    #[derive(Clone, Debug)]
    struct Seen {
        method: String,
        path: String,
        signed: bool,
        tag: String,
    }

    /// 一个只会几句话的假转发口：记下收到的每个请求，按 `answer` 回话。
    struct Fake {
        port: u16,
        seen: Arc<Mutex<Vec<Seen>>>,
    }

    fn serve(answer: impl Fn(&Seen) -> Vec<u8> + Send + 'static) -> Fake {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        thread::spawn(move || {
            for stream in listener.incoming().take(64) {
                let Ok(mut stream) = stream else { continue };
                let mut reader = io::BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let mut words = line.split_whitespace();
                let mut request =
                    Seen { method: words.next().unwrap_or("").into(), path: words.next().unwrap_or("").into(), signed: false, tag: String::new() };
                loop {
                    line.clear();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if lower.starts_with("cookie:") {
                        request.signed = line.trim().ends_with("mcprust_cust=secret-value");
                    } else if let Some(tag) = lower.strip_prefix("if-none-match:") {
                        request.tag = tag.trim().to_owned();
                    }
                }
                let reply = answer(&request);
                log.lock().unwrap().push(request);
                let _ = stream.write_all(&reply);
            }
        });
        Fake { port, seen }
    }

    fn plain(status: &str, headers: &[&str], body: &[u8]) -> Vec<u8> {
        let mut out = format!("HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n", body.len()).into_bytes();
        for header in headers {
            out.extend_from_slice(header.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(body);
        out
    }

    /// 压缩后分块发出，和真的转发口一样。
    fn packed(headers: &[&str], body: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(body).unwrap();
        let zipped = encoder.finish().unwrap();
        let mut out = b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Encoding: gzip\r\nTransfer-Encoding: chunked\r\n".to_vec();
        for header in headers {
            out.extend_from_slice(header.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"\r\n");
        for chunk in zipped.chunks(37) {
            out.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
            out.extend_from_slice(chunk);
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"0\r\n\r\n");
        out
    }

    const PROBE: &[u8] = br#"{"proxy":"baishitong-chat","remote":"https://example.invalid","customer":true}"#;

    fn list_json(t: i64) -> String {
        json!({"ok": true, "scope": "all", "sessions": [
            {"sessionId": "card-1", "name": "修网关", "model": "Claude Fable 5.2 Max Fast", "modelId": "claude-fable-5-2-max-fast", "maxMode": true,
             "state": "active", "agentPhase": "busy", "ended": false, "createdAt": t, "lastMessageAt": t + 1000, "lastAgentAt": t + 1000,
             "historyCount": 2, "contextBytes": 900, "workspaceRoot": "F:\\work"},
            {"sessionId": "card-2", "name": "旧对话", "model": "Claude Fable 5.2", "state": "released", "agentPhase": "idle", "ended": true,
             "createdAt": t - 30 * DAY, "lastMessageAt": t - 29 * DAY, "lastAgentAt": t - 29 * DAY, "historyCount": 8},
            {"sessionId": "../evil", "name": "x"},
            "not an object"
        ]})
        .to_string()
    }

    fn page_json(t: i64) -> String {
        json!({"ok": true, "sessionId": "card-1", "rev": 7, "rows": [
            {"kind": "note", "id": "p-1", "seq": t + 100, "at": t + 100, "endedAt": t + 100, "label": "助手已接入", "text": "x", "rev": 3},
            {"kind": "tool", "id": "t-0", "seq": t + 2000, "at": t + 2000, "endedAt": t + 3_000_000, "tool": "baishitong_switch_workspace", "server": "baishitong-workspace",
             "label": "进入工作区", "target": "", "status": "completed", "ok": true, "detail": "没有待办"},
            {"kind": "tool", "id": "t-a", "seq": t + 5000, "at": t + 5000, "endedAt": t + 5400, "tool": "baishitong_read_file", "server": "baishitong-workspace",
             "label": "读取", "target": "src/main.rs", "status": "completed", "ok": true, "detail": "200 行", "rev": "3"},
            {"kind": "thinking", "id": "k-1", "seq": t + 8000, "at": t + 8000, "endedAt": t + 18_000, "durationMs": 10_000, "text": "想".repeat(100), "rev": 3},
            {"kind": "tool", "id": "t-b", "seq": t + 20_000, "at": t + 20_000, "endedAt": t + 21_000, "tool": "baishitong_exec", "server": "baishitong-workspace",
             "label": "运行", "target": "cargo test", "status": "completed", "ok": false, "detail": "退出码 1", "output": "x".repeat(1000)},
            {"kind": "tool", "id": "t-c-thinking", "seq": t + 30_000, "at": t + 30_000, "endedAt": t + 30_000, "tool": "", "server": "cursor-cloud",
             "label": "助手思考中 · 已 12 秒", "status": "running", "ok": true, "detail": ""}
        ], "anchors": [
            {"id": "initial-1", "role": "user", "time": t + 1000},
            {"id": "relay-2", "role": "agent", "time": t + 40_000},
            {"id": "sys-3", "role": "system", "time": t + 41_000}
        ]})
        .to_string()
    }

    #[test]
    fn reads_sessions_and_rows_through_the_local_forward() {
        let t = 1_800_000_000_000i64;
        let (list, page) = (list_json(t), page_json(t));
        let fake = serve(move |request| match (request.path.as_str(), request.signed) {
            ("/__baishitong-chat-proxy", _) => plain("200 OK", &[], PROBE),
            ("/chat", false) => plain("302 Found", &["Location: /c/login"], b""),
            ("/c/login", false) => plain("302 Found", &["Location: /c/login?code=one-time"], b""),
            ("/c/login?code=one-time", false) => {
                plain("302 Found", &["Location: /", "Set-Cookie: mcprust_cust=secret-value; HttpOnly; Path=/; SameSite=Lax"], b"")
            }
            (_, false) => plain("401 Unauthorized", &[], br#"{"ok":false}"#),
            ("/api/sessions", true) if request.tag == "w/\"v1\"" => plain("304 Not Modified", &[], b""),
            ("/api/sessions", true) => packed(&["ETag: W/\"v1\""], list.as_bytes()),
            ("/api/process?session=card-1", true) => packed(&[], page.as_bytes()),
            ("/api/process?session=card-1&rev=7", true) => plain(
                "200 OK",
                &[],
                br#"{"ok":true,"sessionId":"card-1","rev":7,"unchanged":true,"anchors":[{"id":"initial-1","role":"user","time":1800000001000},{"id":"user-3","role":"user","time":1800000050000}]}"#,
            ),
            _ => plain("404 Not Found", &[], b"{}"),
        });

        let shared = Mutex::new(Shared::default());
        // 用量历史已经把旧对话记到了它最后一条消息之后
        lock(&shared).recorded_until.insert("card-2".into(), t - 29 * DAY + 5000);
        let mut client = Client::new(fake.port);
        assert!(!client.step(t + 60_000, &shared), "nothing is left unread");
        {
            let mut state = lock(&shared);
            assert!(state.alive && !state.soon);
            let status = &state.status;
            assert_eq!(
                (status.link, status.sessions, status.busy, status.pending, status.ok_at, status.retry_at),
                (Link::Online, 2, 1, 0, t + 60_000, 0)
            );
            let list = state.list.take().expect("the session list");
            assert_eq!(list.len(), 2, "ids that could not be card names are dropped");
            assert_eq!((list[0].id.as_str(), list[0].busy, list[0].max_mode, list[0].active_at), ("card-1", true, true, t + 1000));
            assert_eq!((list[0].model.as_str(), list[0].cwd.as_str(), list[0].created_at), ("claude-fable-5-2-max-fast", r"F:\work", t));
            assert_eq!((list[1].model.as_str(), list[1].busy), ("Claude Fable 5.2", false));
            let page = state.pages.remove("card-1").expect("rows of the running session");
            assert!(!page.partial);
            assert_eq!(
                page.rows.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
                ["t-0", "t-a", "k-1", "t-b"],
                "notes and the thinking placeholder are no steps"
            );
            let (entered, read, thought, run) = (&page.rows[0].1, &page.rows[1].1, &page.rows[2].1, &page.rows[3].1);
            assert_eq!((entered.at, entered.end), (t + 2000, t + 2000), "a call that waits for work is no long step");
            assert_eq!((read.tool.as_str(), read.at, read.end, read.ok), ("baishitong_read_file", t + 5000, t + 5400, true));
            assert_eq!(read.result, 200 * 60 + 300);
            assert_eq!((thought.kind.as_str(), thought.text, thought.duration_ms, thought.end), ("thinking", 500, 10_000, t + 18_000));
            assert!(!run.ok && run.result >= 1000);
            assert_eq!(page.thinking_since, Some(t + 30_000));
            assert_eq!((page.prompts.len(), page.replies.len()), (1, 1));
            assert!(state.pages.is_empty());
        }

        // 两秒后：列表没到点，旧对话历史里已经记全，什么都不发
        client.step(t + 62_000, &shared);
        // 一分钟后：列表带着 ETag 去问，过程记录带着 rev 去问
        client.step(t + 125_000, &shared);
        let seen = fake.seen.lock().unwrap().clone();
        assert!(seen.iter().all(|request| request.method == "GET"), "nothing but GET is ever sent");
        let paths: Vec<&str> = seen.iter().map(|request| request.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "/__baishitong-chat-proxy",
                "/chat",
                "/c/login",
                "/c/login?code=one-time",
                "/api/sessions",
                "/api/process?session=card-1",
                "/api/sessions",
                "/api/process?session=card-1&rev=7"
            ]
        );
        assert!(seen[..4].iter().all(|request| !request.signed) && seen[4..].iter().all(|request| request.signed));
        let update = lock(&shared).pages.remove("card-1").expect("the unchanged answer still carries message times");
        assert!(update.partial && update.rows.is_empty());
        assert_eq!(update.prompts.len(), 2);
        assert!(lock(&shared).list.is_none(), "an unchanged list is not handed over again");
    }

    #[test]
    fn hands_over_to_one_cursor_profile_only() {
        let feed = Feed { shared: Arc::new(Mutex::new(Shared::default())) };
        let (first, second) = (Path::new("a/globalStorage"), Path::new("b/globalStorage"));
        *lock(&feed.shared) =
            Shared { alive: true, soon: true, list: Some(vec![Session { id: "card-1".into(), ..Session::default() }]), ..Shared::default() };
        let batch = feed.take(first, 1000, None);
        assert!(batch.alive && batch.soon && batch.list.is_some());
        lock(&feed.shared).list = Some(Vec::new());
        let other = feed.take(second, 2000, None);
        assert!(!other.alive && !other.soon && other.list.is_none(), "a second profile gets nothing");
        assert!(feed.take(first, 3000, None).list.is_some());
        assert_eq!(lock(&feed.shared).wanted_ms, 3000);
    }

    #[test]
    fn keeps_rows_when_an_unchanged_answer_follows_before_the_page_is_taken() {
        let row = |id: &str| (id.to_owned(), Row { kind: "tool".into(), at: 1, end: 2, ..Row::default() });
        let mut page = Page { rows: vec![row("a")], ..Page::default() };
        page.merge(Page { partial: true, prompts: vec![("u".into(), Prompt { ts: 5, weight: USER_MESSAGE })], ..Page::default() });
        assert_eq!((page.rows.len(), page.prompts.len(), page.partial), (1, 1, false));
        page.merge(Page { rows: vec![row("a"), row("b")], ..Page::default() });
        assert_eq!((page.rows.len(), page.prompts.len()), (2, 0));
    }

    #[test]
    fn stays_away_from_a_port_that_is_not_the_forward() {
        let fake = serve(|_| plain("200 OK", &[], br#"{"hello":"world"}"#));
        let shared = Mutex::new(Shared { alive: true, ..Shared::default() });
        let mut client = Client::new(fake.port);
        let t = 1_800_000_000_000i64;
        client.step(t, &shared);
        client.step(t + 10_000, &shared);
        assert_eq!(fake.seen.lock().unwrap().len(), 1, "no login is attempted on a foreign port");
        assert!(!lock(&shared).alive);
        assert_eq!(lock(&shared).status.link, Link::Foreign);
    }

    #[test]
    fn waits_before_logging_in_again() {
        let fake = serve(|request| match request.path.as_str() {
            "/__baishitong-chat-proxy" => plain("200 OK", &[], PROBE),
            // 插件还没登录服务端：对话页打得开，但不发 Cookie
            _ => plain("200 OK", &[], b"<html></html>"),
        });
        let shared = Mutex::new(Shared::default());
        let mut client = Client::new(fake.port);
        let t = 1_800_000_000_000i64;
        let logins = || fake.seen.lock().unwrap().iter().filter(|request| request.path == "/chat").count();
        assert!(!client.step(t, &shared));
        assert_eq!(logins(), 1);
        let status = lock(&shared).status.clone();
        assert_eq!((status.link, status.ok_at, status.retry_at), (Link::Denied, 0, t + 30 * SECOND), "the page can say why, and when it tries again");
        // 头几次没登录上，隔半分钟、一分钟、两分钟再试；之后每五分钟一次
        let mut at = t;
        for (nth, wait) in [30 * SECOND, MINUTE, 2 * MINUTE, LOGIN_EVERY_MS, LOGIN_EVERY_MS].into_iter().enumerate() {
            client.step(at + 2000, &shared);
            client.step(at + wait - 1, &shared);
            assert_eq!(logins(), nth + 1, "the plugin's login budget is shared with the user's own page");
            at += wait;
            client.step(at, &shared);
            assert_eq!(logins(), nth + 2);
        }
        assert!(!lock(&shared).alive);
    }

    #[test]
    fn does_not_log_in_again_right_after_its_cookie_is_refused() {
        let fake = serve(|request| match request.path.as_str() {
            "/__baishitong-chat-proxy" => plain("200 OK", &[], PROBE),
            "/chat" => plain("302 Found", &["Location: /", "Set-Cookie: mcprust_cust=secret-value; HttpOnly"], b""),
            _ => plain("401 Unauthorized", &[], b"{}"),
        });
        let shared = Mutex::new(Shared::default());
        let mut client = Client::new(fake.port);
        let t = 1_800_000_000_000i64;
        let logins = || fake.seen.lock().unwrap().iter().filter(|request| request.path == "/chat").count();
        client.step(t, &shared);
        for later in [2000, 30 * SECOND, MINUTE, LOGIN_EVERY_MS - 1] {
            client.step(t + later, &shared);
        }
        assert_eq!(logins(), 1, "a login that worked is not repeated for five minutes");
        client.step(t + LOGIN_EVERY_MS, &shared);
        assert_eq!(logins(), 2);
    }

    #[test]
    fn a_reply_cut_short_does_not_hold_up_the_other_conversations() {
        let t = 1_800_000_000_000i64;
        let session = |id: &str| json!({"sessionId": id, "state": "active", "agentPhase": "busy", "createdAt": t, "lastMessageAt": t, "lastAgentAt": t});
        let list = json!({"ok": true, "sessions": [session("card-1"), session("card-2")]}).to_string();
        let page = page_json(t);
        let fake = serve(move |request| match request.path.as_str() {
            "/__baishitong-chat-proxy" => plain("200 OK", &[], PROBE),
            "/chat" => plain("302 Found", &["Location: /", "Set-Cookie: mcprust_cust=secret-value; HttpOnly"], b""),
            "/api/sessions" => plain("200 OK", &[], list.as_bytes()),
            // 说好了九百多字节，发了几个就断了
            "/api/process?session=card-1" => b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 999\r\n\r\n[1, 2,".to_vec(),
            "/api/process?session=card-2" => plain("200 OK", &[], page.as_bytes()),
            _ => plain("404 Not Found", &[], b"{}"),
        });
        let shared = Mutex::new(Shared::default());
        let mut client = Client::new(fake.port);
        let reads = |id: &str| fake.seen.lock().unwrap().iter().filter(|request| request.path.ends_with(id)).count();

        assert!(!client.step(t, &shared), "no hurry while it eases off");
        assert_eq!((reads("card-1"), reads("card-2")), (1, 0));
        assert!(lock(&shared).alive, "the list did arrive");
        assert_eq!(lock(&shared).status.link, Link::Online, "one reply cut short is no failure");
        // 停几秒就接着读下一段
        client.step(t + 2000, &shared);
        assert_eq!((reads("card-1"), reads("card-2")), (1, 0));
        client.step(t + 5000, &shared);
        assert_eq!((reads("card-1"), reads("card-2")), (1, 1));
        assert!(lock(&shared).pages.contains_key("card-2"));
        // 断掉的那一段半分钟后再试；又断了，这回隔一分钟
        client.step(t + 29_000, &shared);
        assert_eq!(reads("card-1"), 1);
        client.step(t + 30_000, &shared);
        assert_eq!(reads("card-1"), 2);
        client.step(t + 60_000, &shared);
        client.step(t + 89_000, &shared);
        assert_eq!(reads("card-1"), 2);
        client.step(t + 90_000, &shared);
        assert_eq!(reads("card-1"), 3);
        assert!(!lock(&shared).pages.contains_key("card-1"));
    }

    #[test]
    fn reads_the_running_conversations_first_and_one_right_after_another() {
        let t = 1_800_000_000_000i64;
        let session = |id: &str, phase: &str, at: i64| {
            json!({"sessionId": id, "state": "active", "agentPhase": phase, "createdAt": t - DAY, "lastMessageAt": at, "lastAgentAt": at})
        };
        let list = json!({"ok": true, "sessions": [
            session("card-1", "idle", t - 3 * MINUTE),
            session("card-2", "busy", t - 2 * MINUTE),
            session("card-3", "working", t - MINUTE),
            session("card-4", "idle", t - 4 * MINUTE),
        ]})
        .to_string();
        let page = page_json(t);
        let fake = serve(move |request| match request.path.as_str() {
            "/__baishitong-chat-proxy" => plain("200 OK", &[], PROBE),
            "/chat" => plain("302 Found", &["Location: /", "Set-Cookie: mcprust_cust=secret-value; HttpOnly"], b""),
            "/api/sessions" => plain("200 OK", &[], list.as_bytes()),
            path if path.starts_with("/api/process?session=") => plain("200 OK", &[], page.as_bytes()),
            _ => plain("404 Not Found", &[], b"{}"),
        });
        let shared = Mutex::new(Shared::default());
        let mut client = Client::new(fake.port);
        // 列表一到就交出去，里面有每段对话最后有动静的时刻；还有没读过的对话，不用等整整一拍
        assert!(client.step(t, &shared));
        {
            let state = lock(&shared);
            assert!(state.soon);
            let listed: Vec<(bool, i64)> = state.list.as_ref().expect("the session list").iter().map(|session| (session.busy, session.active_at)).collect();
            assert_eq!(listed, [(false, t - 3 * MINUTE), (true, t - 2 * MINUTE), (true, t - MINUTE), (false, t - 4 * MINUTE)]);
            assert_eq!((state.status.link, state.status.sessions, state.status.busy, state.status.pending), (Link::Online, 4, 2, 3));
        }
        assert!(client.step(t + 400, &shared));
        assert!(client.step(t + 800, &shared));
        assert!(!client.step(t + 1200, &shared), "every conversation has been read once");
        let read: Vec<String> =
            fake.seen.lock().unwrap().iter().filter_map(|request| request.path.strip_prefix("/api/process?session=").map(str::to_owned)).collect();
        assert_eq!(read, ["card-3", "card-2", "card-1", "card-4"], "those at work first, then the most recently active");
        {
            let state = lock(&shared);
            assert!(!state.soon);
            assert_eq!((state.status.pending, state.status.ok_at, state.status.retry_at), (0, t + 1200, 0));
        }
        // 读过一遍之后回到平时的节奏：正在运行的对话到了间隔才重读
        assert!(!client.step(t + 1600, &shared));
        assert_eq!(fake.seen.lock().unwrap().len(), 7, "the probe, the login, the list and four pages");
    }

    #[test]
    fn the_list_comes_with_the_word_that_the_service_answers() {
        let t = 1_800_000_000_000i64;
        let (list, page) = (list_json(t), page_json(t));
        let shared = Arc::new(Mutex::new(Shared::default()));
        // 读一段过程记录要一两秒，解析器可能正好在这时来取：记下那一刻桌上放着什么
        let found = Arc::new(Mutex::new(None));
        let (table, note) = (shared.clone(), found.clone());
        let fake = serve(move |request| match (request.path.as_str(), request.signed) {
            ("/__baishitong-chat-proxy", _) => plain("200 OK", &[], PROBE),
            ("/chat", false) => plain("302 Found", &["Location: /", "Set-Cookie: mcprust_cust=secret-value; Path=/"], b""),
            ("/api/sessions", true) => plain("200 OK", &[], list.as_bytes()),
            ("/api/process?session=card-1", true) => {
                let state = lock(&table);
                *lock(&note) = Some((state.list.as_ref().map(Vec::len), state.alive, state.soon, state.status.clone()));
                plain("200 OK", &[], page.as_bytes())
            }
            _ => plain("404 Not Found", &[], b"{}"),
        });
        let mut client = Client::new(fake.port);
        client.step(t + 60_000, &shared);
        let (listed, alive, soon, status) = lock(&found).take().expect("the rows of the running conversation were asked for");
        assert_eq!((listed, alive), (Some(2), true), "a list handed over alone reads as: nothing is running");
        assert!(soon, "the parser is told to come for it at once");
        assert_eq!((status.link, status.sessions, status.busy, status.pending), (Link::Online, 2, 1, 2));
    }

    #[test]
    fn asks_for_the_whole_list_again_after_losing_touch() {
        let t = 1_800_000_000_000i64;
        let list = list_json(t);
        let fake = serve(move |request| match (request.path.as_str(), request.signed) {
            ("/__baishitong-chat-proxy", _) => plain("200 OK", &[], PROBE),
            ("/chat", false) => plain("302 Found", &["Location: /", "Set-Cookie: mcprust_cust=secret-value; Path=/"], b""),
            ("/api/sessions", true) if request.tag == "w/\"v1\"" => plain("304 Not Modified", &[], b""),
            ("/api/sessions", true) => plain("200 OK", &["ETag: W/\"v1\""], list.as_bytes()),
            _ => plain("404 Not Found", &[], b"{}"),
        });
        let shared = Mutex::new(Shared::default());
        let mut client = Client::new(fake.port);
        client.step(t, &shared);
        {
            let mut state = lock(&shared);
            assert!(state.alive && state.list.take().is_some());
        }
        // 列表没变：不再交一遍
        client.step(t + 30 * SECOND, &shared);
        {
            let state = lock(&shared);
            assert!(state.alive && state.list.is_none());
        }
        // 停了很久没问成：解析器已经把各段对话都当成没在运行
        client.paused = (t + 40 * SECOND, 10 * MINUTE);
        client.step(t + 40 * SECOND + ALIVE_MS, &shared);
        assert!(!lock(&shared).alive);
        client.paused = (0, 0);
        client.step(t + 41 * SECOND + ALIVE_MS, &shared);
        {
            let state = lock(&shared);
            assert!(state.alive && state.list.as_ref().is_some_and(|list| list.len() == 2), "an unchanged list would leave them all at rest");
        }
        let seen = fake.seen.lock().unwrap().clone();
        let tags: Vec<&str> = seen.iter().filter(|request| request.path == "/api/sessions").map(|request| request.tag.as_str()).collect();
        assert_eq!(tags, ["", "w/\"v1\"", ""]);
    }

    #[test]
    fn the_status_goes_to_the_page_as_plain_words_and_numbers() {
        let status = Status { link: Link::Offline, sessions: 3, busy: 2, pending: 1, ok_at: 5, retry_at: 9 };
        assert_eq!(serde_json::to_value(&status).unwrap(), json!({"link": "offline", "sessions": 3, "busy": 2, "pending": 1, "okAt": 5, "retryAt": 9}));
        assert_eq!(serde_json::to_value(Link::Failing).unwrap(), json!("failing"));
    }

    #[test]
    fn eases_off_when_the_service_asks_for_it() {
        let fake = serve(|request| match request.path.as_str() {
            "/__baishitong-chat-proxy" => plain("200 OK", &[], PROBE),
            "/chat" => plain("302 Found", &["Location: /", "Set-Cookie: mcprust_cust=secret-value; HttpOnly"], b""),
            _ => plain("429 Too Many Requests", &[], b"{}"),
        });
        let shared = Mutex::new(Shared::default());
        let mut client = Client::new(fake.port);
        let t = 1_800_000_000_000i64;
        client.step(t, &shared);
        let status = lock(&shared).status.clone();
        assert_eq!((status.link, status.retry_at), (Link::Busy, t + MINUTE));
        client.step(t + 30 * SECOND, &shared);
        assert_eq!(fake.seen.lock().unwrap().len(), 3, "nothing is asked for a minute");
        client.step(t + MINUTE, &shared);
        assert_eq!(fake.seen.lock().unwrap().len(), 4);
    }

    #[test]
    fn spaces_retries_out() {
        assert_eq!([1, 2, 3, 9].map(error_pause), [5 * SECOND, 20 * SECOND, MINUTE, MINUTE]);
        assert_eq!([0, 1, 2, 3, 4, 9].map(login_wait), [5 * MINUTE, 30 * SECOND, MINUTE, 2 * MINUTE, 5 * MINUTE, 5 * MINUTE]);
        let gap = |failed: u32| Poll { failed, ..Poll::default() }.gap();
        assert_eq!([0, 1, 2, 3, 4, 5, 99].map(gap), [15 * SECOND, 30 * SECOND, MINUTE, 2 * MINUTE, 4 * MINUTE, 5 * MINUTE, 5 * MINUTE]);
    }

    #[test]
    fn backs_off_when_nothing_listens() {
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port();
        let shared = Mutex::new(Shared { alive: true, ..Shared::default() });
        let mut client = Client::new(port);
        let t = 1_800_000_000_000i64;
        client.step(t, &shared);
        assert!(!lock(&shared).alive);
        let status = lock(&shared).status.clone();
        assert_eq!((status.link, status.sessions, status.retry_at), (Link::Offline, 0, t + OFFLINE_RETRY_MS));
        assert!(within(t + OFFLINE_RETRY_MS - 1, client.paused.0, client.paused.1));
        assert!(!within(t + OFFLINE_RETRY_MS, client.paused.0, client.paused.1));
    }

    #[test]
    fn reads_replies_of_every_framing() {
        let reply = read_reply(&mut &b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nETag: W/\"a\"\r\n\r\nhello, and more"[..]).unwrap();
        assert_eq!((reply.status, reply.body.as_slice(), reply.etag.as_deref()), (200, &b"hello"[..], Some("W/\"a\"")));
        let reply = read_reply(&mut &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;x=y\r\nde\r\n0\r\n\r\n"[..]).unwrap();
        assert_eq!(reply.body, b"abcde");
        let reply = read_reply(&mut &b"HTTP/1.1 200 OK\r\n\r\nuntil the end"[..]).unwrap();
        assert_eq!(reply.body, b"until the end");
        let reply = read_reply(&mut packed(&[], "压缩过的正文".as_bytes()).as_slice()).unwrap();
        assert_eq!(reply.body, "压缩过的正文".as_bytes());
        let reply = read_reply(&mut &b"HTTP/1.1 304 Not Modified\r\nContent-Length: 99\r\n\r\n"[..]).unwrap();
        assert_eq!((reply.status, reply.body.len()), (304, 0));
        let reply = read_reply(&mut &b"HTTP/1.1 302 Found\r\nlocation: /c/login?code=x\r\nSet-Cookie: other=1\r\nset-cookie: mcprust_cust=abc; HttpOnly\r\nContent-Length: 0\r\n\r\n"[..]).unwrap();
        assert_eq!((reply.location.as_deref(), reply.cookie.as_deref()), (Some("/c/login?code=x"), Some("mcprust_cust=abc")));
        assert!(read_reply(&mut &b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nshort"[..]).is_err());
        assert!(read_reply(&mut &b"HTTP/1.1 200 OK\r\nContent-Encoding: br\r\n\r\nx"[..]).is_err());
    }

    #[test]
    fn follows_redirects_only_within_the_forward() {
        assert_eq!(local_path("/c/login?code=a-b_c", 9791).as_deref(), Some("/c/login?code=a-b_c"));
        assert_eq!(local_path("http://127.0.0.1:9791/", 9791).as_deref(), Some("/"));
        assert_eq!(local_path("https://example.invalid/c/login", 9791), None);
        assert_eq!(local_path("//example.invalid/", 9791), None);
        assert_eq!(local_path("/a b", 9791), None);
        assert_eq!(cookie_pair("mcprust_chat=abc; Path=/"), None);
    }

    #[test]
    fn an_assistant_between_two_calls_is_still_at_work() {
        let read = |phase: &str, ended: bool| {
            let body = json!({"ok": true, "sessions": [
                {"sessionId": "card-1", "state": "active", "agentPhase": phase, "ended": ended, "lastMessageAt": 5, "lastAgentAt": 5}
            ]})
            .to_string();
            let listed = sessions_of(body.as_bytes()).unwrap();
            (listed[0].session.busy, listed[0].stamp)
        };
        let (busy, working, idle) = (read("busy", false), read("working", false), read("idle", false));
        assert!(busy.0 && working.0 && !idle.0);
        assert_eq!(busy.1, working.1, "going back and forth between the two is no news");
        assert_ne!(busy.1, idle.1);
        assert!(!read("busy", true).0, "nobody is at work on an ended conversation");
    }

    #[test]
    fn sizes_steps_from_what_the_service_keeps() {
        let path = "src/sources/mod.rs";
        assert_eq!(sizes("baishitong_read_file", path, 0, "120 行"), (18 + 60, 120 * 60 + 300));
        assert_eq!(sizes("baishitong_read_file", path, 0, "657 行").1, RESULT_CAP + 300, "longer results are cut by the gateway");
        assert_eq!(sizes("baishitong_read_file", path, 0, ""), (78, 0), "left to the card's typical size");
        assert_eq!(sizes("baishitong_read_file", path, 0, "99999 行").1, RESULT_CAP + 300);
        assert_eq!(sizes("baishitong_search", "needle", 0, "31 处匹配").1, 31 * 150 + 300);
        assert_eq!(sizes("baishitong_write_file", path, 0, "新建 3245 字节").0, 18 + 3245);
        assert_eq!(sizes("baishitong_edit_file", path, 0, "替换 2 处").0, 18 + 3200);
        assert_eq!(sizes("baishitong_exec", "cargo test", 2000, "退出码 0"), (70, 2130));
        let (brief, long) = ("好".repeat(20), "好".repeat(200));
        assert_eq!(sizes("baishitong_reply", &brief, 0, "已写入对话").0, 100, "a short reply is kept whole");
        assert_eq!(sizes("baishitong_reply", &long, 0, "已写入对话").0, REPLY, "a clipped preview is not the whole reply");
        assert_eq!(sizes("run-info", "", 0, ""), (40, 100));
        assert_eq!(poll_every(100 * 1024), MINUTE);
        assert_eq!(poll_every(700 * 1024), 2 * MINUTE);
        assert_eq!(poll_every(50 * 1024 * 1024), 5 * MINUTE);
    }
}
