import { useEffect, useState } from 'react'
import type { CSSProperties } from 'react'
import { AgentDetails } from './AgentDetails'
import { BandRings, CompositionBar, Heatmap, MultiLine, Ring, StackedBars } from '../components/charts'
import type { BarColumn, LineSeries, Segment } from '../components/charts'
import { AgentBadge, BrandMark, Card, Empty, FillRows, NetChip, PhaseChip, Segmented, Switch } from '../components/ui'
import { everyWhileVisible, fetchSessions, useNow, useStored } from '../lib/api'
import { AGENT_META, EFFORT_META, EFFORT_ORDER, PHASE_META, baishitongText, compactParts, delta, fmtAgo, fmtClock, fmtDate, fmtDelta, fmtDuration, fmtFull, fmtMs, fmtPercent, fmtTime, fmtTokens, hitRate, sessionName } from '../lib/format'
import { AGENTS } from '../lib/types'
import type { Agent, DayCell, ModelAgentTotals, ModelTotals, Phase, RangeId, RangeStats, RatePoint, RateWindow, SeriesPoint, SessionView, Snapshot, Totals } from '../lib/types'

const RANGE_OPTIONS: { value: RangeId; label: string }[] = [
  { value: 'today', label: '今天' },
  { value: '7d', label: '近 7 天' },
  { value: '30d', label: '近 30 天' },
]

const MODEL_RANGE_OPTIONS: { value: RangeId; label: string }[] = [...RANGE_OPTIONS, { value: 'all', label: '历史累计' }]

const RANGE_LABEL: Record<RangeId, string> = { today: '今日', '7d': '近 7 天', '30d': '近 30 天', all: '历史累计' }
const BUCKET_LABEL = { hour: '按小时', day: '按天', week: '按周' }

/** 记在本机的时段选项读回来时用它把关 */
const isRange = (value: unknown) => typeof value === 'string' && value in RANGE_LABEL

const PREVIOUS_LABEL: Record<RangeId, string> = { today: '较昨天同时段', '7d': '较前 7 天', '30d': '较前 30 天', all: '' }

const WEEKDAYS = ['周日', '周一', '周二', '周三', '周四', '周五', '周六']

const ESTIMATE_HINT = 'Claude Code 和 Deepseek Harness 的记录里没有单独的思考 token，这部分按思考文本在输出里的占比估算'

const PLUGIN_HINT = '插件（网关、百事通）的对话算在 Cursor 里。旧版网关插件把每次对话的过程记在本机；百事通的对话存在它的服务端，经插件在本机开的端口读取，Cursor 开着、插件在运行时才读得到。用量按每一步的大小估算，读到过的都记在用量历史里。'

const EFFORT_NOTE = '同一个模型并成一条：不同 agent 用的、名字写法不同的都算在一起，模型名末尾的强度后缀不算在名字里。Claude Code、Codex、Deepseek Harness 的思考强度来自各自的记录；Cursor、反重力、Kiro 的记录里没有这一项，按模型名后缀（如 -high、-max）认，后面还跟着 -fast 的单算一档（-max-fast 是 Max Fast，和 Max 分开），认不出来的记为“未标注”。'

/** 「模型使用累计」默认列出的行数 */
const TOP_MODELS = 8
const TOP_PROJECTS = 8
/** 底部两列对齐时，矮的那列里各个列表最多再往下多显示几行（对话、项目、模型、网络异常） */
const MORE_SESSIONS = 10
const MORE_PROJECTS = 8
const MORE_MODELS = 10
const MORE_NET_EVENTS = 6
/** 对话卡片往下多显示时，最多备着多少条更早的对话 */
const HISTORY_ROWS = 40
/** 速率卡的三个圈：都是合计 token 量，只是时段不同，一个时段一种颜色 */
const RATE_WINDOWS = [
  { key: 'minute', label: '近 1 分钟', minutes: 1, color: 'var(--c-rate-1m)' },
  { key: 'halfHour', label: '近 30 分钟', minutes: 30, color: 'var(--c-rate-30m)' },
  { key: 'hour', label: '近 1 小时', minutes: 60, color: 'var(--c-rate-1h)' },
] as const

const RATE_HINT =
  '三个圈都是合计 token 量，由外到内：近 1 分钟、近 30 分钟、近 1 小时。满圈 = 这一小时里用量最多的那一分钟，30 分钟和 1 小时两圈按平均每分钟的量来填。' +
  '近 1 分钟按“记下来”的时刻算：来源记下一次请求总比它实际发生晚一截，百事通的对话通常晚几十秒到几分钟。'

/** 多线卡的粒度：5 秒一格（近 10 分钟）、1 分钟一格（近 1 小时）、按所选时段 */
type Grain = '5s' | '1m' | 'range'

const sumPoint = (point: RatePoint | SeriesPoint) => point.input + point.cacheRead + point.cacheWrite + point.output

function tokenParts(totals: Totals | SeriesPoint): Record<'input' | 'cacheRead' | 'cacheWrite' | 'output', Segment> {
  return {
    input: { key: 'input', label: '输入', color: 'var(--c-input)', value: totals.input },
    cacheRead: { key: 'cacheRead', label: '缓存命中', color: 'var(--c-cache-read)', value: totals.cacheRead },
    cacheWrite: { key: 'cacheWrite', label: '缓存写入', color: 'var(--c-cache-write)', value: totals.cacheWrite },
    output: { key: 'output', label: '输出', color: 'var(--c-output)', value: totals.output },
  }
}

function columnText(range: RangeStats, t: number): { label: string; title: string } {
  const date = new Date(t)
  if (range.unit === 'hour') {
    return { label: `${date.getHours()}时`, title: `${fmtDate(t)} ${fmtTime(t)} – ${fmtTime(t + range.stepMs)}` }
  }
  const short = `${date.getMonth() + 1}/${date.getDate()}`
  if (range.unit === 'week') return { label: short, title: `${fmtDate(t)} 起的一周` }
  return { label: short, title: `${fmtDate(t)} ${WEEKDAYS[date.getDay()]}` }
}

