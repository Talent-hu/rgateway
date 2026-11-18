//! 可编程测试上游：hyper HTTP/1.1 `serve_connection` + `service_fn`。
//!
//! 每个用例按需起自己的上游实例（`127.0.0.1:0`），行为由 [`Behavior`]
//! 决定：回显请求头（代理头断言）、回显请求体、返回自身端口（负载均衡
//! 分布断言）、固定状态码、延迟响应（超时断言）、命中计数（闸门用例断言
//! 「上游未被触达 / 在途成立」）。另有裸 TCP 的 [`SlowStreamUpstream`]：
//! 响应体前段先到、后段挂起——制造「响应体未耗尽」窗口。

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, Notify};
use tokio::task::JoinHandle;

/// 上游回应行为。
#[derive(Clone)]
pub enum Behavior {
    /// 200 + JSON `{method, path, headers}`：把收到的请求头原样回显给断言。
    EchoHeaders,
    /// 200 + 请求体字节回显。
    EchoBody,
    /// 200 + 自身端口文本（负载均衡分布断言用）。
    SelfIdentify,
    /// 固定状态码 + 固定文本体。
    Fixed { status: u16, body: String },
    /// 先睡眠 `millis` 毫秒再回 200 + 文本体（超时断言用）。
    Sleep { millis: u64, body: String },
    /// 命中计数 + 延迟响应：请求一到达就 `hits` 加一（观察「上游未被
    /// 触达 / 请求在途」），睡 `delay_millis` 毫秒后回 200 + `body`。
    Counting {
        hits: Arc<AtomicUsize>,
        delay_millis: u64,
        body: String,
    },
}

/// 一个在跑的测试上游；drop 时停止 accept 循环。
pub struct Upstream {
    /// 监听地址。
    pub addr: SocketAddr,
    /// 监听端口（配置 discovery / 直连 uri 用）。
    pub port: u16,
    accept: JoinHandle<()>,
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

/// 起一个测试上游，返回句柄（含实际端口）。
pub async fn start(behavior: Behavior) -> Upstream {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let addr = listener.local_addr().expect("upstream local addr");
    let accept = tokio::spawn(accept_loop(listener, behavior, addr.port()));
    Upstream {
        addr,
        port: addr.port(),
        accept,
    }
}

/// accept 循环：每条连接一个独立 hyper HTTP/1.1 服务任务。
async fn accept_loop(listener: TcpListener, behavior: Behavior, port: u16) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            break;
        };
        let behavior = behavior.clone();
        let service = service_fn(move |req| handle(req, behavior.clone(), port));
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let _ = http1::Builder::new().serve_connection(io, service).await;
        });
    }
}

/// 按行为回应一次请求。
async fn handle(
    req: Request<hyper::body::Incoming>,
    behavior: Behavior,
    port: u16,
) -> Result<Response<Full<Bytes>>, Infallible> {
    Ok(match behavior {
        Behavior::EchoHeaders => echo_headers(&req),
        Behavior::EchoBody => echo_body(req).await,
        Behavior::SelfIdentify => text(200, &port.to_string()),
        Behavior::Fixed { status, body } => text(status, &body),
        Behavior::Sleep { millis, body } => {
            tokio::time::sleep(std::time::Duration::from_millis(millis)).await;
            text(200, &body)
        }
        Behavior::Counting {
            hits,
            delay_millis,
            body,
        } => {
            // 到达即计数（在途/触达观察点），再按需延迟。
            hits.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(delay_millis)).await;
            text(200, &body)
        }
    })
}

/// 回显请求头：`headers` 对象把小写头名映射到值（多值用 `", "` 连接）。
fn echo_headers(req: &Request<hyper::body::Incoming>) -> Response<Full<Bytes>> {
    let mut seen: Vec<(String, String)> = Vec::new();
    for (name, value) in req.headers() {
        let text = value.to_str().unwrap_or("<non-utf8>").to_string();
        match seen.iter_mut().find(|(n, _)| n == name.as_str()) {
            Some((_, existing)) => {
                existing.push_str(", ");
                existing.push_str(&text);
            }
            None => seen.push((name.as_str().to_string(), text)),
        }
    }
    let headers: serde_json::Map<String, serde_json::Value> = seen
        .into_iter()
        .map(|(n, v)| (n, serde_json::Value::String(v)))
        .collect();
    let payload = serde_json::json!({
        "method": req.method().as_str(),
        "path": req.uri().path_and_query().map(|pq| pq.as_str().to_string()).unwrap_or_default(),
        "headers": headers,
    });
    json(200, &payload)
}

