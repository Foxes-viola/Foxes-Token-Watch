// 开发时用浏览器直接打开页面用的假数据，只为了调界面；软件里不会用到。
import { version } from '../../package.json'
import { AGENTS } from './types'
import type {
  Agent,
  AgentLive,
  AppInfo,
  BaishitongLink,
  DayCell,
  Effort,
  LatencyPoint,
  ModelTotals,
  NetStats,
  Phase,
  RangeId,
  RangeStats,
  RatePoint,
  RequestRow,
  SeriesPoint,
  SessionDetail,
  SessionView,
  Snapshot,
  Totals,
} from './types'

const MINUTE = 60_000
const HOUR = 3_600_000
const DAY = 86_400_000

function rng(seed: number) {
  let state = seed >>> 0
  return () => {
    state = (state * 1664525 + 1013904223) >>> 0
    return state / 4294967296
  }
}

/**
 * 预览时在地址后面加参数，看内容多和少两种情况下的排版，比如 ?sessions=1&projects=2&models=2&events=1&quotas=1：
 * 数字是各自给几条（对话、项目、模型、网络异常、额度），不写的那几项用默认的示例数据。
 */
const PREVIEW = new URLSearchParams(window.location.search)

function resized<T>(items: T[], name: string, more: (index: number) => T): T[] {
  const raw = PREVIEW.get(name)
  const count = raw === null || raw.trim() === '' ? NaN : Number(raw)
  if (!Number.isFinite(count) || count < 0) return items
  return Array.from({ length: Math.floor(count) }, (_, index) => items[index] ?? more(index))
}

const ZERO: Totals = { input: 0, cacheRead: 0, cacheWrite: 0, output: 0, reasoning: 0, reasoningEstimated: 0, estimated: 0, credits: 0, total: 0, requests: 0, thinkMs: 0 }

function add(a: Totals, b: Totals): Totals {
  return {
    estimated: a.estimated + b.estimated, credits: a.credits + b.credits,
    input: a.input + b.input,
    cacheRead: a.cacheRead + b.cacheRead,
    cacheWrite: a.cacheWrite + b.cacheWrite,
    output: a.output + b.output,
    reasoning: a.reasoning + b.reasoning,
    reasoningEstimated: a.reasoningEstimated + b.reasoningEstimated,
    total: a.total + b.total,
    requests: a.requests + b.requests,
    thinkMs: a.thinkMs + b.thinkMs,
  }
}

/** 各 agent 的 token 构成：未命中输入、缓存写入、输出占总量的比例，思考占输出的比例 */
const MIX: Record<Agent, { input: number; write: number; output: number; think: number; estimated: boolean; weight: number }> = {
  kiro: { input: 0.5, write: 0, output: 0.08, think: 0.2, estimated: true, weight: 0.02 },
  cursor: { input: 0.35, write: 0, output: 0.65, think: 0.2, estimated: true, weight: 0.015 },
  webmcp: { input: 0.4, write: 0, output: 0.6, think: 0.3, estimated: true, weight: 0.01 },
  claude: { input: 0.012, write: 0.11, output: 0.014, think: 0.45, estimated: true, weight: 0.56 },
  codex: { input: 0.026, write: 0, output: 0.006, think: 0.38, estimated: false, weight: 0.14 },
  deepseek: { input: 0.009, write: 0, output: 0.0035, think: 0.5, estimated: true, weight: 0.26 },
  antigravity: { input: 0.08, write: 0, output: 0.011, think: 0.4, estimated: false, weight: 0.04 },
}

function split(agent: Agent, total: number): Totals {
  const mix = MIX[agent]
  const input = Math.round(total * mix.input)
  const cacheWrite = Math.round(total * mix.write)
  const output = Math.round(total * mix.output)
  const reasoning = Math.round(output * mix.think)
  return {
    estimated: ['kiro','cursor','webmcp'].includes(agent) ? total : 0, credits: 0,
    input,
    cacheRead: total - input - cacheWrite - output,
    cacheWrite,
    output,
    reasoning,
    reasoningEstimated: mix.estimated ? reasoning : 0,
    total,
    requests: Math.max(1, Math.round(total / 165_000)),
    thinkMs: Math.round(reasoning * 18),
  }
}

