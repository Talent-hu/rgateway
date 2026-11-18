//! 并发闸（体阶段）端到端：`max_concurrent_requests=1` 时，首请求的
//! 响应体慢速流式**未耗尽**期间，第二个请求撞并发闸得 429；响应体耗尽
//! （许可随体释放）后新请求恢复 200。
//!
//! 断言的生产代码：`dispatch` 的并发闸（`try_acquire_owned` 超限快速失败，
//! 不排队）与许可生命周期——许可挂在响应体包装层上，响应体耗尽或被
//! drop 才释放；若响应头一返回就释放，流式响应会在「已计入并发」的
//! 假象下无界突破上限。真实时间（网络 I/O 用例）。

mod common;

use std::time::Duration;

use common::*;

/// 在途任务收尾的宽松上界。
const AWAIT_BOUND: Duration = Duration::from_secs(5);

/// 首请求响应体未耗尽期间，第二个请求 429；体耗尽后恢复。
#[tokio::test]
async fn streaming_response_holds_permit_until_body_exhausted() {
    let mut upstream = SlowStreamUpstream::start("hello ", "world").await;
    let dir = TestDir::new();
    let yaml = config_with_concurrency(
        &route_entry(
            "api",
            "/api/**",
            &format!("http://127.0.0.1:{}", upstream.port),
            None,
            None,
        ),
        1,
    );
    let gw = start(dir.path(), &yaml).await;

    // 首请求：响应头 + "hello " 先到，"world" 挂起——响应体未耗尽
    let addr = gw.addr;
    let first = tokio::spawn(async move {
        request(addr, "GET", "/api/slow", &[("Host", "h")], BodySpec::Empty).await
    });
    upstream.wait_accepted().await;

    // 首请求许可仍被响应体持有：第二个请求撞并发闸 → 429 快速失败
    let resp = request(
        gw.addr,
        "GET",
        "/api/other",
        &[("Host", "h")],
        BodySpec::Empty,
    )
    .await;
    assert_eq!(resp.status, 429);
    assert_eq!(resp.body_text(), "{\"error\":\"rate limit exceeded\"}");

    // 放行首请求响应体：完整收妥（hello world）
    upstream.release();
    let resp = tokio::time::timeout(AWAIT_BOUND, first)
        .await
        .expect("first response must finish")
        .expect("client task");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_text(), "hello world");

    // 许可随响应体耗尽释放：新请求恢复 200
    let resp = request(
        gw.addr,
        "GET",
        "/api/again",
        &[("Host", "h")],
        BodySpec::Empty,
    )
    .await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_text(), "hello world");
}
