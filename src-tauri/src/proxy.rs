//! 本地反向代理（受控 ingress 边界，§10.7 / §8.1 / §13.8 D5）。
//!
//! axum 监听随机 loopback 端口，校验运行期随机 token（cookie 握手 + query 兜底），
//! 拦截 DNS 重绑定，将请求转发给仅监听 loopback 的 `dsh` 后端。随机端口≠认证，
//! token 才是真实认证边界；dsh 裸端口无法关闭，反代仅加固本机任意进程直连面。
//!
//! 关键修复（2026-08-19）：此前代理仅用 reqwest 做 HTTP 转发，对
//! `/api/events.mux`、`/api/events.host` 这类 WebSocket 升级请求无能为力——
//! reqwest 无法隧道化双向 WS，导致 SPA 的实时事件流（用户消息 / AI 回复）
//! 被缓冲或丢弃，UI 要等 ~20s 才显示。现对这两个端点做真正的 WS 隧道：
//! 用 axum 接受浏览器升级，用 tokio-tungstenite 连上游，双向透传帧。

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::ws::{Message as AMessage, WebSocket as AWebSocket, WebSocketUpgrade};
use axum::extract::{OriginalUri, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use futures_util::{SinkExt, StreamExt, TryStreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message as TMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

pub struct ProxyState {
    pub agent_port: u16,
    pub token: String,
}

/// 上游 WebSocket 流类型（明文字节，无 TLS）。
type UpstreamWs = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// 启动反代，返回 (proxy_port, proxy_url)。proxy_url 含首次握手 token。
pub async fn start_proxy(agent_port: u16, token: String) -> Result<(u16, String), String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| e.to_string())?;
    let proxy_port = listener
        .local_addr()
        .map_err(|e| e.to_string())?
        .port();
    let state = Arc::new(ProxyState {
        agent_port,
        token: token.clone(),
    });
    // 仅 WS 端点走专用隧道 handler；其余全部回退到通用 HTTP handler。
    let app = Router::new()
        .route("/api/events.mux", any(ws_handler))
        .route("/api/events.host", any(ws_handler))
        .fallback(any(handler))
        .with_state(state);

    tauri::async_runtime::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("proxy server error: {e}");
        }
    });

    let proxy_url = format!("http://127.0.0.1:{}/?t={}", proxy_port, token);
    Ok((proxy_port, proxy_url))
}

/// 校验请求携带的 token（cookie `dsh_token` 或首次 query `t`）。
fn valid_token(s: &ProxyState, uri: &OriginalUri, headers: &HeaderMap) -> bool {
    let from_query = extract_query(uri.0.query().unwrap_or(""), "t");
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

    let path_and_query = uri
        .path_and_query()
        .map(|x| x.as_str())
        .unwrap_or("/");
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
    rb = rb.body(body);

    match rb.send().await {
        Ok(resp) => {
            let status = resp.status();
            let mut builder = Response::builder().status(status);
            for (k, v) in resp.headers() {
                let kn = k.as_str();
                if kn.eq_ignore_ascii_case("content-length")
                    || kn.eq_ignore_ascii_case("transfer-encoding")
                    || kn.eq_ignore_ascii_case("connection")
                {
                    continue;
                }
                builder = builder.header(kn, v);
            }
            if let Some(c) = &set_cookie {
                builder = builder.header("set-cookie", c);
            }
            let stream = resp
                .bytes_stream()
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e));
            match builder.body(Body::from_stream(stream)) {
                Ok(r) => r,
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
            }
        }
        Err(e) => (StatusCode::BAD_GATEWAY, format!("upstream error: {e}")).into_response(),
    }
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

    // 上游 WS 地址：剥离代理专用 token `t`，避免泄露给裸端口。
    let pq = strip_proxy_token(uri.path_and_query().map(|x| x.as_str()).unwrap_or("/"));
    let upstream = format!("ws://127.0.0.1:{}{}", s.agent_port, pq);

    ws.on_upgrade(move |client_ws| async move {
        match tokio_tungstenite::connect_async(upstream.as_str()).await {
            Ok((upstream_ws, _)) => pipe(client_ws, upstream_ws).await,
            // 上游连不上：client_ws 出作用域自动关闭，浏览器会按自身重连逻辑重试。
            Err(e) => {
                eprintln!("proxy ws upstream connect failed: {e}");
            }
        }
    })
}

/// 双向透传：浏览器 <-> 上游，逐帧转发（含 Ping/Pong/Close）。
async fn pipe(mut client: AWebSocket, mut upstream: UpstreamWs) {
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
                    if cw.send(t_to_a(m)).await.is_err() {
                        break;
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

/// axum WS 消息 -> tungstenite WS 消息（类型在 axum 0.7 / tungstenite 0.21 下一致）。
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
fn t_to_a(m: TMessage) -> AMessage {
    match m {
        TMessage::Text(t) => AMessage::Text(t),
        TMessage::Binary(b) => AMessage::Binary(b),
        TMessage::Ping(b) => AMessage::Ping(b),
        TMessage::Pong(b) => AMessage::Pong(b),
        TMessage::Close(_) => AMessage::Close(None),
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
