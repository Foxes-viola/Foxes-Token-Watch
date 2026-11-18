import { useLayoutEffect, useRef, useState } from 'react'
import type { CSSProperties, ReactNode } from 'react'
import { fmtDate, fmtFull, fmtTokens } from '../lib/format'
import type { DayCell } from '../lib/types'

export interface Segment {
  key: string
  label: string
  color: string
  value: number
}

/** 横向的构成条：各段宽度按数值占比 */
export function CompositionBar({ parts, height = 12 }: { parts: Segment[]; height?: number }) {
  return (
    <div className="compbar" style={{ height }}>
      {parts.map((part) => (part.value > 0 ? <i key={part.key} style={{ flexGrow: part.value, background: part.color }} title={`${part.label} ${fmtFull(part.value)}`} /> : null))}
    </div>
  )
}

export function Ring({ value, size = 84, stroke = 8, color, children }: { value: number; size?: number; stroke?: number; color: string; children?: ReactNode }) {
  const radius = (size - stroke) / 2
  const circumference = 2 * Math.PI * radius
  const clamped = Math.max(0, Math.min(1, value))
  const center = size / 2
  return (
    <div className="ring" style={{ width: size, height: size }}>
      <svg width={size} height={size}>
        <circle cx={center} cy={center} r={radius} fill="none" stroke="var(--line-strong)" strokeWidth={stroke} />
        <circle
          cx={center}
          cy={center}
          r={radius}
          fill="none"
          stroke={color}
          strokeWidth={stroke}
          strokeLinecap="round"
          strokeDasharray={`${circumference * clamped} ${circumference}`}
          transform={`rotate(-90 ${center} ${center})`}
        />
      </svg>
      <div className="ring-label">{children}</div>
    </div>
  )
}

export interface BarColumn {
  t: number
  /** 横轴上的短标签 */
  label: string
  /** 悬停提示的标题 */
  title: string
  segments: Segment[]
  note?: string
}

function niceMax(value: number): number {
  if (value <= 0) return 1
  const exponent = Math.pow(10, Math.floor(Math.log10(value)))
  const fraction = value / exponent
  const nice = fraction <= 1 ? 1 : fraction <= 2 ? 2 : fraction <= 2.5 ? 2.5 : fraction <= 5 ? 5 : 10
  return nice * exponent
}

/** 堆叠柱状图：每根柱子自下而上按 segments 的顺序堆叠。`fill` 时占满父容器（纵向 flex）剩下的高度，`height` 作为下限 */
export function StackedBars({ columns, height = 220, fill = false }: { columns: BarColumn[]; height?: number; fill?: boolean }) {
  const [hover, setHover] = useState<number | null>(null)
  const sums = columns.map((column) => column.segments.reduce((sum, segment) => sum + segment.value, 0))
  const max = niceMax(Math.max(...sums, 0))
  const ticks = [1, 0.75, 0.5, 0.25]
  const labelEvery = Math.max(1, Math.ceil(columns.length / 9))
  const active = hover !== null && hover < columns.length ? columns[hover] : null
  const activeSum = hover !== null && hover < columns.length ? sums[hover] : 0
  const tipSide = hover !== null && hover / Math.max(1, columns.length) > 0.6 ? 'left' : 'right'

  return (
    <div className={fill ? 'trend fill' : 'trend'} style={fill ? { minHeight: height } : { height }}>
      <div className="trend-y">
        {ticks.map((tick) => (
          <span key={tick} style={{ bottom: `${tick * 100}%` }} className="num">
            {fmtTokens(max * tick)}
          </span>
        ))}
      </div>
      <div className="trend-main">
        <div className="trend-plot" onMouseLeave={() => setHover(null)}>
          {ticks.map((tick) => (
            <i key={tick} className="trend-grid" style={{ bottom: `${tick * 100}%` }} />
          ))}
          <div className="trend-bars">
            {columns.map((column, index) => (
              <div key={column.t} className={index === hover ? 'trend-col hover' : 'trend-col'} onMouseEnter={() => setHover(index)}>
                <div className="trend-bar" style={{ height: `${(sums[index] / max) * 100}%`, minHeight: sums[index] > 0 ? 2 : 0 }}>
                  {column.segments.map((segment) => (segment.value > 0 ? <i key={segment.key} style={{ flexGrow: segment.value, background: segment.color }} /> : null))}
                </div>
              </div>
            ))}
          </div>
          {active ? (
            <div className={`trend-tip ${tipSide}`} style={tipSide === 'right' ? { left: `${(((hover ?? 0) + 1) / columns.length) * 100}%` } : { right: `${(1 - (hover ?? 0) / columns.length) * 100}%` }}>
              <div className="trend-tip-title">{active.title}</div>
              {active.segments.map((segment) => (
                <div key={segment.key} className="trend-tip-row">
                  <i style={{ background: segment.color }} />
                  <span>{segment.label}</span>
                  <b className="num">{fmtTokens(segment.value)}</b>
                </div>
              ))}
              <div className="trend-tip-row total">
                <span>合计</span>
                <b className="num">{fmtTokens(activeSum)}</b>
              </div>
              {active.note ? <div className="trend-tip-note">{active.note}</div> : null}
            </div>
          ) : null}
        </div>
        <div className="trend-x">
          {columns.map((column, index) => (
            <span key={column.t}>{index % labelEvery === 0 ? column.label : ''}</span>
          ))}
        </div>
      </div>
    </div>
  )
}