function point(t: number, perAgent: number[]): SeriesPoint {
  let sum = ZERO
  perAgent.forEach((total, index) => {
    if (total > 0) sum = add(sum, split(AGENTS[index], total))
  })
  return {
    t,
    input: sum.input,
    cacheRead: sum.cacheRead,
    cacheWrite: sum.cacheWrite,
    output: sum.output,
    reasoning: sum.reasoning,
    requests: sum.requests,
    byAgent: perAgent,
  }
}

/** 各模型在不同思考强度下的用量占比；没列出的算没有记录 */
const EFFORTS: Record<string, [Effort, number][]> = {
  'claude-opus-5-5': [['high', 0.44], ['max-fast', 0.26], ['max', 0.2], ['medium', 0.06], ['unknown', 0.04]],
  'deepseek-flash': [['high', 0.7], ['off', 0.3]],
  'gpt-6.1-sol': [['xhigh', 0.55], ['medium', 0.45]],
  'Claude Fable 5.2': [['medium', 0.55], ['max-fast', 0.3], ['unknown', 0.15]],
  'gemini-4-argon': [['high', 1]],
  'codex-auto-review': [['low', 1]],
}
const NO_EFFORT: [Effort, number][] = [['unknown', 1]]

/** 模型名、用它的各个 agent 各占多少（用量多的在前）、它占总量的比例 */
const MODELS: [string, [Agent, number][], number][] = [
  ['claude-opus-5-5', [['claude', 0.7], ['cursor', 0.26], ['kiro', 0.04]], 0.5],
  ['deepseek-flash', [['deepseek', 1]], 0.26],
  ['gpt-6.1-sol', [['codex', 1]], 0.12],
  ['Claude Fable 5.2', [['claude', 0.55], ['cursor', 0.45]], 0.068],
  ['gemini-4-argon', [['antigravity', 1]], 0.03],
  ['codex-auto-review', [['codex', 1]], 0.02],
  ['gemini-3.8-flash', [['antigravity', 1]], 0.01],
  ['claude-sonnet-4.5', [['kiro', 1]], 0.004],
]

function modelTotals(model: string, agents: [Agent, number][], total: number): ModelTotals {
  const efforts = (EFFORTS[model] ?? NO_EFFORT).map(([effort, share]) => ({ effort, totals: split(agents[0][0], Math.round(total * share)) }))
  return {
    model,
    totals: efforts.map((item) => item.totals).reduce(add, ZERO),
    efforts,
    agents: agents.map(([agent, share]) => ({ agent, totals: split(agent, Math.round(total * share)) })),
  }
}

const PROJECTS: [string, number, Agent[]][] = [
  ['shop-admin', 0.34, ['claude', 'codex']],
  ['data-pipeline', 0.22, ['deepseek', 'claude']],
  ['mobile-app', 0.16, ['claude']],
  ['docs-site', 0.11, ['codex', 'antigravity']],
  ['infra', 0.09, ['deepseek']],
  ['playground', 0.08, ['antigravity', 'claude']],
]

const TITLES = ['重构登录流程', '修复导出 CSV 的编码问题', '给订单列表加分页', '排查内存泄漏', '写单元测试', '升级依赖并修复类型错误', '优化首屏加载', '迁移到新的鉴权接口']