function HeatStats({ cells }: { cells: DayCell[] }) {
  let sum = 0
  let active = 0
  let peak = 0
  let peakAt = 0
  for (const cell of cells) {
    sum += cell.total
    if (cell.total > 0) active += 1
    if (cell.total > peak) {
      peak = cell.total
      peakAt = cell.t
    }
  }
  // 从今天往回数连续有用量的天数；今天还没用不算断
  let streak = 0
  for (let index = cells.length - 1; index >= 0; index -= 1) {
    if (cells[index].total > 0) streak += 1
    else if (index !== cells.length - 1) break
  }
  return (
    <div className="heat-stats">
      <div>
        <span className="muted">合计</span>
        <b className="num" title={fmtFull(sum)}>
          {fmtTokens(sum)}
        </b>
      </div>
      <div>
        <span className="muted">有用量的天数</span>
        <b className="num">{active}</b>
      </div>
      <div>
        <span className="muted">平均每天</span>
        <b className="num">{active > 0 ? fmtTokens(sum / active) : '—'}</b>
      </div>
      <div>
        <span className="muted">最高单日{peakAt > 0 ? ` · ${fmtDate(peakAt)}` : ''}</span>
        <b className="num">{peak > 0 ? fmtTokens(peak) : '—'}</b>
      </div>
      <div>
        <span className="muted">连续使用</span>
        <b className="num">{streak} 天</b>
      </div>
    </div>
  )
}

function windowLabel(window: RateWindow): string {
  if (window.label) return window.label
  if (window.windowMinutes === 300) return '5 小时额度'
  if (window.windowMinutes === 10080) return '每周额度'
  if (window.windowMinutes % 1440 === 0) return `${window.windowMinutes / 1440} 天额度`
  return `${Math.round(window.windowMinutes / 60)} 小时额度`
}

interface LineDef extends LineSeries {
  /** 所选时段的累计 */
  total: number
  sub: string
  /** 放不进卡片、只在悬停提示里说的补充 */
  hint?: string
  tag?: string
  tagHint?: string
}

/** 多线卡的图例：一项对应一条线，点一下隐藏或显示；数字是所选时段的累计 */
function LineKey({ line, off, toggle }: { line: LineDef; off: boolean; toggle: () => void }) {
  return (
    <button className={off ? 'line-key off' : 'line-key'} onClick={toggle} aria-pressed={!off} title={`${fmtFull(line.total)}${line.hint ? ` · ${line.hint}` : ''} · 点击${off ? '显示' : '隐藏'}这条线`}>
      <span className="line-key-label">
        <i style={{ background: line.color }} />
        {line.label}
        {line.tag ? (
          <span className="tag" title={line.tagHint}>
            {line.tag}
          </span>
        ) : null}
      </span>
      <b className="num">{line.total >= 1e7 ? fmtTokens(line.total) : fmtFull(line.total)}</b>
      <span className="line-key-sub">{line.sub}</span>
    </button>
  )
}

/** 累计条：总长按用量占最高那一行的比例，里面按思考强度分段上色 */
function UsageBar({ parts, share }: { parts: { key: string; color: string; value: number }[]; share: number }) {
  return (
    <span className="usage-track">
      <span className="usage-bar" style={{ width: `${Math.max(0, Math.min(1, share)) * 100}%` }}>
        {parts.map((part) => (part.value > 0 ? <i key={part.key} style={{ flexGrow: part.value, background: part.color }} /> : null))}
      </span>
    </span>
  )
}

const thinkShare = (part: Totals) => (part.output > 0 ? fmtPercent(part.reasoning / part.output, 0) : '—')

/** 一条横条按各个 agent 的用量比例分段上色 */
function agentFill(agents: ModelAgentTotals[]): string {
  if (agents.length === 0) return 'var(--text-3)'
  const total = agents.reduce((sum, part) => sum + part.totals.total, 0)
  if (agents.length === 1 || total <= 0) return AGENT_META[agents[0].agent].color
  let from = 0
  const stops = agents.map((part, index) => {
    const to = index === agents.length - 1 ? 100 : from + (part.totals.total / total) * 100
    const stop = `${AGENT_META[part.agent].color} ${from.toFixed(2)}% ${to.toFixed(2)}%`
    from = to
    return stop
  })
  return `linear-gradient(90deg, ${stops.join(', ')})`
}

/** 用了这个模型的各个 agent 的徽标，用量多的在前；悬停看它用了多少 */
function ModelBadges({ model }: { model: ModelTotals }) {
  return (
    <span className="badge-group">
      {model.agents.map((part) => (
        <AgentBadge key={part.agent} agent={part.agent} size={20} title={`${AGENT_META[part.agent].label} · ${fmtFull(part.totals.total)} tokens · ${fmtFull(part.totals.requests)} 次请求`} />
      ))}
    </span>
  )
}

/** 模型行展开后的一行细分（某个思考强度，或某个 agent）：各列和模型行对齐 */
function UsageSubRow({ label, color, part, whole, top }: { label: string; color: string; part: Totals; whole: number; top: number }) {
  return (
    <div className="usage-row usage-sub">
      <span className="usage-model">
        <i className="effort-dot" style={{ background: color }} />
        <span>{label}</span>
      </span>
      <span className="muted">{whole > 0 ? `占这个模型 ${fmtPercent(part.total / whole)}` : ''}</span>
      <span className="usage-total">
        <UsageBar share={part.total / top} parts={[{ key: 'part', color, value: part.total }]} />
        <b className="num" title={fmtFull(part.total)}>
          {fmtTokens(part.total)}
        </b>
      </span>
      <span className="num">{fmtFull(part.requests)}</span>
      <span className="num">{fmtTokens(part.output)}</span>
      <span className="num">
        {fmtTokens(part.reasoning)}
        <em>{thinkShare(part)}</em>
      </span>
      <span />
    </div>
  )
}

