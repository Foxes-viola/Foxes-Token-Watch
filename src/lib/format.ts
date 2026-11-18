import type { Agent, BaishitongStatus, Effort, NetStatus, Phase, Totals, UnitStyle } from './types'

let unitStyle: UnitStyle = 'kmb'

export function setUnitStyle(style: UnitStyle) {
  unitStyle = style
}

/** 保留三位有效数字：1.67 / 29.2 / 123 */
function threeDigits(value: number): string {
  if (value >= 100) return Math.round(value).toString()
  if (value >= 10) return value.toFixed(1)
  return value.toFixed(2)
}

export interface Compact {
  value: string
  unit: string
}

export function compactParts(n: number, style: UnitStyle = unitStyle): Compact {
  const abs = Math.abs(n)
  if (style === 'wanyi') {
    if (abs >= 1e8) return { value: threeDigits(n / 1e8), unit: '亿' }
    if (abs >= 1e4) return { value: abs >= 1e7 ? Math.round(n / 1e4).toLocaleString('en-US') : threeDigits(n / 1e4), unit: '万' }
    return { value: Math.round(n).toLocaleString('en-US'), unit: '' }
  }
  if (abs >= 1e9) return { value: threeDigits(n / 1e9), unit: 'B' }
  if (abs >= 1e6) return { value: threeDigits(n / 1e6), unit: 'M' }
  if (abs >= 1e4) return { value: threeDigits(n / 1e3), unit: 'K' }
  return { value: Math.round(n).toLocaleString('en-US'), unit: '' }
}

/** 29.2M / 2915 万 */
export function fmtTokens(n: number, style: UnitStyle = unitStyle): string {
  const { value, unit } = compactParts(n, style)
  return unit ? (style === 'wanyi' ? `${value} ${unit}` : `${value}${unit}`) : value
}

/** 1,672,878,520 */
export function fmtFull(n: number): string {
  return Math.round(n).toLocaleString('en-US')
}

export function fmtPercent(ratio: number, digits = 1): string {
  if (!Number.isFinite(ratio)) return '—'
  return `${(ratio * 100).toFixed(digits)}%`
}

/** 缓存命中率：命中 / 全部输入侧 token */
export function hitRate(totals: Totals): number {
  const prompt = totals.input + totals.cacheRead + totals.cacheWrite
  return prompt > 0 ? totals.cacheRead / prompt : 0
}

/** 相对上一时段的变化，没有可比基数时返回 null */
export function delta(current: number, previous: number | undefined | null): number | null {
  if (previous === undefined || previous === null || previous <= 0) return null
  return (current - previous) / previous
}

export function fmtDelta(ratio: number): string {
  const percent = Math.abs(ratio) * 100
  const text = percent >= 1000 ? `${(percent / 100).toFixed(1)} 倍` : `${percent >= 100 ? Math.round(percent) : percent.toFixed(1)}%`
  return `${ratio >= 0 ? '↑' : '↓'} ${text}`
}

/** 0:47 / 12:05 / 1:02:33 */
export function fmtClock(ms: number): string {
  const total = Math.max(0, Math.floor(ms / 1000))
  const hours = Math.floor(total / 3600)
  const minutes = Math.floor((total % 3600) / 60)
  const seconds = total % 60
  const pad = (value: number) => value.toString().padStart(2, '0')
  return hours > 0 ? `${hours}:${pad(minutes)}:${pad(seconds)}` : `${minutes}:${pad(seconds)}`
}

/** 45 秒 / 2 分 13 秒 / 1 小时 5 分 */
export function fmtDuration(ms: number): string {
  const total = Math.max(0, Math.round(ms / 1000))
  if (total < 60) return `${total} 秒`
  const minutes = Math.floor(total / 60)
  if (minutes < 60) return total % 60 ? `${minutes} 分 ${total % 60} 秒` : `${minutes} 分`
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return minutes % 60 ? `${hours} 小时 ${minutes % 60} 分` : `${hours} 小时`
  const days = Math.floor(hours / 24)
  return hours % 24 ? `${days} 天 ${hours % 24} 小时` : `${days} 天`
}

