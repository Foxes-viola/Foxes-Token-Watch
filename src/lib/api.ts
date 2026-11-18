import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { getCurrentWindow } from '@tauri-apps/api/window'
import { useEffect, useState, useSyncExternalStore } from 'react'
import { setUnitStyle } from './format'
import type { AppInfo, SessionDetail, SessionView, Settings, Snapshot } from './types'

/** 在软件窗口里运行（而不是开发时用浏览器打开） */
export const inApp = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window

export const DEFAULT_SETTINGS: Settings = {
  island: true,
  edgeAutoHide: true,
  autostart: false,
  notifyFinished: true,
  units: 'kmb',
  theme: 'dark',
  reduceMotion: false,
}

type Listener = () => void

function createStore<T>(initial: T) {
  let value = initial
  const listeners = new Set<Listener>()
  return {
    get: () => value,
    set(next: T) {
      value = next
      listeners.forEach((listener) => listener())
    },
    subscribe(listener: Listener) {
      listeners.add(listener)
      return () => {
        listeners.delete(listener)
      }
    },
  }
}

const snapshotStore = createStore<Snapshot | null>(null)
const settingsStore = createStore<Settings>(DEFAULT_SETTINGS)

const systemLight = typeof window !== 'undefined' && typeof window.matchMedia === 'function' ? window.matchMedia('(prefers-color-scheme: light)') : null

function applyTheme(theme: Settings['theme']) {
  document.documentElement.dataset.theme = theme === 'system' ? (systemLight?.matches ? 'light' : 'dark') : theme
}

function applySettings(settings: Settings) {
  setUnitStyle(settings.units)
  applyTheme(settings.theme)
  document.documentElement.dataset.motion = settings.reduceMotion ? 'reduced' : 'full'
  settingsStore.set(settings)
}

// 选了“跟随系统”时，系统换了深浅色就跟着换
systemLight?.addEventListener('change', () => applyTheme(settingsStore.get().theme))

function publishSnapshot(snapshot: Snapshot) {
  const current = snapshotStore.get()
  if (!current || snapshot.generatedAt >= current.generatedAt) snapshotStore.set(snapshot)
}

let started = false

function start() {
  if (started) return
  started = true
  if (inApp) {
    // 快照是分窗口发的；不指明窗口的 listen 连发给另一个窗口的也会收到，所以只听发给本窗口的
    void getCurrentWindow().listen<Snapshot>('snapshot', (event) => publishSnapshot(event.payload))
    void listen<Settings>('settings', (event) => applySettings(event.payload))
    void invoke<Snapshot | null>('get_snapshot').then((snapshot) => snapshot && publishSnapshot(snapshot))
    void invoke<Settings>('get_settings').then(applySettings)
  } else {
    void import('./mock').then((mock) => mock.startMock(publishSnapshot))
    applySettings(DEFAULT_SETTINGS)
  }
}

function subscribeSnapshot(listener: Listener) {
  start()
  return snapshotStore.subscribe(listener)
}

function subscribeSettings(listener: Listener) {
  start()
  return settingsStore.subscribe(listener)
}

/** 最新的统计快照；第一次扫描完成前为 null */
export function useSnapshot(): Snapshot | null {
  return useSyncExternalStore(subscribeSnapshot, snapshotStore.get)
}

export function useSettings(): Settings {
  return useSyncExternalStore(subscribeSettings, settingsStore.get)
}

export async function updateSettings(patch: Partial<Settings>): Promise<void> {
  const next = { ...settingsStore.get(), ...patch }
  applySettings(next)
  if (inApp) applySettings(await invoke<Settings>('set_settings', { patch }))
}

/**
 * 每隔一段时间做一次 run，但只在页面看得见的时候做：窗口藏在托盘里时不白忙，重新露面时立刻补一次。
 * 返回停止用的函数。
 */
export function everyWhileVisible(run: () => void, intervalMs: number): () => void {
  const tick = () => {
    if (!document.hidden) run()
  }
  const timer = window.setInterval(tick, intervalMs)
  document.addEventListener('visibilitychange', tick)
  return () => {
    window.clearInterval(timer)
    document.removeEventListener('visibilitychange', tick)
  }
}

/** 每隔一段时间重新渲染一次，给计时器用 */
export function useNow(intervalMs = 1000): number {
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => everyWhileVisible(() => setNow(Date.now()), intervalMs), [intervalMs])
  return now
}

