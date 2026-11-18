//! 把各文件的解析结果汇总成面板要用的数字：分时段用量、正在运行的对话、网络状况。

use std::collections::{HashMap, HashSet};

use serde::Serialize;

use crate::intake::Recorded;
use crate::model::{Agent, Effort, NetKind, Phase, RateWindow, UsageEvent};
use crate::sources::baishitong::Status as FeedStatus;
use crate::sources::{project_name, FileEntry};
use crate::store::{ClaudeProc, ScanStats, Store};
use crate::timeutil::{day_start, local_offset_ms, DAY, HOUR, MINUTE, SECOND};

/// 网络状况看最近这么长时间
const NET_WINDOW: i64 = 15 * MINUTE;
/// 首字延迟的中位数超过这个值就算“响应慢”（大上下文的请求正常也要十几秒）
const SLOW_TTFT_MS: u32 = 30_000;
/// 最近活跃过的对话也列进实时列表
const RECENT_SESSION_MS: i64 = 30 * MINUTE;
/// 刚结束的一轮保留多久（灵动岛提示用）
const FINISHED_MS: i64 = 5 * MINUTE;
const HEAT_DAYS: usize = 182;
const MAX_MODELS: usize = 60;
/// 实时曲线的采样间隔与点数（5 秒一格，共 10 分钟）
const RECENT_STEP: i64 = 5 * SECOND;
const RECENT_POINTS: usize = 120;
const MAX_PROJECTS: usize = 16;
const MAX_LIVE_SESSIONS: usize = 40;
const MAX_NET_EVENTS: usize = 60;
const MAX_LATENCY_POINTS: usize = 80;

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    /// 思考 token，是 `output` 的一部分
    pub reasoning: u64,
    /// `reasoning` 里按文本占比估算出来的部分
    pub reasoning_estimated: u64,
    pub total: u64,
    pub estimated: u64,
    pub credits: f64,
    pub requests: u64,
    pub think_ms: u64,
}

impl Totals {
    pub fn add_event(&mut self, event: &UsageEvent) {
        let usage = &event.usage;
        self.input += usage.input;
        self.cache_read += usage.cache_read;
        self.cache_write += usage.cache_write;
        self.output += usage.output;
        self.reasoning += usage.reasoning;
        if event.reasoning_estimated {
            self.reasoning_estimated += usage.reasoning;
        }
        if event.estimated { self.estimated += usage.total(); }
        self.credits += f64::from(event.credits);
        self.total += usage.total();
        self.requests += 1;
        self.think_ms += event.think_ms as u64;
    }