export interface LineSeries {
  key: string
  label: string
  color: string
  values: number[]
  /** 悬停提示里数值的写法，默认按 token 数缩写 */
  format?: (value: number) => string
  /** 单位不是 token（比如请求数）：不参与共用刻度，始终按自己的峰值缩放 */
  ownScale?: boolean
}

/** 平滑但不越过数据点的曲线（单调三次插值）：两点之间不会画出比两端更高或更低的值 */
function smoothPath(points: [number, number][]): string {
  const count = points.length
  if (count === 0) return ''
  if (count < 3) return points.map(([x, y], index) => `${index ? 'L' : 'M'}${x.toFixed(2)} ${y.toFixed(2)}`).join(' ')
  const slopes: number[] = []
  for (let index = 0; index < count - 1; index += 1) {
    slopes.push((points[index + 1][1] - points[index][1]) / (points[index + 1][0] - points[index][0]))
  }
  const tangents = [slopes[0]]
  for (let index = 1; index < count - 1; index += 1) {
    const before = slopes[index - 1]
    const after = slopes[index]
    tangents.push(before * after <= 0 ? 0 : (2 * before * after) / (before + after))
  }
  tangents.push(slopes[count - 2])
  let path = `M${points[0][0].toFixed(2)} ${points[0][1].toFixed(2)}`
  for (let index = 0; index < count - 1; index += 1) {
    const [x0, y0] = points[index]
    const [x1, y1] = points[index + 1]
    const third = (x1 - x0) / 3
    path += ` C${(x0 + third).toFixed(2)} ${(y0 + tangents[index] * third).toFixed(2)} ${(x1 - third).toFixed(2)} ${(y1 - tangents[index + 1] * third).toFixed(2)} ${x1.toFixed(2)} ${y1.toFixed(2)}`
  }
  return path
}

const LINE_WIDTH = 1000

/**
 * 多条实线共用一个横轴。默认每条线按自己的峰值缩放，量级差很多的几项也都看得清走势；
 * `shared` 时改用同一个纵轴，方便比较大小。
 */
