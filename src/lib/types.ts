// 与 crates/foxes-core/src/aggregate.rs 里的序列化结构一一对应。

export type Agent = 'claude' | 'codex' | 'deepseek' | 'antigravity' | 'kiro' | 'cursor' | 'webmcp'

/** 保持后台 Agent::ALL 的索引；Cursor 内的网页 MCP 一并归入 Cursor。 */
export const AGENTS: Agent[] = ['claude', 'codex', 'deepseek', 'antigravity', 'kiro', 'cursor']

export type Phase = 'idle' | 'requesting' | 'thinking' | 'writing' | 'generating' | 'tool' | 'retrying' | 'waiting'

export type NetStatus = 'idle' | 'ok' | 'slow' | 'unstable' | 'down'

export type RangeId = 'today' | '7d' | '30d' | 'all'

/** 思考强度，前八档从低到高；unknown 表示来源没有记录。带 -fast 的是模型名里强度后面还跟着 fast 的那一档（-max-fast），和不带的分开算 */
export type Effort = 'unknown' | 'off' | 'minimal' | 'low' | 'medium' | 'high' | 'xhigh' | 'max' | 'minimal-fast' | 'low-fast' | 'medium-fast' | 'high-fast' | 'xhigh-fast' | 'max-fast'

export interface Totals {
  input: number
  cacheRead: number
  cacheWrite: number
  output: number
  /** 思考 token，是 output 的一部分 */
  reasoning: number
  /** reasoning 里估算出来的部分 */
  reasoningEstimated: number
  total: number
  /** total 里整条估算的部分（Kiro / Cursor / 网页 MCP 的记录里没有 token 数） */
  estimated: number
  /** 消耗的计费额度（目前只有 Kiro 记录 credits） */
  credits: number
  requests: number
  thinkMs: number
}

export interface SeriesPoint {
  t: number
  input: number
  cacheRead: number
  cacheWrite: number
  output: number
  reasoning: number
  requests: number
  /** 各 agent 的总量，顺序同 AGENTS */
  byAgent: number[]
}

export interface AgentTotals {
  agent: Agent
  totals: Totals
  sessions: number
}

export interface EffortTotals {
  effort: Effort
  totals: Totals
}

/** 某个 agent 在一个模型上的用量 */
export interface ModelAgentTotals {
  agent: Agent
  totals: Totals
}

/** 一个模型的用量：各个 agent 用的、名字写法不同的、带不同强度后缀的都并在这一条 */
export interface ModelTotals {
  /** 按思考强度拆开，用量高的在前 */
  efforts: EffortTotals[]
  /** 用过它的各个 agent，用量高的在前 */
  agents: ModelAgentTotals[]
  /** 不带强度后缀的模型名 */
  model: string
  totals: Totals
}

export interface ProjectTotals {
  project: string
  cwd: string
  totals: Totals
  sessions: number
  agents: Agent[]
}

/** Cursor 插件（网关）对话的用量，已经包含在 Cursor 的数字里 */
export interface PluginTotals {
  totals: Totals
  sessions: number
}

export interface RangeStats {
  plugin: PluginTotals
  id: RangeId
  from: number
  to: number
  totals: Totals
  previous: Totals | null
  sessions: number
  byAgent: AgentTotals[]
  byModel: ModelTotals[]
  byProject: ProjectTotals[]
  unit: 'hour' | 'day' | 'week'
  stepMs: number
  series: SeriesPoint[]
}

export interface SessionView {
  key: string
  agent: Agent
  id: string
  title: string
  project: string
  cwd: string
  model: string
  running: boolean
  subAgent: boolean
  phase: Phase
  phaseSince: number
  tool: string
  turnStarted: number
  started: number
  lastActive: number
  totals: Totals
  turn: Totals
  contextUsed: number
  contextWindow: number
  turns: number
  subAgents: number
  runningSubAgents: number
  sourceGone: boolean
  /** 是不是插件（网关、百事通）里的对话 */
  plugin: boolean
  /** 这段会话是别的电脑记下的，由用量历史带过来 */
  sourceElsewhere: boolean
}