function buildRange(id: RangeId, now: number, random: () => number): RangeStats {
  const dayStart = new Date(now).setHours(0, 0, 0, 0)
  const series: SeriesPoint[] = []
  let from = dayStart
  let unit: RangeStats['unit'] = 'day'
  let stepMs = DAY
  if (id === 'today') {
    unit = 'hour'
    stepMs = HOUR
    const currentHour = new Date(now).getHours()
    for (let hour = 0; hour < 24; hour++) {
      const busy = hour >= 9 && hour <= 23 ? 1 : 0.08
      const active = hour <= currentHour
      series.push(point(dayStart + hour * HOUR, AGENTS.map((agent) => (active ? Math.round(MIX[agent].weight * busy * (0.3 + random()) * 6_000_000) : 0))))
    }
  } else {
    const days = id === '7d' ? 7 : id === '30d' ? 30 : 34
    from = dayStart - (days - 1) * DAY
    for (let day = 0; day < days; day++) {
      const ramp = id === 'all' ? Math.min(1, 0.15 + day / 20) : 1
      series.push(point(from + day * DAY, AGENTS.map((agent) => Math.round(MIX[agent].weight * ramp * (0.25 + random() * 1.1) * 82_000_000))))
    }
  }
  const agentTotals = AGENTS.map((agent, index) => split(agent, series.reduce((sum, item) => sum + item.byAgent[index], 0)))
  const totals = agentTotals.reduce(add, ZERO)
  const previousScale = id === 'today' ? 0.42 : id === '7d' ? 0.61 : 1.3
  return {
    id,
    from,
    to: now,
    totals,
    previous: id === 'all' ? null : AGENTS.map((agent, index) => split(agent, Math.round(agentTotals[index].total * previousScale))).reduce(add, ZERO),
    sessions: id === 'today' ? 9 : id === '7d' ? 41 : id === '30d' ? 96 : 127,
    byAgent: AGENTS.map((agent, index) => ({ agent, totals: agentTotals[index], sessions: Math.round((id === 'today' ? 9 : 60) * MIX[agent].weight) + 1 })),
    byModel: resized<(typeof MODELS)[number]>(MODELS, 'models', (index) => [`sample-model-${index + 1}`, [['codex', 1]], 0.02 / (index + 1)]).map(([model, agents, share]) =>
      modelTotals(model, agents, Math.round(totals.total * share)),
    ),
    plugin: { totals: split('cursor', Math.round(agentTotals[AGENTS.indexOf('cursor')].total * 0.8)), sessions: id === 'today' ? 1 : 6 },
    byProject: resized<(typeof PROJECTS)[number]>(PROJECTS, 'projects', (index) => [`sample-project-${index + 1}`, 0.3 / (index + 1), ['codex']]).map(([project, share, agents]) => ({
      project,
      cwd: `c:/work/${project}`,
      totals: split(agents[0], Math.round(totals.total * share)),
      sessions: Math.round(share * 40) + 1,
      agents,
    })),
    unit,
    stepMs,
    series,
  }
}

function session(index: number, agent: Agent, now: number, running: boolean, phase: Phase, sincePhase: number, tool = ''): SessionView {
  const [project] = PROJECTS[index % PROJECTS.length]
  const totals = split(agent, 2_400_000 + index * 5_300_000)
  return {
    key: `${agent}:mock-${index}`,
    agent,
    id: `mock-${index.toString().padStart(4, '0')}`,
    title: TITLES[index % TITLES.length],
    project,
    cwd: `c:/work/${project}`,
    model: MODELS.find(([, agents]) => agents.some(([owner]) => owner === agent))?.[0] ?? '',
    running,
    subAgent: false,
    phase: running ? phase : 'idle',
    phaseSince: now - sincePhase,
    tool,
    turnStarted: running ? now - sincePhase - 95_000 : 0,
    started: now - (index + 1) * 47 * MINUTE,
    lastActive: running ? now - 1500 : now - (index + 1) * 13 * MINUTE,
    totals,
    turn: running ? split(agent, 310_000 + index * 420_000) : ZERO,
    contextUsed: 64_000 + index * 31_000,
    contextWindow: agent === 'claude' ? 0 : 400_000,
    turns: 3 + index * 2,
    subAgents: index === 0 ? 2 : 0,
    runningSubAgents: index === 0 && running ? 1 : 0,
    sourceGone: false,
    plugin: false,
    sourceElsewhere: false,
  }
}

function net(status: NetStats['status'], latencyMs: number, ttftMs: number, retries: number, now: number): NetStats {
  return {
    status,
    latencyMs,
    ttftMs,
    samples: status === 'idle' ? 0 : 12,
    requests: status === 'idle' ? 0 : 18,
    retries,
    errors: 0,
    lastOk: status === 'idle' ? 0 : now - 4000,
    lastIssue: retries ? now - 3 * MINUTE : 0,
    lastCode: retries ? 'HTTP 502' : '',
  }
}