export function MultiLine({
  series,
  times,
  height = 220,
  shared = false,
  formatTime,
  formatTitle = formatTime,
  empty = '这段时间没有用量',
}: {
  series: LineSeries[]
  /** 每个点的时刻，和各条线的 values 一一对应 */
  times: number[]
  height?: number
  shared?: boolean
  /** 横轴上时刻的写法 */
  formatTime: (t: number) => string
  /** 悬停提示的标题，默认和横轴一样 */
  formatTitle?: (t: number) => string
  empty?: string
}) {
  const [hover, setHover] = useState<number | null>(null)
  const count = times.length
  const peaks = series.map((line) => Math.max(0, ...line.values))
  const sharedMax = niceMax(Math.max(0, ...series.map((line, index) => (line.ownScale ? 0 : peaks[index]))))
  const scales = series.map((line, index) => (shared && !line.ownScale ? sharedMax : peaks[index]))
  const ticks = [1, 0.75, 0.5, 0.25]
  const at = (index: number) => (count > 1 ? index / (count - 1) : 0.5)
  const level = (line: number, index: number) => (scales[line] > 0 ? Math.min(1, (series[line].values[index] ?? 0) / scales[line]) : 0)
  const active = hover !== null && hover < count ? hover : null
  const tipSide = active !== null && at(active) > 0.6 ? 'left' : 'right'
  const slots = Math.min(7, count)
  const labels = slots > 1 ? Array.from({ length: slots }, (_, slot) => Math.round((slot / (slots - 1)) * (count - 1))) : count === 1 ? [0] : []
  // 只有一个点（比如今天刚过零点、按小时只有一格）时画成一条横线，不然图上什么都没有
  const points = (line: number): [number, number][] =>
    count > 1
      ? series[line].values.map((_, index) => [at(index) * LINE_WIDTH, 100 - level(line, index) * 100])
      : [
          [0, 100 - level(line, 0) * 100],
          [LINE_WIDTH, 100 - level(line, 0) * 100],
        ]

  return (
    <div className="trend lines" style={{ height }}>
      <div className="trend-y">
        {ticks.map((tick) => (
          <span key={tick} style={{ bottom: `${tick * 100}%` }} className="num">
            {shared ? fmtTokens(sharedMax * tick) : tick === 1 ? '峰值' : `${tick * 100}%`}
          </span>
        ))}
      </div>
      <div className="trend-main">
        <div
          className="trend-plot"
          onMouseMove={(event) => {
            const box = event.currentTarget.getBoundingClientRect()
            if (count > 0 && box.width > 0) setHover(Math.max(0, Math.min(count - 1, Math.round(((event.clientX - box.left) / box.width) * (count - 1)))))
          }}
          onMouseLeave={() => setHover(null)}
        >
          {ticks.map((tick) => (
            <i key={tick} className="trend-grid" style={{ bottom: `${tick * 100}%` }} />
          ))}
          <svg className="lines-svg" viewBox={`0 0 ${LINE_WIDTH} 100`} preserveAspectRatio="none">
            {count > 0
              ? series.map((line, lineIndex) => (
                  <path
                    key={line.key}
                    d={smoothPath(points(lineIndex))}
                    fill="none"
                    stroke={line.color}
                    strokeWidth="1.8"
                    strokeLinejoin="round"
                    strokeLinecap="round"
                    vectorEffect="non-scaling-stroke"
                  />
                ))
              : null}
          </svg>
          {peaks.every((peak) => peak <= 0) ? <div className="lines-empty muted">{empty}</div> : null}
          {active !== null ? (
            <>
              <i className="lines-guide" style={{ left: `${at(active) * 100}%` }} />
              {series.map((line, lineIndex) => (
                <i key={line.key} className="lines-dot" style={{ left: `${at(active) * 100}%`, bottom: `${level(lineIndex, active) * 100}%`, background: line.color }} />
              ))}
              <div className={`trend-tip ${tipSide}`} style={tipSide === 'right' ? { left: `${at(active) * 100}%` } : { right: `${(1 - at(active)) * 100}%` }}>
                <div className="trend-tip-title">{formatTitle(times[active])}</div>
                {series.map((line) => (
                  <div key={line.key} className="trend-tip-row">
                    <i style={{ background: line.color }} />
                    <span>{line.label}</span>
                    <b className="num">{(line.format ?? fmtTokens)(line.values[active] ?? 0)}</b>
                  </div>
                ))}
              </div>
            </>
          ) : null}
        </div>
        <div className="trend-x spread">
          {labels.map((index, slot) => (
            <span key={index} className={labels.length === 1 ? undefined : slot === 0 ? 'first' : slot === labels.length - 1 ? 'last' : undefined} style={{ left: `${at(index) * 100}%` }}>
              {formatTime(times[index])}
            </span>
          ))}
        </div>
      </div>
    </div>
  )
}

