import { Fragment, useEffect, useMemo, useRef, useState } from 'react'
import { AgentBadge, Empty, Icon, PhaseChip, Segmented } from '../components/ui'
import { copyText, fetchSessionDetail, fetchSessions, useNow, useStored } from '../lib/api'
import { AGENT_META, EFFORT_META, fmtAgo, fmtDateTime, fmtFull, fmtMs, fmtPercent, fmtTime, fmtTokens, hitRate, sessionName } from '../lib/format'
import { AGENTS } from '../lib/types'
import type { Agent, SessionDetail, SessionView, Snapshot, ToolCall } from '../lib/types'

type Filter = 'all' | Agent
type SortKey = 'recent' | 'turns' | 'input' | 'cacheRead' | 'cacheWrite' | 'output' | 'hit' | 'total'
type Since = 'all' | 'today' | '7d' | '30d'

const FILTERS: { value: Filter; label: string }[] = [{ value: 'all', label: '全部' }, ...AGENTS.map((agent) => ({ value: agent as Filter, label: AGENT_META[agent].short }))]

const SINCE: { value: Since; label: string; days: number }[] = [
  { value: 'all', label: '全部时间', days: 0 },
  { value: 'today', label: '今天', days: 1 },
  { value: '7d', label: '近 7 天', days: 7 },
  { value: '30d', label: '近 30 天', days: 30 },
]

/** 各列排序时比的数 */
const SORT_VALUE: Record<SortKey, (session: SessionView) => number> = {
  recent: (session) => session.lastActive,
  turns: (session) => session.turns,
  input: (session) => session.totals.input,
  cacheRead: (session) => session.totals.cacheRead,
  cacheWrite: (session) => session.totals.cacheWrite,
  output: (session) => session.totals.output,
  hit: (session) => (session.totals.total > 0 ? hitRate(session.totals) : -1),
  total: (session) => session.totals.total,
}

/** 列表一次先画这么多行，往下还有就点按钮接着画：几千行一口气画出来会卡 */
const PAGE = 200
/** 明细里一次先列这么多次请求 */
const DETAIL_PAGE = 60
/** 下拉里最多列这么多个项目 / 模型 */
const MAX_OPTIONS = 150

const PLUGIN_TAG_HINT = '插件（网关、百事通）里的对话，算在 Cursor 里'

/** 表格里的一格：去掉会把格子切开的字符；以 = + - @ 开头的文字前面加一个单引号，贴进表格软件时不会被当成公式 */
function cell(value: string | number): string {
  if (typeof value === 'number') return String(value)
  const text = value.replace(/[\t\r\n]+/g, ' ')
  return /^[=+\-@]/.test(text) ? `'${text}` : text
}

/** 把列表拼成制表符分隔的文字，可以直接贴进表格软件；数字不缩写 */
function asTable(rows: SessionView[]): string {
  const head = ['对话', 'Agent', '插件', '子代理', '项目', '模型', '状态', '最近活动', '轮数', '输入', '缓存命中', '缓存写入', '输出', '命中率', '总量']
  const body = rows.map((session) =>
    [
      sessionName(session),
      AGENT_META[session.agent].label,
      session.plugin ? '是' : '',
      session.subAgent ? '是' : '',
      session.project,
      session.model,
      session.running ? '运行中' : '',
      session.lastActive > 0 ? fmtDateTime(session.lastActive) : '',
      session.turns,
      session.totals.input,
      session.totals.cacheRead,
      session.totals.cacheWrite,
      session.totals.output,
      session.totals.total > 0 ? fmtPercent(hitRate(session.totals), 0) : '',
      session.totals.total,
    ]
      .map(cell)
      .join('\t'),
  )
  return [head.join('\t'), ...body].join('\r\n')
}

/** 出现次数最多的几个名字（下拉筛选用）；正选着的那个即使不在里面也列出来，好让人看见、能改回去 */
function topNames(names: string[], chosen: string): string[] {
  const counts = new Map<string, number>()
  for (const name of names) if (name) counts.set(name, (counts.get(name) ?? 0) + 1)
  const list = [...counts.entries()]
    .sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))
    .slice(0, MAX_OPTIONS)
    .map(([name]) => name)
  if (chosen && !list.includes(chosen)) list.unshift(chosen)
  return list
}

interface ToolCount {
  name: string
  server: string
  count: number
  failed: number
}

