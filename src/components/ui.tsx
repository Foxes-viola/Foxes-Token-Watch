import { useLayoutEffect, useRef, useState } from 'react'
import type { CSSProperties, ReactNode } from 'react'
import appIconUrl from '../../design/icon.svg'
import { AGENT_META, NET_META, PHASE_META } from '../lib/format'
import type { Agent, NetStatus, Phase } from '../lib/types'

const ICONS = {
  overview: (
    <>
      <rect x="3.5" y="3.5" width="7" height="7" rx="2" />
      <rect x="13.5" y="3.5" width="7" height="7" rx="2" />
      <rect x="3.5" y="13.5" width="7" height="7" rx="2" />
      <rect x="13.5" y="13.5" width="7" height="7" rx="2" />
    </>
  ),
  sessions: (
    <>
      <path d="M4 6.5A2.5 2.5 0 0 1 6.5 4h11A2.5 2.5 0 0 1 20 6.5v7a2.5 2.5 0 0 1-2.5 2.5H11l-4.5 3.5V16A2.5 2.5 0 0 1 4 13.5z" />
      <path d="M8.5 9h7M8.5 12h4.5" />
    </>
  ),
  models: (
    <>
      <path d="M12 3.5 20 8l-8 4.5L4 8z" />
      <path d="m4 12 8 4.5 8-4.5" />
      <path d="m4 16 8 4.5 8-4.5" />
    </>
  ),
  network: <path d="M3 12h4l2.5-6 4 12 2.5-6H21" />,
  settings: (
    <>
      <path d="M4 7h9M17 7h3M4 17h3M11 17h9" />
      <circle cx="15" cy="7" r="2" />
      <circle cx="9" cy="17" r="2" />
    </>
  ),
  minimize: <path d="M6 12h12" />,
  maximize: <rect x="6.5" y="6.5" width="11" height="11" rx="1.5" />,
  close: <path d="m7 7 10 10M17 7 7 17" />,
  panel: (
    <>
      <rect x="3.5" y="4.5" width="17" height="15" rx="2.5" />
      <path d="M3.5 9h17" />
    </>
  ),
  search: (
    <>
      <circle cx="11" cy="11" r="6" />
      <path d="m16 16 4 4" />
    </>
  ),
  bolt: <path d="M13 3 5 13.5h6L10 21l8-10.5h-6z" />,
  check: <path d="m5 12.5 4.5 4.5L19 7.5" />,
} satisfies Record<string, ReactNode>

export type IconName = keyof typeof ICONS

export function Icon({ name, size = 20 }: { name: IconName; size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
      {ICONS[name]}
    </svg>
  )
}

/** 软件图标：直接用 design/icon.svg 这张图（exe、安装包、任务栏上的图标都是拿它出的），页面里的标志和软件图标是同一张 */
export function AppIcon({ size = 20 }: { size?: number }) {
  return <img className="app-icon" src={appIconUrl} width={size} height={size} alt="" draggable={false} />
}

/** 主页顶部的品牌区：带光晕的软件图标 + 艺术字 */
export function BrandMark({ children }: { children?: ReactNode }) {
  return (
    <div className="brandmark">
      <div className="brandmark-logo">
        <AppIcon size={64} />
      </div>
      <div className="brandmark-text">
        <div className="wordmark" aria-label="Foxes Token Watch">
          <span className="wordmark-fox">FOXES</span>
          <span className="wordmark-rest">Token Watch</span>
        </div>
        {children ? <div className="brandmark-sub">{children}</div> : null}
      </div>
    </div>
  )
}

const BADGE_TEXT: Record<Agent, string> = { claude: 'Cl', codex: 'Cx', deepseek: 'Ds', antigravity: 'Ag', kiro: 'Ki', cursor: 'Cu', webmcp: 'W' }

export function AgentBadge({ agent, size = 22, title }: { agent: Agent; size?: number; title?: string }) {
  const style = { width: size, height: size, fontSize: Math.round(size * 0.46), '--badge-color': AGENT_META[agent].color } as CSSProperties
  return (
    <span className="agent-badge" style={style} title={title ?? AGENT_META[agent].label}>
      {BADGE_TEXT[agent]}
    </span>
  )
}

export function PhaseChip({ phase, tool }: { phase: Phase; tool?: string }) {
  const meta = PHASE_META[phase]
  return (
    <span className={`chip tone-${meta.tone}`}>
      <span className={phase === 'idle' ? 'dot' : 'dot pulse'} />
      {meta.label}
      {phase === 'tool' && tool ? <span className="chip-extra">{tool}</span> : null}
    </span>
  )
}

export function NetChip({ status }: { status: NetStatus }) {
  const meta = NET_META[status]
  return (
    <span className={`chip tone-${meta.tone}`} title={meta.hint}>
      <span className={status === 'unstable' || status === 'down' ? 'dot pulse' : 'dot'} />
      {meta.label}
    </span>
  )
}

