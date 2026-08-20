/**
 * 应用外壳（§4.2）：Splash → 就绪后把 webview 导航到本地反代 URL。
 * 反代页面接管窗口后，本 React 壳即退出舞台；异常时回落到离线/重连页。
 */

import { useCallback, useEffect, useRef, useState } from 'react'
import { getCurrentWebview } from '@tauri-apps/api/webview'
import type { UnlistenFn } from '@tauri-apps/api/event'
import {
  agentStart,
  getStatus,
  listenError,
  listenLog,
  listenReady,
  listenState,
  minimize,
  getThemePreference,
  type LogLine,
  type Status,
} from './api'
import Settings from './settings'

const MAX_LOGS = 500

/**
 * 把窗口导航到反代地址。
 * @tauri-apps/api 2.11 的 Webview 尚未暴露 navigate()，此处优先用官方方法（未来版本可用），
 * 缺失时回落到标准跳转（tauri.conf.json 的 csp 为 null，不受策略拦截）。
 */
type Navigable = { navigate?: (url: URL) => Promise<void> }

async function navigateWebview(url: string): Promise<void> {
  const webview = getCurrentWebview() as unknown as Navigable
  if (typeof webview.navigate === 'function') {
    await webview.navigate(new URL(url))
  } else {
    window.location.replace(url)
  }
}

const STATE_TEXT: Record<string, string> = {
  starting: '启动中',
  running: '运行中',
  stopped: '已停止',
  error: '异常',
}

export default function App() {
  const [status, setStatus] = useState<Status | null>(null)
  const [offline, setOffline] = useState('')
  const [logs, setLogs] = useState<LogLine[]>([])
  const [logOpen, setLogOpen] = useState(false)
  const [settingsOpen, setSettingsOpen] = useState(false)
  const navigated = useRef(false)
  const logEndRef = useRef<HTMLDivElement>(null)

  /** 导航到反代地址；只允许发生一次。 */
  const navigate = useCallback((url: string) => {
    if (navigated.current || !url) return
    navigated.current = true
    void navigateWebview(url).catch((e: unknown) => {
      navigated.current = false
      setOffline(`加载界面失败：${String(e)}`)
    })
  }, [])

  // 启动时恢复主题偏好
  useEffect(() => {
    getThemePreference().then((theme) => {
      localStorage.setItem('dsh-angelina-themes.selection', theme)
    }).catch(() => {
      // 忽略错误，不设置主题
    })
  }, [])

  useEffect(() => {
    let disposed = false
    const unlistens: UnlistenFn[] = []

    const boot = async () => {
      const subs = await Promise.all([
        listenLog((log) => setLogs((prev) => [...prev, log].slice(-MAX_LOGS))),
        listenReady((ready) => {
          setOffline('')
          navigate(ready.proxyUrl)
        }),
        listenState((s) =>
          setStatus({
            state: s.state,
            agentPort: s.agentPort,
            proxyPort: s.proxyPort,
            proxyUrl: s.proxyUrl,
            pid: s.pid,
          }),
        ),
        listenError((message) => setOffline(message || 'Agent 启动失败')),
      ])

      if (disposed) {
        subs.forEach((un) => un())
        return
      }
      unlistens.push(...subs)

      try {
        const st = await getStatus()
        if (disposed) return
        setStatus(st)
        if (st.state === 'running' && st.proxyUrl) navigate(st.proxyUrl)
      } catch (e) {
        if (!disposed) setOffline(`获取状态失败：${String(e)}`)
      }
    }

    void boot()
    return () => {
      disposed = true
      unlistens.forEach((un) => un())
    }
  }, [navigate])

  useEffect(() => {
    if (logOpen) logEndRef.current?.scrollIntoView({ block: 'end' })
  }, [logs, logOpen])

  const retry = async () => {
    setOffline('')
    try {
      const st = await agentStart()
      setStatus(st)
      if (st.state === 'running' && st.proxyUrl) navigate(st.proxyUrl)
    } catch (e) {
      setOffline(`重试失败：${String(e)}`)
    }
  }

  const running = status?.state === 'running'

  return (
    <div className="app">
      <header className="topbar">
        <span className="brand">DeepSeek Harness</span>
        <span className={`badge badge-${status?.state ?? 'stopped'}`}>
          {STATE_TEXT[status?.state ?? 'stopped'] ?? status?.state}
        </span>
        <span className="spacer" />
        <button className="btn" onClick={() => setLogOpen((v) => !v)}>
          {logOpen ? '隐藏日志' : '查看日志'}
        </button>
        <button className="btn" onClick={() => setSettingsOpen(true)}>
          设置
        </button>
        <button className="btn" onClick={() => void minimize()}>
          最小化到托盘
        </button>
      </header>

      <main className="stage">
        {offline ? (
          <div className="panel">
            <h1 className="title">无法连接到 Agent</h1>
            <p className="reason">{offline}</p>
            <button className="btn btn-primary" onClick={() => void retry()}>
              重试
            </button>
          </div>
        ) : (
          <div className="panel">
            <div className="spinner" />
            <h1 className="title">正在启动 Agent…</h1>
            <p className="hint">首次启动需要初始化本地运行环境，请稍候</p>
          </div>
        )}
      </main>

      {logOpen && (
        <section className="logs">
          <div className="logs-head">
            <span>运行日志</span>
            <span className="spacer" />
            <button className="btn btn-sm" onClick={() => setLogs([])}>
              清空
            </button>
          </div>
          <div className="logs-body">
            {logs.length === 0 ? (
              <div className="log-empty">暂无日志</div>
            ) : (
              logs.map((log, i) => (
                <div key={i} className={`log-line log-${log.stream}`}>
                  {log.line}
                </div>
              ))
            )}
            <div ref={logEndRef} />
          </div>
        </section>
      )}

      {settingsOpen && <Settings running={running} onClose={() => setSettingsOpen(false)} />}
    </div>
  )
}