    pub fn add(&mut self, other: &Totals) {
        self.input += other.input;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.output += other.output;
        self.reasoning += other.reasoning;
        self.reasoning_estimated += other.reasoning_estimated;
        self.estimated += other.estimated;
        self.credits += other.credits;
        self.total += other.total;
        self.requests += other.requests;
        self.think_ms += other.think_ms;
    }
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SeriesPoint {
    pub t: i64,
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    pub reasoning: u64,
    pub requests: u64,
    /// 各 agent 的总量，顺序同 `Agent::ALL`
    pub by_agent: [u64; Agent::COUNT],
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentTotals {
    pub agent: Agent,
    pub totals: Totals,
    pub sessions: u32,
}

/// 一个模型的用量。同一个模型不管是哪个 agent 用的、名字怎么写、末尾带什么强度后缀，都并在这一条里。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelTotals {
    /// 这个模型累计用得最多的那种写法，不带强度后缀
    pub model: String,
    pub totals: Totals,
    /// 用过它的各个 agent，用量多的在前
    pub agents: Vec<ModelAgentTotals>,
    /// 按思考强度拆开，用量多的在前
    pub efforts: Vec<EffortTotals>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelAgentTotals {
    pub agent: Agent,
    pub totals: Totals,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EffortTotals {
    pub effort: Effort,
    pub totals: Totals,
}

/// 经 Cursor 插件（网关卡片）进行的对话。这部分已经计入 Cursor，这里单独列出。
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginTotals {
    pub totals: Totals,
    pub sessions: u32,
}

/// 本机留有的插件卡片记录的概况。
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginInfo {
    /// 有卡片记录的对话数
    pub sessions: u32,
    /// 卡片记录里最近一次活动的时刻，0 表示本机没有卡片记录
    pub last_active: i64,
    /// 百事通的对话读得怎么样（连没连上、有几段对话、几段在运行）；没打开这个来源时为空
    pub baishitong: Option<FeedStatus>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectTotals {
    pub project: String,
    pub cwd: String,
    pub totals: Totals,
    pub sessions: u32,
    pub agents: Vec<Agent>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RangeStats {
    /// `today` / `7d` / `30d` / `all`
    pub id: &'static str,
    pub from: i64,
    pub to: i64,
    pub totals: Totals,
    /// 上一个同样长度的时段（“全部”没有）
    pub previous: Option<Totals>,
    /// 有用量的对话数（不含子代理）
    pub sessions: u32,
    pub by_agent: Vec<AgentTotals>,
    pub by_model: Vec<ModelTotals>,
    pub by_project: Vec<ProjectTotals>,
    pub plugin: PluginTotals,
    /// `hour` / `day` / `week`
    pub unit: &'static str,
    pub step_ms: i64,
    pub series: Vec<SeriesPoint>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    pub key: String,
    pub agent: Agent,
    pub id: String,
    pub title: String,
    pub project: String,
    pub cwd: String,
    pub model: String,
    pub running: bool,
    pub sub_agent: bool,
    pub phase: Phase,
    pub phase_since: i64,
    pub tool: String,
    pub turn_started: i64,
    pub started: i64,
    pub last_active: i64,
    pub totals: Totals,
    /// 当前这一轮已经用掉的量（只在运行中有值）
    pub turn: Totals,
    pub context_used: u64,
    pub context_window: u64,
    pub turns: u32,
    pub sub_agents: u32,
    pub running_sub_agents: u32,
    /// 源记录已被删除，数字来自此前的统计
    pub source_gone: bool,
    /// 这段会话是别的电脑记下的，由用量历史带过来
    pub source_elsewhere: bool,
    /// 经 Cursor 插件（网关卡片、百事通）进行的对话
    pub plugin: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NetStatus {
    /// 最近没有请求，无从判断
    #[default]
    Idle,
    Ok,
    Slow,
    /// 最近有重试或报错，但还能通
    Unstable,
    /// 连续失败，之后没有成功的响应
    Down,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetStats {
    pub status: NetStatus,
    /// 最近请求的响应耗时中位数，0 表示没有样本
    pub latency_ms: u32,
    /// 首字延迟中位数，0 表示没有样本
    pub ttft_ms: u32,
    pub samples: u32,
    pub requests: u32,
    pub retries: u32,
    pub errors: u32,
    pub last_ok: i64,
    pub last_issue: i64,
    pub last_code: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetEventView {
    pub ts: i64,
    pub agent: Agent,
    pub kind: NetKind,
    pub code: String,
    pub attempt: u32,
    pub max_attempts: u32,
    pub delay_ms: u32,
    pub project: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LatencyPoint {
    pub t: i64,
    pub agent: Agent,
    pub latency_ms: u32,
    pub ttft_ms: u32,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentLive {
    pub agent: Agent,
    pub label: &'static str,
    /// 本机找到了它的数据目录
    pub detected: bool,
    /// 是否有它的进程在跑；判断不了时为空
    pub process_alive: Option<bool>,
    pub running: u32,
    pub running_sub_agents: u32,
    pub last_active: i64,
    pub files: u32,
    /// 读取源文件失败时的原因，与模型请求的网络状态分开。
    pub read_error: Option<String>,
    pub conversations: usize,
    pub estimated: bool,
    pub recent_tools: Vec<crate::model::ToolCall>,
    pub net: NetStats,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RatePoint {
    pub t: i64,
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    pub reasoning: u64,
    pub requests: u64,
}

impl RatePoint {
    fn add(&mut self, event: &UsageEvent) {
        let usage = &event.usage;
        self.input += usage.input;
        self.cache_read += usage.cache_read;
        self.cache_write += usage.cache_write;
        self.output += usage.output;
        self.reasoning += usage.reasoning;
        self.requests += 1;
    }
}

/// 返回以当前时间桶结尾的连续 `count` 个采样点，空桶保留为零。
fn recent_points(now: i64, interval: i64, count: usize) -> Vec<RatePoint> {
    let end = now.div_euclid(interval) * interval;
    (0..count as i64).rev().map(|age| RatePoint { t: end - age * interval, ..Default::default() }).collect()
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhaseCounts {
    pub requesting: u32,
    pub thinking: u32,
    pub writing: u32,
    pub generating: u32,
    pub tool: u32,
    pub retrying: u32,
    pub waiting: u32,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FinishedTurn {
    pub key: String,
    pub agent: Agent,
    pub title: String,
    pub project: String,
    pub ended_at: i64,
    pub duration_ms: u64,
    pub totals: Totals,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitView {
    pub agent: Agent,
    pub ts: i64,
    pub plan: String,
    pub primary: Option<RateWindow>,
    pub secondary: Option<RateWindow>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveStats {
    /// 正在运行的对话数（子代理不单独计数）
    pub running: u32,
    pub running_sub_agents: u32,
    pub phases: PhaseCounts,
    /// 运行中的排在前面，其后是最近活跃过的
    pub sessions: Vec<SessionView>,
    pub agents: Vec<AgentLive>,
    pub net: NetStats,
    pub net_events: Vec<NetEventView>,
    pub latency: Vec<LatencyPoint>,
    /// 最近一小时每分钟的用量
    pub rate: Vec<RatePoint>,
    /// 最近十分钟每 5 秒的用量
    pub recent: Vec<RatePoint>,
    /// 最近一分钟每秒的用量
    pub per_second: Vec<RatePoint>,
    /// 近一分钟里记下的用量（见 [`crate::intake`]）。要跟上一轮比才算得出来，由持续运行的一方填；这里给的是空值
    pub recorded: Recorded,
    pub finished: Vec<FinishedTurn>,
    pub rate_limits: Vec<RateLimitView>,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DayCell {
    pub t: i64,
    pub total: u64,
    pub requests: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub generated_at: i64,
    pub version: u64,
    pub utc_offset_ms: i64,
    pub ranges: Vec<RangeStats>,
    pub live: LiveStats,
    /// 最近 26 周每天的用量
    pub heatmap: Vec<DayCell>,
    pub plugin: PluginInfo,
    pub scan: ScanStats,
}

pub struct Analysis {
    pub snapshot: Snapshot,
    /// 全部对话，按最近活跃排序
    pub sessions: Vec<SessionView>,
}

/// 汇总时需要的运行环境信息。
pub struct Env<'a> {
    pub claude_registry: bool,
    pub claude_procs: &'a [ClaudeProc],
    /// 各 agent 是否有进程在跑，顺序同 `Agent::ALL`；判断不了时为 `None`
    pub agent_alive: [Option<bool>; Agent::COUNT],
    pub detected: [bool; Agent::COUNT],
    pub version: u64,
    pub scan: ScanStats,
}

impl<'a> Env<'a> {
    pub fn of(store: &'a Store) -> Env<'a> {
        let processes = store.processes();
        let procs = store.claude_procs();
        let claude = if store.claude_registry() && procs.iter().any(|proc| proc.alive) { Some(true) } else { processes.any_named(&["claude"]) };
        let hub_alive = store.antigravity_hubs().iter().any(|pid| processes.alive(*pid) == Some(true));
        let antigravity = if hub_alive { Some(true) } else { processes.any_named(&["antigravity"]) };
        Env {
            claude_registry: store.claude_registry(),
            claude_procs: procs,
            agent_alive: [claude, processes.any_named(&["codex"]), processes.any_named(&["deepseek", "dsh"]), antigravity, processes.any_named(&["kiro"]), processes.any_named(&["cursor"]), None],
            detected: Agent::ALL.map(|agent| !store.roots().of(agent).is_empty()),
            version: store.version(),
            scan: store.stats().clone(),
        }
    }
}

pub fn analyze(store: &Store, now: i64) -> Analysis {
    let mut analysis = analyze_entries(store.entries().collect(), &Env::of(store), now, local_offset_ms());
    if let Some(limits) = store.kiro_limits() {
        let views = &mut analysis.snapshot.live.rate_limits;
        views.retain(|view| view.agent != Agent::Kiro);
        views.push(RateLimitView {
            agent: Agent::Kiro,
            ts: limits.ts,
            plan: limits.plan.clone(),
            primary: limits.primary.clone(),
            secondary: limits.secondary.clone(),
        });
        views.sort_by_key(|view| view.agent.index());
    }
    let counts = store.file_counts();
    for agent in &mut analysis.snapshot.live.agents {
        agent.files = counts[agent.agent.index()];
        agent.detected |= agent.conversations > 0 || agent.files > 0;
        agent.read_error = store.source_files()
            .filter(|file| file.agent == agent.agent && !file.gone)
            .find_map(|file| file.last_error.clone());
    }
    analysis.snapshot.plugin.baishitong = store.baishitong_status();
    analysis
}

/// 对话明细里的一次请求。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestRow {
    pub ts: i64,
    pub model: String,
    pub effort: Effort,
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    pub reasoning: u64,
    pub total: u64,
    pub latency_ms: u32,
    pub ttft_ms: u32,
    pub think_ms: u32,
    pub credits: f32,
    pub estimated: bool,
    /// 这次请求是子代理发的
    pub sub_agent: bool,
}

/// 一段对话的逐次请求和工具调用（界面的对话详情用）。只有数字、模型名和工具名，没有对话内容。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionDetail {
    pub key: String,
    /// 请求总数；`requests` 只带最近的一批
    pub request_count: usize,
    pub requests: Vec<RequestRow>,
    pub tool_count: usize,
    pub tools: Vec<crate::model::ToolCall>,
    /// 原始记录不在这台电脑上（已经删掉，或者是别的电脑记下的）：只有用量，没有工具记录
    pub usage_only: bool,
}

/// 明细最多带这么多条，超出的只留最近的
const DETAIL_REQUESTS: usize = 1500;
const DETAIL_TOOLS: usize = 600;

/// 找出标识为 `key`（和 [`SessionView::key`] 同一种拼法）的对话，列出它的请求和工具调用；子代理的记录一并算在内。
/// 同一次请求在这段对话的几份记录里都出现时只列一次。和列表上的合计不完全是一回事：
/// 列表把跨对话重复的请求记在最早的那段对话上，这里照记录原样列出。
pub fn session_detail(store: &Store, key: &str) -> Option<SessionDetail> {
    let mut requests: Vec<RequestRow> = Vec::new();
    let mut tools: Vec<crate::model::ToolCall> = Vec::new();
    let mut seen: HashSet<u64> = HashSet::new();
    let mut found = false;
    let mut usage_only = true;
    for entry in store.entries() {
        let mine = key.strip_prefix(entry.agent.id()).and_then(|rest| rest.strip_prefix(':')) == Some(session_id_of(entry));
        if !mine {
            continue;
        }
        found = true;
        usage_only &= entry.gone || entry.elsewhere;
        for event in &entry.events {
            if event.key != 0 && !seen.insert(event.key) {
                continue;
            }
            requests.push(RequestRow {
                ts: event.ts,
                model: entry.models.get(usize::from(event.model)).cloned().unwrap_or_default(),
                effort: event.effort,
                input: event.usage.input,
                cache_read: event.usage.cache_read,
                cache_write: event.usage.cache_write,
                output: event.usage.output,
                reasoning: event.usage.reasoning,
                total: event.usage.total(),
                latency_ms: event.latency_ms,
                ttft_ms: event.ttft_ms,
                think_ms: event.think_ms,
                credits: event.credits,
                estimated: event.estimated,
                sub_agent: entry.session.sub_agent,
            });
        }
        tools.extend(entry.tools.iter().cloned());
    }
    if !found {
        return None;
    }
    requests.sort_by_key(|row| row.ts);
    tools.sort_by_key(|tool| tool.ts);
    let (request_count, tool_count) = (requests.len(), tools.len());
    requests.drain(..request_count.saturating_sub(DETAIL_REQUESTS));
    tools.drain(..tool_count.saturating_sub(DETAIL_TOOLS));
    Some(SessionDetail { key: key.to_owned(), request_count, requests, tool_count, tools, usage_only })
}

struct ProjectAcc<'a> {
    cwd: &'a str,
    totals: Totals,
    sessions: HashSet<(usize, &'a str)>,
    agents: [bool; Agent::COUNT],
}

struct RangeAcc<'a> {
    id: &'static str,
    from: i64,
    to: i64,
    series_from: i64,
    step: i64,
    unit: &'static str,
    previous_window: Option<(i64, i64)>,
    totals: Totals,
    previous: Totals,
    by_agent: [Totals; Agent::COUNT],
    agent_sessions: [HashSet<&'a str>; Agent::COUNT],
    /// (agent, 来源里写的模型名) → 各思考强度的用量，下标同 `Effort::ALL`；出结果时再按 [`model_key`] 并成一个模型一条
    models: HashMap<(usize, &'a str), [Totals; Effort::COUNT]>,
    projects: HashMap<String, ProjectAcc<'a>>,
    plugin: Totals,
    plugin_sessions: HashSet<&'a str>,
    points: Vec<SeriesPoint>,
}

impl<'a> RangeAcc<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(id: &'static str, from: i64, to: i64, series_from: i64, step: i64, unit: &'static str, count: usize, previous_window: Option<(i64, i64)>) -> Self {
        let points = (0..count).map(|index| SeriesPoint { t: series_from + index as i64 * step, ..SeriesPoint::default() }).collect();
        RangeAcc {
            id,
            from,
            to,
            series_from,
            step,
            unit,
            previous_window,
            totals: Totals::default(),
            previous: Totals::default(),
            by_agent: [Totals::default(); Agent::COUNT],
            agent_sessions: Default::default(),
            models: HashMap::new(),
            projects: HashMap::new(),
            plugin: Totals::default(),
            plugin_sessions: HashSet::new(),
            points,
        }
    }

    fn add(&mut self, agent: usize, ts: i64, event: &UsageEvent) {
        self.totals.add_event(event);
        self.by_agent[agent].add_event(event);
        if self.points.is_empty() {
            return;
        }
        let index = ((ts - self.series_from).max(0) / self.step) as usize;
        let last = self.points.len() - 1;
        let point = &mut self.points[index.min(last)];
        let usage = &event.usage;
        point.input += usage.input;
        point.cache_read += usage.cache_read;
        point.cache_write += usage.cache_write;
        point.output += usage.output;
        point.reasoning += usage.reasoning;
        point.requests += 1;
        point.by_agent[agent] += usage.total();
    }

    /// `model_names`：各个模型显示用的名字，见 [`model_display_names`]
    fn finish(self, model_names: &HashMap<String, &str>) -> RangeStats {
        let mut merged: HashMap<String, ([Totals; Effort::COUNT], [Totals; Agent::COUNT])> = HashMap::new();
        for ((agent, name), by_effort) in self.models {
            let (efforts, agents) = merged.entry(model_key(name)).or_insert_with(|| ([Totals::default(); Effort::COUNT], [Totals::default(); Agent::COUNT]));
            for (slot, part) in efforts.iter_mut().zip(&by_effort) {
                slot.add(part);
                agents[agent].add(part);
            }
        }
        let mut by_model: Vec<ModelTotals> = merged
            .into_iter()
            .map(|(key, (by_effort, by_agent))| {
                let mut totals = Totals::default();
                let mut efforts: Vec<EffortTotals> = Vec::new();
                for (index, part) in by_effort.iter().enumerate().filter(|(_, part)| part.requests > 0) {
                    totals.add(part);
                    efforts.push(EffortTotals { effort: Effort::from_index(index), totals: *part });
                }
                efforts.sort_by(|a, b| b.totals.total.cmp(&a.totals.total).then_with(|| b.effort.cmp(&a.effort)));
                let mut agents: Vec<ModelAgentTotals> =
                    Agent::ALL.iter().zip(&by_agent).filter(|(_, part)| part.requests > 0).map(|(agent, part)| ModelAgentTotals { agent: *agent, totals: *part }).collect();
                agents.sort_by_key(|part| std::cmp::Reverse(part.totals.total));
                let model = model_names.get(&key).copied().unwrap_or(key.as_str()).to_owned();
                ModelTotals { model, totals, agents, efforts }
            })
            .collect();
        by_model.sort_by(|a, b| b.totals.total.cmp(&a.totals.total).then_with(|| a.model.cmp(&b.model)));
        by_model.truncate(MAX_MODELS);

        let mut by_project: Vec<ProjectTotals> = self
            .projects
            .into_values()
            .map(|project| ProjectTotals {
                project: project_name(project.cwd),
                cwd: project.cwd.to_owned(),
                totals: project.totals,
                sessions: project.sessions.len() as u32,
                agents: Agent::ALL.iter().copied().filter(|agent| project.agents[agent.index()]).collect(),
            })
            .collect();
        by_project.sort_by(|a, b| b.totals.total.cmp(&a.totals.total).then_with(|| a.cwd.cmp(&b.cwd)));
        by_project.truncate(MAX_PROJECTS);

        let by_agent: Vec<AgentTotals> = Agent::ALL
            .iter()
            .map(|agent| AgentTotals { agent: *agent, totals: self.by_agent[agent.index()], sessions: self.agent_sessions[agent.index()].len() as u32 })
            .collect();
        RangeStats {
            id: self.id,
            from: if self.from == i64::MIN { self.series_from } else { self.from },
            to: self.to,
            totals: self.totals,
            previous: self.previous_window.map(|_| self.previous),
            sessions: by_agent.iter().map(|agent| agent.sessions).sum(),
            by_agent,
            by_model,
            by_project,
            plugin: PluginTotals { totals: self.plugin, sessions: self.plugin_sessions.len() as u32 },
            unit: self.unit,
            step_ms: self.step,
            series: self.points,
        }
    }
}

struct EntryRange {
    totals: Totals,
    /// (模型下标, 思考强度) → 用量；一个文件里通常只有一两种组合
    models: Vec<(u16, Effort, Totals)>,
}

struct Part<'a> {
    entry: &'a FileEntry,
    session_id: &'a str,
    totals: Totals,
    turn: Totals,
    last_turn: Totals,
    model: &'a str,
    running: Option<(Phase, i64)>,
}

struct Sample {
    ts: i64,
    agent: Agent,
    latency_ms: u32,
    ttft_ms: u32,
}

fn session_id_of(entry: &FileEntry) -> &str {
    if entry.session.id.is_empty() {
        entry.path.file_stem().and_then(|stem| stem.to_str()).unwrap_or("")
    } else {
        &entry.session.id
    }
}

fn project_key(cwd: &str) -> String {
    cwd.trim_end_matches(['/', '\\']).replace('\\', "/").to_lowercase()
}

/// 合并模型用的键。同一个模型在各处的写法不一样（`Claude Fable 5.2`、`claude-fable-5-2-max-fast`、`claude-fable-5.2`）：
/// 去掉末尾的强度后缀，不分大小写，字母数字以外的都当成分隔符。
fn model_key(name: &str) -> String {
    let (base, _) = Effort::split_model_name(name);
    let mut key = String::with_capacity(base.len());
    for word in base.split(|ch: char| !ch.is_alphanumeric()).filter(|word| !word.is_empty()) {
        if !key.is_empty() {
            key.push('-');
        }
        key.extend(word.chars().flat_map(char::to_lowercase));
    }
    if key.is_empty() {
        base.to_owned()
    } else {
        key
    }
}

/// 各个模型显示用的名字（[`model_key`] → 名字）：取这个模型用得最多的那种写法。
/// 按全部历史来定，各个时段用同一个名字。
fn model_display_names<'a>(models: &HashMap<(usize, &'a str), [Totals; Effort::COUNT]>) -> HashMap<String, &'a str> {
    let mut spellings: HashMap<(String, &'a str), u64> = HashMap::new();
    for (&(_, name), by_effort) in models {
        let (base, _) = Effort::split_model_name(name);
        *spellings.entry((model_key(name), base)).or_default() += by_effort.iter().map(|part| part.total).sum::<u64>();
    }
    let mut names: HashMap<String, (u64, &'a str)> = HashMap::new();
    for ((key, base), total) in spellings {
        // 用量一样时取字典序靠前的，结果不随遍历顺序变
        let better = match names.get(&key) {
            Some((most, known)) => total > *most || (total == *most && base < *known),
            None => true,
        };
        if better {
            names.insert(key, (total, base));
        }
    }
    names.into_iter().map(|(key, (_, base))| (key, base)).collect()
}

/// 这个文件对应的会话现在是否在运行；在运行时返回当前阶段和进入该阶段的时刻。
fn running_state(entry: &FileEntry, env: &Env, now: i64) -> Option<(Phase, i64)> {
    if entry.gone {
        return None;
    }
    let live = &entry.live;
    let last_activity = entry.session.last_active_ms.max(live.phase_since_ms).max(entry.sig.latest_mtime());
    let idle = now - last_activity;

    if entry.agent == Agent::Claude && env.claude_registry {
        // Claude 自己登记了每个进程的状态，以它为准
        let session_id = session_id_of(entry);
        let proc = env.claude_procs.iter().find(|proc| proc.alive && proc.status == "busy" && proc.session_id == session_id)?;
        if live.turn_open {
            if entry.session.sub_agent && idle > 10 * MINUTE {
                return None;
            }
            return Some((live.phase, live.phase_since_ms));
        }
        if entry.session.sub_agent {
            return None;
        }
        // 进程已经在忙，但记录还没写到：请求刚发出
        return Some((Phase::Requesting, proc.status_updated_ms.max(live.last_turn_end_ms)));
    }

    if !live.turn_open {
        return None;
    }
    // 记录里这一轮没有收尾：太久没动静就当作已经中断（进程被关掉时不会写收尾记录）
    let mut limit = match live.phase {
        Phase::Tool => 30 * MINUTE,
        Phase::Waiting => 60 * MINUTE,
        _ => 10 * MINUTE,
    };
    if env.agent_alive[entry.agent.index()] == Some(false) {
        limit = limit.min(90 * SECOND);
    }
    (idle <= limit).then_some((live.phase, live.phase_since_ms))
}

fn median(values: &mut [u32]) -> u32 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    values[values.len() / 2]
}

fn net_stats(now: i64, agent: Option<Agent>, samples: &[Sample], issues: &[NetEventView], last_ok: i64, requests: u32) -> NetStats {
    let from = now - NET_WINDOW;
    let wanted = |candidate: Agent| agent.map_or(true, |agent| agent == candidate);
    let mut latencies: Vec<u32> = samples.iter().filter(|s| wanted(s.agent) && s.ts >= from && s.latency_ms > 0).map(|s| s.latency_ms).collect();
    let mut ttfts: Vec<u32> = samples.iter().filter(|s| wanted(s.agent) && s.ts >= from && s.ttft_ms > 0).map(|s| s.ttft_ms).collect();
    let mine = || issues.iter().filter(|issue| wanted(issue.agent));
    let retries = mine().filter(|issue| issue.ts >= from && issue.kind == NetKind::Retry).count() as u32;
    let errors = mine().filter(|issue| issue.ts >= from && issue.kind == NetKind::Error).count() as u32;
    let latest = mine().max_by_key(|issue| issue.ts);
    let recent = mine().filter(|issue| issue.ts >= now - 5 * MINUTE).count();
    // 最后一次成功之后、两分钟以内的失败次数
    let streak = mine().filter(|issue| issue.ts > last_ok && issue.ts >= now - 2 * MINUTE).count();

    let samples_count = latencies.len().max(ttfts.len()) as u32;
    let ttft_count = ttfts.len();
    let latency_ms = median(&mut latencies);
    let ttft_ms = median(&mut ttfts);
    let status = if requests == 0 && retries + errors == 0 {
        NetStatus::Idle
    } else if streak >= 2 {
        NetStatus::Down
    } else if streak == 1 || recent > 0 {
        NetStatus::Unstable
    } else if ttft_count >= 3 && ttft_ms > SLOW_TTFT_MS {
        NetStatus::Slow
    } else {
        NetStatus::Ok
    };
    NetStats {
        status,
        latency_ms,
        ttft_ms,
        samples: samples_count,
        requests,
        retries,
        errors,
        last_ok,
        last_issue: latest.map_or(0, |issue| issue.ts),
        last_code: latest.map(|issue| issue.code.clone()).unwrap_or_default(),
    }
}

fn view_of(part: &Part) -> SessionView {
    let entry = part.entry;
    let live = &entry.live;
    let (phase, phase_since) = part.running.unwrap_or((Phase::Idle, live.last_turn_end_ms));
    let running = part.running.is_some();
    SessionView {
        key: format!("{}:{}", entry.agent.id(), part.session_id),
        agent: entry.agent,
        id: part.session_id.to_owned(),
        title: entry.session.title.clone(),
        project: project_name(&entry.session.cwd),
        cwd: entry.session.cwd.clone(),
        model: part.model.to_owned(),
        running,
        sub_agent: entry.session.sub_agent,
        phase,
        phase_since,
        tool: if phase == Phase::Tool { live.tool.clone() } else { String::new() },
        turn_started: if running { live.turn_started_ms } else { 0 },
        started: entry.session.started_ms,
        last_active: entry.session.last_active_ms,
        totals: part.totals,
        turn: if running { part.turn } else { Totals::default() },
        context_used: entry.session.context_used,
        context_window: entry.session.context_window,
        turns: live.turns,
        sub_agents: 0,
        running_sub_agents: 0,
        source_gone: entry.gone,
        source_elsewhere: entry.elsewhere,
        plugin: false,
    }
}

/// 同一个会话的另一份记录（子代理，或重复的主记录）并进已有的视图。
fn merge_into(view: &mut SessionView, part: &Part) {
    let entry = part.entry;
    view.totals.add(&part.totals);
    view.started = match (view.started, entry.session.started_ms) {
        (0, other) => other,
        (mine, 0) => mine,
        (mine, other) => mine.min(other),
    };
    view.last_active = view.last_active.max(entry.session.last_active_ms);
    if entry.session.sub_agent {
        view.sub_agents += 1;
        if part.running.is_some() {
            view.running_sub_agents += 1;
            view.turn.add(&part.turn);
        }
        return;
    }
    // 重复的主记录：实时状态取正在运行的那份
    if !view.running && part.running.is_some() {
        let fresh = view_of(part);
        view.running = true;
        view.phase = fresh.phase;
        view.phase_since = fresh.phase_since;
        view.tool = fresh.tool;
        view.turn_started = fresh.turn_started;
        view.turn = fresh.turn;
        view.model = fresh.model;
        view.context_used = fresh.context_used;
        view.context_window = fresh.context_window;
    }
    if view.title.is_empty() {
        view.title = entry.session.title.clone();
    }
    view.turns += entry.live.turns;
    view.source_gone &= entry.gone;
    view.source_elsewhere &= entry.elsewhere;
}

pub fn analyze_entries<'a>(mut entries: Vec<&'a FileEntry>, env: &Env<'a>, now: i64, offset: i64) -> Analysis {
    // 按会话开始时间排序：跨文件重复的响应始终记在最早的那个会话上
    entries.sort_by(|a, b| a.session.started_ms.cmp(&b.session.started_ms).then_with(|| a.path.cmp(&b.path)));

    let today = day_start(now, offset);
    let first = entries.iter().flat_map(|entry| entry.events.iter()).map(|event| event.ts).filter(|ts| *ts > 0).min().unwrap_or(now).min(now);
    let first_day = day_start(first, offset);
    let span_days = (today - first_day) / DAY + 1;
    let (all_step, all_unit) = if span_days > 120 { (7 * DAY, "week") } else { (DAY, "day") };
    let all_count = ((today + DAY - first_day + all_step - 1) / all_step).max(1) as usize;
    let to = now + 1;
    let mut ranges = [
        RangeAcc::new("today", today, to, today, HOUR, "hour", 24, Some((today - DAY, to - DAY))),
        RangeAcc::new("7d", today - 6 * DAY, to, today - 6 * DAY, DAY, "day", 7, Some((today - 13 * DAY, to - 7 * DAY))),
        RangeAcc::new("30d", today - 29 * DAY, to, today - 29 * DAY, DAY, "day", 30, Some((today - 59 * DAY, to - 30 * DAY))),
        RangeAcc::new("all", i64::MIN, to, first_day, all_step, all_unit, all_count, None),
    ];

    let heat_from = today - (HEAT_DAYS as i64 - 1) * DAY;
    let mut heatmap: Vec<DayCell> = (0..HEAT_DAYS).map(|index| DayCell { t: heat_from + index as i64 * DAY, ..DayCell::default() }).collect();
    let mut rate = recent_points(now, MINUTE, 60);
    let mut recent = recent_points(now, RECENT_STEP, RECENT_POINTS);
    let mut per_second = recent_points(now, SECOND, 60);
    let rate_from = rate[0].t;
    let recent_from = recent[0].t;
    let seconds_from = per_second[0].t;
    // 卡片记录和它所在的 Cursor 对话共用一个会话 id，整个会话都算插件对话
    let plugin_ids: HashSet<&str> = entries.iter().copied().filter(|entry| entry.plugin).map(session_id_of).collect();
    let mut plugin_last_active = 0i64;

    let mut seen: HashSet<u64> = HashSet::new();
    let mut parts: Vec<Part<'a>> = Vec::with_capacity(entries.len());
    let mut samples: Vec<Sample> = Vec::new();
    let mut issues: Vec<NetEventView> = Vec::new();
    let mut last_ok = [0i64; Agent::COUNT];
    let mut window_requests = [0u32; Agent::COUNT];
    let mut files = [0u32; Agent::COUNT];
    let mut agent_last_active = [0i64; Agent::COUNT];
    let mut rate_limits: [Option<RateLimitView>; Agent::COUNT] = Default::default();

    for entry in entries.iter().copied() {
        let agent = entry.agent.index();
        let session_id = session_id_of(entry);
        let live = &entry.live;
        if !entry.gone {
            files[agent] += 1;
        }
        agent_last_active[agent] = agent_last_active[agent].max(entry.session.last_active_ms);
        let via_plugin = entry.plugin || (matches!(entry.agent, Agent::Cursor | Agent::WebMcp) && plugin_ids.contains(session_id));
        if entry.plugin {
            plugin_last_active = plugin_last_active.max(entry.session.last_active_ms);
        }

        let fallback_ts = if entry.session.last_active_ms > 0 { entry.session.last_active_ms } else { entry.sig.latest_mtime() };
        let turn_from = if live.turn_open { live.turn_started_ms } else { i64::MAX };
        let last_turn_window = if !live.turn_open && live.last_turn_end_ms >= now - FINISHED_MS {
            (live.last_turn_end_ms - live.last_turn_ms as i64 - SECOND, live.last_turn_end_ms + 5 * SECOND)
        } else {
            (i64::MAX, i64::MIN)
        };

        // 模型名后缀里带的思考强度
        let name_efforts: Vec<Effort> = entry.models.iter().map(|name| Effort::from_model_name(name)).collect();
        let mut local: Vec<EntryRange> = ranges.iter().map(|_| EntryRange { totals: Totals::default(), models: Vec::new() }).collect();
        let mut totals = Totals::default();
        let mut turn = Totals::default();
        let mut last_turn = Totals::default();
        let mut model: &'a str = "";

        for event in &entry.events {
            if event.key != 0 && !seen.insert(event.key) {
                continue;
            }
            let ts = if event.ts > 0 { event.ts.min(now) } else { fallback_ts.min(now) };
            let usage = &event.usage;
            totals.add_event(event);
            if ts >= turn_from {
                turn.add_event(event);
            }
            if ts >= last_turn_window.0 && ts <= last_turn_window.1 {
                last_turn.add_event(event);
            }
            model = entry.model_name(event.model);
            // 来源没有单独记录思考强度时看模型名的后缀。旧版把 `-max-fast` 记成了 max：名字里的比记下来的多一个 fast 时也以名字为准
            let named = name_efforts.get(event.model as usize).copied().unwrap_or_default();
            let effort = if event.effort == Effort::Unknown || named.without_fast() == event.effort { named } else { event.effort };

            for (range, local) in ranges.iter_mut().zip(local.iter_mut()) {
                if ts >= range.from && ts < range.to {
                    range.add(agent, ts, event);
                    local.totals.add_event(event);
                    match local.models.iter_mut().find(|(index, level, _)| *index == event.model && *level == effort) {
                        Some((_, _, slot)) => slot.add_event(event),
                        None => {
                            let mut slot = Totals::default();
                            slot.add_event(event);
                            local.models.push((event.model, effort, slot));
                        }
                    }
                } else if let Some((from, to)) = range.previous_window {
                    if ts >= from && ts < to {
                        range.previous.add_event(event);
                    }
                }
            }

            if ts >= heat_from {
                if let Some(cell) = heatmap.get_mut(((ts - heat_from) / DAY) as usize) {
                    cell.total += usage.total();
                    cell.requests += 1;
                }
            }
            if ts >= rate_from {
                if let Some(point) = rate.get_mut(((ts - rate_from) / MINUTE) as usize) {
                    point.add(event);
                }
                if ts >= recent_from {
                    if let Some(point) = recent.get_mut(((ts - recent_from) / RECENT_STEP) as usize) {
                        point.add(event);
                    }
                }
                if ts >= seconds_from {
                    per_second[((ts - seconds_from) / SECOND) as usize].add(event);
                }
            }
            if ts >= now - HOUR && (event.latency_ms > 0 || event.ttft_ms > 0) {
                samples.push(Sample { ts, agent: entry.agent, latency_ms: event.latency_ms, ttft_ms: event.ttft_ms });
            }
            if ts >= now - NET_WINDOW {
                window_requests[agent] += 1;
            }
            last_ok[agent] = last_ok[agent].max(ts);
        }

        for (range, local) in ranges.iter_mut().zip(local) {
            if local.totals.requests == 0 {
                continue;
            }
            for (index, effort, model_totals) in &local.models {
                range.models.entry((agent, entry.model_name(*index))).or_default()[effort.index()].add(model_totals);
            }
            if via_plugin {
                range.plugin.add(&local.totals);
                if !entry.session.sub_agent {
                    range.plugin_sessions.insert(session_id);
                }
            }
            let project = range.projects.entry(project_key(&entry.session.cwd)).or_insert_with(|| ProjectAcc {
                cwd: entry.session.cwd.as_str(),
                totals: Totals::default(),
                sessions: HashSet::new(),
                agents: [false; Agent::COUNT],
            });
            project.totals.add(&local.totals);
            project.agents[agent] = true;
            if !entry.session.sub_agent {
                project.sessions.insert((agent, session_id));
                range.agent_sessions[agent].insert(session_id);
            }
        }

        for event in &entry.net {
            if event.ts >= now - DAY {
                issues.push(NetEventView {
                    ts: event.ts,
                    agent: entry.agent,
                    kind: event.kind,
                    code: event.code.clone(),
                    attempt: event.attempt,
                    max_attempts: event.max_attempts,
                    delay_ms: event.delay_ms,
                    project: project_name(&entry.session.cwd),
                });
            }
        }

        if let Some(limits) = &entry.rate_limits {
            let newer = rate_limits[agent].as_ref().map_or(true, |known| limits.ts >= known.ts);
            if newer {
                rate_limits[agent] = Some(RateLimitView {
                    agent: entry.agent,
                    ts: limits.ts,
                    plan: limits.plan.clone(),
                    primary: limits.primary.clone(),
                    secondary: limits.secondary.clone(),
                });
            }
        }

        parts.push(Part { entry, session_id, totals, turn, last_turn, model, running: running_state(entry, env, now) });
    }

    // ── 会话视图：主记录先建，子代理记录并进所属会话 ──
    let mut views: Vec<SessionView> = Vec::with_capacity(parts.len());
    let mut index: HashMap<(usize, &str), usize> = HashMap::new();
    for sub_pass in [false, true] {
        for part in parts.iter().filter(|part| part.entry.session.sub_agent == sub_pass) {
            let key = (part.entry.agent.index(), part.session_id);
            match index.get(&key).copied() {
                Some(position) => merge_into(&mut views[position], part),
                None => {
                    index.insert(key, views.len());
                    views.push(view_of(part));
                }
            }
        }
    }
    for proc in env.claude_procs {
        let key = (Agent::Claude.index(), proc.session_id.as_str());
        match index.get(&key).copied() {
            Some(position) => {
                let view = &mut views[position];
                if view.title.is_empty() {
                    view.title = proc.name.clone();
                }
            }
            None if proc.alive && proc.status == "busy" => {
                // 新对话的第一个请求还在路上，记录文件尚未出现
                index.insert(key, views.len());
                views.push(SessionView {
                    key: format!("{}:{}", Agent::Claude.id(), proc.session_id),
                    agent: Agent::Claude,
                    id: proc.session_id.clone(),
                    title: proc.name.clone(),
                    project: project_name(&proc.cwd),
                    cwd: proc.cwd.clone(),
                    model: String::new(),
                    running: true,
                    sub_agent: false,
                    phase: Phase::Requesting,
                    phase_since: proc.status_updated_ms,
                    tool: String::new(),
                    turn_started: proc.status_updated_ms,
                    started: proc.started_ms,
                    last_active: proc.updated_ms.max(proc.status_updated_ms),
                    totals: Totals::default(),
                    turn: Totals::default(),
                    context_used: 0,
                    context_window: 0,
                    turns: 0,
                    sub_agents: 0,
                    running_sub_agents: 0,
                    source_gone: false,
                    source_elsewhere: false,
                    plugin: false,
                });
            }
            None => {}
        }
    }
    // 插件对话：卡片记录自己，和它所在的那段 Cursor 对话（两者共用一个会话 id）
    for view in &mut views {
        view.plugin = matches!(view.agent, Agent::Cursor | Agent::WebMcp) && plugin_ids.contains(view.id.as_str());
    }
    views.sort_by(|a, b| b.last_active.cmp(&a.last_active).then_with(|| a.key.cmp(&b.key)));

    // ── 实时统计 ──
    let mut phases = PhaseCounts::default();
    let mut running = [0u32; Agent::COUNT];
    let mut running_sub = [0u32; Agent::COUNT];
    for view in &views {
        let agent = view.agent.index();
        running_sub[agent] += view.running_sub_agents;
        if !view.running {
            continue;
        }
        if view.sub_agent {
            running_sub[agent] += 1;
            continue;
        }
        running[agent] += 1;
        match view.phase {
            Phase::Requesting => phases.requesting += 1,
            Phase::Thinking => phases.thinking += 1,
            Phase::Writing => phases.writing += 1,
            Phase::Generating => phases.generating += 1,
            Phase::Tool => phases.tool += 1,
            Phase::Retrying => phases.retrying += 1,
            Phase::Waiting => phases.waiting += 1,
            Phase::Idle => {}
        }
    }

    let mut live_sessions: Vec<SessionView> = views.iter().filter(|view| view.running || view.last_active >= now - RECENT_SESSION_MS).cloned().collect();
    live_sessions.sort_by(|a, b| {
        (b.running && !b.sub_agent).cmp(&(a.running && !a.sub_agent)).then_with(|| b.running.cmp(&a.running)).then_with(|| b.last_active.cmp(&a.last_active))
    });
    live_sessions.truncate(MAX_LIVE_SESSIONS);

    let mut finished: Vec<FinishedTurn> = parts
        .iter()
        .filter(|part| {
            let live = &part.entry.live;
            !part.entry.session.sub_agent && part.running.is_none() && live.turns > 0 && live.last_turn_end_ms >= now - FINISHED_MS
        })
        .map(|part| {
            let entry = part.entry;
            FinishedTurn {
                key: format!("{}:{}:{}", entry.agent.id(), part.session_id, entry.live.last_turn_end_ms),
                agent: entry.agent,
                title: entry.session.title.clone(),
                project: project_name(&entry.session.cwd),
                ended_at: entry.live.last_turn_end_ms,
                duration_ms: entry.live.last_turn_ms,
                totals: part.last_turn,
            }
        })
        .collect();
    finished.sort_by(|a, b| b.ended_at.cmp(&a.ended_at));

    issues.sort_by(|a, b| b.ts.cmp(&a.ts));
    samples.sort_by_key(|sample| sample.ts);
    let agents: Vec<AgentLive> = Agent::ALL
        .iter()
        .map(|agent| {
            let i = agent.index();
            let has_mirror = *agent == Agent::WebMcp && entries.iter().any(|e| e.agent == *agent && e.session.id.starts_with("gateway/") && !e.tools.is_empty());
            let mut recent_tools: Vec<_> = entries.iter().filter(|e| e.agent == *agent && (!has_mirror || e.session.id.starts_with("gateway/")))
                .flat_map(|e| e.tools.iter()).collect();
            recent_tools.sort_unstable_by_key(|tool| std::cmp::Reverse(tool.ts));
            AgentLive {
                agent: *agent,
                label: agent.label(),
                detected: env.detected[i],
                process_alive: env.agent_alive[i],
                running: running[i],
                running_sub_agents: running_sub[i],
                last_active: agent_last_active[i],
                files: files[i],
                read_error: None,
                conversations: views.iter().filter(|s| s.agent == *agent && !s.sub_agent).count(),
                estimated: agent.estimates_tokens(),
                recent_tools: recent_tools.into_iter().take(20).cloned().collect(),
                net: net_stats(now, Some(*agent), &samples, &issues, last_ok[i], window_requests[i]),
            }
        })
        .collect();
    let net = net_stats(now, None, &samples, &issues, last_ok.iter().copied().max().unwrap_or(0), window_requests.iter().sum());
    let latency: Vec<LatencyPoint> = samples
        .iter()
        .skip(samples.len().saturating_sub(MAX_LATENCY_POINTS))
        .map(|sample| LatencyPoint { t: sample.ts, agent: sample.agent, latency_ms: sample.latency_ms, ttft_ms: sample.ttft_ms })
        .collect();
    issues.truncate(MAX_NET_EVENTS);

    // 最后一个时段是“全部”，其余时段里出现的模型它都有
    let model_names = ranges.last().map(|all| model_display_names(&all.models)).unwrap_or_default();
    let snapshot = Snapshot {
        generated_at: now,
        version: env.version,
        utc_offset_ms: offset,
        ranges: ranges.into_iter().map(|range| range.finish(&model_names)).collect(),
        live: LiveStats {
            running: running.iter().sum(),
            running_sub_agents: running_sub.iter().sum(),
            phases,
            sessions: live_sessions,
            agents,
            net,
            net_events: issues,
            latency,
            rate,
            recent,
            per_second,
            recorded: Recorded::default(),
            finished,
            rate_limits: rate_limits.into_iter().flatten().collect(),
        },
        heatmap,
        plugin: PluginInfo { sessions: plugin_ids.len() as u32, last_active: plugin_last_active, baishitong: None },
        scan: env.scan.clone(),
    };
    Analysis { snapshot, sessions: views }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Effort, NetEvent, Usage};
    use std::path::PathBuf;

    const NOW: i64 = 1_767_268_800_000 + 15 * HOUR; // 2026-01-01 12:00 UTC + 15h = 2026-01-02 03:00 UTC
    const OFFSET: i64 = 8 * HOUR; // 本地 2026-01-02 11:00

    fn event(ts: i64, input: u64, output: u64, key: u64) -> UsageEvent {
        UsageEvent {
            ts,
            usage: Usage { input, cache_read: input * 10, cache_write: 5, output, reasoning: output / 2 },
            model: 0,
            key,
            latency_ms: 4_000,
            ttft_ms: 900,
            think_ms: 300,
            reasoning_estimated: true,
            ..UsageEvent::default()
        }
    }

    fn entry(agent: Agent, name: &str, session: &str, cwd: &str) -> FileEntry {
        let mut entry = FileEntry::new(agent, PathBuf::from(name));
        entry.session.id = session.to_owned();
        entry.session.cwd = cwd.to_owned();
        entry.models.push(format!("{}-model", agent.id()));
        entry
    }

    fn env<'a>(procs: &'a [ClaudeProc], registry: bool) -> Env<'a> {
        Env { claude_registry: registry, claude_procs: procs, agent_alive: [None, Some(true), Some(false), None, None, None, None], detected: [true; Agent::COUNT], version: 7, scan: ScanStats::default() }
    }

    fn touch(entry: &mut FileEntry) {
        entry.session.started_ms = entry.events.iter().map(|event| event.ts).min().unwrap_or(0);
        entry.session.last_active_ms = entry.events.iter().map(|event| event.ts).max().unwrap_or(0);
    }

    #[test]
    fn sums_ranges_and_deduplicates() {
        let today = day_start(NOW, OFFSET);
        assert_eq!(today, NOW - 11 * HOUR);

        let mut claude = entry(Agent::Claude, "a.jsonl", "s-claude", "C:\\Work\\App");
        claude.events.push(event(today + 2 * HOUR + 5, 100, 40, 11)); // 今天 02 点
        claude.events.push(event(today - 3 * HOUR, 200, 60, 12)); // 昨天 21 点
        claude.events.push(event(today - 40 * DAY, 300, 80, 13)); // 40 天前
        touch(&mut claude);

        // 另一个文件里重复了 key=11 的响应，只该算一次
        let mut copy = entry(Agent::Claude, "b.jsonl", "s-claude-2", "c:/work/app/");
        copy.events.push(event(today + 2 * HOUR + 5, 100, 40, 11));
        copy.events.push(event(today + 3 * HOUR, 1_000, 10, 14));
        touch(&mut copy);

        let mut codex = entry(Agent::Codex, "c.jsonl", "s-codex", "D:\\other");
        codex.events.push(event(today + 30 * MINUTE, 50, 5, 0));
        touch(&mut codex);

        let analysis = analyze_entries(vec![&claude, &copy, &codex], &env(&[], false), NOW, OFFSET);
        let snapshot = &analysis.snapshot;
        assert_eq!(snapshot.ranges.iter().map(|range| range.id).collect::<Vec<_>>(), ["today", "7d", "30d", "all"]);

        let today_stats = &snapshot.ranges[0];
        assert_eq!(today_stats.totals.requests, 3);
        assert_eq!(today_stats.totals.input, 100 + 1_000 + 50);
        assert_eq!(today_stats.totals.cache_read, 10 * (100 + 1_000 + 50));
        assert_eq!(today_stats.totals.output, 40 + 10 + 5);
        assert_eq!(today_stats.totals.reasoning_estimated, today_stats.totals.reasoning);
        assert_eq!(today_stats.totals.total, today_stats.totals.input + today_stats.totals.cache_read + today_stats.totals.cache_write + today_stats.totals.output);
        assert_eq!(today_stats.sessions, 3);
        assert_eq!(today_stats.series.len(), 24);
        assert_eq!(today_stats.series[0].requests, 1);
        assert_eq!(today_stats.series[2].requests, 1);
        assert_eq!(today_stats.series[3].requests, 1);
        assert_eq!(today_stats.series[0].by_agent[Agent::Codex.index()], 50 + 500 + 5 + 5);
        // 昨天同一时段（到 11:00 为止）没有请求；昨天 21 点那条不在对比窗口里
        assert_eq!(today_stats.previous.unwrap().requests, 0);
        // 两个路径写法不同的目录算同一个项目
        assert_eq!(today_stats.by_project.len(), 2);
        assert_eq!(today_stats.by_project[0].totals.requests, 2);
        assert_eq!(today_stats.by_project[0].sessions, 2);
        assert_eq!(today_stats.by_model.len(), 2);

        assert_eq!(snapshot.ranges[1].totals.requests, 4);
        assert_eq!(snapshot.ranges[2].totals.requests, 4);
        let all = &snapshot.ranges[3];
        assert_eq!(all.totals.requests, 5);
        assert_eq!(all.unit, "day");
        assert_eq!(all.series.len(), 41);
        assert_eq!(all.series[0].requests, 1);
        assert_eq!(all.series[40].requests, 3);
        assert_eq!(all.from, today - 40 * DAY);

        assert_eq!(snapshot.heatmap.len(), HEAT_DAYS);
        assert_eq!(snapshot.heatmap[HEAT_DAYS - 1].requests, 3);
        assert_eq!(snapshot.heatmap[HEAT_DAYS - 2].requests, 1);
        assert_eq!(analysis.sessions.len(), 3);
        assert_eq!(snapshot.live.running, 0);
        assert_eq!(snapshot.version, 7);
    }

    #[test]
    fn detects_running_sessions() {
        // Claude：以进程登记为准
        let mut busy = entry(Agent::Claude, "busy.jsonl", "s-busy", "C:\\a");
        busy.events.push(event(NOW - 20 * SECOND, 10, 5, 1));
        touch(&mut busy);
        busy.live.open_turn(NOW - 60 * SECOND);
        busy.live.set_phase(Phase::Thinking, NOW - 5 * SECOND);

        let mut sub = entry(Agent::Claude, "busy/subagents/agent-1.jsonl", "s-busy", "C:\\a");
        sub.session.sub_agent = true;
        sub.events.push(event(NOW - 10 * SECOND, 7, 3, 2));
        touch(&mut sub);
        sub.live.open_turn(NOW - 30 * SECOND);
        sub.live.set_phase(Phase::Tool, NOW - 8 * SECOND);

        // 记录里这一轮没收尾，但登记里已经不忙了
        let mut idle = entry(Agent::Claude, "idle.jsonl", "s-idle", "C:\\b");
        idle.events.push(event(NOW - 20 * SECOND, 10, 5, 3));
        touch(&mut idle);
        idle.live.open_turn(NOW - 60 * SECOND);

        // Codex：靠记录末尾判断
        let mut codex = entry(Agent::Codex, "codex.jsonl", "s-codex", "C:\\c");
        codex.events.push(event(NOW - 2 * MINUTE, 10, 5, 0));
        touch(&mut codex);
        codex.live.open_turn(NOW - 5 * MINUTE);
        codex.live.set_phase(Phase::Writing, NOW - 2 * MINUTE);

        let mut stale = entry(Agent::Codex, "stale.jsonl", "s-stale", "C:\\c");
        stale.events.push(event(NOW - 3 * HOUR, 10, 5, 0));
        touch(&mut stale);
        stale.live.open_turn(NOW - 3 * HOUR);

        // Deepseek：进程不在，两分钟没动静就不算运行
        let mut dsh = entry(Agent::Deepseek, "dsh.zstd", "s-dsh", "C:\\d");
        dsh.events.push(event(NOW - 2 * MINUTE, 10, 5, 0));
        dsh.net.push(NetEvent { ts: NOW - MINUTE, kind: NetKind::Retry, code: "ETIMEDOUT".into(), attempt: 1, max_attempts: 5, delay_ms: 500 });
        touch(&mut dsh);
        dsh.live.open_turn(NOW - 5 * MINUTE);

        // 刚结束的一轮
        let mut done = entry(Agent::Antigravity, "done.db", "s-done", "C:\\e");
        done.events.push(event(NOW - 40 * SECOND, 10, 5, 0));
        touch(&mut done);
        done.live.open_turn(NOW - 90 * SECOND);
        done.live.close_turn(NOW - 30 * SECOND, None);

        let procs = [
            ClaudeProc { pid: 1, session_id: "s-busy".into(), status: "busy".into(), alive: true, name: "demo".into(), ..ClaudeProc::default() },
            ClaudeProc { pid: 2, session_id: "s-idle".into(), status: "idle".into(), alive: true, ..ClaudeProc::default() },
            ClaudeProc { pid: 3, session_id: "s-new".into(), status: "busy".into(), alive: true, status_updated_ms: NOW - 2 * SECOND, ..ClaudeProc::default() },
            ClaudeProc { pid: 4, session_id: "s-dead".into(), status: "busy".into(), alive: false, ..ClaudeProc::default() },
        ];
        let analysis = analyze_entries(vec![&busy, &sub, &idle, &codex, &stale, &dsh, &done], &env(&procs, true), NOW, OFFSET);
        let live = &analysis.snapshot.live;

        // s-busy、s-new（只有登记、还没有记录）、s-codex
        assert_eq!(live.running, 3);
        assert_eq!(live.running_sub_agents, 1);
        assert_eq!(live.phases, PhaseCounts { requesting: 1, thinking: 1, writing: 1, ..PhaseCounts::default() });
        let by_key = |key: &str| analysis.sessions.iter().find(|view| view.key == key).unwrap();
        let busy_view = by_key("claude:s-busy");
        assert!(busy_view.running);
        assert_eq!(busy_view.phase, Phase::Thinking);
        assert_eq!(busy_view.title, "demo");
        assert_eq!(busy_view.sub_agents, 1);
        assert_eq!(busy_view.running_sub_agents, 1);
        assert_eq!(busy_view.totals.requests, 2);
        assert_eq!(busy_view.turn.requests, 2);
        assert!(!by_key("claude:s-idle").running);
        assert!(by_key("claude:s-new").running);
        assert!(by_key("codex:s-codex").running);
        assert!(!by_key("codex:s-stale").running);
        assert!(!by_key("deepseek:s-dsh").running);
        assert!(analysis.sessions.iter().all(|view| view.key != "claude:s-dead"));
        assert!(live.sessions[0].running);

        assert_eq!(live.finished.len(), 1);
        assert_eq!(live.finished[0].agent, Agent::Antigravity);
        assert_eq!(live.finished[0].duration_ms, 60_000);
        assert_eq!(live.finished[0].totals.requests, 1);

        let dsh_live = &live.agents[Agent::Deepseek.index()];
        assert_eq!(dsh_live.net.retries, 1);
        assert_eq!(dsh_live.net.status, NetStatus::Unstable);
        assert_eq!(dsh_live.net.last_code, "ETIMEDOUT");
        assert_eq!(live.agents[Agent::Codex.index()].net.status, NetStatus::Ok);
        assert_eq!(live.agents[Agent::Codex.index()].net.latency_ms, 4_000);
        assert_eq!(live.agents[Agent::Codex.index()].running, 1);
        assert_eq!(live.net.status, NetStatus::Unstable);
        assert_eq!(live.net_events.len(), 1);
        assert_eq!(live.rate.len(), 60);
        assert_eq!(live.per_second.len(), 60);
        assert_eq!(live.per_second[59].t, NOW);
        let point = &live.per_second[39];
        assert_eq!(point.t, NOW - 20 * SECOND);
        assert_eq!((point.input, point.cache_read, point.cache_write, point.output, point.reasoning, point.requests), (20, 200, 10, 10, 4, 2));
        assert_eq!(live.per_second.iter().map(|p| p.requests).sum::<u64>(), 4);
        assert!(live.rate.iter().map(|point| point.requests).sum::<u64>() >= 6);
        // 5 秒一格，共 10 分钟
        assert_eq!(live.recent.len(), 120);
        assert_eq!(live.recent[119].t, NOW);
        assert_eq!(live.recent[115].requests, 2);
        assert_eq!(live.recent[117].requests, 1);
        assert_eq!(live.recent.iter().map(|p| p.requests).sum::<u64>(), 6);
    }

    #[test]
    fn splits_models_by_effort_and_lists_plugin_sessions() {
        let today = day_start(NOW, OFFSET);
        let mut codex = entry(Agent::Codex, "c.jsonl", "s-codex", "D:\\other");
        codex.events.push(UsageEvent { effort: Effort::XHigh, ..event(today + HOUR, 100, 10, 0) });
        codex.events.push(UsageEvent { effort: Effort::Low, ..event(today + 2 * HOUR, 10, 1, 0) });
        codex.events.push(UsageEvent { effort: Effort::XHigh, ..event(today + 3 * HOUR, 100, 10, 0) });
        touch(&mut codex);

        // 来源没有记录强度：看模型名后缀
        let mut cursor = entry(Agent::Cursor, "state.vscdb", "c-1", "D:\\other");
        cursor.models[0] = "claude-opus-5-5-max-fast".into();
        cursor.events.push(event(today + HOUR, 50, 5, 0));
        touch(&mut cursor);

        // 插件卡片：一张独立的，一张并在 Cursor 对话 c-2 里
        let mut card = entry(Agent::Cursor, "card-1/process.json", "gateway/card-1", "");
        card.plugin = true;
        card.events.push(event(today + 4 * HOUR, 30, 3, 0));
        touch(&mut card);
        let mut linked = entry(Agent::Cursor, "card-2/process.json", "c-2", "");
        linked.plugin = true;
        linked.events.push(event(today + 5 * HOUR, 20, 2, 0));
        touch(&mut linked);
        let mut host = entry(Agent::Cursor, "state.vscdb#c-2", "c-2", "D:\\other");
        host.events.push(event(today + 5 * HOUR + MINUTE, 40, 4, 0));
        touch(&mut host);

        let analysis = analyze_entries(vec![&codex, &cursor, &card, &linked, &host], &env(&[], false), NOW, OFFSET);
        let today_stats = &analysis.snapshot.ranges[0];
        let model = |name: &str| today_stats.by_model.iter().find(|model| model.model == name).unwrap();
        let codex_model = model("codex-model");
        assert_eq!(codex_model.totals.requests, 3);
        assert_eq!(codex_model.efforts.iter().map(|part| (part.effort, part.totals.requests)).collect::<Vec<_>>(), [(Effort::XHigh, 2), (Effort::Low, 1)]);
        assert_eq!(codex_model.efforts.iter().map(|part| part.totals.total).sum::<u64>(), codex_model.totals.total);
        assert_eq!(codex_model.agents.iter().map(|part| (part.agent, part.totals.requests)).collect::<Vec<_>>(), [(Agent::Codex, 3)]);
        // 强度后缀不算在模型名里，`-max-fast` 自成一档
        assert_eq!(model("claude-opus-5-5").efforts[0].effort, Effort::MaxFast);
        assert_eq!(model("cursor-model").efforts[0].effort, Effort::Unknown);

        assert_eq!(today_stats.plugin.sessions, 2);
        assert_eq!(today_stats.plugin.totals.requests, 3);
        assert_eq!(today_stats.by_agent[Agent::Cursor.index()].totals.requests, 4);
        assert_eq!(analysis.snapshot.plugin.sessions, 2);
        assert_eq!(analysis.snapshot.plugin.last_active, today + 5 * HOUR);
        // 对话列表里插件对话带着标记：独立的卡片，和并了卡片的那段 Cursor 对话
        let mut marked: Vec<&str> = analysis.sessions.iter().filter(|view| view.plugin).map(|view| view.id.as_str()).collect();
        marked.sort_unstable();
        assert_eq!(marked, ["c-2", "gateway/card-1"]);
        assert!(analysis.snapshot.live.sessions.iter().all(|view| view.plugin == marked.contains(&view.id.as_str())));
    }

    #[test]
    fn merges_a_model_across_spellings_suffixes_and_agents() {
        let today = day_start(NOW, OFFSET);
        // Claude Code 自己记了强度
        let mut claude = entry(Agent::Claude, "a.jsonl", "s-claude", "D:\\work");
        claude.models[0] = "claude-opus-5-5".into();
        claude.events.push(UsageEvent { effort: Effort::Max, ..event(today + HOUR, 100, 10, 0) });
        claude.events.push(UsageEvent { effort: Effort::High, ..event(today + 2 * HOUR, 100, 10, 0) });
        touch(&mut claude);

        // Cursor 的强度在模型名后缀里；同一个模型还有带空格和小数点的写法
        let mut cursor = entry(Agent::Cursor, "state.vscdb", "c-1", "D:\\work");
        cursor.models = ["claude-opus-5-5-max-fast", "claude-opus-5-5-max", "claude-opus-5-5", "Claude Fable 5.2", "claude-fable-5-2-max-fast"].map(String::from).to_vec();
        cursor.events.push(UsageEvent { model: 0, ..event(today + HOUR, 300, 30, 0) });
        // 旧版把 `-max-fast` 记成了 max
        cursor.events.push(UsageEvent { model: 0, effort: Effort::Max, ..event(today + HOUR, 300, 30, 0) });
        cursor.events.push(UsageEvent { model: 1, ..event(today + HOUR, 50, 5, 0) });
        cursor.events.push(UsageEvent { model: 2, ..event(today + HOUR, 50, 5, 0) });
        cursor.events.push(UsageEvent { model: 3, ..event(today - 2 * DAY, 500, 50, 0) });
        cursor.events.push(UsageEvent { model: 4, ..event(today + HOUR, 20, 2, 0) });
        touch(&mut cursor);

        let mut kiro = entry(Agent::Kiro, "messages.jsonl", "k-1", "D:\\work");
        kiro.models[0] = "claude-opus-5.5".into();
        kiro.events.push(event(today + HOUR, 10, 1, 0));
        touch(&mut kiro);

        let analysis = analyze_entries(vec![&claude, &cursor, &kiro], &env(&[], false), NOW, OFFSET);
        let today_stats = &analysis.snapshot.ranges[0];
        // 今天只用过 `claude-fable-5-2-max-fast`，名字仍然按累计用得最多的写法显示
        assert_eq!(today_stats.by_model.iter().map(|model| model.model.as_str()).collect::<Vec<_>>(), ["claude-opus-5-5", "Claude Fable 5.2"]);

        let opus = &today_stats.by_model[0];
        assert_eq!(opus.totals.requests, 7);
        assert_eq!(opus.agents.iter().map(|part| (part.agent, part.totals.requests)).collect::<Vec<_>>(), [(Agent::Cursor, 4), (Agent::Claude, 2), (Agent::Kiro, 1)]);
        assert_eq!(
            opus.efforts.iter().map(|part| (part.effort, part.totals.requests)).collect::<Vec<_>>(),
            [(Effort::MaxFast, 2), (Effort::Max, 2), (Effort::High, 1), (Effort::Unknown, 2)]
        );
        assert_eq!(opus.agents.iter().map(|part| part.totals.total).sum::<u64>(), opus.totals.total);
        assert_eq!(opus.efforts.iter().map(|part| part.totals.total).sum::<u64>(), opus.totals.total);

        let fable = &today_stats.by_model[1];
        assert_eq!(fable.efforts.iter().map(|part| (part.effort, part.totals.requests)).collect::<Vec<_>>(), [(Effort::MaxFast, 1)]);
        assert_eq!(fable.agents.iter().map(|part| part.agent).collect::<Vec<_>>(), [Agent::Cursor]);

        let week = &analysis.snapshot.ranges[1];
        let fable = week.by_model.iter().find(|model| model.model == "Claude Fable 5.2").unwrap();
        assert_eq!(fable.efforts.iter().map(|part| (part.effort, part.totals.requests)).collect::<Vec<_>>(), [(Effort::Unknown, 1), (Effort::MaxFast, 1)]);
        assert_eq!(week.by_model.len(), 2);
        assert_eq!(week.by_model.iter().map(|model| model.totals.requests).sum::<u64>(), week.totals.requests);
    }
}