function liveSessions(now: number): SessionView[] {
  const cycle = Math.floor(now / 6000) % 3
  const claudePhase: Phase = (['thinking', 'writing', 'tool'] as Phase[])[cycle]
  return [
    session(0, 'claude', now, true, claudePhase, 4000 + (now % 6000), claudePhase === 'tool' ? 'Bash' : ''),
    session(1, 'codex', now, true, 'tool', 41_000, 'shell'),
    session(2, 'deepseek', now, true, 'generating', 9000),
    { ...session(40, 'cursor', now, true, 'generating', 12_000), plugin: true },
    session(3, 'claude', now, false, 'idle', 0),
    session(4, 'antigravity', now, false, 'idle', 0),
    session(5, 'codex', now, false, 'idle', 0),
  ]
}

/** 快照里“正在运行和近 30 分钟”的对话；预览参数 sessions 可以改条数 */
function shownSessions(now: number): SessionView[] {
  return resized(liveSessions(now), 'sessions', (index) => session(index, AGENTS[index % 4], now, false, 'idle', 0))
}

export function buildSnapshot(now: number): Snapshot {
  // 同一分钟内数字保持不变，避免每秒全部跳动
  const random = rng(Math.floor(now / MINUTE))
  const stable = rng(20260101)
  const ranges = (['today', '7d', '30d', 'all'] as RangeId[]).map((id) => buildRange(id, now, id === 'today' ? random : stable))
  const sessions = shownSessions(now)
  const agents: AgentLive[] = AGENTS.map((agent) => ({
    agent,
    label: agent,
    detected: true,
    readError: null,
    processAlive: agent !== 'antigravity',
    running: sessions.filter((item) => item.running && item.agent === agent).length,
    runningSubAgents: agent === 'claude' ? 1 : 0,
    lastActive: agent === 'antigravity' ? now - 26 * HOUR : now - 2000,
    estimated: ['kiro','cursor','webmcp'].includes(agent), conversations: 10, recentTools: [],
    files: { kiro: 3, cursor: 1, webmcp: 1, claude: 110, codex: 41, deepseek: 83, antigravity: 12 }[agent],
    net:
      agent === 'claude'
        ? net('ok', 18_400, 2100, 0, now)
        : agent === 'codex'
          ? net('unstable', 9200, 1500, 2, now)
          : agent === 'deepseek'
            ? net('ok', 4100, 800, 0, now)
            : net('idle', 0, 0, 0, now),
  }))
  const minuteStart = Math.floor(now / MINUTE) * MINUTE
  const rateRandom = rng(7)
  const rate: RatePoint[] = Array.from({ length: 60 }, (_, index) => {
    const total = Math.round((0.2 + rateRandom()) * 900_000 * (index > 12 ? 1 : 0.1))
    const part = split('claude', total)
    return { t: minuteStart - (59 - index) * MINUTE, input: part.input, cacheRead: part.cacheRead, cacheWrite: part.cacheWrite, output: part.output, reasoning: part.reasoning, requests: part.requests }
  })
  // 每 5 秒一格、共 10 分钟：每格的内容只由它的时刻决定，曲线向左滚动时不会变形
  const recentStart = Math.floor(now / 5000) * 5000
  const recent: RatePoint[] = Array.from({ length: 120 }, (_, index) => {
    const t = recentStart - (119 - index) * 5000
    const noise = rng(Math.imul(t / 5000, 2654435761))
    const total = noise() < 0.6 ? Math.round((0.15 + noise()) * 260_000) : 0
    const part = total ? split(noise() < 0.7 ? 'claude' : 'codex', total) : ZERO
    return { t, input: part.input, cacheRead: part.cacheRead, cacheWrite: part.cacheWrite, output: part.output, reasoning: part.reasoning, requests: total ? 1 + Math.floor(total / 120_000) : 0 }
  })
  const perSecond: RatePoint[] = Array.from({ length: 60 }, (_, i) => {
    const t = Math.floor(now / 1000) - 59 + i
    return { ...split('claude', t % 7 === 0 ? 40_000 + (t % 23) * 9000 : t % 3 === 0 ? 6000 + (t % 11) * 1500 : 0), t: t * 1000 }
  })
  const latencyRandom = rng(11)
  const latency: LatencyPoint[] = Array.from({ length: 48 }, (_, index) => ({
    t: now - (48 - index) * 70_000,
    agent: AGENTS[index % 3],
    latencyMs: Math.round(3000 + latencyRandom() * 22_000),
    ttftMs: Math.round(600 + latencyRandom() * 2600),
  }))
  const heatRandom = rng(3)
  const dayStart = new Date(now).setHours(0, 0, 0, 0)
  const heatmap: DayCell[] = Array.from({ length: 182 }, (_, index) => {
    const age = 181 - index
    const total = age > 34 ? 0 : Math.round(heatRandom() * heatRandom() * 160_000_000)
    return { t: dayStart - age * DAY, total, requests: Math.round(total / 165_000) }
  })
  return {
    generatedAt: now,
    version: Math.floor(now / 1000),
    utcOffsetMs: -new Date(now).getTimezoneOffset() * MINUTE,
    ranges,
    live: {
      running: sessions.filter((item) => item.running).length,
      runningSubAgents: 1,
      phases: { requesting: 0, thinking: 0, writing: 0, generating: 2, tool: 1, retrying: 0, waiting: 0, ...(sessions[0] ? { [sessions[0].phase]: sessions[0].phase === 'tool' ? 2 : 1 } : {}) },
      sessions,
      agents,
      net: net('unstable', 9800, 1600, 2, now),
      netEvents: resized<Snapshot['live']['netEvents'][number]>(
        [
          { ts: now - 3 * MINUTE, agent: 'codex', kind: 'retry', code: 'HTTP 502', attempt: 1, maxAttempts: 5, delayMs: 1200, project: 'docs-site' },
          { ts: now - 4 * MINUTE, agent: 'codex', kind: 'retry', code: 'stream_error', attempt: 2, maxAttempts: 5, delayMs: 2400, project: 'docs-site' },
          { ts: now - 3 * HOUR, agent: 'claude', kind: 'retry', code: 'ECONNRESET', attempt: 1, maxAttempts: 10, delayMs: 600, project: 'shop-admin' },
          { ts: now - 9 * HOUR, agent: 'deepseek', kind: 'error', code: 'HTTP 429', attempt: 0, maxAttempts: 0, delayMs: 0, project: 'infra' },
        ],
        'events',
        (index) => ({ ts: now - (10 + index) * HOUR, agent: 'codex', kind: 'retry', code: 'HTTP 502', attempt: 1, maxAttempts: 5, delayMs: 1200, project: 'docs-site' }),
      ),
      latency,
      rate,
      recent,
      perSecond,
      recorded: { minute: perSecond.reduce((sum, point) => sum + point.input + point.cacheRead + point.cacheWrite + point.output, 0), lastAt: now - 4000 },
      finished: [{ key: 'claude:mock-3', agent: 'claude', title: TITLES[3], project: 'shop-admin', endedAt: now - 20_000, durationMs: 133_000, totals: split('claude', 1_240_000) }],
      rateLimits: resized<Snapshot['live']['rateLimits'][number]>(
        [
          {
            agent: 'codex',
            ts: now - 60_000,
            plan: 'pro',
            primary: { used: 0, limit: 0, unit: '', label: '', expires: false, usedPercent: 34, windowMinutes: 300, resetsAtMs: now + 2.4 * HOUR },
            secondary: { used: 0, limit: 0, unit: '', label: '', expires: false, usedPercent: 8, windowMinutes: 10080, resetsAtMs: now + 4.2 * DAY },
          },
          {
            agent: 'kiro',
            ts: now - 6 * DAY,
            plan: '',
            primary: { used: 50, limit: 50, unit: 'credits', label: '每月额度', expires: false, usedPercent: 100, windowMinutes: 0, resetsAtMs: now + 19 * DAY },
            secondary: { used: 0, limit: 500, unit: 'credits', label: '赠送额度', expires: true, usedPercent: 0, windowMinutes: 0, resetsAtMs: now + 357 * DAY },
          },
        ],
        'quotas',
        () => ({ agent: 'claude', ts: now - 60_000, plan: 'max', primary: { used: 0, limit: 0, unit: '', label: '', expires: false, usedPercent: 61, windowMinutes: 300, resetsAtMs: now + 1.5 * HOUR }, secondary: null }),
      ),
    },
    heatmap,
    plugin: { sessions: 6, lastActive: now - 1500, baishitong: feed(now) },
    scan: {
      history: { sessions: 123, devices: 1, elsewhere: 4, carried: 'Foxes-watch-data/history.json' },
      files: 246,
      bytes: 393_349_889,
      events: 10_427,
      failing: 0,
      parsed: 0,
      scanMs: 1,
      firstScanMs: 706,
      scans: 120,
    },
  }
}