export function Segmented<T extends string>({ options, value, onChange }: { options: { value: T; label: string }[]; value: T; onChange: (value: T) => void }) {
  return (
    <div className="segmented" role="tablist">
      {options.map((option) => (
        <button key={option.value} role="tab" aria-selected={option.value === value} className={option.value === value ? 'active' : ''} onClick={() => onChange(option.value)}>
          {option.label}
        </button>
      ))}
    </div>
  )
}

export function Switch({ on, onChange }: { on: boolean; onChange: (on: boolean) => void }) {
  return <button className={on ? 'switch on' : 'switch'} role="switch" aria-checked={on} onClick={() => onChange(!on)} />
}

/**
 * rows：这张卡片里有几行内容。卡片排在底边对齐的两列里（.col.flush）、所在那列又比旁边矮时，
 * 多出来的高度按这个数分给各张卡片，每一行拉开的距离才一样；不给就按 1 行算。
 */
export function Card({ title, extra, className = '', rows, children }: { title?: ReactNode; extra?: ReactNode; className?: string; rows?: number; children: ReactNode }) {
  return (
    <section className={`card ${className}`} style={rows === undefined ? undefined : ({ '--rows': rows } as CSSProperties)}>
      {title || extra ? (
        <header className="card-head">
          <span className="card-title">{title}</span>
          {extra}
        </header>
      ) : null}
      {children}
    </section>
  )
}

/**
 * 被拉高时能多显示几行的列表。平时只显示 items 的前 base 行（至少 1 行），列表占多高也只按这几行算；
 * 排在底边对齐的两列里（.col.flush）、所在那列比旁边矮而被拉高时，多出来的高度够放几行就再多显示几行
 * （最多再加 extra 行），剩下不够一行的零头匀进行距里。多显示的行不算进列表自己的高度，
 * 所以不会反过来把这一列撑高，旁边那列变矮时它们会自己收回去。items 为空时什么都不画。
 */
export function FillRows<T>({ items, base, extra, className, style, children }: { items: T[]; base: number; extra: number; className: string; style?: CSSProperties; children: (item: T, index: number) => ReactNode }) {
  const list = useRef<HTMLDivElement>(null)
  // size：只显示平时那几行时列表该占的高度；room：列表现在的高度放得下几行
  const [fit, setFit] = useState<{ size: number; room: number } | null>(null)
  const empty = items.length === 0
  const baseRows = Math.min(items.length, Math.max(1, base))
  const measure = () => {
    const node = list.current
    if (!node) return
    const px = (value: string) => parseFloat(value) || 0
    const box = getComputedStyle(node)
    const gap = px(box.rowGap)
    // 只量平时就显示的那几行：量出来的结果不随多显示了几行而变，行数不会来回跳。
    // 行被拉开以后量不出它本来的高度，改量行里的内容：内容在行里上下居中，不跟着拉高
    let sum = 0
    let tallest = 0
    for (const item of Array.from(node.children).slice(0, baseRows)) {
      const own = getComputedStyle(item)
      const content = Math.max(0, ...Array.from(item.children, (child) => child.getBoundingClientRect().height))
      const height = content + px(own.paddingTop) + px(own.paddingBottom) + px(own.borderTopWidth) + px(own.borderBottomWidth)
      sum += height
      tallest = Math.max(tallest, height)
    }
    if (tallest <= 0) return
    const size = sum + (baseRows - 1) * gap + px(box.paddingTop) + px(box.paddingBottom) + px(box.borderTopWidth) + px(box.borderBottomWidth)
    const spare = node.getBoundingClientRect().height - size
    const room = baseRows + Math.max(0, Math.floor((spare + 0.5) / (tallest + gap)))
    setFit((old) => (old && Math.abs(old.size - size) < 0.1 && old.room === room ? old : { size, room }))
  }
  const latest = useRef(measure)
  // 每次画完都重量一遍（行里的内容变了，列表自己的高度不一定变），列表高度变了也重量
  useLayoutEffect(() => {
    latest.current = measure
    measure()
  })
  useLayoutEffect(() => {
    const node = list.current
    if (!node) return
    const observer = new ResizeObserver(() => latest.current())
    observer.observe(node)
    return () => observer.disconnect()
  }, [empty])
  if (empty) return null
  const rows = fit ? Math.min(Math.max(fit.room, baseRows), baseRows + Math.max(0, extra)) : baseRows
  return (
    <div ref={list} className={`${className} fill-rows`} style={fit ? { ...style, flexBasis: fit.size, minHeight: fit.size } : style}>
      {items.slice(0, rows).map(children)}
    </div>
  )
}

export function Empty({ children }: { children: ReactNode }) {
  return <div className="empty">{children}</div>
}
