import { StrictMode, useEffect, useLayoutEffect, useRef, useState } from 'react'
import type { PointerEvent } from 'react'
import { createRoot } from 'react-dom/client'
import './styles/base.css'
import './styles/island.css'
import { CompositionBar } from './components/charts'
import { AgentBadge, AppIcon, Icon, PhaseChip } from './components/ui'
import { inApp, islandRegion, onIslandLeave, openPanel, startDragging, updateSettings, useIslandDock, useNow, useSettings, useSnapshot } from './lib/api'
import { AGENT_META, NET_META, fmtAgo, fmtClock, fmtDuration, fmtMs, fmtPercent, fmtTokens, hitRate, sessionName } from './lib/format'
import type { FinishedTurn, Snapshot, Totals } from './lib/types'

// 这几个尺寸要和 island.css 以及后台 main.rs 里的灵动岛窗口尺寸对上（单位：逻辑像素）
const TOP = 2
const PILL_HEIGHT = 34
const CARD_WIDTH = 340
/**
 * 岛周围留给阴影（展开时还有回弹动画）的范围：左右、下方。这圈之外鼠标可以直接点到后面的窗口，
 * 所以收起时留得很窄，尽量不挡住胶囊旁边的东西。
 */
const MARGIN_CLOSED = { side: 5, bottom: 9 }
const MARGIN_OPEN = { side: 14, bottom: 26 }
const MAX_ROWS = 4
const MORPH_MS = 360
/** 对话跑完一轮后，小结在展开卡片里保留多久 */
const TOAST_MS = 60_000
/** 头像上的“刚完成”标记保留多久 */
const DONE_BADGE_MS = 8000

const HOLD_OPEN = new URLSearchParams(location.search).has('hold')

const EMPTY_TOTALS: Totals = { input: 0, cacheRead: 0, cacheWrite: 0, output: 0, reasoning: 0, reasoningEstimated: 0, estimated: 0, credits: 0, total: 0, requests: 0, thinkMs: 0 }

interface Box {
  width: number
  height: number
}

/** 最近一次“跑完一轮”的对话；软件启动前就结束的不算，过一段时间自动清掉 */
function useFinished(snapshot: Snapshot | null, enabled: boolean): FinishedTurn | null {
  const [latest, setLatest] = useState<FinishedTurn | null>(null)
  const seen = useRef<number | null>(null)
  const finished = snapshot?.live.finished

  useEffect(() => {
    if (!enabled) setLatest(null)
    if (!finished) return
    const newest = finished.reduce<FinishedTurn | null>((best, item) => (!best || item.endedAt > best.endedAt ? item : best), null)
    const newestAt = newest?.endedAt ?? 0
    if (seen.current === null) {
      seen.current = newestAt
      return
    }
    if (newest && newestAt > seen.current) {
      seen.current = newestAt
      if (enabled) setLatest(newest)
    }
  }, [finished, enabled])

  useEffect(() => {
    if (!latest) return
    const timer = window.setTimeout(() => setLatest(null), TOAST_MS)
    return () => window.clearTimeout(timer)
  }, [latest])

  return enabled ? latest : null
}

function useSize<T extends HTMLElement>(): [React.RefObject<T | null>, Box] {
  const ref = useRef<T>(null)
  const [box, setBox] = useState<Box>({ width: 0, height: 0 })
  useLayoutEffect(() => {
    const element = ref.current
    if (!element) return
    const measure = () => {
      const rect = element.getBoundingClientRect()
      // 宽度取偶数，居中后才不会落在半个像素上发虚
      const next = { width: Math.ceil(rect.width / 2) * 2, height: Math.ceil(rect.height) }
      setBox((current) => (current.width === next.width && current.height === next.height ? current : next))
    }
    const observer = new ResizeObserver(measure)
    observer.observe(element)
    measure()
    return () => observer.disconnect()
  }, [])
  return [ref, box]
}