/** 预览时在地址后面加 ?feed=offline（或 unknown / foreign / denied / busy / failing）看百事通连不上时的样子 */
function feed(now: number): Snapshot['plugin']['baishitong'] {
  const wanted = new URLSearchParams(window.location.search).get('feed') ?? 'online'
  const links: BaishitongLink[] = ['unknown', 'online', 'offline', 'foreign', 'denied', 'busy', 'failing']
  const link = links.find((item) => item === wanted) ?? 'online'
  if (link === 'online') return { link, sessions: 6, busy: 1, pending: Math.floor(now / 4000) % 3, okAt: now - 1500, retryAt: 0 }
  if (link === 'unknown') return { link, sessions: 0, busy: 0, pending: 0, okAt: 0, retryAt: 0 }
  return { link, sessions: 6, busy: 0, pending: 0, okAt: now - 7 * MINUTE, retryAt: now + 42_000 }
}

export function mockAppInfo(): AppInfo {
  return { version, roots: [['~/.claude'], ['~/.codex'], ['%LOCALAPPDATA%/Deepseek Harness EAC'], ['~/.gemini/antigravity']], settingsPath: 'Foxes-watch-data/settings.json' }
}

export function mockSessions(): SessionView[] {
  const now = Date.now()
  // 最后几段的来源不在这台电脑上：先是记录已经删掉的，再是别的电脑记下的
  const extra = Array.from({ length: 22 }, (_, index) => ({
    ...session(index + 6, AGENTS[index % 4], now, false, 'idle', 0),
    sourceGone: index >= 14,
    sourceElsewhere: index >= 18,
  }))
  const shown = shownSessions(now)
  const keys = new Set(shown.map((item) => item.key))
  // 预览参数把快照里的对话裁少以后，裁掉的那几条算作早些时候结束的
  const earlier = liveSessions(now)
    .filter((item) => !keys.has(item.key))
    .map((item, index): SessionView => ({ ...item, running: false, phase: 'idle', turn: ZERO, lastActive: now - (40 + index * 9) * MINUTE }))
  return [...shown, ...earlier, ...extra, { ...session(41, 'cursor', now, false, 'idle', 0), plugin: true }, session(42, 'cursor', now, false, 'idle', 0)]
}