export interface NetStats {
  status: NetStatus
  latencyMs: number
  ttftMs: number
  samples: number
  requests: number
  retries: number
  errors: number
  lastOk: number
  lastIssue: number
  lastCode: string
}

export interface NetEventView {
  ts: number
  agent: Agent
  kind: 'retry' | 'error'
  code: string
  attempt: number
  maxAttempts: number
  delayMs: number
  project: string
}

export interface LatencyPoint {
  t: number
  agent: Agent
  latencyMs: number
  ttftMs: number
}

export interface AgentLive {
  agent: Agent
  label: string
  detected: boolean
  readError: string | null
  processAlive: boolean | null
  running: number
  runningSubAgents: number
  lastActive: number
  files: number
  /** 见过的对话数（不含子代理） */
  conversations: number
  recentTools: { ts: number; name: string; server: string; ok: boolean }[]
  /** 这家的 token 是估算的（来源不记录 token） */
  estimated: boolean
  net: NetStats
}

export interface RatePoint {
  t: number
  input: number
  cacheRead: number
  cacheWrite: number
  output: number
  reasoning: number
  requests: number
}

/** 按“记下来的时刻”算的用量：来源记下一次请求总比它实际发生晚一截（百事通晚几十秒到几分钟） */
export interface Recorded {
  /** 近一分钟里记下的 token 数（输入 + 缓存 + 输出） */
  minute: number
  /** 最近一次记下用量的时刻；启动以来还没有记到过时为 0 */
  lastAt: number
}

export interface PhaseCounts {
  requesting: number
  thinking: number
  writing: number
  generating: number
  tool: number
  retrying: number
  waiting: number
}

export interface FinishedTurn {
  key: string
  agent: Agent
  title: string
  project: string
  endedAt: number
  durationMs: number
  totals: Totals
}

export interface RateWindow {
  usedPercent: number
  windowMinutes: number
  resetsAtMs: number
  /** 已用量和上限（按 unit 计）；limit 为 0 表示来源只给了百分比 */
  used: number
  limit: number
  /** 计量单位，比如 credits；空表示未知 */
  unit: string
  /** 额度名称；空时按 windowMinutes 命名 */
  label: string
  /** resetsAtMs 是到期时刻（赠送额度到期作废），不是重置时刻 */
  expires: boolean
}

export interface RateLimitView {
  agent: Agent
  ts: number
  plan: string
  primary: RateWindow | null
  secondary: RateWindow | null
}

export interface LiveStats {
  running: number
  runningSubAgents: number
  phases: PhaseCounts
  sessions: SessionView[]
  agents: AgentLive[]
  net: NetStats
  netEvents: NetEventView[]
  latency: LatencyPoint[]
  rate: RatePoint[]
  /** 近 10 分钟，每 5 秒一格 */
  recent: RatePoint[]
  perSecond: RatePoint[]
  recorded: Recorded
  finished: FinishedTurn[]
  rateLimits: RateLimitView[]
}

export interface DayCell {
  t: number
  total: number
  requests: number
}

export interface HistoryInfo {
  /** 这台电脑记下的会话数 */
  sessions: number
  /** 历史里另外还有几台电脑的记录 */
  devices: number
  /** 别的电脑记下、这台电脑上没有的会话数（算在总数里） */
  elsewhere: number
  /** 跟着程序走的那一份放在哪；空串表示没有这一份 */
  carried: string
}

export interface ScanStats {
  historyError?: string | null
  history: HistoryInfo
  files: number
  bytes: number
  events: number
  failing: number
  parsed: number
  scanMs: number
  firstScanMs: number
  scans: number
  /** 最近一次汇总花的毫秒数 */
  analysisMs?: number
  /** 统计出错后自动重来的次数，和最近一次的原因 */
  monitorRestarts?: number
  monitorError?: string | null
}

/** 对话明细里的一次请求 */
export interface RequestRow {
  ts: number
  model: string
  effort: Effort
  input: number
  cacheRead: number
  cacheWrite: number
  output: number
  reasoning: number
  total: number
  latencyMs: number
  ttftMs: number
  thinkMs: number
  credits: number
  estimated: boolean
  subAgent: boolean
}

