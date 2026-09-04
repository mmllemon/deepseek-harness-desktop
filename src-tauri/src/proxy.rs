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

use std::io::Read;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use brotli::Decompressor;
use flate2::read::{GzDecoder, ZlibDecoder};

use axum::body::{Body, Bytes};
use tauri::Manager;

use crate::config;
use crate::state::AppState;
use axum::extract::ws::{Message as AMessage, WebSocket as AWebSocket, WebSocketUpgrade};
use axum::extract::{OriginalUri, State};
use axum::http::{header::COOKIE, HeaderMap, HeaderValue, Method, Request, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as TMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

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
    pub agent_cookie: Arc<Mutex<Option<String>>>,
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
    let agent_cookie: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

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
            *agent_cookie.lock().unwrap() = Some(c);
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
    // /__dsh_theme 接收前端主题上报；其余回退到通用 HTTP handler。
    let mut router = Router::new().route("/__dsh_theme", any(theme_handler));
    for route in WS_ROUTES.iter() {
        router = router.route(*route, any(ws_handler));
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

/// 向上游 harness 发起一次 launch-token 交换，返回 `name=value` 形式的会话 cookie。
/// harness 在 `GET /?token=<launchToken>` 时返回 303 + Set-Cookie（HttpOnly, SameSite=Strict）。
/// 该 cookie 由 harness 用 `$DSH_HOME/.credentials.yaml` 中的密钥签名，桌面无法伪造，必须换取。
/// cookie 名 = `dsh-auth-` + base64url(sha256(Host))，因此请求 Host 必须与交换时一致（127.0.0.1:<port>）。
async fn handshake_cookie(agent_port: u16, agent_token: &str) -> Option<String> {
    if agent_token.is_empty() {
        return None;
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .ok()?;
    let url = format!("http://127.0.0.1:{}/?token={}", agent_port, agent_token);
    for _ in 0..20 {
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

/// 惰性握手：若尚无 cookie，则尝试换取一次（用于启动时交换失败的场景）。
/// 修复（2026-09-01）：token 一律从 AppState 读取「实时」值，而非 ProxyState 启动时的
/// 快照——stdout 解析可能在代理启动后才写入 agent_token，用快照会永远拿不到 cookie。
async fn ensure_cookie(s: &Arc<ProxyState>) {
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
    let has = s.agent_cookie.lock().unwrap().is_some();
    if !has && !tok.is_empty() {
        if let Some(c) = handshake_cookie(s.agent_port, &tok).await {
            *s.agent_cookie.lock().unwrap() = Some(c);
        }
    }
}

/// 等待会话 cookie 就绪：先惰性握手一次，再有界等待（最多约 3.2s）。
/// 首个请求（HTTP 或 WS）可能抢在 stdout token 解析完成前到达——TCP 回退路径
/// 先触发 on_ready 时 token 仍空——此处轮询等待 token 落盘后重试握手，
/// 避免一次性 502 / WS 1006 误伤首次加载。
async fn wait_cookie(s: &Arc<ProxyState>) -> String {
    ensure_cookie(s).await;
    for _ in 0..8 {
        let cookie = s.agent_cookie.lock().unwrap().clone().unwrap_or_default();
        if !cookie.is_empty() {
            return cookie;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
        ensure_cookie(s).await;
    }
    s.agent_cookie.lock().unwrap().clone().unwrap_or_default()
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
    let cookie = wait_cookie(&s).await;
    if cookie.is_empty() {
        return (StatusCode::BAD_GATEWAY, "harness auth cookie unavailable").into_response();
    }

    let is_root = uri.path() == "/";
    let theme = s.theme.clone();

    // 首次转发
    let resp =
        match forward_upstream(&s, method.clone(), &uri, &headers, body.clone(), &cookie).await {
            Ok(r) => r,
            Err(e) => return (StatusCode::BAD_GATEWAY, e).into_response(),
        };
    let status = resp.status();

    // 401 = 会话 cookie 过期或被拒：重新握手一次后重试（同样使用实时 token）
    if status == StatusCode::UNAUTHORIZED {
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
        if !tok.is_empty() {
            if let Some(c) = handshake_cookie(s.agent_port, &tok).await {
                *s.agent_cookie.lock().unwrap() = Some(c.clone());
                let retry = match forward_upstream(&s, method, &uri, &headers, body, &c).await {
                    Ok(r) => r,
                    Err(e) => return (StatusCode::BAD_GATEWAY, e).into_response(),
                };
                return transform_upstream(retry, &theme, set_cookie.as_deref(), is_root).await;
            }
        }
        return (StatusCode::UNAUTHORIZED, "harness auth required").into_response();
    }

    transform_upstream(resp, &theme, set_cookie.as_deref(), is_root).await
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
            for i in h..bytes.len().min(h + 64) {
                if bytes[i] == b'>' {
                    insert_at = Some(i + 1);
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
                .filter(|p| p.splitn(2, '=').next().unwrap_or("") != "t")
                .collect();
            if kept.is_empty() {
                path.to_string()
            } else {
                format!("{}?{}", path, kept.join("&"))
            }
        }
    }
}

fn extract_query<'a>(query: &'a str, key: &str) -> Option<String> {
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
