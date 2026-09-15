//! 本地反向代理（受控 ingress 边界，§10.7 / §8.1 / §13.8 D5）。
//!
//! axum 监听随机 loopback 端口，校验运行期随机 token（cookie 握手 + query 兜底），
//! 拦截 DNS 重绑定，将请求转发给仅监听 loopback 的 `dsh` 后端。随机端口≠认证，
//! token 才是真实认证边界；dsh 裸端口无法关闭，反代仅加固本机任意进程直连面。
//!
//! 关键修复（2026-08-19）：此前代理仅用 reqwest 做 HTTP 转发，对 WebSocket 升级请求无能为力——
//! reqwest 无法隧道化双向 WS，导致 SPA 的实时事件流（用户消息 / AI 回复）被缓冲或丢弃，
//! UI 要等 ~20s 才显示。现对 WS 端点做真正的隧道：axum 接受浏览器升级，
//! tokio-tungstenite 连上游，双向透传帧。
//!
//! 关键修复（2026-09-02）：WS 端点名在 harness 各版本间漂移过——
//! 早期为 `/api/events.mux` / `/api/events.host`，alpha.3 起统一为 `/api/remote.mux`
//! （证据：运行时下发的插件 combo bundle 中仅出现 `/api/remote.mux`；直连 sidecar 时
//! `/api/remote.mux` 返回 401 鉴权响应，而 `events.*` 与任意伪造路径均 `socket hang up`，
//! 即 webserver 的 upgrade 路由表中不存在）。代理若仍只注册旧名，SPA 的升级请求会落到
//! HTTP fallback（reqwest）→ 无法完成 101 握手 → 浏览器 WS 以 1006 关闭 → UI 永久
//! "连接中…" + "正在加载模型…"。
//! 对策：路由侧同时注册三代端点名，上游侧按候选列表依次尝试，取首个握手成功者。
//!
//! 优化（2026-09）：
//! - 401 重试改为指数退避（最多 3 次），避免 token 长期无效时的无效请求风暴。
//! - Cookie 增加过期时间戳，超过 25 分钟自动标记 stale 强制重新握手。
//! - 新增 `/__dsh_health` 端点，供外部监控或内部健康检查使用。

use std::io::Read;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use brotli::Decompressor;
use flate2::read::{GzDecoder, ZlibDecoder};

use axum::body::{Body, Bytes};
use tauri::Manager;

use crate::config;
use crate::state::AppState;
use axum::extract::ws::{Message as AMessage, WebSocket as AWebSocket, WebSocketUpgrade};
use axum::extract::{OriginalUri, State};
use axum::http::{header::COOKIE, HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as TMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// Cookie 有效期（毫秒）。超过此时间后视为 stale，下次请求前强制重新握手。
/// alpha.3 的 session cookie 由 harness 签名，通常有效 30 分钟；设 25 分钟
/// 留有余量，避免临期导致 401 后再重试。
const COOKIE_TTL_MS: u64 = 25 * 60 * 1000;

/// 401 最大重试次数（含首次请求）。
const MAX_401_RETRIES: u32 = 3;

pub struct ProxyState {
    pub agent_port: u16,
    pub token: String,
    /// 主题 id（来自 AppConfig.ui.theme），注入 HTML 时写入 localStorage
    pub theme: Option<String>,
    /// Tauri app handle，用于把 SPA 上报的主题持久化到 config.json（AppConfig.ui.theme）
    pub app: tauri::AppHandle,
    /// 上游 harness 的 launch token（来自 ready 行 `?token=`），用于换取签名 cookie
    pub agent_token: String,
    /// 与上游 harness 完成 token 交换后 Harvest 的会话 cookie（`name=value`，已剥离属性）
    pub agent_cookie: Arc<Mutex<CookieCache>>,
}

/// Cookie 缓存：存 cookie 字符串 + 获取时间，用于过期检测。
#[derive(Clone)]
pub(crate) struct CookieCache {
    value: Option<String>,
    /// 获取时间；None 表示尚未获取
    obtained_at: Option<Instant>,
}

impl CookieCache {
    fn new() -> Self {
        Self {
            value: None,
            obtained_at: None,
        }
    }

    fn is_some(&self) -> bool {
        self.value.is_some()
    }

    /// 检查 cookie 是否过期（超过 COOKIE_TTL_MS）。
    fn is_expired(&self) -> bool {
        match (&self.value, self.obtained_at) {
            (Some(_), Some(obtained)) => obtained.elapsed() > Duration::from_millis(COOKIE_TTL_MS),
            _ => true,
        }
    }

    fn get(&self) -> Option<String> {
        self.value.clone()
    }

    fn set(&mut self, value: String, now: Instant) {
        self.value = Some(value);
        self.obtained_at = Some(now);
    }

    fn clear(&mut self) {
        self.value = None;
        self.obtained_at = None;
    }
}

/// 上游 WebSocket 流类型（明文字节，无 TLS）。
type UpstreamWs = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// 各版本 harness 曾用/现用的 WS 升级端点名，按「当前版本优先」排列。
/// 路由侧全部注册；上游侧按此顺序做候选回退，取首个握手成功者。
const WS_ROUTES: [&str; 3] = ["/api/remote.mux", "/api/events.mux", "/api/events.host"];

/// 启动反代，返回 (proxy_port, proxy_url)。proxy_url 含首次握手 token。
/// `app`：用于把 SPA 上报的主题持久化到 config.json。
/// `initial_theme`：AppConfig.ui.theme 的初始值，用于注入插件 localStorage。
/// 传 None 则仅注入「上报脚本」（仍捕获用户后续的主题选择），不预置初始主题。
/// `agent_token`：上游 harness 的 launch token（alpha.3+ 鉴权所需），用于换取会话 cookie。
pub async fn start_proxy(
    agent_port: u16,
    token: String,
    app: tauri::AppHandle,
    initial_theme: Option<String>,
    agent_token: Option<String>,
) -> Result<(u16, String), String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| e.to_string())?;
    let proxy_port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let agent_cookie = Arc::new(Mutex::new(CookieCache::new()));

    // 与上游 harness 完成 token 交换，harvest 签名会话 cookie（alpha.3+ 强制鉴权）。
    // 交换失败（如 harness 尚未就绪）时回退到惰性握手：首个请求到达时再尝试一次。
    // 修复（2026-09-01）：优先使用调用方传入的 token，若为空则回退到 AppState 中
    // 「实时」的 agent_token——TCP 回退路径可能在 stdout 解析出 token 之前就触发 on_ready，
    // 此时传入的快照为空，必须用最新值，否则代理将以空 token 永久运行 → 502。
    let passed = agent_token.unwrap_or_default();
    let live = app
        .state::<AppState>()
        .inner
        .lock()
        .unwrap()
        .agent_token
        .clone()
        .unwrap_or_default();
    let agent_token = if passed.is_empty() { live } else { passed };
    if !agent_token.is_empty() {
        if let Some(c) = handshake_cookie(agent_port, &agent_token).await {
            let mut cache = agent_cookie.lock().unwrap();
            cache.set(c, Instant::now());
        }
    }

    let state = Arc::new(ProxyState {
        agent_port,
        token: token.clone(),
        theme: initial_theme,
        app,
        agent_token,
        agent_cookie,
    });
    // 仅 WS 端点走专用隧道 handler（三代端点名全注册，避免上游改名后再次失配）；
    // /__dsh_theme 接收前端主题上报；/__dsh_health 健康检查；其余回退到通用 HTTP handler。
    let mut router = Router::new()
        .route("/__dsh_theme", any(theme_handler))
        .route("/__dsh_health", any(health_handler));
    for route in WS_ROUTES.iter() {
        router = router.route(route, any(ws_handler));
    }
    let app = router.fallback(any(handler)).with_state(state);

    tauri::async_runtime::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("proxy server error: {e}");
        }
    });

    let proxy_url = format!("http://127.0.0.1:{}/?t={}", proxy_port, token);
    Ok((proxy_port, proxy_url))
}

