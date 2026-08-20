/**
 * 前端 ↔ Rust 后端的唯一交互层（IPC 契约，见开发文档 §10.3）。
 * 所有 invoke 调用与事件监听都收拢在此文件，UI 组件不直接依赖 Tauri API。
 */

import { invoke } from '@tauri-apps/api/core'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'

// ---------------- 类型契约 ----------------

export interface Config {
  server: { host: string; port: number; autoStart: boolean }
  model: { provider: string; model: string; apiKey: string; baseURL: string }
  paths: { dataDir: string; logDir: string }
  advanced: { extraEnv: Record<string, string>; logLevel: string }
  ui: { theme: string }
  updates: { channel: string }
}

export interface Status {
  state: string // starting | running | stopped | error
  agentPort: number
  proxyPort: number
  proxyUrl: string
  pid: number | null
}

export interface LogLine {
  stream: string // stdout | stderr
  line: string
}

export interface ReadyPayload {
  proxyUrl: string
  agentPort: number
  proxyPort: number
}

export interface StatePayload {
  state: string
  proxyUrl: string
  agentPort: number
  proxyPort: number
  pid: number | null
}

// ---------------- 线上载荷归一化 ----------------
// Rust 侧 ModelConfig.base_url 经 serde camelCase 序列化为 `baseUrl`，
// 而契约文档写作 `baseURL`；此处双向兼容，避免任一侧改名即断链。

type WireModel = {
  provider: string
  model: string
  apiKey: string
  baseUrl?: string
  baseURL?: string
}

type WireConfig = Omit<Config, 'model'> & { model: WireModel }

function configFromWire(c: WireConfig): Config {
  return {
    ...c,
    model: {
      provider: c.model.provider,
      model: c.model.model,
      apiKey: c.model.apiKey ?? '',
      baseURL: c.model.baseUrl ?? c.model.baseURL ?? '',
    },
  }
}

function configToWire(c: Config): WireConfig {
  return {
    ...c,
    model: {
      provider: c.model.provider,
      model: c.model.model,
      apiKey: c.model.apiKey,
      baseUrl: c.model.baseURL,
      baseURL: c.model.baseURL,
    },
  }
}

// Rust 侧 StateEvent 未标注 camelCase，实际线上为 snake_case；两种键名都接受。
type WireState = {
  state: string
  proxyUrl?: string
  proxy_url?: string
  agentPort?: number
  agent_port?: number
  proxyPort?: number
  proxy_port?: number
  pid: number | null
}

function stateFromWire(s: WireState): StatePayload {
  return {
    state: s.state,
    proxyUrl: s.proxyUrl ?? s.proxy_url ?? '',
    agentPort: s.agentPort ?? s.agent_port ?? 0,
    proxyPort: s.proxyPort ?? s.proxy_port ?? 0,
    pid: s.pid ?? null,
  }
}

// ---------------- Tauri commands ----------------

export const agentStart = (): Promise<Status> => invoke<Status>('agent_start')

export const agentStop = (): Promise<void> => invoke<void>('agent_stop')

export const agentRestart = (): Promise<Status> => invoke<Status>('agent_restart')

export const getStatus = (): Promise<Status> => invoke<Status>('agent_get_status')

export async function getConfig(): Promise<Config> {
  return configFromWire(await invoke<WireConfig>('config_get'))
}

export async function setConfig(partial: Config): Promise<Config> {
  return configFromWire(
    await invoke<WireConfig>('config_set', { partial: configToWire(partial) }),
  )
}

export const minimize = (): Promise<void> => invoke<void>('window_minimize_to_tray')

export const show = (): Promise<void> => invoke<void>('window_show')

export const getThemePreference = (): Promise<string> => invoke<string>('theme_get_preference')

// ---------------- 事件监听（返回的 unlisten 需在卸载时调用） ----------------

export function listenLog(cb: (log: LogLine) => void): Promise<UnlistenFn> {
  return listen<LogLine>('agent://log', (e) => cb(e.payload))
}

export function listenReady(cb: (ready: ReadyPayload) => void): Promise<UnlistenFn> {
  return listen<ReadyPayload>('agent://ready', (e) => cb(e.payload))
}

export function listenState(cb: (state: StatePayload) => void): Promise<UnlistenFn> {
  return listen<WireState>('agent://state', (e) => cb(stateFromWire(e.payload)))
}

export function listenError(cb: (message: string) => void): Promise<UnlistenFn> {
  return listen<string>('agent://error', (e) => cb(e.payload))
}