export function mockSessionDetail(key: string): SessionDetail | null {
  const item = mockSessions().find((candidate) => candidate.key === key)
  if (!item) return null

  const usageOnly = item.sourceGone || item.sourceElsewhere
  const count = Math.min(6, item.totals.requests)
  const requests = Array.from({ length: count }, (_, index): RequestRow => {
    const totals = split(item.agent, 110_000 + index * 37_000)
    return {
      ts: item.lastActive - (count - index - 1) * MINUTE,
      model: item.model,
      effort: EFFORTS[item.model]?.[index % EFFORTS[item.model].length]?.[0] ?? 'unknown',
      input: totals.input,
      cacheRead: totals.cacheRead,
      cacheWrite: totals.cacheWrite,
      output: totals.output,
      reasoning: totals.reasoning,
      total: totals.total,
      latencyMs: 4800 + index * 1800,
      ttftMs: 640 + index * 90,
      thinkMs: totals.thinkMs,
      credits: totals.credits,
      estimated: totals.estimated > 0,
      subAgent: item.subAgents > 0 && index === count - 2,
    }
  })
  const tools = usageOnly ? [] : ['read_file', 'search', 'read_file', 'run_tests'].map((name, index) => ({
    ts: item.lastActive - (3 - index) * 4000,
    name,
    server: '',
    ok: index !== 3,
  }))
  return { key, requestCount: count, requests, toolCount: tools.length, tools, usageOnly }
}

export function startMock(publish: (snapshot: Snapshot) => void) {
  publish(buildSnapshot(Date.now()))
  window.setInterval(() => publish(buildSnapshot(Date.now())), 1000)
}