function ModelRow({ model, top }: { model: ModelTotals; top: number }) {
  const totals = model.totals
  return (
    <details className="usage-item">
      <summary className="usage-row">
        <span className="usage-model">
          <ModelBadges model={model} />
          <span className="ellipsis" title={model.model}>
            {model.model || '未知模型'}
          </span>
        </span>
        <span className="usage-efforts">
          {model.efforts.map((part) => (
            <span key={part.effort} className="effort-chip" style={{ '--chip': EFFORT_META[part.effort].color } as CSSProperties} title={`${fmtFull(part.totals.total)} tokens · ${fmtFull(part.totals.requests)} 次请求`}>
              {EFFORT_META[part.effort].label}
              {model.efforts.length > 1 ? <em className="num">{totals.total > 0 ? fmtPercent(part.totals.total / totals.total, 0) : '—'}</em> : null}
            </span>
          ))}
        </span>
        <span className="usage-total">
          <UsageBar share={totals.total / top} parts={model.efforts.map((part) => ({ key: part.effort, color: EFFORT_META[part.effort].color, value: part.totals.total }))} />
          <b className="num" title={fmtFull(totals.total)}>
            {fmtTokens(totals.total)}
          </b>
        </span>
        <span className="num">{fmtFull(totals.requests)}</span>
        <span className="num">{fmtTokens(totals.output)}</span>
        <span className="num">
          {fmtTokens(totals.reasoning)}
          <em>{thinkShare(totals)}</em>
        </span>
        <span className="disclosure-arrow" aria-hidden="true">
          ⌄
        </span>
      </summary>
      <div className="usage-detail">
        {model.efforts.map((part) => (
          <UsageSubRow key={part.effort} label={`思考强度：${EFFORT_META[part.effort].label}`} color={EFFORT_META[part.effort].color} part={part.totals} whole={totals.total} top={top} />
        ))}
        {model.agents.length > 1 ? (
          <div className="usage-sources">
            {model.agents.map((part) => (
              <UsageSubRow key={part.agent} label={`来自 ${AGENT_META[part.agent].label}`} color={AGENT_META[part.agent].color} part={part.totals} whole={totals.total} top={top} />
            ))}
          </div>
        ) : null}
      </div>
    </details>
  )
}

/** 模型使用累计：用了哪些模型、各用什么思考强度、累计多少。时段自己选，默认看全部历史 */
function ModelUsage({ snapshot }: { snapshot: Snapshot }) {
  const [rangeId, setRangeId] = useStored<RangeId>('models.range', 'all', isRange)
  const [showAll, setShowAll] = useState(false)
  const range = snapshot.ranges.find((item) => item.id === rangeId) ?? snapshot.ranges[0]
  const models = [...range.byModel].sort((a, b) => b.totals.total - a.totals.total)
  const top = Math.max(1, ...models.map((model) => model.totals.total))
  // 徽标最多的那一行有几个，各行都留出这么宽，模型名才对得齐
  const badgeSlots = Math.max(1, ...models.map((model) => model.agents.length))
  const sum = models.reduce((total, model) => total + model.totals.total, 0)
  const byEffort = EFFORT_ORDER.map((effort) => {
    const parts = models.flatMap((model) => model.efforts.filter((part) => part.effort === effort))
    return { effort, total: parts.reduce((total, part) => total + part.totals.total, 0), requests: parts.reduce((total, part) => total + part.totals.requests, 0) }
  }).filter((item) => item.requests > 0)
  const shown = showAll ? models : models.slice(0, TOP_MODELS)
  return (
    <Card
      className="span-12"
      title={
        <>
          模型使用累计
          <span className="card-caption">一个模型一条，点一行看按思考强度、按 Agent 的细分</span>
        </>
      }
      extra={<Segmented options={MODEL_RANGE_OPTIONS} value={rangeId} onChange={setRangeId} />}
    >
      {models.length === 0 ? (
        <Empty>这段时间没有用量</Empty>
      ) : (
        <>
          <div className="effort-summary">
            <div className="effort-summary-head">
              <span>按思考强度累计</span>
              <span className="muted">
                {models.length} 个模型 · <span className="num">{fmtTokens(sum)}</span> tokens
              </span>
            </div>
            <CompositionBar parts={byEffort.map((item) => ({ key: item.effort, label: EFFORT_META[item.effort].label, color: EFFORT_META[item.effort].color, value: item.total }))} height={10} />
            <div className="effort-legend">
              {byEffort.map((item) => (
                <span key={item.effort} title={`${fmtFull(item.total)} tokens · ${fmtFull(item.requests)} 次请求`}>
                  <i style={{ background: EFFORT_META[item.effort].color }} />
                  {EFFORT_META[item.effort].label}
                  <b className="num">{fmtTokens(item.total)}</b>
                  <em className="num">{sum > 0 ? fmtPercent(item.total / sum) : '—'}</em>
                </span>
              ))}
            </div>
          </div>
          <div className="usage-table" style={{ '--badges': badgeSlots } as CSSProperties}>
            <div className="usage-row usage-head">
              <span>模型</span>
              <span>思考强度</span>
              <span>累计用量</span>
              <span>请求</span>
              <span>输出</span>
              <span>思考 / 占输出</span>
              <span />
            </div>
            {shown.map((model) => (
              <ModelRow key={model.model} model={model} top={top} />
            ))}
          </div>
          {models.length > TOP_MODELS ? (
            <button className="btn usage-more" onClick={() => setShowAll(!showAll)}>
              {showAll ? `只看前 ${TOP_MODELS} 个` : `显示全部 ${models.length} 个模型`}
            </button>
          ) : null}
          <div className="usage-note muted">{EFFORT_NOTE}</div>
        </>
      )}
    </Card>
  )
}

/** 额度数字最多保留两位小数：1.25 / 50 */
const fmtAmount = (value: number) => String(Number(value.toFixed(2)))

function QuotaBar({ window, now, ts }: { window: RateWindow; now: number; ts: number }) {
  const used = Math.max(0, Math.min(100, window.usedPercent))
  const tone = used >= 90 ? 'var(--down)' : used >= 70 ? 'var(--unstable)' : 'var(--ok)'
  const deadline =
    window.resetsAtMs > now
      ? `${fmtDuration(window.resetsAtMs - now)}后${window.expires ? '到期' : '重置'}`
      : window.resetsAtMs > 0
        ? window.expires
          ? '已到期'
          : '已到重置时间'
        : ''
  // 有的额度只在客户端运行时更新，读数旧了要说明是什么时候的
  const foot = [
    window.limit > 0 ? `${fmtAmount(window.used)} / ${fmtAmount(window.limit)} ${window.unit}`.trim() : '',
    deadline,
    ts > 0 && now - ts > 3_600_000 ? `${fmtAgo(ts, now)}的数据` : '',
  ].filter(Boolean)
  return (
    <div className="quota">
      <div className="quota-head">
        <span>{windowLabel(window)}</span>
        <b className="num">已用 {used.toFixed(used < 10 ? 1 : 0)}%</b>
      </div>
      <div className="quota-track">
        <i style={{ width: `${used}%`, background: tone }} />
      </div>
      <div className="quota-foot muted">{foot.join(' · ')}</div>
    </div>
  )
}

