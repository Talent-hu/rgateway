//! 热重载端到端：文件改写 → notify 监听 → 防抖 reload → 新快照生效；
//! 坏配置失败保旧（旧快照继续服务）。
//!
//! 断言的生产代码：`watcher`（父目录监听 + 防抖 + 溢出标记）、
//! `snapshot::reload`（校验/编译成功才换快照）、`Dispatcher` 逐请求读
//! 最新快照。装配走生产 `bootstrap::start`（watcher 恒挂）。
//!
//! 快照版本 / 失败计数的断言不可用：`bootstrap::start` 只返回
//! `RunningGateway`（仅暴露监听地址），`SnapshotStore` 不可达——版本语义
//! 改为行为断言（新路由生效 / 旧快照继续服务），失败重载用「越过防抖窗口
//! 的等待」代替 `failed_reloads()` 轮询。优雅关闭用例见
//! `graceful_shutdown.rs`。

mod common;

use std::time::{Duration, Instant};

use common::*;

/// 轮询上限：重载生效的观察窗口（防抖 300ms + 编译 + 调度余量）。
const POLL_DEADLINE: Duration = Duration::from_secs(5);

/// 轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// 失败重载的沉淀等待：防抖 300ms 的数倍余量，保证坏配置确实已被
/// 尝试重载过（无 `failed_reloads()` 可轮询，见模块文档）。
const FAILED_RELOAD_SETTLE: Duration = Duration::from_secs(1);

/// 两路由配置（old + new 各指向一个上游端口）。
fn two_route_config(up_old_port: u16, up_new_port: u16) -> String {
    config_with_routes(&format!(
        "{}{}",
        route_entry(
            "old",
            "/old/**",
            &format!("http://127.0.0.1:{up_old_port}"),
            None,
            None
        ),
        route_entry(
            "new",
            "/new/**",
            &format!("http://127.0.0.1:{up_new_port}"),
            None,
            None
        )
    ))
}

/// 轮询直到路径返回 200 且 body 与期望一致（至多 [`POLL_DEADLINE`]）。
async fn wait_route_served(addr: std::net::SocketAddr, path: &str, want_body: &str) {
    let deadline = Instant::now() + POLL_DEADLINE;
    loop {
        let resp = request(addr, "GET", path, &[("Host", "h")], BodySpec::Empty).await;
        if resp.status == 200 {
            assert_eq!(
                resp.body_text(),
                want_body,
                "{path} must hit expected upstream"
            );
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{path} not served within {POLL_DEADLINE:?} (last status {})",
            resp.status
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// 加路由热重载：新路由开始服务，旧路由不受影响。
#[tokio::test]
async fn added_route_becomes_served_and_old_route_survives() {
    let up_old = start_upstream(Behavior::SelfIdentify).await;
    let up_new = start_upstream(Behavior::SelfIdentify).await;
    let dir = TestDir::new();
    let v1 = config_with_routes(&route_entry(
        "old",
        "/old/**",
        &format!("http://127.0.0.1:{}", up_old.port),
        None,
        None,
    ));
    let gw = start(dir.path(), &v1).await;

    // 起步：只有 old 路由可用
    let resp = request(gw.addr, "GET", "/old/x", &[("Host", "h")], BodySpec::Empty).await;
    assert_eq!(resp.body_text(), up_old.port.to_string());

    // 改写配置：追加 new 路由（old 保留），轮询直到新路由生效
    std::fs::write(&gw.config_path, two_route_config(up_old.port, up_new.port))
        .expect("rewrite config");
    wait_route_served(gw.addr, "/new/x", &up_new.port.to_string()).await;

    // 旧路由仍可访问（重载不清空旧路由）
    let resp = request(gw.addr, "GET", "/old/x", &[("Host", "h")], BodySpec::Empty).await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_text(), up_old.port.to_string());
}

/// 写坏配置（非法 YAML）：失败保旧——旧快照继续服务，未配置路径仍 404，
/// 没有半新半旧的中间态。
#[tokio::test]
async fn invalid_config_keeps_previous_snapshot() {
    let up_old = start_upstream(Behavior::SelfIdentify).await;
    let dir = TestDir::new();
    let v1 = config_with_routes(&route_entry(
        "old",
        "/old/**",
        &format!("http://127.0.0.1:{}", up_old.port),
        None,
        None,
    ));
    let gw = start(dir.path(), &v1).await;
    let resp = request(gw.addr, "GET", "/old/x", &[("Host", "h")], BodySpec::Empty).await;
    assert_eq!(resp.status, 200);

    // 非法 YAML（未闭合的流式序列）
    std::fs::write(&gw.config_path, "foo: [bar").expect("write bad config");

    // 等过防抖窗口，保证「失败的 reload 确实发生过」而非没轮到
    // （`failed_reloads()` 计数不可达，见模块文档）。
    tokio::time::sleep(FAILED_RELOAD_SETTLE).await;

    // 旧路由照常 200；未配置的路径仍是 404（没有半新半旧的中间态）
    let resp = request(gw.addr, "GET", "/old/x", &[("Host", "h")], BodySpec::Empty).await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_text(), up_old.port.to_string());
    let resp = request(gw.addr, "GET", "/gone/x", &[("Host", "h")], BodySpec::Empty).await;
    assert_eq!(resp.status, 404);
}