/** 一次工具调用：只有工具名和成败 */
export interface ToolCall {
  ts: number
  name: string
  /** MCP 服务名；内置工具为空 */
  server: string
  ok: boolean
}

/** 一段对话的逐次请求和工具调用 */
export interface SessionDetail {
  key: string
  /** 请求总数；requests 只带最近的一批 */
  requestCount: number
  requests: RequestRow[]
  toolCount: number
  tools: ToolCall[]
  /** 原始记录不在这台电脑上：只有用量，没有工具记录 */
  usageOnly: boolean
}

/** 和百事通对话转发口连得怎么样 */
export type BaishitongLink = 'unknown' | 'online' | 'offline' | 'foreign' | 'denied' | 'busy' | 'failing'

export interface BaishitongStatus {
  link: BaishitongLink
  /** 列表里的对话数，其中正在运行的、还没读过过程记录的 */
  sessions: number
  busy: number
  pending: number
  /** 上一次读成的时刻，0 表示还没读成过 */
  okAt: number
  /** 出了错在等的话，下一次什么时候再试；没在等为 0 */
  retryAt: number
}

export interface PluginInfo {
  /** 本机能读到的插件（网关）对话数 */
  sessions: number
  /** 百事通对话读得怎么样；没有去读（比如预览时）为 null */
  baishitong: BaishitongStatus | null
  /** 其中最近一次活动的时刻，0 表示一条也没有 */
  lastActive: number
}

export interface Snapshot {
  plugin: PluginInfo
  generatedAt: number
  version: number
  utcOffsetMs: number
  ranges: RangeStats[]
  live: LiveStats
  heatmap: DayCell[]
  scan: ScanStats
}

// —— 单个 agent 的详情（get_agent_detail）——

export interface AgentPoint {
  t: number
  input: number
  cacheRead: number
  cacheWrite: number
  output: number
  reasoning: number
  requests: number
  credits: number
}

export interface AgentRange {
  id: RangeId
  from: number
  to: number
  totals: Totals
  previous: Totals | null
  /** 有用量的对话数（不含子代理） */
  sessions: number
  unit: 'hour' | 'day' | 'week'
  stepMs: number
  series: AgentPoint[]
  byModel: ModelTotals[]
  byProject: ProjectTotals[]
}

export interface ToolStat {
  name: string
  /** MCP 服务名；内置工具为空 */
  server: string
  calls: number
  errors: number
  last: number
}

export interface ServerStat {
  server: string
  calls: number
  tools: number
  errors: number
  last: number
}

export interface HourCell {
  /** 本地时间 0~23 点 */
  hour: number
  total: number
  requests: number
}

export interface AgentDetail {
  agent: Agent
  label: string
  /** token 是估算的 */
  estimated: boolean
  /** 估算口径的说明；不估算时为空 */
  estimateNote: string
  /** today / 7d / 30d / all */
  ranges: AgentRange[]
  /** 近 26 周每天 */
  heatmap: DayCell[]
  /** 近 30 天按小时分布 */
  hours: HourCell[]
  /** 调用次数最多的工具（全部时间） */
  tools: ToolStat[]
  /** 用到的 MCP 服务 */
  servers: ServerStat[]
  toolCalls: number
  toolErrors: number
  conversations: number
  subAgents: number
  /** 跑完的轮数 */
  turns: number
  firstSeen: number
  lastActive: number
}

export type UnitStyle = 'kmb' | 'wanyi'

export interface Settings {
  /** 显示灵动岛 */
  island: boolean
  edgeAutoHide: boolean
  /** 开机自启 */
  autostart: boolean
  /** 对话跑完一轮时在灵动岛上提示 */
  notifyFinished: boolean
  /** 数字单位：K/M/B 或 万/亿 */
  units: UnitStyle
  /** system：跟随系统的深浅色 */
  theme: 'dark' | 'light' | 'system'
  /** 少用动画，省处理器 */
  reduceMotion: boolean
}

export interface AppInfo {
  version: string
  /** 各 agent 的数据目录，顺序同 AGENTS */
  roots: string[][]
  /** 设置文件保存的位置 */
  settingsPath: string
}