function toolCounts(tools: ToolCall[]): ToolCount[] {
  const byName = new Map<string, ToolCount>()
  for (const tool of tools) {
    const id = `${tool.server}\n${tool.name}`
    const entry = byName.get(id) ?? { name: tool.name, server: tool.server, count: 0, failed: 0 }
    entry.count += 1
    if (!tool.ok) entry.failed += 1
    byName.set(id, entry)
  }
  return [...byName.values()].sort((a, b) => b.count - a.count || a.name.localeCompare(b.name))
}

function stamp(ts: number): string {
  return ts > 0 ? `${fmtDateTime(ts).slice(5, 10)} ${fmtTime(ts, true)}` : '—'
}

/** 一段对话的明细：逐次请求的用量和耗时，用过的工具。只有数字、模型名和工具名。 */
function Detail({ session }: { session: SessionView }) {
  const [detail, setDetail] = useState<SessionDetail | null | undefined>(undefined)
  const [shown, setShown] = useState(DETAIL_PAGE)

  // 这段对话的用量一变就重新取一次
  useEffect(() => {
    let current = true
    void fetchSessionDetail(session.key).then((next) => {
      if (current) setDetail(next)
    })
    return () => {
      current = false
    }
  }, [session.key, session.totals.total, session.totals.requests])

  const requests = useMemo(() => (detail ? [...detail.requests].reverse() : []), [detail])
  const tools = useMemo(() => (detail ? toolCounts(detail.tools) : []), [detail])

  if (detail === undefined) return <div className="sess-detail muted">正在读取明细…</div>
  if (detail === null) return <div className="sess-detail muted">这段对话的明细这会儿取不到，稍后再点开试试</div>

  const failed = detail.tools.filter((tool) => !tool.ok).length
  return (
    <div className="sess-detail">
      <div className="detail-head muted">
        <span>
          共 {fmtFull(detail.requestCount)} 次请求
          {detail.requestCount > detail.requests.length ? `（只列最近 ${detail.requests.length} 次）` : ''}
        </span>
        {session.started > 0 ? <span>开始于 {fmtDateTime(session.started)}</span> : null}
        {detail.toolCount > 0 ? (
          <span>
            {fmtFull(detail.toolCount)} 次工具调用
            {failed > 0 ? `，其中 ${failed} 次没成` : ''}
            {detail.toolCount > detail.tools.length ? `（按最近 ${detail.tools.length} 次统计）` : ''}
          </span>
        ) : null}
        {detail.usageOnly ? <span>原始记录不在这台电脑上，只有用量</span> : null}
      </div>

      {tools.length > 0 ? (
        <div className="detail-tools">
          {tools.slice(0, 16).map((tool) => (
            <span key={`${tool.server}\n${tool.name}`} className={tool.failed > 0 ? 'tag warn' : 'tag'} title={tool.server ? `MCP 服务：${tool.server}` : '内置工具'}>
              {tool.name} ×{tool.count}
              {tool.failed > 0 ? ` · ${tool.failed} 次没成` : ''}
            </span>
          ))}
          {tools.length > 16 ? <span className="muted">等 {tools.length} 种工具</span> : null}
        </div>
      ) : null}

      {requests.length > 0 ? (
        <div className="detail-table">
          <div className="dr th">
            <span>时间</span>
            <span>模型</span>
            <span className="r">输入</span>
            <span className="r">缓存命中</span>
            <span className="r">缓存写入</span>
            <span className="r">输出</span>
            <span className="r">其中思考</span>
            <span className="r">合计</span>
            <span className="r">首字</span>
            <span className="r">耗时</span>
          </div>
          {requests.slice(0, shown).map((row, index) => (
            <div key={`${row.ts}-${index}`} className="dr">
              <span className="num" title={fmtDateTime(row.ts)}>
                {stamp(row.ts)}
              </span>
              <span className="ellipsis" title={row.model}>
                {row.model || '—'}
                {row.effort !== 'unknown' ? <span className="tag">{EFFORT_META[row.effort].label}</span> : null}
                {row.subAgent ? <span className="tag">子代理</span> : null}
                {row.estimated ? (
                  <span className="tag" title="记录里没有 token 数，这次请求的用量是估算的">
                    估算
                  </span>
                ) : null}
              </span>
              <span className="r num">{fmtTokens(row.input)}</span>
              <span className="r num">{row.cacheRead > 0 ? fmtTokens(row.cacheRead) : '—'}</span>
              <span className="r num">{row.cacheWrite > 0 ? fmtTokens(row.cacheWrite) : '—'}</span>
              <span className="r num">{fmtTokens(row.output)}</span>
              <span className="r num">{row.reasoning > 0 ? fmtTokens(row.reasoning) : '—'}</span>
              <span className="r num strong" title={fmtFull(row.total)}>
                {fmtTokens(row.total)}
              </span>
              <span className="r num">{fmtMs(row.ttftMs)}</span>
              <span className="r num">{fmtMs(row.latencyMs)}</span>
            </div>
          ))}
        </div>
      ) : (
        <div className="muted">这段对话还没有记到请求</div>
      )}
      {requests.length > shown ? (
        <div className="table-more">
          <button className="toggle" onClick={() => setShown(shown + DETAIL_PAGE * 4)}>
            再列 {Math.min(DETAIL_PAGE * 4, requests.length - shown)} 次（还有 {requests.length - shown} 次）
          </button>
        </div>
      ) : null}
    </div>
  )
}