/** 850 ms / 12.3 秒 / 2 分 5 秒 */
export function fmtMs(ms: number): string {
  if (ms <= 0) return '—'
  if (ms < 1000) return `${Math.round(ms)} ms`
  if (ms < 60_000) return `${(ms / 1000).toFixed(ms < 10_000 ? 2 : 1)} 秒`
  return fmtDuration(ms)
}

const pad2 = (value: number) => value.toString().padStart(2, '0')

export function fmtTime(ts: number, withSeconds = false): string {
  const date = new Date(ts)
  const base = `${pad2(date.getHours())}:${pad2(date.getMinutes())}`
  return withSeconds ? `${base}:${pad2(date.getSeconds())}` : base
}

export function fmtDate(ts: number): string {
  const date = new Date(ts)
  return `${date.getMonth() + 1}月${date.getDate()}日`
}

export function fmtDateTime(ts: number): string {
  const date = new Date(ts)
  return `${date.getFullYear()}-${pad2(date.getMonth() + 1)}-${pad2(date.getDate())} ${fmtTime(ts)}`
}

export function fmtAgo(ts: number, now: number): string {
  if (ts <= 0) return '—'
  const diff = now - ts
  if (diff < 60_000) return '刚刚'
  if (diff < 3_600_000) return `${Math.floor(diff / 60_000)} 分钟前`
  if (diff < 86_400_000) return `${Math.floor(diff / 3_600_000)} 小时前`
  if (diff < 7 * 86_400_000) return `${Math.floor(diff / 86_400_000)} 天前`
  return fmtDate(ts)
}

export function fmtBytes(bytes: number): string {
  if (bytes >= 1 << 30) return `${(bytes / (1 << 30)).toFixed(2)} GB`
  if (bytes >= 1 << 20) return `${(bytes / (1 << 20)).toFixed(1)} MB`
  if (bytes >= 1 << 10) return `${(bytes / (1 << 10)).toFixed(0)} KB`
  return `${bytes} B`
}

export const AGENT_META: Record<Agent, { label: string; short: string; color: string }> = {
  claude: { label: 'Claude Code', short: 'Claude', color: 'var(--agent-claude)' },
  codex: { label: 'Codex', short: 'Codex', color: 'var(--agent-codex)' },
  deepseek: { label: 'Deepseek Harness', short: 'Deepseek', color: 'var(--agent-deepseek)' },
  kiro: { label: 'Kiro', short: 'Kiro', color: '#ae8bf5' },
  cursor: { label: 'Cursor', short: 'Cursor', color: '#7f9ab8' },
  webmcp: { label: '网页 MCP', short: '网页 MCP', color: '#2eb6a5' },
  antigravity: { label: '反重力 Antigravity', short: '反重力', color: 'var(--agent-antigravity)' },
}

export const PHASE_META: Record<Phase, { label: string; tone: string }> = {
  idle: { label: '空闲', tone: 'idle' },
  requesting: { label: '等待响应', tone: 'request' },
  thinking: { label: '思考中', tone: 'think' },
  writing: { label: '写入中', tone: 'write' },
  generating: { label: '生成中', tone: 'write' },
  tool: { label: '执行工具', tone: 'tool' },
  retrying: { label: '重试中', tone: 'retry' },
  waiting: { label: '等待确认', tone: 'wait' },
}