/// 向上游 harness 发起 launch-token 交换，返回 `name=value` 形式的会话 cookie。
/// harness 在 `GET /?token=<launchToken>` 时返回 303 + Set-Cookie（HttpOnly, SameSite=Strict）。
/// 该 cookie 由 harness 用 `$DSH_HOME/.credentials.yaml` 中的密钥签名，桌面无法伪造，必须换取。
/// cookie 名 = `dsh-auth-` + base64url(sha256(Host))，因此请求 Host 必须与交换时一致（127.0.0.1:<port>）。
///
/// `attempts`：允许调用方控制重试预算 —— 页面请求用 20 次（约 10s），
/// `/__dsh_health` 轮询用 1 次（每次轮询只敲一次，避免把轮询变成阻塞的 10s 重试）。
async fn handshake_cookie_attempts(
    agent_port: u16,
    agent_token: &str,
    attempts: u32,
) -> Option<String> {
    if agent_token.is_empty() {
        return None;
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .ok()?;
    let url = format!("http://127.0.0.1:{}/?token={}", agent_port, agent_token);
    for _ in 0..attempts {
        if let Ok(resp) = client.get(&url).send().await {
            if let Some(sc) = resp.headers().get(reqwest::header::SET_COOKIE) {
                if let Ok(s) = sc.to_str() {
                    // 仅取首个 name=value 对（剥离 Max-Age/Path/Expires/HttpOnly/SameSite 等响应属性）
                    let pair = s.split(';').next().unwrap_or("").trim().to_string();
                    if !pair.is_empty() {
                        return Some(pair);
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    None
}

async fn handshake_cookie(agent_port: u16, agent_token: &str) -> Option<String> {
    handshake_cookie_attempts(agent_port, agent_token, 20).await
}

/// 取「当前」的 harness launch token：优先 AppState 实时值，回退到代理启动时的快照。
///
/// 修复（2026-09-01）：不能用启动快照——stdout 解析可能在代理启动后才写入 agent_token，
/// 用快照会永远拿不到 cookie。
fn live_agent_token(s: &Arc<ProxyState>) -> String {
    let live = s
        .app
        .state::<AppState>()
        .inner
        .lock()
        .unwrap()
        .agent_token
        .clone()
        .unwrap_or_default();
    if live.is_empty() {
        s.agent_token.clone()
    } else {
        live
    }
}

/// 惰性握手：若尚无 cookie 或 cookie 已过期，则尝试换取。
async fn ensure_cookie_with(s: &Arc<ProxyState>, attempts: u32) {
    let tok = live_agent_token(s);
    // 已有有效 cookie 则跳过（短锁：判定后立即释放 guard，避免持 MutexGuard 跨 await）
    {
        let cache = s.agent_cookie.lock().unwrap();
        if cache.is_some() && !cache.is_expired() {
            return;
        }
    }
    if !tok.is_empty() {
        if let Some(c) = handshake_cookie_attempts(s.agent_port, &tok, attempts).await {
            let mut cache = s.agent_cookie.lock().unwrap();
            cache.set(c, Instant::now());
        } else {
            // 握手失败时清空，下次请求再重试（而非带着过期 cookie 反复失败）
            s.agent_cookie.lock().unwrap().clear();
        }
    }
}

async fn ensure_cookie(s: &Arc<ProxyState>) {
    ensure_cookie_with(s, 20).await;
}

/// 等待会话 cookie 就绪：先惰性握手一次，再有界等待（最多约 3.2s）。
/// 首个请求（HTTP 或 WS）可能抢在 stdout token 解析完成前到达——TCP 回退路径
/// 先触发 on_ready 时 token 仍空——此处轮询等待 token 落盘后重试握手，
/// 避免一次性 502 / WS 1006 误伤首次加载。
async fn wait_cookie(s: &Arc<ProxyState>) -> String {
    ensure_cookie(s).await;
    for _ in 0..8 {
        let cookie = s.agent_cookie.lock().unwrap().get();
        if cookie.is_some() {
            return cookie.unwrap_or_default();
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
        ensure_cookie(s).await;
    }
    s.agent_cookie.lock().unwrap().get().unwrap_or_default()
}

/// 根页面导航专用的「快速」cookie 获取：最多约 8 秒（拿到即返回）。
///
/// 为什么是 8 秒而不是 3 秒（2026-09-14 实机回归）：
/// TCP 回退探测可能在 **stdout 解析出 launch token 之前**就触发 `on_ready`，
/// 此时代理启动快照里的 token 为空（见 `on_ready` 的注释）。之后 launch token 才落盘，
/// `ensure_cookie` 才能用「实时」token 换取 cookie。
/// 3 秒窗口会在这个空档里超时 → 一律落到引导页；而引导页当时只轮询 `/__dsh_health`
/// → 死锁（连热重启都救不回来，比引导页之前更糟）。
/// 实测 8 秒足以覆盖 token 落盘的延迟，又不至于让窗口长时间空白；
/// 真正慢的上游仍由引导页兜底（引导页现在会自己驱动握手，见 `health_handler`）。
async fn wait_cookie_brief(s: &Arc<ProxyState>) -> String {
    if let Some(c) = { s.agent_cookie.lock().unwrap().get() } {
        return c;
    }
    // 每次只做「一次」握手尝试（不动用 `ensure_cookie` 的 10 秒重试预算），
    // 用有界外层循环轮询，保证总耗时可控、且在下游不可达时能立刻失败重试。
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        ensure_cookie_with(s, 1).await;
        if let Some(c) = s.agent_cookie.lock().unwrap().get() {
            return c;
        }
        if Instant::now() >= deadline {
            return String::new();
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
}

/// 判断这是浏览器「导航到页面」的请求（`Accept` 含 `text/html`），
/// 而不是 SPA 发出的 XHR / fetch（那类请求的 Accept 是 application/json 等）。
fn wants_html(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|a| a.contains("text/html"))
        .unwrap_or(false)
}

/// 冷启动自愈引导页。
///
/// 背景（2026-09-14 实机）：新装 / 升级后的**首次**启动时，上游 harness 需要先完成初始化
/// （生成 `.credentials.yaml` 签名密钥、编译 profile 等），其可服务时刻可能明显晚于代理启动。
/// 此时 WebView 的首次导航会撞上「会话 cookie 尚未 harvest」。
///
/// 旧行为是直接回一行 `502 harness auth cookie unavailable` 纯文本 ——
/// 浏览器会把它当成最终页面，**既不是 HTML、也不会自动重试**，
/// 于是 UI 永久停在加载态，只能人工重启应用（这正是线上观察到的症状）。
///
/// 现改为返回一个自带轮询的 HTML：每 800ms 查一次 `/__dsh_health`，
/// 一旦 `ready=true` 就 `location.replace` 回原 URL（保留 `?t=` token），
/// 把「一次性失败」变成「自动等待、就绪即接管」。
fn boot_page_response(uri: &Uri, reason: &str) -> Response {
    // 回跳目标即本次请求的 path+query（根路径 + token），不额外信任任何外部输入。
    let target = uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .filter(|s| s.starts_with('/'))
        .unwrap_or("/");
    let target_json = serde_json::to_string(target).unwrap_or_else(|_| "\"/\"".to_string());
    let reason_json = serde_json::to_string(reason).unwrap_or_else(|_| "\"\"".to_string());

    let html = format!(
        r#"<!doctype html>
<html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>正在连接…</title>
<style>
 html,body{{height:100%;margin:0}}
 body{{display:flex;align-items:center;justify-content:center;
   font:14px/1.6 -apple-system,"Segoe UI","Microsoft YaHei",sans-serif;
   background:#0f1115;color:#e6e6e6}}
 .box{{text-align:center;max-width:440px;padding:0 24px}}
 .spin{{width:28px;height:28px;margin:0 auto 18px;border:3px solid #3a3f4b;
   border-top-color:#6b8afd;border-radius:50%;animation:r 1s linear infinite}}
 @keyframes r{{to{{transform:rotate(360deg)}}}}
 h1{{font-size:16px;font-weight:600;margin:0 0 8px}}
 p{{margin:6px 0;color:#9aa4b2;font-size:13px}}
 code{{background:#1a1d24;padding:1px 5px;border-radius:4px;font-size:12px;color:#8b95a5}}
 button{{margin-top:16px;padding:7px 18px;border:0;border-radius:6px;
   background:#3b5bdb;color:#fff;font-size:13px;cursor:pointer}}
 #warn{{display:none;color:#e0a458}}
</style></head><body><div class="box">
<div class="spin" id="sp"></div>
<h1 id="t">正在连接本地服务…</h1>
<p>首次启动需要初始化运行环境，可能需要几十秒，请稍候。</p>
<p id="warn">等待时间较长，可点击下方按钮重试。</p>
<p><code id="why"></code></p>
<button id="btn" style="display:none" onclick="location.reload()">重试</button>
</div>
<script>
(function(){{
  var TARGET={target_json};
  var REASON={reason_json};
  var n=0, MAXWAIT=225;   // 225 × 800ms ≈ 3 分钟
  document.getElementById('why').textContent=REASON;
  function giveUp(){{
    document.getElementById('sp').style.display='none';
    document.getElementById('t').textContent='暂时无法连接本地服务';
    document.getElementById('warn').style.display='block';
    document.getElementById('btn').style.display='inline-block';
  }}
  function next(){{
    n++;
    if(n%4===0){{ nudge(); }}
    if(n===40){{ document.getElementById('warn').style.display='block'; }}
    if(n>=MAXWAIT){{ giveUp(); return; }}
    setTimeout(poll, 800);
  }}
  function poll(){{
    fetch('/__dsh_health',{{cache:'no-store'}})
      .then(function(r){{return r.json()}})
      .then(function(j){{ if(j&&j.ready===true){{ location.replace(TARGET); return; }} next(); }})
      .catch(function(){{ next(); }});
  }}
  // 每 4 次轮询（约 3.2s）用 HEAD 真敲一次页面地址：这条请求与浏览器导航走
  // **同一条**处理路径（含 token 校验与会话握手），因此即使 `/__dsh_health` 的
  // 语义将来变了，引导页也一定能收敛——不把成败押在单一「就绪标志」上。
  function nudge(){{
    try {{ fetch(TARGET, {{method:'HEAD', cache:'no-store'}}).catch(function(){{}}); }} catch(e) {{}}
  }}
  setTimeout(poll, 500);
}})();
</script></body></html>"#,
        target_json = target_json,
        reason_json = reason_json,
    );

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/html; charset=utf-8")
        .header("cache-control", "no-store")
        .body(Body::from(html))
        .unwrap_or_else(|_| (StatusCode::INTERNAL_SERVER_ERROR, "boot page failed").into_response())
}

/// 校验请求携带的 token（cookie `dsh_token` 或首次 query `t`）。
fn valid_token(s: &ProxyState, uri: &Uri, headers: &HeaderMap) -> bool {
    let from_query = extract_query(uri.query().unwrap_or(""), "t");
    let from_cookie = extract_cookie(headers, "dsh_token");
    let provided = from_cookie.clone().or(from_query.clone());
    matches!(&provided, Some(t) if t == &s.token)
}

/// 通用 HTTP 处理（GET/POST/...），reqwest 转发上游并流式回传。
async fn handler(
    State(s): State<Arc<ProxyState>>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // —— token 校验：cookie dsh_token 或首次 query t ——
    if !valid_token(&s, &uri, &headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }

    // 首次带 query token：下发 HttpOnly cookie，后续走 cookie（避免 token 落入 URL 历史/referrer）
    let set_cookie = if extract_query(uri.query().unwrap_or(""), "t").is_some()
        && extract_cookie(&headers, "dsh_token").is_none()
    {
        Some(format!(
            "dsh_token={}; HttpOnly; Path=/; Max-Age=86400; SameSite=Strict",
            s.token
        ))
    } else {
        None
    };

    // DNS 重绑定防护：Origin 若存在须为回环或 null
    if let Some(origin) = headers.get("origin") {
        let o = origin.to_str().unwrap_or("");
        if !(o.is_empty()
            || o.starts_with("http://127.0.0.1")
            || o.starts_with("http://localhost")
            || o == "null")
        {
            return (StatusCode::FORBIDDEN, "forbidden origin").into_response();
        }
    }

    // 附加上游 harness 鉴权 cookie（alpha.3+ 强制）：惰性握手兜底
    //
    // 冷启动容错（2026-09-14）：若这是浏览器的**页面导航**（根路径 + Accept: text/html），
    // 则走「快速失败 + 自愈引导页」，而不是长时间阻塞后回一行裸 502 文本
    // （浏览器不会重试纯文本错误页 → UI 永久卡死）。详见 `boot_page_response`。
    let is_root = uri.path() == "/";
    let html_nav = is_root && wants_html(&headers);
    let theme = s.theme.clone();

    let cookie = if html_nav {
        wait_cookie_brief(&s).await
    } else {
        wait_cookie(&s).await
    };
    if cookie.is_empty() {
        return if html_nav {
            boot_page_response(&uri, "等待上游鉴权会话就绪…")
        } else {
            (StatusCode::BAD_GATEWAY, "harness auth cookie unavailable").into_response()
        };
    }

    // 首次转发
    let resp =
        match forward_upstream(&s, method.clone(), &uri, &headers, body.clone(), &cookie).await {
            Ok(r) => r,
            Err(e) => {
                return if html_nav {
                    boot_page_response(&uri, "上游 harness 尚未就绪…")
                } else {
                    (StatusCode::BAD_GATEWAY, e).into_response()
                }
            }
        };
    let status = resp.status();

    // 401 = 会话 cookie 过期或被拒：指数退避重试（最多 MAX_401_RETRIES 次）
    if status == StatusCode::UNAUTHORIZED {
        let resp =
            retry_with_backoff(&s, method, &uri, &headers, body, &cookie, is_root, &theme).await;
        // 重试耗尽仍未通过：页面导航交给引导页继续轮询（而非把 401 文本丢给浏览器）
        return if html_nav && !resp.status().is_success() {
            boot_page_response(&uri, "上游鉴权尚未就绪…")
        } else {
            resp
        };
    }

    // 根页面导航拿到任何非 2xx（例如上游路由尚未挂载返回 404/503）同样交给引导页
    if html_nav && !status.is_success() {
        return boot_page_response(&uri, &format!("上游返回 {}…", status.as_u16()));
    }

    transform_upstream(resp, &theme, set_cookie.as_deref(), is_root).await
}

/// 401 重试：指数退避，最多 MAX_401_RETRIES 次。
/// 每次重试前重新握手获取新 cookie（解决 cookie 过期问题）。
#[allow(clippy::too_many_arguments)] // 转发需透传 method/uri/headers/body/token/root/theme，超参边界可接受
async fn retry_with_backoff(
    s: &Arc<ProxyState>,
    method: Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: Bytes,
    _orig_cookie: &str,
    is_root: bool,
    theme: &Option<String>,
) -> Response {
    let live = s
        .app
        .state::<AppState>()
        .inner
        .lock()
        .unwrap()
        .agent_token
        .clone()
        .unwrap_or_default();
    let tok = if live.is_empty() {
        s.agent_token.clone()
    } else {
        live
    };
    if tok.is_empty() {
        return (StatusCode::UNAUTHORIZED, "harness auth required").into_response();
    }
    let mut backoff_ms: u64 = 200;
    for attempt in 1..=MAX_401_RETRIES {
        // 每次重试前重新握手获取新 cookie
        if let Some(c) = handshake_cookie(s.agent_port, &tok).await {
            // 短锁：set 后立即释放 guard，避免 MutexGuard 跨 await 使 future 非 Send
            {
                let mut cache = s.agent_cookie.lock().unwrap();
                cache.set(c.clone(), Instant::now());
            }
            match forward_upstream(s, method.clone(), uri, headers, body.clone(), &c).await {
                Ok(r) => {
                    if r.status() != StatusCode::UNAUTHORIZED {
                        return transform_upstream(r, theme, None, is_root).await;
                    }
                }
                Err(e) => {
                    return (StatusCode::BAD_GATEWAY, e).into_response();
                }
            }
        }
        if attempt < MAX_401_RETRIES {
            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            backoff_ms *= 2; // 200ms → 400ms → 800ms
        }
    }
    (StatusCode::UNAUTHORIZED, "harness auth required after retries").into_response()
}

/// 向上游 harness 转发一次请求，附上已 harvest 的会话 cookie（alpha.3+ 鉴权必需）。
/// 上游 Host 由 reqwest 按 URL 自动设为 `127.0.0.1:<agent_port>`，与 cookie 绑定的 authority 一致。
async fn forward_upstream(
    s: &Arc<ProxyState>,
    method: Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: Bytes,
    cookie: &str,
) -> Result<reqwest::Response, String> {
    let path_and_query = uri.path_and_query().map(|x| x.as_str()).unwrap_or("/");
    let upstream = format!("http://127.0.0.1:{}{}", s.agent_port, path_and_query);
    let client = reqwest::Client::new();
    let m = match method {
        Method::GET => reqwest::Method::GET,
        Method::POST => reqwest::Method::POST,
        Method::PUT => reqwest::Method::PUT,
        Method::DELETE => reqwest::Method::DELETE,
        Method::PATCH => reqwest::Method::PATCH,
        Method::HEAD => reqwest::Method::HEAD,
        Method::OPTIONS => reqwest::Method::OPTIONS,
        _ => reqwest::Method::GET,
    };
    let mut rb = client.request(m, &upstream);
    for (k, v) in headers.iter() {
        let kn = k.as_str();
        if kn.eq_ignore_ascii_case("host")
            || kn.eq_ignore_ascii_case("cookie")
            || kn.eq_ignore_ascii_case("origin")
        {
            continue;
        }
        rb = rb.header(kn, v);
    }
    rb = rb.header("cookie", cookie);
    rb = rb.body(body);
    match rb.send().await {
        Ok(resp) => Ok(resp),
        Err(e) => Err(format!("upstream error: {e}")),
    }
}

/// 把上游响应转回 axum Response，并对 / 根路径的 HTML 注入主题脚本。
async fn transform_upstream(
    resp: reqwest::Response,
    theme: &Option<String>,
    set_cookie: Option<&str>,
    is_root: bool,
) -> Response {
    let status = resp.status();

    // 收集完整字节（注入脚本需要知道 </head> 位置）
    // 注意：reqwest::Response::bytes() 会消费 resp（move），需先克隆 headers。
    let upstream_headers = resp.headers().clone();
    let raw = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("read upstream body failed: {e}"),
            )
                .into_response()
        }
    };
    // 关键修复（2026-09-04）：reqwest 默认 feature 不含 gzip/brotli/deflate，不会自动解压响应体。
    // 若上游 harness 对 root HTML 启用 Content-Encoding 压缩，raw 即为压缩字节流，
    // 下游对 <head> 的搜索会失败、注入脚本被追加到压缩流末尾，浏览器解压后丢弃尾部游离文本，
    // 导致注入的 localStorage.setItem 永不执行（症状：主题不保存）。此处先按 content-encoding 解压为明文。
    let content_encoding = upstream_headers
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let bytes: Vec<u8> = if content_encoding.contains("gzip") {
        decode_gzip(&raw).unwrap_or_else(|_| raw.to_vec())
    } else if content_encoding.contains("deflate") {
        decode_deflate(&raw).unwrap_or_else(|_| raw.to_vec())
    } else if content_encoding.contains("br") {
        decode_brotli(&raw).unwrap_or_else(|_| raw.to_vec())
    } else {
        raw.to_vec()
    };

    // 主题注入：对 / 根路径、且响应体看起来像 HTML 时，注入「上报脚本」（始终）+「初始主题设置」（仅当已保存主题非空）。
    // 上报脚本：拦截 localStorage.setItem 并轮询 dsh-angelina-themes.selection，
    //   一旦 SPA（angelina-themes 插件）改动主题即 POST 到 /__dsh_theme，由后端持久化到 AppConfig.ui.theme。
    // 目的：proxy 端口每次随机 → origin 变化 → localStorage 清空；靠后端记住主题，启动期注入还原。
    // 说明（2026-09-04）：这里对 Content-Type 做宽松匹配 + body 嗅探双兜底，是为了兜底
    //   harness 某些构建把 root 响应的 Content-Type 写成非 "text/html" 前缀的情况（防御性）。
    //   真正的根因见上方解压逻辑：reqwest 默认 feature 不含 gzip/brotli/deflate，不会自动解压，
    //   压缩响应体若不先解压再注入，脚本会被追加到压缩流末尾、浏览器解压后丢弃 → 永不执行。
    let body_is_html = {
        let ct = upstream_headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ct.starts_with("text/html") || ct.starts_with("application/xhtml") {
            true
        } else {
            let head: &[u8] = &bytes[..bytes.len().min(1024)];
            let low = String::from_utf8_lossy(head).to_ascii_lowercase();
            low.contains("<!doctype") || low.contains("<html") || low.contains("<head")
        }
    };
    let inject_script = if is_root && body_is_html {
        let theme_id = theme.as_ref().filter(|t| !t.is_empty());
        let set_part = match theme_id {
            Some(t) => format!("try{{localStorage.setItem(KEY,'{}')}}catch(e){{}}", t),
            None => String::new(),
        };
        let reporter = r#"<script>var KEY='dsh-angelina-themes.selection';function __dsh_report(v){try{fetch('/__dsh_theme',{method:'POST',body:''+v}).catch(function(){})}catch(e){}}var __dsh_s=Storage.prototype.setItem;Storage.prototype.setItem=function(k,v){__dsh_s.call(this,k,v);if(k===KEY)__dsh_report(v);};var __dsh_l=localStorage.getItem(KEY);setInterval(function(){var c=localStorage.getItem(KEY);if(c!==__dsh_l){__dsh_l=c;__dsh_report(c);}},1500);{set_part}</script>"#;
        Some(reporter.replace("{set_part}", &set_part))
    } else {
        None
    };

    // 诊断日志（写入 %TEMP%/dsh_proxy_inject.log）：确认注入是否真正触发、Content-Type 实际值，
    // 便于下次运行后核对根因（主题不保存 = 注入被跳过）。
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::temp_dir().join("dsh_proxy_inject.log"))
    {
        use std::io::Write;
        let ct = upstream_headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let _ = writeln!(
            f,
            "[inject] is_root={} body_is_html={} ce={:?} ct={:?} len={} injected={}",
            is_root,
            body_is_html,
            content_encoding,
            ct,
            bytes.len(),
            inject_script.is_some()
        );
    }

    let mut builder = Response::builder().status(status);
    for (k, v) in &upstream_headers {
        let kn = k.as_str();
        if kn.eq_ignore_ascii_case("content-length")
            || kn.eq_ignore_ascii_case("transfer-encoding")
            || kn.eq_ignore_ascii_case("connection")
            || kn.eq_ignore_ascii_case("content-encoding")
        {
            continue;
        }
        builder = builder.header(kn, v);
    }
    if let Some(c) = set_cookie {
        builder = builder.header("set-cookie", c);
    }

    let body = if let Some(script) = &inject_script {
        // 注入位置优先级：<head> 开标签之后（最早）> </head> 之前 > 末尾追加。
        // 根因：harness 客户端由 <head> 里的经典脚本（/plugins/??...client.js）同步加载并 mount
        //   angelina-themes 插件，插件 mount 时 bridge.restore() 即读 localStorage[KEY]。若还原脚本
        //   注入在 </head>（晚于该经典脚本），插件读到空 localStorage -> 用默认主题；随后代理脚本
        //   才写入保存值，但插件已挂载不再重读 -> 主题重启回默认，且插件触发的 theme/change 还会
        //   把默认 'angelina-light' 写回 config.ui.theme，造成「配置也被改回默认」。
        // 抢在 <head> 开标签之后注入，保证 localStorage[KEY] 在插件 mount/restore 之前已就位。
        let mut insert_at: Option<usize> = None;
        if let Some(h) = bytes.windows(5).position(|w| w == b"<head") {
            for (i, b) in bytes[h..bytes.len().min(h + 64)].iter().enumerate() {
                if *b == b'>' {
                    insert_at = Some(h + i + 1);
                    break;
                }
            }
        }
        if insert_at.is_none() {
            if let Some(pos) = bytes.windows(7).position(|w| w == b"</head>") {
                insert_at = Some(pos + 7);
            }
        }
        let modified = match insert_at {
            Some(at) => {
                let mut out = bytes[..at].to_vec();
                out.extend_from_slice(script.as_bytes());
                out.extend_from_slice(&bytes[at..]);
                out
            }
            None => {
                let mut out = bytes.to_vec();
                out.extend_from_slice(script.as_bytes());
                out
            }
        };
        Body::from(modified)
    } else {
        Body::from(bytes)
    };

    builder
        .body(body)
        .unwrap_or((StatusCode::INTERNAL_SERVER_ERROR, "build response failed").into_response())
}

/// 健康检查端点：返回 200 表明代理自身运行正常（不依赖上游 harness 状态）。
/// 可用于容器编排健康探针或人工诊断。
///
/// `ready` 字段（2026-09-14 新增）= 已 harvest 且未过期 —— 冷启动引导页据此判断
/// 「上游可服务」，然后 `location.replace` 回真正的界面。
/// 顺带修复：原实现的 cookie 值未经引号包裹，产出的是**非法 JSON**
/// （形如 `"cookie":ok( expired=false)`），任何 JSON 解析器都会失败。
///
/// **循环依赖修复（2026-09-14 实机复现）**：`ready` 取决于会话 cookie 是否已 harvest，
/// 而 harvest 只发生在 `handler` 的页面请求路径上。引导页却**只轮询本端点、不再请求页面**
/// ——于是「等一个只有它自己去请求页面才会变的标志」，**永远等不到**：
/// 实机表现为 `/__dsh_health` 连续 180s 返回 `"cookie":"absent"`，引导页永不跳转。
/// 因此本端点必须**自己驱动一次握手**（单次尝试，轮询开销恒定、秒级收敛）。
async fn health_handler(State(s): State<Arc<ProxyState>>) -> Response {
    ensure_cookie_with(&s, 1).await;
    let (has_cookie, expired) = {
        let c = s.agent_cookie.lock().unwrap();
        (c.is_some(), c.is_expired())
    };
    let ready = has_cookie && !expired;
    let cookie_status = match (has_cookie, expired) {
        (true, false) => "ok",
        (true, true) => "expired",
        (false, _) => "absent",
    };
    // has_agent_token：仅布尔值，便于判断「上游 launch token 是否已解析出来」，
    // 用于区分「上游还没起来」与「token 没解析到」（后者单靠握手重试永远好不了）。
    let has_agent_token = !live_agent_token(&s).is_empty();
    let resp = format!(
        "{{\"status\":\"ok\",\"ready\":{},\"agent_port\":{},\"cookie\":\"{}\",\
         \"agent_token\":{},\"proxy_uptime\":\"healthy\"}}",
        ready, s.agent_port, cookie_status, has_agent_token
    );
    (StatusCode::OK, resp).into_response()
}

/// 接收 SPA（angelina-themes 插件）上报的当前主题，持久化到 AppConfig.ui.theme（config.json）。
/// 浏览器在同源下带 dsh_token cookie，经 valid_token 校验后写入；与上游无关（不转发）。
async fn theme_handler(
    State(s): State<Arc<ProxyState>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !valid_token(&s, &uri, &headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let theme = String::from_utf8_lossy(&body).trim().to_string();
    let app = s.app.clone();
    let mut cfg = app.state::<AppState>().config.lock().unwrap().clone();
    if cfg.ui.theme != theme {
        cfg.ui.theme = theme.clone();
        let _ = config::save_config(&app, &cfg);
        *app.state::<AppState>().config.lock().unwrap() = cfg;
    }
    (StatusCode::OK, "ok").into_response()
}

/// WebSocket 隧道 handler：接受浏览器升级，连上游并双向透传帧。
async fn ws_handler(
    State(s): State<Arc<ProxyState>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    // token 校验（与 HTTP 同口径：cookie 或 query）
    if !valid_token(&s, &uri, &headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    // DNS 重绑定防护
    if let Some(origin) = headers.get("origin") {
        let o = origin.to_str().unwrap_or("");
        if !(o.is_empty()
            || o.starts_with("http://127.0.0.1")
            || o.starts_with("http://localhost")
            || o == "null")
        {
            return (StatusCode::FORBIDDEN, "forbidden origin").into_response();
        }
    }

    // 上游候选路径：浏览器请求的路径优先，其后依次回退到各代端点名。
    // 目的：harness 改端点名时，代理无需同步发版也能连上（详见文件头 2026-09-02 修复说明）。
    let requested = strip_proxy_token(uri.path_and_query().map(|x| x.as_str()).unwrap_or("/"));
    let query = match requested.find('?') {
        Some(pos) if pos + 1 < requested.len() => Some(requested[pos + 1..].to_string()),
        _ => None,
    };
    let mut candidates: Vec<String> = vec![requested.clone()];
    for route in WS_ROUTES.iter() {
        let cand = match &query {
            Some(q) => format!("{}?{}", route, q),
            None => (*route).to_string(),
        };
        if !candidates.contains(&cand) {
            candidates.push(cand);
        }
    }

    ws.on_upgrade(move |client_ws| async move {
        // alpha.3+ 强制鉴权：WS 升级同样是一次 HTTP 请求，需带上 harvest 的会话 cookie
        // （直连 sidecar 时 /api/remote.mux 无 cookie 会返回 401）。
        let cookie = wait_cookie(&s).await;
        if cookie.is_empty() {
            eprintln!("proxy ws: harness auth cookie unavailable, dropping upgrade");
            return;
        }

        // 逐个候选尝试上游升级，取首个成功者；全部失败则记录每个候选的失败原因。
        let mut connected: Option<UpstreamWs> = None;
        let mut last_err = String::new();
        for cand in &candidates {
            let upstream = format!("ws://127.0.0.1:{}{}", s.agent_port, cand);
            // 关键修复（2026-09-03）：必须用 into_client_request() 由 URL 构造请求，
            // 让 tungstenite 自动补齐握手头（Sec-WebSocket-Key / Upgrade / Connection /
            // Sec-WebSocket-Version）。手动 Request::builder() 不会补 Sec-WebSocket-Key，
            // 上游会回 "Missing sec-websocket-key" 而升级失败 -> 客户端 WS 1006。
            // Host 由 URL 自动设为 127.0.0.1:<agent_port>，与 cookie 绑定的 authority 一致。
            let mut req = match upstream.as_str().into_client_request() {
                Ok(r) => r,
                Err(e) => {
                    last_err.push_str(&format!("[{cand}] bad request: {e}; "));
                    continue;
                }
            };
            if let Ok(v) = HeaderValue::from_str(&cookie) {
                req.headers_mut().insert(COOKIE, v);
            }
            match tokio_tungstenite::connect_async(req).await {
                Ok((upstream_ws, _)) => {
                    if cand != &candidates[0] {
                        eprintln!("proxy ws: upstream path fell back to {cand}");
                    }
                    connected = Some(upstream_ws);
                    break;
                }
                Err(e) => last_err.push_str(&format!("[{cand}] {e}; ")),
            }
        }

        match connected {
            // 上游连不上：client_ws 出作用域自动关闭，浏览器会按自身重连逻辑重试。
            None => eprintln!("proxy ws upstream connect failed: {last_err}"),
            Some(upstream_ws) => pipe(client_ws, upstream_ws).await,
        }
    })
}

/// 双向透传：浏览器 <-> 上游，逐帧转发（含 Ping/Pong/Close）。
async fn pipe(client: AWebSocket, upstream: UpstreamWs) {
    let (mut cw, mut cr) = client.split();
    let (mut uw, mut ur) = upstream.split();

    let client_to_upstream = async {
        while let Some(msg) = cr.next().await {
            match msg {
                Ok(m) => {
                    if uw.send(a_to_t(m)).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    };
    let upstream_to_client = async {
        while let Some(msg) = ur.next().await {
            match msg {
                Ok(m) => {
                    if let Some(am) = t_to_a(m) {
                        if cw.send(am).await.is_err() {
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }
    };

    tokio::select! {
        _ = client_to_upstream => {
            let _ = uw.close().await;
        }
        _ = upstream_to_client => {
            let _ = cw.close().await;
        }
    }
}

/// axum WS 消息 -> tungstenite WS 消息（axum 0.7 与 tungstenite 0.21 载荷类型一致：
/// Text=String / Binary=Ping=Pong=Vec<u8>，直接透传；仅 Close 的 CloseFrame 类型不同）。
fn a_to_t(m: AMessage) -> TMessage {
    match m {
        AMessage::Text(t) => TMessage::Text(t),
        AMessage::Binary(b) => TMessage::Binary(b),
        AMessage::Ping(b) => TMessage::Ping(b),
        AMessage::Pong(b) => TMessage::Pong(b),
        AMessage::Close(_) => TMessage::Close(None),
    }
}

/// tungstenite WS 消息 -> axum WS 消息。
/// `Frame(_)` 是 tungstenite 0.21 独有的原始扩展帧，无法映射到 axum 的 `Message`，
/// 返回 `None` 由调用方跳过。
fn t_to_a(m: TMessage) -> Option<AMessage> {
    match m {
        TMessage::Text(t) => Some(AMessage::Text(t)),
        TMessage::Binary(b) => Some(AMessage::Binary(b)),
        TMessage::Ping(b) => Some(AMessage::Ping(b)),
        TMessage::Pong(b) => Some(AMessage::Pong(b)),
        TMessage::Close(_) => Some(AMessage::Close(None)),
        TMessage::Frame(_) => None,
    }
}

/// 从 path_and_query 中剔除代理握手 token `t=`（上游不需要，也不应收到）。
fn strip_proxy_token(pq: &str) -> String {
    match pq.find('?') {
        None => pq.to_string(),
        Some(pos) => {
            let (path, q) = pq.split_at(pos);
            let q = &q[1..];
            let kept: Vec<&str> = q
                .split('&')
                .filter(|p| p.split('=').next().unwrap_or("") != "t")
                .collect();
            if kept.is_empty() {
                path.to_string()
            } else {
                format!("{}?{}", path, kept.join("&"))
            }
        }
    }
}

fn extract_query(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        let mut it = pair.splitn(2, '=');
        let k = it.next().unwrap_or("");
        let v = it.next().unwrap_or("");
        if k == key {
            return Some(v.to_string());
        }
    }
    None
}

fn extract_cookie(headers: &HeaderMap, key: &str) -> Option<String> {
    let cookie = headers.get("cookie")?.to_str().ok()?;
    for part in cookie.split(';') {
        let mut it = part.trim().splitn(2, '=');
        let k = it.next().unwrap_or("");
        let v = it.next().unwrap_or("");
        if k == key {
            return Some(v.to_string());
        }
    }
    None
}

// —— 响应体解压（reqwest 默认 feature 不含压缩支持，需手动处理）——
fn decode_gzip(b: &[u8]) -> Result<Vec<u8>, std::io::Error> {
    let mut d = GzDecoder::new(b);
    let mut out = Vec::new();
    d.read_to_end(&mut out)?;
    Ok(out)
}

fn decode_deflate(b: &[u8]) -> Result<Vec<u8>, std::io::Error> {
    let mut d = ZlibDecoder::new(b);
    let mut out = Vec::new();
    d.read_to_end(&mut out)?;
    Ok(out)
}

fn decode_brotli(b: &[u8]) -> Result<Vec<u8>, std::io::Error> {
    let mut d = Decompressor::new(b, 4096);
    let mut out = Vec::new();
    d.read_to_end(&mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accept(v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::ACCEPT,
            HeaderValue::from_str(v).unwrap(),
        );
        h
    }

    /// `wants_html` 必须只对浏览器「导航到页面」的请求为真，
    /// 否则 SPA 的 fetch 请求失败时会被替换成引导页 HTML，反而弄坏界面。
    #[test]
    fn wants_html_only_for_browser_navigation() {
        assert!(wants_html(&accept(
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"
        )));
        assert!(wants_html(&accept("text/html")));
        assert!(!wants_html(&accept("application/json, text/plain, */*")));
        assert!(!wants_html(&accept("*/*")));
        assert!(
            !wants_html(&HeaderMap::new()),
            "缺 Accept 时按非导航处理，宁可回 502"
        );
    }

    /// 引导页必须：200 + HTML、回跳目标 = 原始 path+query（保住 ?t= token）、
    /// 轮询 /__dsh_health、并以 ready 作为接管判据。
    #[tokio::test]
    async fn boot_page_targets_original_request_and_polls_health() {
        let uri: Uri = "/?t=abcDEF123_-=".parse().unwrap();
        let resp = boot_page_response(&uri, "等待上游鉴权会话就绪…");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("content-type")
                .unwrap()
                .to_str()
                .unwrap(),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            resp.headers()
                .get("cache-control")
                .unwrap()
                .to_str()
                .unwrap(),
            "no-store"
        );
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            html.contains("var TARGET=\"/?t=abcDEF123_-=\""),
            "应回跳到原始 path+query"
        );
        assert!(html.contains("/__dsh_health"), "应轮询健康端点");
        assert!(html.contains("j.ready===true"), "应以 ready 作为接管判据");
        assert!(html.contains("等待上游鉴权会话就绪…"), "应显示失败原因");
        // 回归护栏（2026-09-14 实机死锁修复）：引导页必须在「轮询就绪标志」之外，
        // **再直接敲一次真实页面地址**。只靠标志会让引导页等一个「只有它自己去请求
        // 页面才会翻真」的位 → 永远等不到（实机 180s 全程 ready=false）。
        assert!(
            html.contains("fetch(TARGET, {method:'HEAD', cache:'no-store'})"),
            "引导页必须周期性 HEAD 真实地址（带 token），不能只依赖 /__dsh_health 标志"
        );
        assert!(
            html.contains("if(n%4===0){ nudge(); }"),
            "HEAD 探活应随轮询周期性触发"
        );
    }

    /// 回跳目标与原因文本都经 JSON 转义，避免引号/反斜杠破坏内联脚本。
    /// 同时验证：非 `/` 开头的 path 不会被当成回跳目标。
    #[tokio::test]
    async fn boot_page_escapes_inline_json() {
        let uri: Uri = "/".parse().unwrap();
        let resp = boot_page_response(&uri, "含\"引号\"与\\反斜杠");
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("var TARGET=\"/\""), "根路径回落为 /");
        assert!(
            html.contains("含\\\"引号\\\"与\\\\反斜杠"),
            "原因文本必须被 JSON 转义: {html}"
        );
    }

    /// `/__dsh_health` 必须是**合法 JSON** 且带 ready 字段
    /// （旧实现产出裸值 `"cookie":ok( expired=false)`，任何 JSON 解析器都会失败）。
    #[test]
    fn health_payload_is_valid_json_with_ready() {
        let empty = CookieCache::new();
        assert!(!empty.is_some(), "新建缓存应为空");

        let mut fresh = CookieCache::new();
        fresh.set("dsh-auth-xxx=yyy".into(), Instant::now());
        assert!(
            fresh.is_some() && !fresh.is_expired(),
            "刚写入的 cookie 应有效"
        );

        for cookie in [&empty, &fresh] {
            let ready = cookie.is_some() && !cookie.is_expired();
            let cookie_status = match (cookie.is_some(), cookie.is_expired()) {
                (true, false) => "ok",
                (true, true) => "expired",
                (false, _) => "absent",
            };
            // 与 health_handler 相同的拼装逻辑（handler 需要 AppHandle，单测无法构造）
            let has_agent_token = true;
            let payload = format!(
                "{{\"status\":\"ok\",\"ready\":{},\"agent_port\":{},\"cookie\":\"{}\",                 \"agent_token\":{},\"proxy_uptime\":\"healthy\"}}",
                ready, 3081, cookie_status, has_agent_token
            );
            let v: serde_json::Value =
                serde_json::from_str(&payload).expect("health 响应必须是合法 JSON");
            assert_eq!(v["ready"].as_bool().unwrap(), ready);
            assert_eq!(v["cookie"].as_str().unwrap(), cookie_status);
            // agent_token 用于区分「上游未就绪」与「token 未解析到」——后者再重试也好不了。
            assert_eq!(v["agent_token"].as_bool().unwrap(), has_agent_token);
        }

        // 回归护栏：旧格式（cookie 值未加引号）确实不是合法 JSON
        assert!(
            serde_json::from_str::<serde_json::Value>(
                r#"{"status":"ok","cookie":ok( expired=false)}"#
            )
            .is_err(),
            "旧格式应被判为非法 JSON —— 这正是本次修复点"
        );
    }
}