function Island() {
  const snapshot = useSnapshot()
  const settings = useSettings()
  const side = useIslandDock()
  const now = useNow(1000)
  const [hovered, setHovered] = useState(false)
  const finished = useFinished(snapshot, settings.notifyFinished)
  const open = snapshot !== null && (hovered || HOLD_OPEN)

  const tucked = side !== null && settings.edgeAutoHide && !open
  const [pillRef, pill] = useSize<HTMLDivElement>()
  const [cardRef, card] = useSize<HTMLDivElement>()

  // 展开前放大窗口和可点范围，收起动画结束后再缩小。
  const target: Box = open ? { width: CARD_WIDTH, height: Math.max(card.height, PILL_HEIGHT) } : { width: Math.max(pill.width, 60), height: PILL_HEIGHT }
  const visibleBox = tucked ? { width: 8, height: 42 } : target
  const [box, setBox] = useState<Box>({ width: 96, height: PILL_HEIGHT })
  const region = useRef<Box>({ width: 0, height: 0 })
  useEffect(() => {
    let active = true
    let timer = 0
    const apply = (size: Box) => {
      region.current = size
      return islandRegion(size.width, size.height, open)
    }
    if (side) {
      // 屏幕接缝处原生裁剪和可见把手使用同一尺寸。
      setBox(visibleBox)
      void apply(visibleBox)
      return
    }
    const margin = open ? MARGIN_OPEN : MARGIN_CLOSED
    const wanted = { width: target.width + 2 * margin.side, height: TOP + target.height + margin.bottom }
    const current = region.current
    const grown = { width: Math.max(wanted.width, current.width), height: Math.max(wanted.height, current.height) }
    const ready = apply(grown)
    void ready.then(() => {
      if (!active) return
      setBox(target)
      timer = window.setTimeout(() => {
        if (region.current.width !== wanted.width || region.current.height !== wanted.height) void apply(wanted)
      }, MORPH_MS)
    })
    return () => {
      active = false
      window.clearTimeout(timer)
    }
  }, [visibleBox.width, visibleBox.height, open, side])

  // 进入和离开都稍等一下再生效，鼠标只是路过时不会一闪一闪
  const hoverTimer = useRef(0)
  const hoverWanted = useRef(false)
  const sideRef = useRef(side)
  sideRef.current = side
  const hover = (value: boolean) => {
    if (hoverWanted.current === value) return
    hoverWanted.current = value
    window.clearTimeout(hoverTimer.current)
    hoverTimer.current = window.setTimeout(() => setHovered(value), value ? (sideRef.current ? 150 : 110) : (sideRef.current ? 600 : 260))
  }
  useEffect(() => {
    const leave = () => hover(false)
    document.addEventListener('mouseleave', leave)
    // 后台发现鼠标已经不在岛上（比如拖动之后没收到离开事件）时也会通知一次
    const unlisten = onIslandLeave(leave)
    return () => {
      window.clearTimeout(hoverTimer.current)
      document.removeEventListener('mouseleave', leave)
      unlisten()
    }
  }, [])

  // 按住拖动超过几像素算拖动窗口，否则算点击（打开面板）
  const press = useRef<{ x: number; y: number; dragged: boolean } | null>(null)
  const onPointerDown = (event: PointerEvent<HTMLElement>) => {
    if (event.button !== 0) return
    event.currentTarget.setPointerCapture(event.pointerId)
    press.current = { x: event.screenX, y: event.screenY, dragged: false }
  }
  const onPointerMove = (event: PointerEvent) => {
    const state = press.current
    if (!state || state.dragged || event.buttons !== 1) return
    if (Math.abs(event.screenX - state.x) + Math.abs(event.screenY - state.y) > 4) {
      state.dragged = true
      startDragging()
    }
  }
  const onClick = () => {
    const state = press.current
    press.current = null
    if (!state?.dragged) openPanel()
  }
  const grip = { onPointerDown, onPointerMove, onClick }

  const live = snapshot?.live
  const running = (live?.sessions ?? []).filter((session) => session.running && !session.subAgent)
  const count = live?.running ?? 0
  const today = snapshot?.ranges.find((range) => range.id === 'today')?.totals ?? EMPTY_TOTALS
  const net = live?.net.status ?? 'idle'
  const netBad = net === 'unstable' || net === 'down'
  const lastActive = Math.max(0, ...(live?.agents ?? []).map((agent) => agent.lastActive))
  const justDone = finished !== null && now - finished.endedAt < DONE_BADGE_MS

  // 头像角上的小圆点：网络异常 > 刚跑完一轮 > 有对话在运行；空闲时不显示
  const badge = netBad ? `dot pulse tone-${NET_META[net].tone}` : justDone ? 'dot done' : count > 0 ? 'dot pulse tone-ok' : ''
  const badgeTitle = netBad ? `网络${NET_META[net].label}` : justDone ? '有对话刚跑完一轮' : count > 0 ? `${count} 个对话运行中` : ''

  const parts = [
    { key: 'input', label: '输入', color: 'var(--c-input)', value: today.input },
    { key: 'cacheRead', label: '缓存命中', color: 'var(--c-cache-read)', value: today.cacheRead },
    { key: 'cacheWrite', label: '缓存写入', color: 'var(--c-cache-write)', value: today.cacheWrite },
    { key: 'output', label: '输出', color: 'var(--c-output)', value: today.output },
  ]
  const cells = [...parts, { key: 'reasoning', label: '思考', color: 'var(--c-think)', value: today.reasoning }]

  return (
    <div data-dock={side ?? undefined} data-tucked={tucked || undefined} className={open ? 'island open' : 'island'} style={{ width: box.width, height: box.height }} onMouseEnter={() => hover(true)} onMouseMove={() => hover(true)} onMouseLeave={() => hover(false)}>
      <button className={justDone ? 'edge-grip done' : 'edge-grip'} {...grip} aria-label="灵动岛，点击打开面板，拖动调整位置" title="悬停展开，按住拖动">
        <span className={netBad ? 'tone-' + NET_META[net].tone : count > 0 ? 'tone-ok' : ''} />
      </button>
      <div className="pill" ref={pillRef} {...grip} title={badgeTitle || '点击打开面板，按住可以拖动'}>
        <span className={justDone ? 'avatar flash' : 'avatar'}>
          <AppIcon size={22} />
          {badge ? <i className={badge} /> : null}
        </span>
        <b className="num">{snapshot ? fmtTokens(today.total) : '…'}</b>
      </div>

      <div className="isl-card" ref={cardRef}>
        <div className="isl-head" {...grip} title="点击打开面板，按住可以拖动">
          <AppIcon size={22} />
          <span className="isl-name">Foxes Token Watch</span>
          <span className={count > 0 ? 'isl-state on' : 'isl-state'}>
            <span className={count > 0 ? 'dot pulse tone-ok' : 'dot tone-idle'} />
            {count > 0 ? (
              <>
                <b className="num">{count}</b> 个对话运行中
              </>
            ) : (
              '空闲'
            )}
          </span>
        </div>

        {snapshot && live ? (
          <div className="isl-body">
            {finished ? (
              <div className="isl-toast">
                <Icon name="check" size={15} />
                <span className="ellipsis">
                  {AGENT_META[finished.agent].short} 跑完了一轮{finished.project ? ` · ${finished.project}` : ''}
                </span>
                <span className="num">
                  {fmtDuration(finished.durationMs)} · {fmtTokens(finished.totals.total)}
                </span>
              </div>
            ) : null}

            <div className="isl-section">
              {running.length === 0 ? (
                <div className="isl-empty muted">
                  当前没有对话在运行{lastActive > 0 ? ` · 上次活动 ${fmtAgo(lastActive, now)}` : ''}
                </div>
              ) : null}
              {running.slice(0, MAX_ROWS).map((session) => (
                <div key={session.key} className="isl-row">
                  <AgentBadge agent={session.agent} size={24} />
                  <div className="isl-main">
                    <div className="isl-title ellipsis">{sessionName(session)}</div>
                    <div className="isl-meta muted ellipsis">
                      本轮 <span className="num">{fmtClock(now - session.turnStarted)}</span> · <span className="num">{fmtTokens(session.turn.total)}</span>
                      {session.runningSubAgents > 0 ? ` · ${session.runningSubAgents} 个子代理` : ''}
                    </div>
                  </div>
                  <PhaseChip phase={session.phase} tool={session.tool} />
                </div>
              ))}
              {running.length > MAX_ROWS ? <div className="isl-more muted">还有 {running.length - MAX_ROWS} 个对话在运行</div> : null}
            </div>

            <div className="isl-section">
              <div className="isl-today">
                <span className="muted">今日总量</span>
                <b className="num">{fmtTokens(today.total)}</b>
                <span className="isl-hit muted">
                  缓存命中率 <b className="num">{today.total > 0 ? fmtPercent(hitRate(today)) : '—'}</b>
                </span>
              </div>
              <CompositionBar parts={parts} height={6} />
              <div className="isl-stats">
                {cells.map((cell) => (
                  <div key={cell.key}>
                    <span className="muted">
                      <i style={{ background: cell.color }} />
                      {cell.label}
                    </span>
                    <b className="num">{fmtTokens(cell.value)}</b>
                  </div>
                ))}
              </div>
            </div>

            <div className="isl-section isl-net">
              {live.agents
                .filter((agent) => agent.detected)
                .map((agent) => (
                  <span key={agent.agent} className={`isl-net-item tone-${NET_META[agent.net.status].tone}`} title={NET_META[agent.net.status].hint}>
                    <span className="dot" />
                    <span className="isl-net-name">{AGENT_META[agent.agent].short}</span>
                    <span className="muted ellipsis">
                      {NET_META[agent.net.status].label}
                      {agent.net.ttftMs > 0 ? ` · ${fmtMs(agent.net.ttftMs)}` : ''}
                    </span>
                  </span>
                ))}
            </div>

            <div className="isl-foot">
              <button className="isl-btn primary" onClick={openPanel}>
                <Icon name="panel" size={14} />
                打开面板
              </button>
              <button className="isl-btn" title="之后可以在托盘菜单或设置里重新打开" onClick={() => void updateSettings({ island: false })}>
                隐藏灵动岛
              </button>
            </div>
          </div>
        ) : null}
      </div>
    </div>
  )
}

if (!inApp) document.documentElement.classList.add('dev')

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <Island />
  </StrictMode>,
)