export function Sessions({ snapshot }: { snapshot: Snapshot }) {
  const [sessions, setSessions] = useState<SessionView[] | null>(null)
  const [filter, setFilter] = useStored<Filter>('sessions.agent', 'all', (value) => value === 'all' || AGENTS.includes(value as Agent))
  const [sortKey, setSortKey] = useStored<SortKey>('sessions.sort', 'recent', (value) => typeof value === 'string' && value in SORT_VALUE)
  const [sortDesc, setSortDesc] = useStored<boolean>('sessions.desc', true)
  const [showSubAgents, setShowSubAgents] = useStored<boolean>('sessions.subAgents', false)
  const [onlyPlugin, setOnlyPlugin] = useStored<boolean>('sessions.plugin', false)
  const [project, setProject] = useStored<string>('sessions.project', '')
  const [model, setModel] = useStored<string>('sessions.model', '')
  const [since, setSince] = useStored<Since>('sessions.since', 'all', (value) => SINCE.some((item) => item.value === value))
  const [query, setQuery] = useState('')
  const [onlyRunning, setOnlyRunning] = useState(false)
  const [limit, setLimit] = useState(PAGE)
  const [open, setOpen] = useState<string | null>(null)
  const [copied, setCopied] = useState<string | null>(null)
  const now = useNow(5000)
  const lastFetch = useRef(0)
  const requestId = useRef(0)

  // 快照每变一次就重新取一次列表，但最多每 2 秒一次
  useEffect(() => {
    const wait = Math.max(0, 2000 - (Date.now() - lastFetch.current))
    const timer = window.setTimeout(() => {
      lastFetch.current = Date.now()
      const id = ++requestId.current
      void fetchSessions().then((rows) => {
        if (id === requestId.current) setSessions(rows)
      })
    }, wait)
    return () => window.clearTimeout(timer)
  }, [snapshot.generatedAt])

  // 筛选一变，列表从头画起
  useEffect(() => setLimit(PAGE), [filter, sortKey, sortDesc, query, showSubAgents, onlyRunning, onlyPlugin, project, model, since])

  // 今天零点；只在跨天时变
  const today = new Date(now).setHours(0, 0, 0, 0)

  // 只按 agent 和子代理开关筛过的列表：项目、模型两个下拉里列的就是这里面有的
  const scoped = useMemo(
    () => (sessions ?? []).filter((session) => (filter === 'all' || session.agent === filter) && (showSubAgents || !session.subAgent)),
    [sessions, filter, showSubAgents],
  )
  const projects = useMemo(() => topNames(scoped.map((session) => session.project), project), [scoped, project])
  const models = useMemo(() => topNames(scoped.map((session) => session.model), model), [scoped, model])

  const rows = useMemo(() => {
    const needle = query.trim().toLowerCase()
    const days = SINCE.find((item) => item.value === since)?.days ?? 0
    const from = days > 0 ? today - (days - 1) * 86_400_000 : 0
    const list = scoped.filter((session) => {
      if (onlyRunning && !session.running) return false
      if (onlyPlugin && !session.plugin) return false
      if (project && session.project !== project) return false
      if (model && session.model !== model) return false
      if (from > 0 && session.lastActive < from) return false
      if (!needle) return true
      return [session.title, session.project, session.model, session.cwd].some((text) => text.toLowerCase().includes(needle))
    })
    const value = SORT_VALUE[sortKey]
    list.sort((a, b) => {
      // 按最近活动从新到旧排时，正在运行的排在最前面
      if (sortKey === 'recent' && sortDesc) {
        const running = Number(b.running) - Number(a.running)
        if (running !== 0) return running
      }
      const diff = value(a) - value(b)
      return (sortDesc ? -diff : diff) || b.lastActive - a.lastActive
    })
    return list
  }, [scoped, sortKey, sortDesc, query, onlyRunning, onlyPlugin, project, model, since, today])

  const subAgentCount = (sessions ?? []).filter((session) => session.subAgent).length
  const pluginCount = (sessions ?? []).filter((session) => session.plugin && !session.subAgent).length
  const narrowed = filter !== 'all' || onlyRunning || onlyPlugin || project !== '' || model !== '' || since !== 'all' || query.trim() !== ''

  const sortBy = (key: SortKey) => {
    if (key === sortKey) setSortDesc(!sortDesc)
    else {
      setSortKey(key)
      setSortDesc(true)
    }
  }

  const clearFilters = () => {
    setFilter('all')
    setOnlyRunning(false)
    setOnlyPlugin(false)
    setProject('')
    setModel('')
    setSince('all')
    setQuery('')
  }

  const copyRows = () => {
    void copyText(asTable(rows)).then((done) => {
      setCopied(done ? `已复制 ${rows.length} 行` : '没复制成')
      window.setTimeout(() => setCopied(null), 2500)
    })
  }

  const head = (key: SortKey, label: string, right = true) => (
    <button
      type="button"
      className={`th-sort${right ? ' r' : ''}${sortKey === key ? ' on' : ''}`}
      onClick={() => sortBy(key)}
      title={sortKey === key ? (sortDesc ? '现在从大到小，再点一次倒过来' : '现在从小到大，再点一次倒过来') : '点一下按这一列排序'}
    >
      {label}
      {sortKey === key ? <span className="th-arrow">{sortDesc ? '↓' : '↑'}</span> : null}
    </button>
  )

  return (
    <div className="page">
      <header className="page-head">
        <div>
          <h1>对话</h1>
          <p className="muted">
            共 {sessions ? sessions.length - subAgentCount : '…'} 个对话
            {subAgentCount > 0 ? `，另有 ${subAgentCount} 个子代理记录` : ''}
            {pluginCount > 0 ? `，其中插件对话 ${pluginCount} 个` : ''} · {snapshot.live.running} 个正在运行
            {sessions && narrowed ? ` · 筛选出 ${rows.length} 条` : ''}
          </p>
        </div>
        <div className="card-tools">
          <label className="search">
            <Icon name="search" size={15} />
            <input value={query} onChange={(event) => setQuery(event.target.value)} placeholder="搜索标题、项目、模型" spellCheck={false} />
          </label>
          <select className="pick" value={project} onChange={(event) => setProject(event.target.value)} title="只看某个项目的对话">
            <option value="">全部项目</option>
            {projects.map((name) => (
              <option key={name} value={name}>
                {name}
              </option>
            ))}
          </select>
          <select className="pick" value={model} onChange={(event) => setModel(event.target.value)} title="只看用某个模型的对话">
            <option value="">全部模型</option>
            {models.map((name) => (
              <option key={name} value={name}>
                {name}
              </option>
            ))}
          </select>
          <select className="pick" value={since} onChange={(event) => setSince(event.target.value as Since)} title="按最近活动的时间筛选">
            {SINCE.map((item) => (
              <option key={item.value} value={item.value}>
                {item.label}
              </option>
            ))}
          </select>
        </div>
      </header>

      <div className="filter-row">
        <Segmented options={FILTERS} value={filter} onChange={setFilter} />
        <div className="card-tools">
          {subAgentCount > 0 ? (
            <button className={showSubAgents ? 'toggle on' : 'toggle'} onClick={() => setShowSubAgents(!showSubAgents)}>
              显示子代理
            </button>
          ) : null}
          <button className={onlyRunning ? 'toggle on' : 'toggle'} onClick={() => setOnlyRunning(!onlyRunning)}>
            只看运行中
          </button>
          {pluginCount > 0 || onlyPlugin ? (
            <button className={onlyPlugin ? 'toggle on' : 'toggle'} onClick={() => setOnlyPlugin(!onlyPlugin)} title={PLUGIN_TAG_HINT}>
              只看插件对话
            </button>
          ) : null}
          {narrowed ? (
            <button className="toggle" onClick={clearFilters} title="回到全部对话">
              清除筛选
            </button>
          ) : null}
          <button className="toggle" onClick={copyRows} disabled={rows.length === 0} title="把现在筛选出来的列表（全部，不只是画出来的这些）复制到剪贴板，可以直接贴进 Excel 等表格软件；数字不缩写">
            {copied ?? '复制成表格'}
          </button>
        </div>
      </div>

      <div className="card table-card">
        <div className="table sess-table">
          <div className="tr th">
            <span>对话</span>
            {head('recent', '最近活动', false)}
            {head('turns', '轮数')}
            {head('input', '输入')}
            {head('cacheRead', '缓存命中')}
            {head('cacheWrite', '缓存写入')}
            {head('output', '输出')}
            {head('hit', '命中率')}
            {head('total', '总量')}
          </div>
          {sessions === null ? <Empty>正在读取…</Empty> : null}
          {sessions !== null && rows.length === 0 ? <Empty>没有符合条件的对话</Empty> : null}
          {rows.slice(0, limit).map((session) => {
            const expanded = open === session.key
            const toggle = () => setOpen(expanded ? null : session.key)
            return (
              <Fragment key={session.key}>
                <div
                  className={`tr clickable${session.running ? ' running' : ''}${expanded ? ' open' : ''}`}
                  role="button"
                  tabIndex={0}
                  aria-expanded={expanded}
                  title={expanded ? undefined : '点一下看这段对话的逐次请求'}
                  onClick={toggle}
                  onKeyDown={(event) => {
                    if (event.key === 'Enter' || event.key === ' ') {
                      event.preventDefault()
                      toggle()
                    }
                  }}
                >
                  <span className="sess-cell">
                    <AgentBadge agent={session.agent} size={26} />
                    <span className="sess-main">
                      <span className="sess-title ellipsis">
                        {sessionName(session)}
                        {session.plugin ? (
                          <span className="tag" title={PLUGIN_TAG_HINT}>
                            插件
                          </span>
                        ) : null}
                        {session.subAgent ? <span className="tag">子代理</span> : null}
                        {session.sourceElsewhere ? (
                          <span className="tag" title="这段对话是在别的电脑上记下的，数字由用量历史带过来">
                            其他电脑
                          </span>
                        ) : session.sourceGone ? (
                          <span className="tag" title="原始记录已被删除，数字来自此前的统计">
                            记录已删
                          </span>
                        ) : null}
                      </span>
                      <span className="sess-meta muted ellipsis" title={session.cwd}>
                        {[session.project, session.model, session.subAgents > 0 ? `${session.subAgents} 个子代理` : ''].filter(Boolean).join(' · ') || '—'}
                      </span>
                    </span>
                  </span>
                  <span>{session.running ? <PhaseChip phase={session.phase} tool={session.tool} /> : <span className="muted">{fmtAgo(session.lastActive, now)}</span>}</span>
                  <span className="r num">{session.turns || '—'}</span>
                  <span className="r num">{fmtTokens(session.totals.input)}</span>
                  <span className="r num">{fmtTokens(session.totals.cacheRead)}</span>
                  <span className="r num">{session.totals.cacheWrite > 0 ? fmtTokens(session.totals.cacheWrite) : '—'}</span>
                  <span className="r num">{fmtTokens(session.totals.output)}</span>
                  <span className="r num">{session.totals.total > 0 ? fmtPercent(hitRate(session.totals), 0) : '—'}</span>
                  <span className="r num strong" title={fmtFull(session.totals.total)}>
                    {fmtTokens(session.totals.total)}
                  </span>
                </div>
                {expanded ? <Detail session={session} /> : null}
              </Fragment>
            )
          })}
          {rows.length > limit ? (
            <div className="muted table-more">
              已列出 {limit} 条，共 {rows.length} 条
              <button className="toggle" onClick={() => setLimit(limit + PAGE * 2)}>
                再列 {Math.min(PAGE * 2, rows.length - limit)} 条
              </button>
              <button className="toggle" onClick={() => setLimit(rows.length)}>
                全部列出
              </button>
            </div>
          ) : null}
        </div>
      </div>
    </div>
  )
}