export interface Band {
  key: string
  label: string
  color: string
  /** 填充比例 0~1 */
  value: number
}

/** 运动手环那样的同心圆环：由外到内一环一个指标，圆头，底轨是同色的淡色 */
export function BandRings({ bands, size = 148, stroke = 11, gap = 4, children }: { bands: Band[]; size?: number; stroke?: number; gap?: number; children?: ReactNode }) {
  const center = size / 2
  return (
    <div className="ring bands" style={{ width: size, height: size }}>
      <svg width={size} height={size}>
        {bands.map((band, index) => {
          const radius = (size - stroke) / 2 - index * (stroke + gap)
          const circumference = 2 * Math.PI * radius
          const value = Math.max(0, Math.min(1, band.value))
          return (
            <g key={band.key}>
              <circle cx={center} cy={center} r={radius} fill="none" stroke={band.color} strokeOpacity="0.16" strokeWidth={stroke} />
              <circle
                className="band-arc"
                cx={center}
                cy={center}
                r={radius}
                fill="none"
                stroke={band.color}
                strokeWidth={stroke}
                strokeLinecap="round"
                strokeDasharray={`${circumference * value} ${circumference}`}
                strokeOpacity={value > 0 ? 1 : 0}
                transform={`rotate(-90 ${center} ${center})`}
              />
            </g>
          )
        })}
      </svg>
      <div className="ring-label">{children}</div>
    </div>
  )
}

/** 热力图一格连间距的边长：卡片窄时是 15，卡片宽了就跟着放大、把宽度占满，最大到 28 */
const HEAT_PITCH_MIN = 15
const HEAT_PITCH_MAX = 28

/** 最近 26 周每天的用量，每列一周（周一在上） */
export function Heatmap({ cells }: { cells: DayCell[] }) {
  const wrap = useRef<HTMLDivElement>(null)
  const [room, setRoom] = useState(0)
  useLayoutEffect(() => {
    const node = wrap.current
    if (!node) return
    // 只量宽度：宽度由卡片决定，不随格子大小变，所以不会来回抖
    const measure = () => setRoom(node.clientWidth)
    measure()
    const observer = new ResizeObserver(measure)
    observer.observe(node)
    return () => observer.disconnect()
  }, [])

  const max = Math.max(...cells.map((cell) => cell.total), 1)
  const offset = cells.length > 0 ? (new Date(cells[0].t).getDay() + 6) % 7 : 0
  const months: { column: number; label: string }[] = []
  cells.forEach((cell, index) => {
    const date = new Date(cell.t)
    if (date.getDate() === 1) months.push({ column: Math.floor((index + offset) / 7), label: `${date.getMonth() + 1}月` })
  })
  const columns = Math.max(1, Math.ceil((cells.length + offset) / 7))
  const pitch = Math.max(HEAT_PITCH_MIN, Math.min(HEAT_PITCH_MAX, Math.floor(room / columns)))
  const gap = pitch >= 22 ? 4 : 3
  return (
    <div className="heatmap-wrap" ref={wrap}>
      {cells.length > 0 ? (
        <div className="heatmap" style={{ width: columns * pitch - gap, '--heat-cell': `${pitch - gap}px`, '--heat-gap': `${gap}px` } as CSSProperties}>
          <div className="heatmap-months">
            {months.map((month) => (
              <span key={month.column} style={{ left: month.column * pitch }}>
                {month.label}
              </span>
            ))}
          </div>
          <div className="heatmap-grid">
            {Array.from({ length: offset }, (_, index) => (
              <i key={`pad-${index}`} className="heat-pad" />
            ))}
            {cells.map((cell) => {
              const level = cell.total <= 0 ? 0 : 1 + Math.min(3, Math.floor(Math.sqrt(cell.total / max) * 4))
              return <i key={cell.t} className={`heat-cell l${level}`} title={`${fmtDate(cell.t)} · ${fmtTokens(cell.total)} · ${cell.requests} 次请求`} />
            })}
          </div>
        </div>
      ) : null}
    </div>
  )
}
