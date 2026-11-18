import { StrictMode, useState } from 'react'
import { createRoot } from 'react-dom/client'
import './styles/base.css'
import './styles/app.css'
import { AppIcon, Icon } from './components/ui'
import type { IconName } from './components/ui'
import { useSettings, useSnapshot, windowAction } from './lib/api'
import { NET_META } from './lib/format'
import { Overview } from './pages/Overview'
import { Sessions } from './pages/Sessions'
import { SettingsPage } from './pages/Settings'

type PageId = 'overview' | 'sessions' | 'settings'

const NAV: { id: PageId; label: string; icon: IconName }[] = [
  { id: 'overview', label: '总览', icon: 'overview' },
  { id: 'sessions', label: '对话', icon: 'sessions' },
  { id: 'settings', label: '设置', icon: 'settings' },
]

function App() {
  const snapshot = useSnapshot()
  useSettings()
  const [page, setPage] = useState<PageId>('overview')
  const running = snapshot?.live.running ?? 0
  const net = snapshot?.live.net.status ?? 'idle'

  return (
    <div className="app">
      <header className="titlebar" data-tauri-drag-region>
        <div className="brand">
          <AppIcon size={20} />
          <span>Foxes Token Watch</span>
        </div>
        <div className="titlebar-drag" data-tauri-drag-region />
        {snapshot ? (
          <div className="titlebar-live">
            <span className={running > 0 ? 'dot pulse tone-ok' : 'dot tone-idle'} />
            <span>
              <b className="num">{running}</b> 个对话运行中
            </span>
            <span className={`dot tone-${NET_META[net].tone}`} />
            <span>网络{NET_META[net].label}</span>
          </div>
        ) : null}
        <button className="win-btn" title="最小化" onClick={() => windowAction('minimize')}>
          <Icon name="minimize" size={16} />
        </button>
        <button className="win-btn" title="最大化 / 还原" onClick={() => windowAction('toggleMaximize')}>
          <Icon name="maximize" size={16} />
        </button>
        <button className="win-btn close" title="收到托盘" onClick={() => windowAction('hide')}>
          <Icon name="close" size={16} />
        </button>
      </header>

      <div className="app-body">
        <nav className="sidebar">
          {NAV.map((item) => (
            <button key={item.id} className={item.id === page ? 'nav-item active' : 'nav-item'} onClick={() => setPage(item.id)}>
              <Icon name={item.icon} size={20} />
              <span>{item.label}</span>
            </button>
          ))}
        </nav>
        <main className="content">
          {snapshot === null ? (
            <div className="loading">
              <AppIcon size={64} />
              <span>正在读取本机的用量记录…</span>
            </div>
          ) : page === 'overview' ? (
            <Overview snapshot={snapshot} />
          ) : page === 'sessions' ? (
            <Sessions snapshot={snapshot} />
          ) : (
            <SettingsPage snapshot={snapshot} />
          )}
        </main>
      </div>
    </div>
  )
}

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <App />
  </StrictMode>,
)