export async function fetchSessions(): Promise<SessionView[]> {
  if (inApp) return invoke<SessionView[]>('get_sessions')
  const mock = await import('./mock')
  return mock.mockSessions()
}

/** 一段对话的逐次请求和工具调用；找不到这段对话（或者一时取不到）时为 null */
export async function fetchSessionDetail(key: string): Promise<SessionDetail | null> {
  if (!inApp) {
    const mock = await import('./mock')
    return mock.mockSessionDetail(key)
  }
  return invoke<SessionDetail | null>('get_session_detail', { key }).catch(() => null)
}

/**
 * 记在本机的界面选项（筛选、排序、时段这些）：下次打开还是上次的样子。只存界面选项，不存任何统计数据。
 * allowed 用来挡住读回来的不合适的值（比如旧版本留下的）。
 */
export function useStored<T>(key: string, initial: T, allowed?: (value: unknown) => boolean): [T, (next: T) => void] {
  const name = `foxes.${key}`
  const [value, setValue] = useState<T>(() => {
    try {
      const raw = window.localStorage.getItem(name)
      if (raw !== null) {
        const parsed: unknown = JSON.parse(raw)
        if (allowed ? allowed(parsed) : typeof parsed === typeof initial) return parsed as T
      }
    } catch {
      // 读不出来就用默认值
    }
    return initial
  })
  const store = (next: T) => {
    setValue(next)
    try {
      window.localStorage.setItem(name, JSON.stringify(next))
    } catch {
      // 存不进去不影响使用
    }
  }
  return [value, store]
}

export async function fetchAppInfo(): Promise<AppInfo> {
  if (inApp) return invoke<AppInfo>('get_app_info')
  const mock = await import('./mock')
  return mock.mockAppInfo()
}

/** 把一段文字放进剪贴板，返回放没放成。只在用户点了按钮时调用。 */
export async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text)
    return true
  } catch {
    // 窗口没拿到剪贴板权限时走老办法：选中一个看不见的文本框再复制
    const box = document.createElement('textarea')
    box.value = text
    box.setAttribute('readonly', '')
    box.style.position = 'fixed'
    box.style.opacity = '0'
    document.body.appendChild(box)
    box.select()
    let done = false
    try {
      done = document.execCommand('copy')
    } catch {
      done = false
    }
    box.remove()
    return done
  }
}

export type WindowAction = 'minimize' | 'toggleMaximize' | 'hide'

export function windowAction(action: WindowAction) {
  if (!inApp) return
  const current = getCurrentWindow()
  if (action === 'minimize') void current.minimize()
  else if (action === 'toggleMaximize') void current.toggleMaximize()
  else void current.hide()
}

export function startDragging() {
  if (inApp) void invoke('drag_island')
}

export type DockSide = 'left' | 'right' | null
const dockStore = createStore<DockSide>(null)
let dockStarted = false

function subscribeDock(listener: Listener) {
  if (!dockStarted && inApp) {
    dockStarted = true
    let received = false
    void listen<DockSide>('island-dock', (event) => {
      received = true
      dockStore.set(event.payload)
    }).then(async () => {
      const initial = await invoke<DockSide>('get_island_dock')
      if (!received) dockStore.set(initial)
    })
  }
  return dockStore.subscribe(listener)
}

export function useIslandDock() {
  return useSyncExternalStore(subscribeDock, dockStore.get)
}

export function setIslandDock(side: DockSide) {
  if (inApp) void invoke('set_island_dock', { side })
  else dockStore.set(side)
}

/** 打开（或唤起）主面板 */
export function openPanel() {
  if (inApp) void invoke('show_panel')
}

/**
 * 设置灵动岛窗口里“看得见、点得到”的范围（逻辑像素，贴着窗口顶边、左右居中）。
 * 范围之外的部分既不显示也不拦鼠标，点击会直接落到后面的窗口上。
 */
export function islandRegion(width: number, height: number, expanded: boolean): Promise<void> {
  return inApp ? invoke<void>('island_region', { width, height, expanded }).catch(() => undefined) : Promise.resolve()
}

/** 后台确认鼠标已经离开灵动岛时回调；返回取消监听的函数 */
export function onIslandLeave(handler: () => void): () => void {
  if (!inApp) return () => undefined
  const pending = listen('island-leave', handler)
  return () => void pending.then((unlisten) => unlisten())
}

export function quitApp() {
  if (inApp) void invoke('quit_app')
}
