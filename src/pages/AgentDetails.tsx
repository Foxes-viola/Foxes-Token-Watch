import { useEffect, useRef, useState, type ReactNode } from 'react'
import { AgentBadge, Card, Empty, NetChip, PhaseChip } from '../components/ui'
import { StackedBars } from '../components/charts'
import { everyWhileVisible, fetchAppInfo, fetchSessions } from '../lib/api'
import { AGENT_META, baishitongText, fmtDate, fmtDateTime, fmtFull, fmtMs, fmtTime, fmtTokens, sessionName } from '../lib/format'
import { AGENTS, type Agent, type RangeStats, type RateWindow, type SessionView, type Snapshot } from '../lib/types'

export function AgentDetails({agent, snapshot, range, period, controls, back}: {
  agent: Agent; snapshot: Snapshot; range: RangeStats; period: string; controls: ReactNode; back: () => void
}) {
  const [sessions, setSessions] = useState<SessionView[]>([])
  const [paths, setPaths] = useState<string[]>([])
  const [error, setError] = useState(false)
  const backButton = useRef<HTMLButtonElement>(null)
  useEffect(() => {
    backButton.current?.focus({preventScroll: true})
    backButton.current?.scrollIntoView({block: 'start'})
  }, [agent])
  useEffect(() => {
    let active = true
    async function refresh() {
      try {
        const [rows, info] = await Promise.all([fetchSessions(), fetchAppInfo()])
        if (active) {
          setSessions(rows.filter(row => row.agent === agent))
          setPaths(info.roots[AGENTS.indexOf(agent)] ?? [])
          setError(false)
        }
      } catch { if (active) setError(true) }
    }
    void refresh()
    const stop = everyWhileVisible(() => void refresh(), 5000)
    return () => { active = false; stop() }
  }, [agent])
  const meta = AGENT_META[agent]
  const live = snapshot.live.agents.find(a => a.agent === agent)
  const stats = range.byAgent.find(a => a.agent === agent)
  const t = stats?.totals
  const index = AGENTS.indexOf(agent)
  const columns = range.series.filter(p => p.t <= snapshot.generatedAt).map(p => ({
    t: p.t, label: range.unit === 'hour' ? fmtTime(p.t) : fmtDate(p.t), title: fmtDateTime(p.t),
    segments: [{key: agent, label: meta.short, value: p.byAgent[index], color: meta.color}], note: ''
  }))
  const counts = [
    ['输入', t?.input], ['缓存命中', t?.cacheRead], ['缓存写入', t?.cacheWrite],
    ['输出', t?.output], ['思考（含在输出内）', t?.reasoning], ['请求数', t?.requests]
  ] as const
  const notes: Partial<Record<Agent,string>> = {
    kiro: 'Kiro 用量按上下文占比、文本和工具记录估算。Credits 取自会话的用量摘要，账户额度取自 Kiro 客户端最近一次查询的结果。',
    cursor: '包含 Cursor 原生 Agent 和插件（网关、百事通）助手的对话。Cursor 已不在本地记录 token 数，用量按对话内容逐次累计上下文估算：每次请求重发全部上下文，上一次发过的部分按缓存命中计。网关卡片与它所在的 Cursor 对话合并为一个会话，同一次调用只统计一次。'
  }
  const pluginText = (range.plugin.totals.total > 0
    ? `其中插件（网关、百事通）对话 ${range.plugin.sessions} 个，共 ${fmtFull(range.plugin.totals.total)} token、${fmtFull(range.plugin.totals.requests)} 次请求。`
    : snapshot.plugin.lastActive > 0
      ? `插件（网关、百事通）对话：这段时间没有记录，最近一次记录在 ${fmtDateTime(snapshot.plugin.lastActive)}。`
      : '插件（网关、百事通）对话：还没有读到记录。')
    + '旧版网关插件把每次对话的过程记在本机；百事通的对话存在它的服务端，经插件在本机开的端口读取（只读不写，Cursor 开着、插件在运行时才读得到）。服务端只留着每一步的概要，所以这部分用量是按每一步的大小估算的；读到过的请求记在用量历史里，之后不再重算，也不会重复计数。'
  const feedText = snapshot.plugin.baishitong ? baishitongText(snapshot.plugin.baishitong, snapshot.generatedAt) : null
  const quota = snapshot.live.rateLimits.find(limit => limit.agent === agent)
  const quotaText = [quota?.primary, quota?.secondary]
    .filter((window): window is RateWindow => !!window && window.limit > 0)
    .map(window => `${window.label || '额度'} 已用 ${Number(window.used.toFixed(2))} / ${Number(window.limit.toFixed(2))} ${window.unit}`)
    .join(' · ')
  const noteTexts = [
    notes[agent] ? notes[agent] + (t?.estimated ? ' 本时段估算 ' + fmtFull(t.estimated) + ' token。' : '') : '',
    agent === 'cursor' ? pluginText : ''
  ].filter(Boolean)
  return <div className="page">
    <button ref={backButton} className="btn detail-back" onClick={back}>← 返回总览</button>
    <header className="page-head">
      <div className="detail-heading"><AgentBadge agent={agent} size={36}/><div><h1>{meta.label}</h1><p className="muted">{period} · {fmtDate(range.from)}{range.id !== 'today' && ' – ' + fmtDate(range.to)}</p></div></div>
      {controls}
    </header>
    <div className="grid">
      <div className="col span-7">
        <Card title={period + ' Token 用量'} extra={!!t?.estimated && <span className="tag">含估算</span>}>
          <div className="detail-total num">{fmtFull(t?.total ?? 0)}</div>
          <div className="stat-grid">{counts.map(([label,value]) => <div key={label}><span className="muted">{label}</span><b className="num">{fmtFull(value ?? 0)}</b></div>)}</div>
          {(agent === 'kiro' || quotaText) && <div className="set-note">
            {agent === 'kiro' && <p>已记录 {Number((t?.credits ?? 0).toFixed(3))} credits</p>}
            {quotaText && <p>账户额度：{quotaText}{quota?.ts ? `（${fmtDateTime(quota.ts)} 的数据）` : ''}</p>}
          </div>}
        </Card>
        <Card title={period + '用量趋势'}><StackedBars columns={columns} height={144}/></Card>
        <Card title="最近对话" extra={<span className="muted">每个对话的累计用量</span>}>
          {error && <p>读取对话列表失败，正在重试。</p>}
          {!sessions.length && !error && <Empty>暂无对话记录</Empty>}
          {sessions.slice(0,15).map(s => <div className="detail-row" key={s.key}>
            <div><b className="ellipsis">{sessionName(s)}</b><small>{[s.model, s.project, fmtDateTime(s.lastActive)].filter(Boolean).join(' · ')}</small></div>
            {s.running && <PhaseChip phase={s.phase} tool={s.tool}/>}
            <span className="num" title={fmtFull(s.totals.total)}>{s.totals.requests ? fmtTokens(s.totals.total) : '—'}</span>
          </div>)}
        </Card>
      </div>
      <div className="col span-5">
        <Card title="连接与活动" extra={live && <NetChip status={live.net.status}/>}>
          {live?.readError && <p role="alert">读取失败：{live.readError}，正在重试。</p>}
          <div className="stat-grid">
            <div><span className="muted">数据源</span><b>{live?.detected ? '已检测到' : '未发现'}</b></div>
            <div><span className="muted">运行中</span><b>{live?.running ?? 0} 个对话</b></div>
            <div><span className="muted">最近活动</span><b>{live?.lastActive ? fmtDateTime(live.lastActive) : '—'}</b></div>
            <div><span className="muted">请求耗时中位数</span><b>{live?.net.latencyMs ? fmtMs(live.net.latencyMs) : '—'}</b></div>
          </div>
          {paths.map(p => <p key={p} className="source-paths detail-path muted">{p}</p>)}
          {agent === 'cursor' && feedText && <p className={'set-note ' + (feedText.tone === 'bad' ? 'feed-flag bad' : feedText.tone === 'wait' ? 'feed-flag' : 'muted')} role={feedText.tone === 'bad' ? 'alert' : undefined}>百事通对话：{feedText.label} · {feedText.detail}</p>}
        </Card>
        {noteTexts.length > 0 && <Card title="统计说明">{noteTexts.map((text, i) => <p key={i} className="plain-note muted">{text}</p>)}</Card>}
        <Card title="近期工具活动">
          {!live?.recentTools.length && <Empty>此来源暂无工具记录</Empty>}
          {live?.recentTools.slice(0,15).map((tool,i) => <div className="detail-row" key={i}>
            <div><b className="ellipsis">{tool.name || '工具调用'}</b><small>{[tool.server, fmtDateTime(tool.ts)].filter(Boolean).join(' · ')}</small></div>
            {!tool.ok && <span className="tag bad">异常</span>}
          </div>)}
        </Card>
      </div>
    </div>
  </div>
}
