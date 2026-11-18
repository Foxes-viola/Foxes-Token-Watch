import { Fragment, useEffect, useState } from 'react'
import type { ReactNode } from 'react'
import { AgentBadge, AppIcon, Card, Segmented, Switch } from '../components/ui'
import { copyText, everyWhileVisible, fetchAppInfo, quitApp, setIslandDock, useIslandDock, updateSettings, useSettings } from '../lib/api'
import { AGENT_META, baishitongText, fmtAgo, fmtBytes, fmtDateTime, fmtFull, fmtMs } from '../lib/format'
import { AGENTS } from '../lib/types'
import type { AppInfo, Snapshot } from '../lib/types'

/** 给排查问题用的一段文字：只有版本、状态和数量，不带对话标题、内容和文件路径 */
function diagnostics(snapshot: Snapshot, info: AppInfo | null): string {
  const now = snapshot.generatedAt
  const { scan, plugin, live } = snapshot
  // 报错里可能带着用户目录
  const tidy = (text: string) => text.replace(/[A-Za-z]:\\Users\\[^\\/\s]+/g, '~').replace(/\/(home|Users)\/[^/\s]+/g, '~')
  const feedText = plugin.baishitong ? baishitongText(plugin.baishitong, now) : null
  const lines = [
    `Foxes Token Watch ${info ? `v${info.version}` : ''} 诊断信息`,
    `时间：${fmtDateTime(now)}`,
    '',
    '数据来源：',
    ...AGENTS.map((agent) => {
      const source = live.agents.find((item) => item.agent === agent)
      const parts = [source?.detected ? '已检测到' : '未检测到']
      if (source?.processAlive) parts.push('进程运行中')
      parts.push(`${source?.files ?? 0} 个记录文件`)
      if (source && source.running > 0) parts.push(`${source.running} 个对话在运行`)
      if (source?.readError) parts.push(`读取失败：${tidy(source.readError)}`)
      return `  ${AGENT_META[agent].label}：${parts.join(' · ')}`
    }),
    `  百事通对话：${feedText ? `${feedText.label} · ${feedText.detail}` : '没有去读'}`,
    '',
    `插件（网关、百事通）对话：${fmtFull(plugin.sessions)} 段，最近活动：${plugin.lastActive > 0 ? fmtAgo(plugin.lastActive, now) : '无'}`,
    `正在运行：${live.running} 个对话，${live.runningSubAgents} 个子代理`,
    `扫描：${fmtFull(scan.files)} 个记录文件 · ${fmtBytes(scan.bytes)} · ${fmtFull(scan.events)} 条请求记录 · 首次扫描 ${fmtMs(scan.firstScanMs)} · 每秒刷新 ${fmtMs(scan.scanMs)} · 汇总 ${fmtMs(scan.analysisMs ?? 0)} · 读取失败 ${scan.failing} · 统计中断 ${scan.monitorRestarts ?? 0} 次${scan.monitorError ? `（${tidy(scan.monitorError)}）` : ''}`,
    `用量历史：这台电脑 ${fmtFull(scan.history.sessions)} 段会话 · 另有 ${scan.history.devices} 台电脑的记录（${fmtFull(scan.history.elsewhere)} 段不在本机）· 程序旁边${scan.history.carried ? '有' : '没有'}随身的一份${scan.historyError ? ` · 读写失败：${tidy(scan.historyError)}` : ''}`,
  ]
  return lines.join('\r\n')
}

function Row({ title, hint, children }: { title: string; hint?: string; children: ReactNode }) {
  return (
    <div className="set-row">
      <div className="set-text">
        <div className="set-title">{title}</div>
        {hint ? <div className="set-hint muted">{hint}</div> : null}
      </div>
      <div className="set-control">{children}</div>
    </div>
  )
}

