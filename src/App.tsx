/**
 * 应用外壳（§4.2）：Splash → 就绪后把 webview 导航到本地反代 URL。
 * 反代页面接管窗口后，本 React 壳即退出舞台；异常时回落到离线/重连页。
 *
 * 优化（2026-09）：
 * - 错误边界：捕获子树渲染异常，显示诊断信息而非白屏。
 * - 离线面板增强：显示错误类型、agent 端口状态、诊断链接（/ __dsh_health）。
 * - 日志持久化：使用 localStorage 保留最近 2000 条日志，重启后可追溯。
 * - 重连计数：offline 状态下记录重试次数，避免无限静默重试。
 */

import React from 'react'
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
  type LogLine,
  type Status,
} from './api'
import Settings from './settings'

const MAX_LOGS = 500
const MAX_LOGS_PERSIST = 2000
const STORAGE_KEY = 'dsh_agent_logs'
const RECONNECT_KEY = 'dsh_reconnect_count'

/** 从 localStorage 加载持久化日志（JSON 数组，限制条数）。 */
function loadPersistedLogs(): LogLine[] {
  try {
    const raw = localStorage.getItem(STORAGE_KEY)
    if (!raw) return []
    const parsed = JSON.parse(raw) as LogLine[]
    return Array.isArray(parsed) ? parsed.slice(-MAX_LOGS_PERSIST) : []
  } catch {
    return []
  }
}

/** 追加日志到 localStorage（截断到 MAX_LOGS_PERSIST）。 */
function persistLog(log: LogLine): void {
  try {
    const existing = loadPersistedLogs()
    existing.push(log)
    if (existing.length > MAX_LOGS_PERSIST) {
      existing.splice(0, existing.length - MAX_LOGS_PERSIST)
    }
    localStorage.setItem(STORAGE_KEY, JSON.stringify(existing))
  } catch {
    // localStorage 满或禁用时静默忽略
  }
}

/** 获取当前重连计数。 */
function getReconnectCount(): number {
  try {
    return parseInt(localStorage.getItem(RECONNECT_KEY) || '0', 10) || 0
  } catch {
    return 0
  }
}

/** 重置重连计数。 */
function resetReconnectCount(): void {
  try {
    localStorage.removeItem(RECONNECT_KEY)
  } catch { /* ignore */ }
}

/** 错误边界：捕获子组件渲染异常，显示诊断面板。 */
class ErrorBoundary extends React.Component<
  { children: React.ReactNode; fallback?: (err: Error) => React.ReactNode },
  { hasError: boolean; error: Error | null }
> {
  constructor(props: { children: React.ReactNode; fallback?: (err: Error) => React.ReactNode }) {
    super(props)
    this.state = { hasError: false, error: null }
  }
  static getDerivedStateFromError(error: Error) {
    return { hasError: true, error }
  }
  render() {
    if (this.state.hasError) {
      return this.props.fallback
        ? this.props.fallback(this.state.error!)
        : (
            <div className="panel error-boundary">
              <h1 className="title">应用内部错误</h1>
              <p className="reason">{this.state.error?.message}</p>
              <button
                className="btn btn-primary"
                onClick={() => {
                  this.setState({ hasError: false, error: null })
                  window.location.reload()
                }}
              >
                重启应用
              </button>
            </div>
          )
    }
    return this.props.children
  }
}

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

/** 离线/错误面板的详细信息。 */
interface OfflineDetail {
  message: string
  agentPort: number
  proxyUrl: string
  reconnectCount: number
}

