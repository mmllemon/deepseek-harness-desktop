//! 本地反向代理（受控 ingress 边界，§10.7 / §8.1 / §13.8 D5）。
//!
//! axum 监听随机 loopback 端口，校验运行期随机 token（cookie 握手 + query 兜底），
//! 拦截 DNS 重绑定，将请求转发给仅监听 loopback 的 `dsh` 后端。随机端口≠认证，
//! token 才是真实认证边界；dsh 裸端口无法关闭，反代仅加固本机任意进程直连面。

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{OriginalUri, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use futures_util::TryStreamExt;

pub struct ProxyState {
    pub agent_port: u16,
    pub token: String,
}

/// 启动反代，返回 (proxy_port, proxy_url)。proxy_url 含首次握手 token。
pub async fn start_proxy(agent_port: u16, token: String) -> Result<(u16, String), String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| e.to_string())?;
    let proxy_port = listener
        .local_addr()
        .map_err(|e| e.to_string())?
        .port();
    let state = Arc::new(ProxyState { agent_port, token });
    let app = Router::new().fallback(any(handler)).with_state(state);

    tauri::async_runtime::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("proxy server error: {e}");
        }
    });

    let proxy_url = format!("http://127.0.0.1:{}/?t={}", proxy_port, token);
    Ok((proxy_port, proxy_url))
}

async fn handler(
    State(s): State<Arc<ProxyState>>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // —— token 校验：cookie dsh_token 或首次 query t ——
    let from_query = extract_query(uri.query().unwrap_or(""), "t");
    let from_cookie = extract_cookie(&headers, "dsh_token");
    let provided = from_cookie.clone().or(from_query.clone());
    let valid = matches!(&provided, Some(t) if t == &s.token);
    if !valid {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }

    // 首次带 query token：下发 HttpOnly cookie，后续走 cookie（避免 token 落入 URL 历史/referrer）
    let set_cookie = if from_query.is_some() && from_cookie.is_none() {
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