export function SettingsPage({ snapshot }: { snapshot: Snapshot }) {
  const settings = useSettings()
  const dock = useIslandDock()
  const [info, setInfo] = useState<AppInfo | null>(null)
  const [copied, setCopied] = useState<'done' | 'failed' | null>(null)

  useEffect(() => {
    const load = () => void fetchAppInfo().then(setInfo)
    load()
    // 运行期间新装的 agent 会被自动发现，这里跟着刷新数据目录
    return everyWhileVisible(load, 5000)
  }, [])

  const scan = snapshot.scan
  const history = scan.history
  const feed = snapshot.plugin.baishitong
  const feedText = feed ? baishitongText(feed, snapshot.generatedAt) : null

  const copyDiagnostics = () => {
    void copyText(diagnostics(snapshot, info)).then((done) => {
      setCopied(done ? 'done' : 'failed')
      window.setTimeout(() => setCopied(null), 2500)
    })
  }

  return (
    <div className="page">
      <header className="page-head">
        <div>
          <h1>设置</h1>
          <p className="muted">改动立即生效，自动保存</p>
        </div>
      </header>

      <div className="grid">
        {/* 左右两列底边对齐：矮的那列把多出来的高度匀给各行设置，不在卡片底下留一块空白 */}
        <div className="col flush span-6">
          <Card title="灵动岛与后台" rows={6}>
            <Row title="显示灵动岛" hint="悬停查看用量，点击打开面板。拖到屏幕左右边缘松手即可停靠，拖回屏幕中间可自由放置。">
              <Switch on={settings.island} onChange={(island) => void updateSettings({ island })} />
            </Row>
            <Row title="灵动岛位置">
              <div className="card-tools">
                <button className="btn" onClick={() => setIslandDock(null)}>回到顶部</button>
                <button className={dock === 'left' ? 'toggle on' : 'toggle'} onClick={() => setIslandDock('left')}>停靠左侧</button>
                <button className={dock === 'right' ? 'toggle on' : 'toggle'} onClick={() => setIslandDock('right')}>停靠右侧</button>
              </div>
            </Row>
            <Row title="贴边自动收起" hint="停靠时只露出一小截把手，鼠标移上去展开。关闭后保留用量胶囊。">
              <Switch on={settings.edgeAutoHide} onChange={(edgeAutoHide) => void updateSettings({ edgeAutoHide })} />
            </Row>
            <Row title="对话结束时提示" hint="某个对话跑完一轮时，灵动岛的头像会跳一下并亮起小圆点，展开后能看到这一轮的用量小结">
              <Switch on={settings.notifyFinished} onChange={(notifyFinished) => void updateSettings({ notifyFinished })} />
            </Row>
            <Row title="开机自启" hint="登录 Windows 后自动在后台启动，只显示托盘图标和灵动岛">
              <Switch on={settings.autostart} onChange={(autostart) => void updateSettings({ autostart })} />
            </Row>
            <Row title="关闭窗口" hint="点右上角的 × 只是收到托盘，统计继续进行；要彻底退出请点这里或托盘菜单">
              <button className="btn danger" onClick={quitApp}>
                退出 Foxes Token Watch
              </button>
            </Row>
          </Card>
        </div>

        <div className="col flush span-6">
          <Card title="显示" rows={3}>
            <Row title="主题">
              <Segmented
                options={[
                  { value: 'dark', label: '深色' },
                  { value: 'light', label: '浅色' },
                  { value: 'system', label: '跟随系统' },
                ]}
                value={settings.theme}
                onChange={(theme) => void updateSettings({ theme })}
              />
            </Row>
            <Row title="数字单位" hint="1,234,567 显示成 1.23M 还是 123.5万">
              <Segmented
                options={[
                  { value: 'kmb', label: 'K / M / B' },
                  { value: 'wanyi', label: '万 / 亿' },
                ]}
                value={settings.units}
                onChange={(units) => void updateSettings({ units })}
              />
            </Row>
            <Row title="减少动效" hint="关掉脉冲、呼吸和数字变化时的过渡动画，面板和灵动岛少占一些处理器和显卡">
              <Switch on={settings.reduceMotion} onChange={(reduceMotion) => void updateSettings({ reduceMotion })} />
            </Row>
            <div className="set-note muted">
              总量 = 输入 + 缓存命中 + 缓存写入 + 输出；思考是输出的一部分。缓存命中率 = 缓存命中 ÷（输入 + 缓存命中 + 缓存写入）。Claude Code 和 Deepseek
              Harness 不单独记录思考 token，按文本占比估算，界面上标了“估算”。
            </div>
          </Card>

          <Card title="关于" rows={1}>
            <div className="about">
              <AppIcon size={48} />
              <div className="about-text">
                <div className="about-name">
                  Foxes Token Watch <span className="muted num">{info ? `v${info.version}` : ''}</span>
                </div>
                <div className="muted">作者：Foxes</div>
                <div className="muted">本机 AI Agent 用量监控 · Claude Code、Codex、Deepseek Harness、Antigravity、Kiro、Cursor（含网页 MCP）。</div>
                <div className="muted">所有统计都在本机完成，只读不写，不连外网；唯一会连的地址是百事通插件在本机开的端口（127.0.0.1:9791）。</div>
                {info?.settingsPath ? (
                  <div className="muted ellipsis" title={info.settingsPath}>
                    设置保存在：{info.settingsPath}
                  </div>
                ) : null}
              </div>
            </div>
          </Card>
        </div>

        <Card title="数据来源" extra={<span className="muted">自动查找当前电脑上各 agent 的数据目录，在本机读取会话用量和工具活动；百事通插件的对话存在它的服务端，经插件在本机开的端口读取（只读）</span>} className="span-12">
          <div className="source-list">
            {AGENTS.map((agent, index) => {
              const live = snapshot.live.agents.find((item) => item.agent === agent)
              const roots = info?.roots[index] ?? []
              return (
                <Fragment key={agent}>
                  <div className="source-row">
                    <AgentBadge agent={agent} size={28} />
                    <div className="source-main">
                      <div className="source-name">
                        {AGENT_META[agent].label}
                        {live?.detected ? <span className="tag ok">已检测到</span> : <span className="tag">未检测到</span>}
                        {live?.readError && <span className="tag bad" title={live.readError}>读取失败</span>}
                        {live?.processAlive ? <span className="tag ok">进程运行中</span> : null}
                      </div>
                      <div className="source-paths muted">
                        {roots.length > 0
                          ? roots.map((root) => (
                              <div key={root} className="ellipsis" title={root}>
                                {root}
                              </div>
                            ))
                          : '没有找到数据目录'}
                      </div>
                      {live?.readError && <div className="set-note muted" role="alert">{live.readError}，正在重试。</div>}
                    </div>
                    <div className="source-count">
                      <b className="num">{live?.files ?? 0}</b>
                      <span className="muted">个记录文件</span>
                    </div>
                  </div>
                  {/* 百事通的对话算在 Cursor 里，紧跟着它写连得怎么样 */}
                  {agent === 'cursor' && feed && feedText ? (
                    <div className="source-row">
                      <AgentBadge agent="cursor" size={28} title="百事通的对话算在 Cursor 里" />
                      <div className="source-main">
                        <div className="source-name">
                          百事通对话
                          <span className={feedText.tone === 'ok' ? 'tag ok' : feedText.tone === 'bad' ? 'tag bad' : 'tag'}>{feedText.label}</span>
                        </div>
                        <div className="source-paths muted">127.0.0.1:9791</div>
                        <div className="source-note muted" role={feedText.tone === 'bad' ? 'alert' : undefined}>
                          {feedText.detail}
                        </div>
                      </div>
                      <div className="source-count">
                        <b className="num">{feed.sessions}</b>
                        <span className="muted">个对话</span>
                      </div>
                    </div>
                  ) : null}
                </Fragment>
              )
            })}
          </div>
        </Card>

        <Card
          title="扫描状态"
          className="span-12"
          extra={
            <button className="btn" onClick={copyDiagnostics} title="版本、各数据来源和百事通的连接状态、扫描耗时、用量历史的条数；不含对话标题、内容和文件路径">
              {copied === 'done' ? '已复制' : copied === 'failed' ? '没复制成' : '复制诊断信息'}
            </button>
          }
        >
          <div className="scan-body">
            <div className="stat-grid">
              <div>
                <span className="muted">记录文件</span>
                <b className="num">{fmtFull(scan.files)}</b>
              </div>
              <div>
                <span className="muted">数据量</span>
                <b className="num">{fmtBytes(scan.bytes)}</b>
              </div>
              <div>
                <span className="muted">请求记录</span>
                <b className="num">{fmtFull(scan.events)}</b>
              </div>
              <div>
                <span className="muted">首次扫描</span>
                <b className="num">{fmtMs(scan.firstScanMs)}</b>
              </div>
              <div>
                <span className="muted">每秒刷新耗时</span>
                <b className="num">{fmtMs(scan.scanMs)}</b>
              </div>
              <div>
                <span className="muted">读取失败</span>
                <b className="num">{scan.failing}</b>
              </div>
              <div title="把全部请求记录算成界面上的数字，最近一次花的时间">
                <span className="muted">汇总耗时</span>
                <b className="num">{fmtMs(scan.analysisMs ?? 0)}</b>
              </div>
              <div title="统计出错后会自动从头再来，这里是这次启动以来重来的次数">
                <span className="muted">统计中断</span>
                <b className="num">{scan.monitorRestarts ?? 0} 次</b>
              </div>
              <div>
                <span className="muted">历史里的会话</span>
                <b className="num">{fmtFull(history.sessions)}</b>
              </div>
            </div>
            <div>
              {(scan.monitorRestarts ?? 0) > 0 ? (
                <div className="set-note muted" role="alert">
                  统计中断过 {scan.monitorRestarts} 次，已经自动恢复{scan.monitorError ? ` · 最近一次：${scan.monitorError}` : ''}。反复出现的话，请点右上角复制诊断信息。
                </div>
              ) : null}
              {scan.historyError ? (
                <div className="set-note muted" role="alert">
                  用量历史读写失败 · {scan.historyError}
                </div>
              ) : null}
              <div className="set-note muted">
                各家的原始记录迟早会被清掉，换一台电脑更是读不到，所以每段会话的用量都在应用数据目录的 history.json 里存一份：只有每次请求的时刻、token
                数和去重用的编号，不含对话内容。历史只增不减，按电脑分开记，同一段会话、同一次请求只算一次。每天还会在旁边的 history-backup
                目录里留一份备份，保留最近 7 天的。
                {history.carried
                  ? `程序旁边还有一份（${history.carried}），跟着程序换电脑：到了别的电脑上运行，那台电脑的用量会接着加上去。`
                  : '这份历史只留在这台电脑上；直接运行免安装的程序时，程序旁边会另放一份，能跟着程序带到别的电脑。'}
                这台电脑记下了 {fmtFull(history.sessions)} 段会话
                {history.devices > 0 ? `，另有 ${history.devices} 台电脑的记录，其中 ${fmtFull(history.elsewhere)} 段是这台电脑上没有的，已算进总数` : ''}。
              </div>
            </div>
          </div>
        </Card>
      </div>
    </div>
  )
}
