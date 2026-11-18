//! 数据面与管理面的 accept 循环和每连接服务任务（bootstrap 的网络侧）。
//!
//! 从 `bootstrap` 拆出：装配序列留在 bootstrap，纯网络调度（accept →
//! auto Builder → 优雅关闭 → 每连接服务）收在本文件。两个循环的关闭语义
//! 一致：`watch` 信号（含 sender drop）→ 停止 accept → 等在途连接在
//! graceful 窗口内结束 → 超时 abort 剩余连接任务。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use http::header::CONTENT_TYPE;
use http::{Request, Response};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use hyper_util::server::graceful::{GracefulShutdown, Watcher};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;

use rgateway_config::compile;
use rgateway_core::boxed_body;
use rgateway_mgmt::grpc::GrpcBody;
use rgateway_mgmt::MgmtState;

use crate::Dispatcher;

/// accept 失败的退避时长（瞬态错误如 fd 耗尽：退避后重试，不退出服务）。
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);

/// 把 `server.http2` 的三个调优值应用到连接 builder。
///
/// 配置声明了这些值就必须生效（静默忽略配置项会让操作者误以为流控已
/// 调优）；`usize → u32` 饱和转换，超大配置值封顶到 `u32::MAX` 而不是
/// 截断错位。HTTP/1.1 侧无对应配置项，保持 hyper 默认。
fn apply_http2_settings(
    builder: &mut auto::Builder<TokioExecutor>,
    server: &compile::ServerSettings,
) {
    builder
        .http2()
        .max_concurrent_streams(server.max_concurrent_streams)
        .initial_stream_window_size(h2_window(server.initial_stream_window_size))
        .initial_connection_window_size(h2_window(server.initial_connection_window_size));
}

/// 窗口大小的 `usize → u32` 饱和转换（配置值超 `u32::MAX` 时封顶）。
pub(super) fn h2_window(size: usize) -> u32 {
    u32::try_from(size).unwrap_or(u32::MAX)
}

/// 数据面 accept 循环：每条连接先做 TLS 握手（如启用），再跑 auto Builder
/// （HTTP/1.1 与 HTTP/2）+ [`Dispatcher`]；收到关闭信号后停止 accept，等
/// 在途连接结束（至多 `server.graceful_shutdown` 窗口），超时 abort 剩余
/// 连接任务。
pub(super) async fn accept_loop(
    listener: TcpListener,
    dispatcher: Arc<Dispatcher>,
    mut shutdown: watch::Receiver<bool>,
    server: compile::ServerSettings,
    tls: Option<TlsAcceptor>,
) {
    let mut builder = auto::Builder::new(TokioExecutor::new());
    apply_http2_settings(&mut builder, &server);
    let graceful_window = server.graceful_shutdown;
    // TLS 握手超时复用 `server.timeouts.connect`（不另设配置字段）。
    let handshake_timeout = server.connect_timeout;
    let graceful = GracefulShutdown::new();
    let mut conns: Vec<JoinHandle<()>> = Vec::new();
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(pair) => pair,
                    Err(err) => {
                        tracing::warn!("accept failed: {err}");
                        tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                        continue;
                    }
                };
                conns.retain(|handle| !handle.is_finished());
                conns.push(spawn_conn(
                    &builder,
                    &graceful,
                    Arc::clone(&dispatcher),
                    stream,
                    peer,
                    tls.clone(),
                    handshake_timeout,
                ));
            }
        }
    }
    drop(listener);
    if tokio::time::timeout(graceful_window, graceful.shutdown())
        .await
        .is_err()
    {
        tracing::warn!(
            "graceful shutdown window {graceful_window:?} elapsed, aborting {} connection(s)",
            conns.len()
        );
        for handle in &conns {
            handle.abort();
        }
    }
}

/// 管理面 accept 循环：auto Builder 同时服务 HTTP/1.1 与 HTTP/2（gRPC 复用
/// 需要 h2），关闭语义与数据面 [`accept_loop`] 一致。
pub(super) async fn mgmt_loop(
    listener: TcpListener,
    state: Arc<MgmtState>,
    mut shutdown: watch::Receiver<bool>,
    graceful_window: Duration,
) {
    let builder = auto::Builder::new(TokioExecutor::new());
    let graceful = GracefulShutdown::new();
    let mut conns: Vec<JoinHandle<()>> = Vec::new();
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(pair) => pair,
                    Err(err) => {
                        tracing::warn!("mgmt accept failed: {err}");
                        tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                        continue;
                    }
                };
                conns.retain(|handle| !handle.is_finished());
                conns.push(spawn_mgmt_conn(&builder, &graceful, Arc::clone(&state), stream, peer));
            }
        }
    }
    drop(listener);
    if tokio::time::timeout(graceful_window, graceful.shutdown())
        .await
        .is_err()
    {
        tracing::warn!(
            "mgmt graceful window {graceful_window:?} elapsed, aborting {} connection(s)",
            conns.len()
        );
        for handle in &conns {
            handle.abort();
        }
    }
}