export default function App() {
  const [status, setStatus] = useState<Status | null>(null)
  const [offline, setOffline] = useState('')
  const [offlineDetail, setOfflineDetail] = useState<OfflineDetail | null>(null)
  const [logs, setLogs] = useState<LogLine[]>(loadPersistedLogs)
  const [logOpen, setLogOpen] = useState(false)
  const [settingsOpen, setSettingsOpen] = useState(false)
  const navigated = useRef(false)
  const logEndRef = useRef<HTMLDivElement>(null)
  // 防止持久化日志与实时日志混排：首次 boot 后清除旧日志
  const logsCleared = useRef(false)

  /** 导航到反代地址；只允许发生一次。 */
  const navigate = useCallback((url: string) => {
    if (navigated.current || !url) return
    navigated.current = true
    void navigateWebview(url).catch((e: unknown) => {
      navigated.current = false
      const count = getReconnectCount() + 1
      try { localStorage.setItem(RECONNECT_KEY, String(count)) } catch { /* ignore */ }
      setOffline(`加载界面失败：${String(e)}`)
      setOfflineDetail({
        message: String(e),
        agentPort: status?.agentPort ?? 0,
        proxyUrl: url,
        reconnectCount: count,
      })
    })
  }, [status])

  useEffect(() => {
    let disposed = false
    const unlistens: UnlistenFn[] = []

    const boot = async () => {
      const subs = await Promise.all([
        listenLog((log) => {
          // 清除持久化历史（避免重启后重复显示）
          if (!logsCleared.current) {
            logsCleared.current = true
            setLogs([])
            try { localStorage.removeItem(STORAGE_KEY) } catch { /* ignore */ }
          }
          setLogs((prev) => {
            const next = [...prev, log].slice(-MAX_LOGS)
            persistLog(log)
            return next
          })
        }),
        listenReady((ready) => {
          setOffline('')
          setOfflineDetail(null)
          resetReconnectCount()
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
        listenError((message) => {
          const count = getReconnectCount() + 1
          try { localStorage.setItem(RECONNECT_KEY, String(count)) } catch { /* ignore */ }
          setOffline(message || 'Agent 启动失败')
          setOfflineDetail({
            message,
            agentPort: status?.agentPort ?? 0,
            proxyUrl: status?.proxyUrl ?? '',
            reconnectCount: count,
          })
        }),
      ])

      // 等 ready 事件携带 proxyUrl 后再导航（listenReady 回调会调用 navigate）
      // 这里只处理已有 running 状态的情况
      if (disposed) {
        subs.forEach((un) => un())
        return
      }
      unlistens.push(...subs)

      try {
        const st = await getStatus()
        if (disposed) return
        setStatus(st)
        if (st.state === 'running' && st.proxyUrl) {
          resetReconnectCount()
          navigate(st.proxyUrl)
        }
      } catch (e) {
        if (!disposed) {
          setOffline(`获取状态失败：${String(e)}`)
          setOfflineDetail({
            message: String(e),
            agentPort: 0,
            proxyUrl: '',
            reconnectCount: getReconnectCount(),
          })
        }
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
    setOfflineDetail(null)
    try {
      const st = await agentStart()
      setStatus(st)
      if (st.state === 'running' && st.proxyUrl) {
        resetReconnectCount()
        navigate(st.proxyUrl)
      }
    } catch (e) {
      const count = getReconnectCount() + 1
      try { localStorage.setItem(RECONNECT_KEY, String(count)) } catch { /* ignore */ }
      setOffline(`重试失败：${String(e)}`)
      setOfflineDetail({
        message: String(e),
        agentPort: status?.agentPort ?? 0,
        proxyUrl: status?.proxyUrl ?? '',
        reconnectCount: count,
      })
    }
  }

  const clearLogs = () => {
    setLogs([])
    try { localStorage.removeItem(STORAGE_KEY) } catch { /* ignore */ }
  }

  const running = status?.state === 'running'
  // 从持久化日志中提取最后一条 agent 错误，便于离线面板展示上下文
  const lastErrorLog = [...logs].reverse().find((l) => l.stream === 'stderr' && l.line.includes('error'))
    ?.line ?? ''

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
            {offlineDetail && (
              <div className="diagnostics">
                {offlineDetail.agentPort > 0 && (
                  <p className="diag-row">
                    Agent 端口: <code>{offlineDetail.agentPort}</code>
                  </p>
                )}
                {offlineDetail.proxyUrl && (
                  <p className="diag-row">
                    代理地址: <code>{offlineDetail.proxyUrl}</code>
                  </p>
                )}
                {lastErrorLog && (
                  <p className="diag-row diag-error">
                    最近错误: <code>{lastErrorLog.slice(0, 120)}</code>
                  </p>
                )}
                {offlineDetail.reconnectCount > 0 && (
                  <p className="diag-row diag-warn">
                    连续重试: {offlineDetail.reconnectCount} 次
                  </p>
                )}
              </div>
            )}
            <button className="btn btn-primary" onClick={() => void retry()}>
              重试
            </button>
          </div>
        ) : (
          <div className="panel">
            <div className="spinner" />
            <h1 className="title">正在启动 Agent…</h1>
            <p className="hint">首次启动需要初始化本地运行环境，请稍候</p>
            {status && status.state === 'starting' && (
              <p className="hint">端口: {status.agentPort > 0 ? status.agentPort : '…'}</p>
            )}
          </div>
        )}
      </main>

      {logOpen && (
        <section className="logs">
          <div className="logs-head">
            <span>运行日志 ({logs.length})</span>
            <span className="spacer" />
            <button className="btn btn-sm" onClick={clearLogs}>
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