/** 思考强度的名称和颜色，由低到高从冷色到暖色。带 fast 的各档用同一色系里更深的颜色；Max Fast 单独用青色，和 Max 的紫色一眼分得开 */
export const EFFORT_META: Record<Effort, { label: string; color: string }> = {
  unknown: { label: '未标注', color: 'var(--text-3)' },
  off: { label: '关闭', color: '#7d8aa5' },
  minimal: { label: '极低', color: '#5aa9ff' },
  low: { label: '低', color: '#3ddc97' },
  medium: { label: '中', color: '#ffd166' },
  high: { label: '高', color: '#ff8a3d' },
  xhigh: { label: '超高', color: '#ff5d6c' },
  max: { label: 'Max', color: '#d16bff' },
  'minimal-fast': { label: '极低 Fast', color: '#3f7fd1' },
  'low-fast': { label: '低 Fast', color: '#1fa36f' },
  'medium-fast': { label: '中 Fast', color: '#c79a2b' },
  'high-fast': { label: '高 Fast', color: '#c7622a' },
  'xhigh-fast': { label: '超高 Fast', color: '#c93f52' },
  'max-fast': { label: 'Max Fast', color: '#35d0e0' },
}

/** 从高到低排列，带 fast 的排在同一档前面，未标注放最后 */
export const EFFORT_ORDER: Effort[] = ['max-fast', 'max', 'xhigh-fast', 'xhigh', 'high-fast', 'high', 'medium-fast', 'medium', 'low-fast', 'low', 'minimal-fast', 'minimal', 'off', 'unknown']

export const NET_META: Record<NetStatus, { label: string; tone: string; hint: string }> = {
  idle: { label: '暂无请求', tone: 'idle', hint: '最近 15 分钟没有请求，无从判断' },
  ok: { label: '通畅', tone: 'ok', hint: '最近的请求都正常返回' },
  slow: { label: '响应慢', tone: 'slow', hint: '首字延迟明显偏高' },
  unstable: { label: '不稳定', tone: 'unstable', hint: '最近有重试或报错，但还能连上' },
  down: { label: '连接失败', tone: 'down', hint: '连续失败，之后没有成功的响应' },
}

export interface FeedText {
  /** ok：连着；wait：还在连，或者服务端让等一等；bad：读不到 */
  tone: 'ok' | 'wait' | 'bad'
  label: string
  detail: string
}

/** 百事通对话读得怎么样：一个短标签和一句说明（读不到时写明卡在哪一步） */
export function baishitongText(status: BaishitongStatus, now: number): FeedText {
  const wait = status.retryAt - now
  const retry = wait > 0 ? `，约 ${wait >= 90_000 ? `${Math.round(wait / 60_000)} 分钟` : `${Math.max(1, Math.round(wait / 1000))} 秒`}后再试` : ''
  switch (status.link) {
    case 'online': {
      const parts = [`${status.sessions} 个对话`, `${status.busy} 个正在运行`]
      if (status.pending > 0) parts.push(`还有 ${status.pending} 个对话的过程记录没读完`)
      if (status.okAt > 0) parts.push(`上次读取：${fmtAgo(status.okAt, now)}`)
      return { tone: 'ok', label: '已连接', detail: parts.join(' · ') }
    }
    case 'unknown':
      return { tone: 'wait', label: '正在连接', detail: '正在向百事通插件要对话列表，一般几秒就好' }
    case 'offline':
      return { tone: 'bad', label: '没连上', detail: `本机没找到百事通插件开的端口（127.0.0.1:9791）。Cursor 开着、百事通插件在运行时才读得到${retry}` }
    case 'foreign':
      return { tone: 'bad', label: '端口被占用', detail: `127.0.0.1:9791 上是别的程序，不是百事通插件开的端口${retry}` }
    case 'denied':
      return { tone: 'bad', label: '登录没通过', detail: `没能登录进百事通的对话页，多半是百事通插件自己还没登录，或者短时间内自动登录的次数用完了${retry}` }
    case 'busy':
      return { tone: 'wait', label: '服务端繁忙', detail: `百事通的服务端正忙，让等一等${retry}` }
    case 'failing':
      return { tone: 'bad', label: '读取出错', detail: `百事通服务端的回应读不出来${retry}` }
  }
}

/** 对话在列表里显示的名字：标题优先，其次项目名 */
export function sessionName(session: { title: string; project: string; id: string }): string {
  return session.title || session.project || `对话 ${session.id.slice(-6)}`
}
