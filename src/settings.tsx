/**
 * 设置弹窗（§4.2）：打开时读取 config_get，保存时整份 config_set。
 * apiKey 仅为 UI 态：提交后由 Rust 侧写入 OS keyring，config.json 不留明文。
 */

import { useEffect, useState } from 'react'
import { getConfig, setConfig, type Config } from './api'

interface Props {
  running: boolean
  onClose: () => void
}

export default function Settings({ running, onClose }: Props) {
  const [cfg, setCfg] = useState<Config | null>(null)
  const [apiKey, setApiKey] = useState('')
  const [saving, setSaving] = useState(false)
  const [error, setError] = useState('')
  const [notice, setNotice] = useState('')

  useEffect(() => {
    getConfig()
      .then((c) => {
        setCfg(c)
        setApiKey(c.model.apiKey)
      })
      .catch((e: unknown) => setError(`读取配置失败：${String(e)}`))
  }, [])

  const patchModel = (key: 'provider' | 'model' | 'baseURL', value: string) =>
    setCfg((c) => (c ? { ...c, model: { ...c.model, [key]: value } } : c))

  const save = async () => {
    if (!cfg) return
    setSaving(true)
    setError('')
    setNotice('')
    try {
      const saved = await setConfig({ ...cfg, model: { ...cfg.model, apiKey } })
      setCfg(saved)
      setNotice(running ? '已保存，部分配置需重启 Agent 后生效' : '已保存')
    } catch (e) {
      setError(`保存失败：${String(e)}`)
    } finally {
      setSaving(false)
    }
  }

  return (
    <div className="mask" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-head">
          <h2>设置</h2>
          <span className="spacer" />
          <button className="btn btn-sm" onClick={onClose}>
            关闭
          </button>
        </div>

        {!cfg ? (
          <div className="modal-body">
            <p className="hint">{error || '加载中…'}</p>
          </div>
        ) : (
          <>
            <div className="modal-body">
              <label className="field">
                <span>Provider</span>
                <input
                  type="text"
                  value={cfg.model.provider}
                  onChange={(e) => patchModel('provider', e.target.value)}
                />
              </label>

              <label className="field">
                <span>模型</span>
                <input
                  type="text"
                  value={cfg.model.model}
                  onChange={(e) => patchModel('model', e.target.value)}
                />
              </label>

              <label className="field">
                <span>API Key</span>
                <input
                  type="password"
                  placeholder="留空则不修改已保存的密钥"
                  value={apiKey}
                  onChange={(e) => setApiKey(e.target.value)}
                />
              </label>

              <label className="field">
                <span>Base URL</span>
                <input
                  type="text"
                  placeholder="留空使用官方地址"
                  value={cfg.model.baseURL}
                  onChange={(e) => patchModel('baseURL', e.target.value)}
                />
              </label>

              <label className="field field-inline">
                <input
                  type="checkbox"
                  checked={cfg.server.autoStart}
                  onChange={(e) =>
                    setCfg({ ...cfg, server: { ...cfg.server, autoStart: e.target.checked } })
                  }
                />
                <span>启动应用时自动运行 Agent</span>
              </label>

              <label className="field">
                <span>主题</span>
                <select
                  value={cfg.ui.theme}
                  onChange={(e) => setCfg({ ...cfg, ui: { theme: e.target.value } })}
                >
                  <option value="system">跟随系统</option>
                  <option value="light">浅色</option>
                  <option value="dark">深色</option>
                </select>
              </label>

              <label className="field">
                <span>更新通道</span>
                <select
                  value={cfg.updates.channel}
                  onChange={(e) => setCfg({ ...cfg, updates: { channel: e.target.value } })}
                >
                  <option value="stable">稳定版</option>
                  <option value="beta">测试版</option>
                </select>
              </label>
            </div>

            <div className="modal-foot">
              {error && <span className="msg msg-error">{error}</span>}
              {notice && <span className="msg msg-ok">{notice}</span>}
              <span className="spacer" />
              <button className="btn" onClick={onClose}>
                取消
              </button>
              <button className="btn btn-primary" disabled={saving} onClick={() => void save()}>
                {saving ? '保存中…' : '保存'}
              </button>
            </div>
          </>
        )}
      </div>
    </div>
  )
}