export function Overview({ snapshot }: { snapshot: Snapshot }) {
  // 时段和几个显示开关记在本机，下次打开还是上次的样子
  const [rangeId, setRangeId] = useStored<RangeId>('overview.range', 'today', isRange)
  const [detail, setDetail] = useState<Agent | null>(null)
  const [mode, setMode] = useStored<'type' | 'agent'>('overview.mode', 'type', (value) => value === 'type' || value === 'agent')
  const [hideCache, setHideCache] = useStored<boolean>('overview.hideCache', false)
  const [grain, setGrain] = useStored<Grain>('overview.grain', '5s', (value) => value === '5s' || value === '1m' || value === 'range')
  const [sharedScale, setSharedScale] = useStored<boolean>('overview.sharedScale', false)
  const [hiddenLines, setHiddenLines] = useState<string[]>([])
  const now = useNow()
  // 对话卡片被旁边那列拉高时会往下多显示几条更早的对话，这里把最近的那些备着；变得慢，半分钟刷新一次就够
  const [history, setHistory] = useState<SessionView[]>([])
  useEffect(() => {
    let active = true
    const load = () => {
      fetchSessions()
        .then((rows) => {
          if (!active) return
          setHistory(
            rows
              .filter((session) => !session.running && !session.subAgent)
              .sort((a, b) => b.lastActive - a.lastActive)
              .slice(0, HISTORY_ROWS),
          )
        })
        .catch(() => undefined)
    }
    load()
    const stop = everyWhileVisible(load, 30_000)
    return () => {
      active = false
      stop()
    }
  }, [])

  const range = snapshot.ranges.find((item) => item.id === rangeId) ?? snapshot.ranges[0]
  const totals = range.totals
  const period = RANGE_LABEL[range.id]
  const elapsedSeries = range.series.filter((point) => point.t <= snapshot.generatedAt)
  const live = snapshot.live
  const parts = tokenParts(totals)
  const big = compactParts(totals.total)
  const change = delta(totals.total, range.previous?.total)
  const hit = hitRate(totals)
  const prompt = totals.input + totals.cacheRead + totals.cacheWrite
  const share = (value: number) => {
    if (totals.total <= 0) return '—'
    const ratio = value / totals.total
    return ratio > 0 && ratio < 0.001 ? '<0.1%' : fmtPercent(ratio)
  }

  const columns: BarColumn[] = elapsedSeries.map((point) => {
    const text = columnText(range, point.t)
    const byType = tokenParts(point)
    const segments =
      mode === 'type'
        ? [byType.cacheRead, byType.cacheWrite, byType.input, byType.output].filter((segment) => !(hideCache && segment.key === 'cacheRead'))
        : AGENTS.map((agent, index) => ({ key: agent, label: AGENT_META[agent].short, color: AGENT_META[agent].color, value: point.byAgent[index] }))
    return { t: point.t, label: text.label, title: text.title, segments, note: `${point.requests} 次请求` }
  })

  // —— Token 速率：近 1 分钟、30 分钟、1 小时的合计 token 量 ——
  // 近 1 分钟取“这一分钟里记下了多少”：按请求发生的时刻算的话，晚到的记录进不了这一分钟，数字总是偏小甚至是 0
  const minuteTotals = live.rate.map(sumPoint)
  const windowTotals = {
    minute: live.recorded.minute,
    halfHour: minuteTotals.slice(-30).reduce((sum, value) => sum + value, 0),
    hour: minuteTotals.reduce((sum, value) => sum + value, 0),
  }
  // 三个圈共用一把尺子：这一小时里用量最多的那一分钟算满圈（刚记下的这一分钟也算在内），各圈按自己那段时间里平均每分钟的量来填
  const busiestMinute = Math.max(0, windowTotals.minute, ...minuteTotals)
  const bands = RATE_WINDOWS.map((item) => {
    const total = windowTotals[item.key]
    const perMinute = total / item.minutes
    return { ...item, total, perMinute, value: busiestMinute > 0 ? Math.min(1, perMinute / busiestMinute) : 0 }
  })

  // —— 实时用量：七条线共用一张图 ——
  const grainPoints: (RatePoint | SeriesPoint)[] = grain === '5s' ? live.recent : grain === '1m' ? live.rate : elapsedSeries
  const pick = (read: (point: RatePoint | SeriesPoint) => number) => grainPoints.map(read)
  const perRequest = (value: number) => (totals.requests > 0 ? fmtTokens(value / totals.requests) : '—')
  const ofPrompt = (value: number) => (prompt > 0 ? fmtPercent(value / prompt) : '—')
  const lines: LineDef[] = [
    { key: 'total', label: '合计', color: 'var(--c-total)', values: pick(sumPoint), total: totals.total, sub: `平均每次请求 ${perRequest(totals.total)}` },
    { key: 'input', label: '输入', color: 'var(--c-input)', values: pick((point) => point.input), total: totals.input, sub: `占输入侧 ${ofPrompt(totals.input)}` },
    { key: 'cacheRead', label: '缓存命中', color: 'var(--c-cache-read)', values: pick((point) => point.cacheRead), total: totals.cacheRead, sub: `命中率 ${prompt > 0 ? fmtPercent(hit) : '—'}` },
    { key: 'cacheWrite', label: '缓存写入', color: 'var(--c-cache-write)', values: pick((point) => point.cacheWrite), total: totals.cacheWrite, sub: `占输入侧 ${ofPrompt(totals.cacheWrite)}` },
    { key: 'output', label: '输出', color: 'var(--c-output)', values: pick((point) => point.output), total: totals.output, sub: `平均每次 ${perRequest(totals.output)}` },
    {
      key: 'reasoning',
      label: '思考',
      color: 'var(--c-think)',
      values: pick((point) => point.reasoning),
      total: totals.reasoning,
      sub: `占输出 ${totals.output > 0 ? fmtPercent(totals.reasoning / totals.output) : '—'}`,
      hint: totals.thinkMs > 0 ? `思考时长 ${fmtDuration(totals.thinkMs)}` : undefined,
      tag: totals.reasoningEstimated > 0 ? '估算' : undefined,
      tagHint: ESTIMATE_HINT,
    },
    { key: 'requests', label: '请求数', color: 'var(--c-requests)', values: pick((point) => point.requests), total: totals.requests, sub: `${range.sessions} 个对话`, format: (value) => `${fmtFull(value)} 次`, ownScale: true },
  ]
  const grainTime: { axis: (t: number) => string; title: (t: number) => string } =
    grain === '5s'
      ? { axis: (t) => fmtTime(t, true), title: (t) => `${fmtTime(t, true)} 起的 5 秒` }
      : grain === '1m'
        ? { axis: (t) => fmtTime(t), title: (t) => `${fmtTime(t)} 这一分钟` }
        : { axis: (t) => columnText(range, t).label, title: (t) => columnText(range, t).title }
  const windowTotal = grainPoints.reduce((sum, point) => sum + sumPoint(point), 0)
  const windowRequests = grainPoints.reduce((sum, point) => sum + point.requests, 0)

  // —— 各 Agent：按所选时段的用量从高到低；用量相同时正在运行的、最近活动的排前面 ——
  const agentRows = AGENTS.map((agent, order) => {
    const stats = range.byAgent.find((item) => item.agent === agent)
    const state = live.agents.find((item) => item.agent === agent)
    return { agent, order, stats, state, total: stats?.totals.total ?? 0 }
  }).sort((a, b) => b.total - a.total || (b.state?.running ?? 0) - (a.state?.running ?? 0) || (b.state?.lastActive ?? 0) - (a.state?.lastActive ?? 0) || a.order - b.order)
  const pluginLine =
    range.plugin.totals.total > 0
      ? `其中插件（网关、百事通）对话 ${range.plugin.sessions} 个 · ${fmtTokens(range.plugin.totals.total)}`
      : snapshot.plugin.lastActive > 0
        ? `插件（网关、百事通）对话：这段时间没有记录，最近一次在${fmtAgo(snapshot.plugin.lastActive, now)}`
        : '插件（网关、百事通）对话：还没有读到记录'
  // 百事通读不到时把卡在哪一步跟在后面；连着的时候不多话
  const feedText = snapshot.plugin.baishitong ? baishitongText(snapshot.plugin.baishitong, snapshot.generatedAt) : null
  const feedFlag = feedText && feedText.tone !== 'ok' ? feedText : null
  const phases = (Object.keys(live.phases) as (keyof typeof live.phases)[]).filter((phase) => live.phases[phase] > 0)
  const running = live.sessions.filter((session) => session.running)
  const recent = live.sessions.filter((session) => !session.running).slice(0, Math.max(0, 6 - running.length))
  const topModels = range.byModel.slice(0, 5)
  const topModelTotal = Math.max(1, ...topModels.map((model) => model.totals.total))
  const topProjects = range.byProject.slice(0, TOP_PROJECTS)
  const topProjectTotal = Math.max(1, ...topProjects.map((project) => project.totals.total))
  // 对话卡片能显示的全部行：平时只显示前 sessionBase 行（正在运行的和近 30 分钟的前几条），
  // 卡片被拉高时再往下显示近 30 分钟里没排进来的和更早的对话
  const liveKeys = new Set(live.sessions.map((session) => session.key))
  const sessionRows = [...running, ...recent, ...live.sessions.filter((session) => !session.running).slice(recent.length), ...history.filter((session) => !liveKeys.has(session.key))]
  const sessionBase = Math.min(sessionRows.length, Math.max(1, running.length + recent.length))
  const sessionExtra = Math.min(MORE_SESSIONS, sessionRows.length - sessionBase)
  const projectExtra = Math.min(MORE_PROJECTS, range.byProject.length - topProjects.length)
  const modelExtra = Math.min(MORE_MODELS, range.byModel.length - topModels.length)
  const netEventExtra = Math.min(MORE_NET_EVENTS, Math.max(0, live.netEvents.length - 3))
  // 底部两列对齐时按行数分多出来的高度：一块额度算标题 1 行加每条进度 1 行，能多显示的行也算进去
  const netAgentRows = live.agents.filter((agent) => agent.detected).length
  const netEventRows = Math.max(1, Math.min(3, live.netEvents.length)) + netEventExtra
  const quotaRows = (limit: (typeof live.rateLimits)[number]) => 1 + (limit.primary ? 1 : 0) + (limit.secondary ? 1 : 0)
  const modelRows = Math.max(1, topModels.length) + modelExtra
  const rangeControls = (
        <div className="range-controls">
          {rangeId !== 'all' && <Segmented options={RANGE_OPTIONS} value={rangeId} onChange={setRangeId} />}
          <label className="history-toggle">
            历史总用量
            <Switch on={rangeId === 'all'} onChange={(on) => setRangeId(on ? 'all' : 'today')} />
          </label>
        </div>
  )
  if (detail) return <AgentDetails agent={detail} snapshot={snapshot} range={range} period={period} controls={rangeControls} back={() => setDetail(null)} />

  const agentRow = ({ agent, stats, state, total }: (typeof agentRows)[number]) => {
    const status = !state?.detected ? '本机未发现' : state.running > 0 ? '' : state.processAlive === false ? '未启动' : state.lastActive > 0 ? `${fmtAgo(state.lastActive, now)}活动` : '空闲'
    return (
      <details key={agent} className="agent-disclosure">
        <summary className={state?.detected ? 'agent-row' : 'agent-row off'}>
          <AgentBadge agent={agent} size={32} />
          <div className="agent-main">
            <div className="agent-name">
              <span className="ellipsis">{AGENT_META[agent].label}</span>
              {(stats?.totals.estimated ?? 0) > 0 && <span className="tag">估算</span>}
              {state && state.running > 0 ? (
                <span className="chip tone-ok">
                  <span className="dot pulse" />
                  运行中 {state.running}
                </span>
              ) : null}
            </div>
            <div className="agent-bar">
              <i style={{ width: `${totals.total > 0 ? (total / totals.total) * 100 : 0}%`, background: AGENT_META[agent].color }} />
            </div>
            <div className="agent-meta muted ellipsis">
              {stats && stats.sessions > 0 ? `${stats.sessions} 个对话 · 命中率 ${fmtPercent(hitRate(stats.totals), 0)}` : '这段时间没有用量'}
              {status ? ` · ${status}` : ''}
            </div>
            {agent === 'cursor' ? (
              <div className="agent-meta agent-note muted" title={feedFlag ? `${feedFlag.detail}\n\n${PLUGIN_HINT}` : PLUGIN_HINT}>
                {pluginLine}
                {feedFlag ? <span className={feedFlag.tone === 'bad' ? 'feed-flag bad' : 'feed-flag'}> · 百事通{feedFlag.label}</span> : null}
              </div>
            ) : null}
          </div>
          <div className="agent-total">
            <b className="num" title={fmtFull(total)}>
              {fmtTokens(total)}
            </b>
            <span className="num muted">{share(total)}</span>
          </div>
          <span className="disclosure-arrow" aria-hidden="true">
            ⌄
          </span>
        </summary>
        <div className="agent-expanded">
          <span>
            输入 {fmtTokens(stats?.totals.input ?? 0)} · 输出 {fmtTokens(stats?.totals.output ?? 0)}
          </span>
          <span>
            {fmtFull(stats?.totals.requests ?? 0)} 次请求 · {stats?.sessions ?? 0} 个对话
          </span>
          {agent === 'cursor' ? <span title={PLUGIN_HINT}>{pluginLine}</span> : null}
          {agent === 'cursor' && feedFlag ? (
            <span className={feedFlag.tone === 'bad' ? 'feed-flag bad' : 'feed-flag'}>
              百事通{feedFlag.label}：{feedFlag.detail}
            </span>
          ) : null}
          <button className="btn" onClick={() => setDetail(agent)}>
            查看{AGENT_META[agent].short}详情 →
          </button>
        </div>
      </details>
    )
  }

  return (
    <div className="page">
      <header className="page-head brand-head">
        <BrandMark>
          <h1>总览</h1>
          <span className="muted">
            {fmtDate(range.from)}
            {range.id === 'today' ? '' : ` – ${fmtDate(range.to)}`} · 数据更新于 {fmtTime(snapshot.generatedAt, true)}
          </span>
        </BrandMark>
        {rangeControls}
      </header>

      <div className="grid">
        <Card className="span-8 hero">
          <div className="hero-top">
            <div className="hero-main">
              <div className="hero-label">{period} Token 用量 {totals.estimated > 0 && <span className="tag" title={fmtFull(totals.estimated) + ' token 为估算值'}>含估算</span>}</div>
              <div className="hero-value num">
                {big.value}
                <small>{big.unit}</small>
              </div>
              <div className="hero-sub">
                <span className="num">{fmtFull(totals.total)}</span> tokens
                <i />
                <span className="num">{fmtFull(totals.requests)}</span> 次请求
                <i />
                <span className="num">{range.sessions}</span> 个对话
                {change !== null ? (
                  <span className={change >= 0 ? 'delta up' : 'delta down'} title={`上一时段 ${fmtFull(range.previous?.total ?? 0)}`}>
                    {fmtDelta(change)} {PREVIOUS_LABEL[range.id]}
                  </span>
                ) : null}
              </div>
            </div>
            <Ring value={hit} size={92} stroke={9} color="var(--c-cache-read)">
              <b className="num">{prompt > 0 ? fmtPercent(hit, hit >= 0.9995 ? 0 : 1) : '—'}</b>
              <span>缓存命中率</span>
            </Ring>
          </div>
          <CompositionBar parts={[parts.input, parts.cacheRead, parts.cacheWrite, parts.output]} height={14} />
          <div className="legend">
            {[parts.input, parts.cacheRead, parts.cacheWrite, parts.output].map((part) => (
              <div key={part.key} className="legend-item">
                <i style={{ background: part.color }} />
                <span>{part.label}</span>
                <b className="num" title={fmtFull(part.value)}>
                  {fmtTokens(part.value)}
                </b>
                <em className="num">{share(part.value)}</em>
              </div>
            ))}
          </div>
        </Card>

        <Card className="span-4 rate-card" title="Token 速率" extra={<NetChip status={live.net.status} />}>
          <div className="rate-body">
            <div title={RATE_HINT}>
              <BandRings bands={bands} size={148} stroke={11} gap={4}>
                <b className="num">{fmtTokens(bands[0].total)}</b>
                <span>tokens / 分钟</span>
              </BandRings>
            </div>
            <div className="rate-side">
              {bands.map((item) => (
                <div
                  key={item.key}
                  className="rate-row"
                  title={item.minutes === 1 ? '这一分钟里记下的合计 token 量' : `${item.label}的合计 token 量，平均每分钟 ${fmtTokens(Math.round(item.perMinute))}`}
                >
                  <i style={{ background: item.color }} />
                  <span>{item.label}</span>
                  <b className="num" title={fmtFull(item.total)}>
                    {fmtTokens(item.total)}
                  </b>
                </div>
              ))}
              <div className="rate-stats">
                <div title="近 1 分钟记下的量折合成每秒">
                  <span className="muted">近 1 分钟平均</span>
                  <b className="num">{fmtTokens(Math.round(bands[0].total / 60))}/秒</b>
                </div>
                <div title="近 1 小时里用量最多的那一分钟（刚记下的这一分钟也算在内），三个圈以它为满圈">
                  <span className="muted">最忙的一分钟</span>
                  <b className="num">{fmtTokens(busiestMinute)}</b>
                </div>
                <div title="最近一次记下用量的时刻">
                  <span className="muted">最近记到</span>
                  <b className="num">{live.recorded.lastAt > 0 ? fmtTime(live.recorded.lastAt, true) : '—'}</b>
                </div>
              </div>
            </div>
          </div>
          <div className="rate-foot">
            <span className={live.running > 0 ? 'rate-running on' : 'rate-running'}>
              <b className="num">{live.running}</b> 个对话正在运行
            </span>
            {live.runningSubAgents > 0 ? <span className="muted">另有 {live.runningSubAgents} 个子代理</span> : null}
            {phases.map((phase) => (
              <span key={phase} className={`chip tone-${PHASE_META[phase as Phase].tone}`}>
                <span className="dot pulse" />
                {PHASE_META[phase as Phase].label}
                <b className="num">{live.phases[phase]}</b>
              </span>
            ))}
            {live.running === 0 && phases.length === 0 ? <span className="muted">所有 agent 都空闲</span> : null}
          </div>
        </Card>

        <Card
          className="span-12 lines-card"
          title={
            <>
              实时用量
              <span className="card-caption">数字是{period}累计，点一项可以隐藏或显示它的线</span>
            </>
          }
          extra={
            <div className="card-tools">
              <button
                className={sharedScale ? 'toggle on' : 'toggle'}
                onClick={() => setSharedScale(!sharedScale)}
                title="默认每条线按自己的峰值缩放，量级差很多的几项也都看得清走势；打开后各项 token 共用左侧刻度，方便比较大小"
              >
                同一刻度
              </button>
              <Segmented
                options={[
                  { value: '5s', label: '5 秒' },
                  { value: '1m', label: '1 分钟' },
                  { value: 'range', label: BUCKET_LABEL[range.unit] },
                ]}
                value={grain}
                onChange={setGrain}
              />
            </div>
          }
        >
          <div className="line-keys">
            {lines.map((line) => (
              <LineKey
                key={line.key}
                line={line}
                off={hiddenLines.includes(line.key)}
                toggle={() => setHiddenLines(hiddenLines.includes(line.key) ? hiddenLines.filter((key) => key !== line.key) : [...hiddenLines, line.key])}
              />
            ))}
          </div>
          <MultiLine
            series={lines.filter((line) => !hiddenLines.includes(line.key))}
            times={grainPoints.map((point) => point.t)}
            height={232}
            shared={sharedScale}
            formatTime={grainTime.axis}
            formatTitle={grainTime.title}
            empty={
              hiddenLines.length >= lines.length
                ? '所有线都隐藏了，点上面的项目可以重新显示'
                : grain === '5s'
                  ? `近 10 分钟没有用量，有请求时这里会实时画出来；想看更早的走势可以切到「${BUCKET_LABEL[range.unit]}」`
                  : grain === '1m'
                    ? `近 1 小时没有用量；想看更早的走势可以切到「${BUCKET_LABEL[range.unit]}」`
                    : '这段时间没有用量'
            }
          />
          <div className="lines-foot muted">
            {grain === '5s' ? '近 10 分钟，每 5 秒一格，随数据实时滚动' : grain === '1m' ? '近 1 小时，每分钟一格' : `${period}，${BUCKET_LABEL[range.unit]}统计`}
            {' · '}这段曲线共 <b className="num">{fmtTokens(windowTotal)}</b> tokens、<b className="num">{fmtFull(windowRequests)}</b> 次请求
            {' · '}
            {sharedScale ? '各项 token 共用左侧刻度，请求数仍按自己的峰值' : '每条线按自己的峰值缩放'}
          </div>
        </Card>

        <Card
          className="span-8 stretch"
          title="用量趋势"
          extra={
            <div className="card-tools">
              {mode === 'type' ? (
                <button className={hideCache ? 'toggle on' : 'toggle'} onClick={() => setHideCache(!hideCache)} title="缓存命中通常占绝大部分，隐藏后能看清其余几项">
                  隐藏缓存命中
                </button>
              ) : null}
              <Segmented
                options={[
                  { value: 'type', label: '按类型' },
                  { value: 'agent', label: '按 Agent' },
                ]}
                value={mode}
                onChange={setMode}
              />
            </div>
          }
        >
          <StackedBars columns={columns} height={236} fill />
          <div className="chart-legend">
            {(columns[0]?.segments ?? []).map((segment) => (
              <span key={segment.key}>
                <i style={{ background: segment.color }} />
                {segment.label}
              </span>
            ))}
          </div>
        </Card>

        <Card className="span-4 agents-card" title="各 Agent" extra={<span className="muted">共 {agentRows.length} 个 · 按{period}用量从高到低</span>}>
          <div className="agent-list agent-scroll">{agentRows.map(agentRow)}</div>
        </Card>

        <ModelUsage snapshot={snapshot} />

        {/* 下面左右两列底边对齐：矮的那列把多出来的高度按行数匀给各张卡片里的每一行，不在哪张卡片底下留一整块空白 */}
        <div className="col flush span-7">
          <Card title="对话" rows={Math.max(1, sessionBase) + sessionExtra} extra={<span className="muted">{running.length > 0 ? `${running.length} 个运行中` : '最近活动'}</span>}>
            {sessionRows.length === 0 ? <Empty>最近 30 分钟没有对话活动</Empty> : null}
            <FillRows className="sess-list" items={sessionRows} base={sessionBase} extra={sessionExtra}>
              {(session) => (
                <div key={session.key} className={session.running ? 'sess-row running' : 'sess-row'}>
                  <AgentBadge agent={session.agent} size={28} />
                  <div className="sess-main">
                    <div className="sess-title ellipsis">
                      {sessionName(session)}
                      {session.plugin ? (
                        <span className="tag" title="插件（网关、百事通）里的对话，算在 Cursor 里">
                          插件
                        </span>
                      ) : null}
                    </div>
                    <div className="sess-meta muted ellipsis">
                      {[session.project, session.model, session.turns > 0 ? `${session.turns} 轮` : '', session.subAgents > 0 ? `${session.subAgents} 个子代理${session.runningSubAgents > 0 ? `（${session.runningSubAgents} 个在跑）` : ''}` : ''].filter(Boolean).join(' · ')}
                    </div>
                  </div>
                  {session.running ? <PhaseChip phase={session.phase} tool={session.tool} /> : <span className="muted sess-ago">{fmtAgo(session.lastActive, now)}</span>}
                  <div className="sess-clock num" title="这一步已持续 / 本轮已持续">
                    {session.running ? (
                      <>
                        {fmtClock(now - session.phaseSince)}
                        <span className="muted">{session.turnStarted > 0 ? fmtClock(now - session.turnStarted) : ''}</span>
                      </>
                    ) : null}
                  </div>
                  <div className="sess-tokens">
                    <b className="num">{fmtTokens(session.running ? session.turn.total : session.totals.total)}</b>
                    <span className="muted">{session.running ? '本轮' : '累计'}</span>
                  </div>
                </div>
              )}
            </FillRows>
          </Card>

          <Card title="近 26 周" rows={1} extra={<span className="muted">每格一天，颜色越深用量越大</span>}>
            <Heatmap cells={snapshot.heatmap} />
            <HeatStats cells={snapshot.heatmap} />
          </Card>

          <Card title="项目排行" rows={Math.max(1, topProjects.length) + projectExtra} extra={<span className="muted">{period}用量最多的项目</span>}>
            {topProjects.length === 0 ? <Empty>这段时间没有用量</Empty> : null}
            <FillRows
              className="project-list"
              items={range.byProject}
              base={topProjects.length}
              extra={projectExtra}
              style={{ '--badges': Math.max(1, ...range.byProject.slice(0, topProjects.length + projectExtra).map((project) => project.agents.length)) } as CSSProperties}
            >
              {(project) => (
                <div
                  key={`${project.cwd}|${project.project}`}
                  className="project-row"
                  title={`${project.cwd || '没有记下目录'}\n${fmtFull(project.totals.total)} tokens · ${fmtFull(project.totals.requests)} 次请求 · 命中率 ${fmtPercent(hitRate(project.totals), 0)}`}
                >
                  <span className="project-name">
                    <span className="badge-group">
                      {project.agents.map((agent) => (
                        <AgentBadge key={agent} agent={agent} size={20} />
                      ))}
                    </span>
                    <span className="ellipsis">{project.project || '未归属项目'}</span>
                  </span>
                  <div className="model-bar">
                    <i style={{ width: `${(project.totals.total / topProjectTotal) * 100}%`, background: project.agents.length === 1 ? AGENT_META[project.agents[0]].color : 'var(--accent)' }} />
                  </div>
                  <span className="project-sessions num">{project.sessions} 个对话</span>
                  <b className="num">{fmtTokens(project.totals.total)}</b>
                </div>
              )}
            </FillRows>
          </Card>
        </div>

        <div className="col flush span-5">
          <Card title="网络状况" rows={netAgentRows + netEventRows} extra={<span className="muted">最近 15 分钟</span>}>
            <div className="net-list" style={{ '--rows': netAgentRows } as CSSProperties}>
              {live.agents
                .filter((agent) => agent.detected)
                .map((agent) => (
                  <div key={agent.agent} className="net-row">
                    <AgentBadge agent={agent.agent} size={24} />
                    <span className="net-name ellipsis">{AGENT_META[agent.agent].short}</span>
                    <NetChip status={agent.net.status} />
                    <span className="net-figures">
                      <span className="net-figure num" title="首字延迟（中位数）">
                        <em>首字</em>
                        {fmtMs(agent.net.ttftMs)}
                      </span>
                      <span className="net-figure num" title="整次响应耗时（中位数）">
                        <em>响应</em>
                        {fmtMs(agent.net.latencyMs)}
                      </span>
                    </span>
                    <span className={agent.net.retries + agent.net.errors > 0 ? 'net-issues num warn' : 'net-issues num'} title="重试 / 报错次数">
                      {agent.net.retries + agent.net.errors > 0 ? `${agent.net.retries + agent.net.errors} 次异常` : '无异常'}
                    </span>
                  </div>
                ))}
            </div>
            {live.netEvents.length === 0 ? (
              <div className="net-events" style={{ '--rows': netEventRows } as CSSProperties}>
                <div className="muted">最近 24 小时没有重试或报错</div>
              </div>
            ) : null}
            <FillRows className="net-events" items={live.netEvents} base={3} extra={netEventExtra} style={{ '--rows': netEventRows } as CSSProperties}>
              {(event, index) => (
                <div key={`${event.ts}-${index}`} className="net-event">
                  <span className="num muted">{now - event.ts < 86_400_000 ? fmtTime(event.ts) : fmtDate(event.ts)}</span>
                  <span>{AGENT_META[event.agent].short}</span>
                  <span className={event.kind === 'error' ? 'tag bad' : 'tag'}>{event.code || (event.kind === 'error' ? '报错' : '重试')}</span>
                  <span className="muted ellipsis">{event.kind === 'retry' ? `第 ${event.attempt}${event.maxAttempts > 0 ? `/${event.maxAttempts}` : ''} 次重试` : '请求失败'}</span>
                </div>
              )}
            </FillRows>
          </Card>

          <Card title={live.rateLimits.length > 0 ? '额度与模型' : '模型排行'} rows={live.rateLimits.reduce((sum, limit) => sum + quotaRows(limit), 0) + modelRows}>
            {live.rateLimits.map((limit) => (
              <div key={limit.agent} className="quota-block" style={{ '--rows': quotaRows(limit) } as CSSProperties}>
                <div className="quota-title">
                  <AgentBadge agent={limit.agent} size={20} />
                  <span>{AGENT_META[limit.agent].short} 额度</span>
                  {limit.plan ? <span className="tag">{limit.plan}</span> : null}
                </div>
                {limit.primary ? <QuotaBar window={limit.primary} now={now} ts={limit.ts} /> : null}
                {limit.secondary ? <QuotaBar window={limit.secondary} now={now} ts={limit.ts} /> : null}
              </div>
            ))}
            {topModels.length === 0 ? <Empty>这段时间没有用量</Empty> : null}
            <FillRows
              className="model-list"
              items={range.byModel}
              base={topModels.length}
              extra={modelExtra}
              style={{ '--badges': Math.max(1, ...range.byModel.slice(0, topModels.length + modelExtra).map((model) => model.agents.length)), '--rows': modelRows } as CSSProperties}
            >
              {(model) => (
                <div key={model.model} className="model-row">
                  <ModelBadges model={model} />
                  <span className="model-name ellipsis" title={model.model}>
                    {model.model || '未知模型'}
                  </span>
                  <div className="model-bar">
                    <i style={{ width: `${(model.totals.total / topModelTotal) * 100}%`, background: agentFill(model.agents) }} />
                  </div>
                  <b className="num">{fmtTokens(model.totals.total)}</b>
                </div>
              )}
            </FillRows>
          </Card>
        </div>
      </div>
    </div>
  )
}