/// 收齐请求体后原样回显。
async fn echo_body(req: Request<hyper::body::Incoming>) -> Response<Full<Bytes>> {
    match req.into_body().collect().await {
        Ok(collected) => raw(200, collected.to_bytes()),
        Err(_) => text(500, "upstream failed to read body"),
    }
}

/// JSON 响应（content-type: application/json）。
fn json(status: u16, payload: &serde_json::Value) -> Response<Full<Bytes>> {
    let mut resp = raw(status, Bytes::from(payload.to_string()));
    resp.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    resp
}

/// 文本响应（content-type: text/plain）。
fn text(status: u16, body: &str) -> Response<Full<Bytes>> {
    let mut resp = raw(status, Bytes::from(body.to_string()));
    resp.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("text/plain"),
    );
    resp
}

/// 无 content-type 的裸响应。
fn raw(status: u16, body: Bytes) -> Response<Full<Bytes>> {
    let mut resp = Response::new(Full::new(body));
    *resp.status_mut() = http::StatusCode::from_u16(status).expect("valid status");
    resp
}

/// 慢速流式上游（裸 TCP）：**首条**连接读到请求后回写响应头 + `first`
/// 字节 body，挂起直到 [`Self::release`]，再补完 `rest` 字节 body；
/// 后续连接直接回完整响应（`first + rest`）。
///
/// 为什么裸 TCP 而不是 hyper：要精确控制 body 的分段到达时刻——
/// 并发闸用例需要「响应体未耗尽」的确定性窗口，hyper 的整装响应体
/// （[`Full`]）做不到分段放行。响应带 `connection: close`（单请求单连接），
/// 上游客户端的连接池不会复用，首条连接的挂起不会波及后续请求。
pub struct SlowStreamUpstream {
    /// 监听地址。
    pub addr: SocketAddr,
    /// 监听端口（配置路由 uri 用）。
    pub port: u16,
    /// 首条请求到达通知（一次性）：`wait_accepted` 后在途状态成立。
    accepted: Option<oneshot::Receiver<()>>,
    /// 放行首条连接剩余 body 的通知。
    release: Arc<Notify>,
    accept: JoinHandle<()>,
}

impl SlowStreamUpstream {
    /// 起一个慢速上游：首条连接的 `first` 先到、`rest` 等
    /// [`Self::release`] 后补完；响应以 Content-Length 分框，
    /// 总长 = `first.len() + rest.len()`。
    pub async fn start(first: &str, rest: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind slow upstream");
        let addr = listener.local_addr().expect("slow upstream local addr");
        let (accepted_tx, accepted_rx) = oneshot::channel();
        let release = Arc::new(Notify::new());
        let first = first.to_string();
        let rest = rest.to_string();
        let accept = tokio::spawn(slow_accept_loop(
            listener,
            first,
            rest,
            Arc::clone(&release),
            accepted_tx,
        ));
        Self {
            addr,
            port: addr.port(),
            accepted: Some(accepted_rx),
            release,
            accept,
        }
    }

    /// 等到首条请求真正到达（「在途」状态成立）。
    pub async fn wait_accepted(&mut self) {
        if let Some(rx) = self.accepted.take() {
            rx.await.expect("slow upstream must accept");
        }
    }

    /// 放行首条连接挂起的剩余 body（响应体从此可被耗尽）。
    pub fn release(&self) {
        self.release.notify_one();
    }
}

impl Drop for SlowStreamUpstream {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

/// 慢速上游的 accept 循环：首条连接挂起等 `release`，其余连接立即回完整响应。
async fn slow_accept_loop(
    listener: TcpListener,
    first: String,
    rest: String,
    release: Arc<Notify>,
    accepted_tx: oneshot::Sender<()>,
) {
    let mut accepted_tx = Some(accepted_tx);
    let first_conn = AtomicBool::new(true);
    while let Ok((mut stream, _)) = listener.accept().await {
        let is_first = first_conn.swap(false, Ordering::SeqCst);
        let first = first.clone();
        let rest = rest.clone();
        let release = Arc::clone(&release);
        let tx = if is_first { accepted_tx.take() } else { None };
        tokio::spawn(async move {
            // 读到请求即视为到达（GET 无体，读到首段即够）。
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;
            if let Some(tx) = tx {
                let _ = tx.send(());
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                first.len() + rest.len()
            );
            let _ = stream.write_all(head.as_bytes()).await;
            let _ = stream.write_all(first.as_bytes()).await;
            let _ = stream.flush().await;
            if is_first {
                release.notified().await;
            }
            let _ = stream.write_all(rest.as_bytes()).await;
            let _ = stream.flush().await;
        });
    }
}
