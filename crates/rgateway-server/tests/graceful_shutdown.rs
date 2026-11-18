//! 优雅关闭端到端：慢在途请求（上游挂起延迟响应）在
//! `server.timeouts.graceful_shutdown` 窗口内完成后，网关才退出。
//!
//! 断言的生产代码：`bootstrap` 的 accept 循环收尾（停止 accept →
//! [`hyper_util::server::graceful::GracefulShutdown`] 等在途连接结束 →
//! 窗口耗尽才 abort）与 [`rgateway_server::bootstrap::RunningGateway::shutdown`]。
//! 时间断言只用「宽松上界 + 未拖满窗口」，真实时间（网络 I/O 用例）。

mod common;

use std::time::{Duration, Instant};

use common::*;

/// shutdown / 在途请求完成的宽松上界。
const AWAIT_BOUND: Duration = Duration::from_secs(6);

/// 收尾完成的观察上界：必须远小于 10s 的 graceful 窗口——
/// 用来区分「窗口内完成后退出」与「拖满窗口才 abort」。
const SHUTDOWN_COMPLETION_BOUND: Duration = Duration::from_secs(5);

/// 挂起在途请求在 graceful 窗口内完成后，shutdown() 才返回。
#[tokio::test]
async fn slow_inflight_request_completes_before_shutdown_returns() {
    let mut upstream = SlowStreamUpstream::start("late", "-ok").await;
    let dir = TestDir::new();
    let yaml = config_with_graceful(
        &route_entry(
            "api",
            "/api/**",
            &format!("http://127.0.0.1:{}", upstream.port),
            None,
            None,
        ),
        "10s",
    );
    let gw = start(dir.path(), &yaml).await;
    let addr = gw.addr;
    let client = tokio::spawn(async move {
        request(
            addr,
            "GET",
            "/api/slow",
            &[("Host", "h"), ("Connection", "close")],
            BodySpec::Empty,
        )
        .await
    });
    // 等到请求真正到达上游：在途状态成立（响应体挂起未完）
    upstream.wait_accepted().await;

    let started = Instant::now();
    let shutdown = tokio::spawn(async move { gw.shutdown().await });
    // 给 shutdown 信号一点时间抵达 accept 循环，再放行在途响应
    tokio::time::sleep(Duration::from_millis(200)).await;
    upstream.release();

    let resp = tokio::time::timeout(AWAIT_BOUND, client)
        .await
        .expect("in-flight request must complete within the graceful window")
        .expect("client task");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_text(), "late-ok");
    tokio::time::timeout(AWAIT_BOUND, shutdown)
        .await
        .expect("shutdown must return after the in-flight request completes")
        .expect("shutdown task");
    // 未拖满 10s 窗口：退出发生在在途请求完成之后、窗口耗尽之前
    assert!(
        started.elapsed() < SHUTDOWN_COMPLETION_BOUND,
        "shutdown dragged past the in-flight completion: {:?}",
        started.elapsed()
    );
}
