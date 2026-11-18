//! 管理面 gRPC 端到端：生产 `bootstrap::start` 起真实监听（REST 与 gRPC
//! 同端口按 content-type 复用），tonic 客户端经共享端口调用
//! Health / Snapshot / ResetBreakers；`mgmt.grpc: false` 的配置下 gRPC 调用
//! 必须得到 UNIMPLEMENTED，而同端口的 REST 半边不受影响。
//!
//! 服务级语义（排序、幂等、统计）在 rgateway-mgmt 的 `grpc_tests.rs` 覆盖，
//! 本文件只验证接线：真实 accept 循环 + h2c + content-type 分流 + 开关。

mod common;

use common::*;
use rgateway_mgmt::pb::{
    mgmt_service_client::MgmtServiceClient, HealthRequest, ResetBreakersRequest, SnapshotRequest,
};
use tonic::Code;

/// 带 `mgmt:` 段的配置：数据面/管理面都 `127.0.0.1:0`，gRPC 开关按参数，
/// 单路由（摘要断言用）。
fn config_with_mgmt_grpc(grpc: bool) -> String {
    format!(
        "server:\n  listen: \"127.0.0.1:0\"\nmgmt:\n  listen: \"127.0.0.1:0\"\n  grpc: {grpc}\nroutes:\n{}",
        route_entry("r1", "/x/**", "http://127.0.0.1:9000", None, None)
    )
}

/// 连接管理面共享端口的 tonic 客户端（h2c 先验）。
async fn connect(mgmt_addr: std::net::SocketAddr) -> MgmtServiceClient<tonic::transport::Channel> {
    MgmtServiceClient::connect(format!("http://{mgmt_addr}"))
        .await
        .expect("grpc client must connect to mgmt port")
}

/// 同端口 REST 半边必须照常服务（content-type 分流回归）。
async fn assert_rest_still_served(mgmt_addr: std::net::SocketAddr) {
    let resp = request(
        mgmt_addr,
        "GET",
        "/health",
        &[("Host", "h")],
        BodySpec::Empty,
    )
    .await;
    assert_eq!(resp.status, 200, "REST /health 必须不受 gRPC 分流影响");
}

// 验证：gRPC 开启时 Health / Snapshot / ResetBreakers 经共享端口可用，
// 输出语义与 REST 契约一致（摘要形态、reset 幂等 0）。
#[tokio::test]
async fn grpc_health_snapshot_reset_via_shared_port() {
    let dir = TestDir::new();
    let gw = start(dir.path(), &config_with_mgmt_grpc(true)).await;
    let mut client = connect(gw.mgmt_addr).await;

    let health = client
        .health(HealthRequest {})
        .await
        .expect("health rpc")
        .into_inner();
    assert_eq!(health.status, "ok");

    let snap = client
        .snapshot(SnapshotRequest {})
        .await
        .expect("snapshot rpc")
        .into_inner();
    assert_eq!(snap.version, 1);
    assert_eq!(snap.failed_reloads, 0);
    assert_eq!(snap.listen, "127.0.0.1:0");
    assert_eq!(snap.route_count, 1);
    assert_eq!(snap.routes.len(), 1);
    assert_eq!(snap.routes[0].id, "r1");
    assert_eq!(snap.routes[0].order, 0);
    assert_eq!(snap.routes[0].uri, "http://127.0.0.1:9000/");
    assert_eq!(snap.routes[0].protocol, "auto");

    let reset = client
        .reset_breakers(ResetBreakersRequest {
            route_id: "missing".to_string(),
            instance_id: None,
        })
        .await
        .expect("reset rpc")
        .into_inner();
    assert_eq!(reset.reset, 0, "未知路由必须幂等返回 0");

    // 错误路径 wire 级断言：空 route_id 全链路（分流 → tonic 路由 → 状态码）
    // 必须是 INVALID_ARGUMENT，而不只是服务层拒绝。
    let err = client
        .reset_breakers(ResetBreakersRequest {
            route_id: String::new(),
            instance_id: None,
        })
        .await
        .expect_err("空 route_id 必须 INVALID_ARGUMENT");
    assert_eq!(err.code(), Code::InvalidArgument);

    assert_rest_still_served(gw.mgmt_addr).await;
    gw.shutdown().await;
}

// 验证：`mgmt.grpc: false` 时 gRPC 调用拿到 UNIMPLEMENTED，REST 照常。
#[tokio::test]
async fn grpc_unimplemented_when_disabled() {
    let dir = TestDir::new();
    let gw = start(dir.path(), &config_with_mgmt_grpc(false)).await;
    let mut client = connect(gw.mgmt_addr).await;

    let err = client
        .health(HealthRequest {})
        .await
        .expect_err("gRPC 关闭时必须 UNIMPLEMENTED");
    assert_eq!(err.code(), Code::Unimplemented);

    assert_rest_still_served(gw.mgmt_addr).await;
    gw.shutdown().await;
}