/// 每条管理面连接的服务任务（[`mgmt_loop`] 用）。
fn spawn_mgmt_conn(
    builder: &auto::Builder<TokioExecutor>,
    graceful: &GracefulShutdown,
    state: Arc<MgmtState>,
    stream: TcpStream,
    peer: SocketAddr,
) -> JoinHandle<()> {
    let service = service_fn(move |req: Request<Incoming>| {
        let state = Arc::clone(&state);
        async move { Ok::<_, std::convert::Infallible>(mgmt_service(state, req).await) }
    });
    let conn = builder.serve_connection_with_upgrades(TokioIo::new(stream), service);
    let watched = graceful.watch(conn.into_owned());
    tokio::spawn(async move {
        if let Err(err) = watched.await {
            tracing::debug!(%peer, "mgmt connection ended with error: {err}");
        }
    })
}

/// 管理面请求分发：`application/grpc` 家族走 gRPC 通道，其余走 REST——
/// REST 与 gRPC 在同一端口按 content-type 复用（见 rgateway-mgmt 模块文档）。
/// 响应 body 统一折算为 [`GrpcBody`]（tonic 的装箱 body），REST 侧只是把
/// JSON 载荷包进同一种 body，hyper 连接层只面对一种响应体类型。
async fn mgmt_service(state: Arc<MgmtState>, req: Request<Incoming>) -> Response<GrpcBody> {
    if is_grpc(req.headers()) {
        return rgateway_mgmt::grpc::handle(state, req).await;
    }
    rgateway_mgmt::handle(state, req).await.map(GrpcBody::new)
}

/// gRPC 请求判定：`application/grpc`（可带 `+codec` 后缀或 `;` 参数）。
///
/// gRPC-Web（`application/grpc-web*`）是**数据面**协议，不进管理面复用
/// 之列——因此判定必须是精确形态而非裸前缀：`starts_with("application/grpc")`
/// 会把 `application/grpc-web` 也吞进 gRPC 通道。
pub(super) fn is_grpc(headers: &http::HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|content_type| {
            let ct = content_type.trim();
            ct == "application/grpc"
                || ct.starts_with("application/grpc+")
                || ct.starts_with("application/grpc;")
        })
}

/// 每连接的服务任务：可选 TLS 握手（失败/超时丢弃连接），随后 hyper
/// 自动协商 HTTP/1.1 与 HTTP/2；连接被 `GracefulShutdown` 监视。
///
/// `UpgradeableConnection` 借用了 `builder`，必须 `into_owned()` 后才能
/// `tokio::spawn`（官方 `examples/server_graceful.rs` 的模式）。TLS 分支
/// 先握手再建连接，因此移交的是 [`Watcher`]（自带订阅）而非
/// `GracefulShutdown` 引用——握手期间的连接任务同样受 abort 兜底保护。
fn spawn_conn(
    builder: &auto::Builder<TokioExecutor>,
    graceful: &GracefulShutdown,
    dispatcher: Arc<Dispatcher>,
    stream: TcpStream,
    peer: SocketAddr,
    tls: Option<TlsAcceptor>,
    handshake_timeout: Duration,
) -> JoinHandle<()> {
    let builder = builder.clone();
    let watcher = graceful.watcher();
    tokio::spawn(async move {
        match tls {
            None => serve_conn(builder, watcher, dispatcher, stream, peer).await,
            Some(acceptor) => {
                let handshake = tokio::time::timeout(handshake_timeout, acceptor.accept(stream));
                match handshake.await {
                    Ok(Ok(tls_stream)) => {
                        serve_conn(builder, watcher, dispatcher, tls_stream, peer).await
                    }
                    Ok(Err(err)) => tracing::warn!(%peer, "TLS handshake failed: {err}"),
                    Err(_) => {
                        tracing::warn!(%peer, "TLS handshake timed out after {handshake_timeout:?}");
                    }
                }
            }
        }
    })
}

/// 在给定 IO（明文 TCP 或 TLS 流）上服务到连接结束：auto Builder 协商
/// HTTP/1.1 与 HTTP/2，连接由 [`Watcher`] 纳入优雅关闭。
async fn serve_conn<T>(
    builder: auto::Builder<TokioExecutor>,
    watcher: Watcher,
    dispatcher: Arc<Dispatcher>,
    stream: T,
    peer: SocketAddr,
) where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let service = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
        let dispatcher = Arc::clone(&dispatcher);
        async move {
            let (parts, body) = req.into_parts();
            let req = hyper::Request::from_parts(parts, boxed_body(body));
            Ok::<_, std::convert::Infallible>(dispatcher.serve(req, Some(peer)).await)
        }
    });
    let conn = builder.serve_connection_with_upgrades(TokioIo::new(stream), service);
    let watched = watcher.watch(conn.into_owned());
    if let Err(err) = watched.await {
        tracing::debug!(%peer, "connection ended with error: {err}");
    }
}
