//! [`crate::route`] 的测试：协议分类、RouteUri 解析、RequestCtx 的
//! target 覆写 / auth set-once / 截止期数学，以及 `Send + Sync` 契约。

use super::*;

// 验证 Route 与 RequestCtx（含 Arc<Route>）满足 Send + Sync 契约，
// 可安全跨 tokio 工作线程共享。
#[test]
fn route_and_ctx_are_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Route>();
    assert_send_sync::<RequestCtx>();
    assert_send_sync::<Arc<Route>>();
}

// 验证协议家族分类：Grpc/GrpcWeb/GrpcWebText 属于 gRPC 家族，
// GrpcWeb/GrpcWebText 属于 gRPC-Web，Http2/WebSocket 不属于。
#[test]
fn protocol_family_classification() {
    assert!(Protocol::Grpc.is_grpc_family());
    assert!(Protocol::GrpcWeb.is_grpc_family());
    assert!(Protocol::GrpcWebText.is_grpc_web());
    assert!(!Protocol::Http2.is_grpc_family());
    assert!(!Protocol::WebSocket.is_grpc_web());
}

// 验证指标标签值稳定不变（标签值变了会撕裂监控面板的连续性）。
#[test]
fn protocol_labels_are_stable() {
    assert_eq!(Protocol::GrpcWeb.as_label(), "grpc-web");
    assert_eq!(Protocol::Http1.as_label(), "http1");
}

// 验证 lb:// 形式的 URI 解析为 RouteUri::Lb，服务名保持原样。
#[test]
fn route_uri_parses_lb() {
    let uri = RouteUri::parse("lb://user-svc").unwrap();
    assert_eq!(uri, RouteUri::Lb(Arc::from("user-svc")));
}

// 验证 http(s) 绝对 URI 解析为 RouteUri::Direct，authority 保留。
#[test]
fn route_uri_parses_direct() {
    let uri = RouteUri::parse("http://127.0.0.1:50051").unwrap();
    match uri {
        RouteUri::Direct(u) => assert_eq!(u.authority().unwrap().as_str(), "127.0.0.1:50051"),
        _ => panic!("expected direct uri"),
    }
}

// 验证非法 URI 全部被拒：空 lb 名、lb 名带路径、非 http(s) scheme、
// 缺 authority、无法解析的字符串。
#[test]
fn route_uri_rejects_bad_values() {
    assert!(RouteUri::parse("lb://").is_err());
    assert!(RouteUri::parse("lb://name/with/path").is_err());
    assert!(RouteUri::parse("ftp://host").is_err());
    assert!(RouteUri::parse("http://").is_err());
    assert!(RouteUri::parse("not a uri").is_err());
}

// 验证 target 的可覆写语义：重试重新选实例时 set_target 必须覆盖旧选择。
#[test]
fn request_ctx_target_can_be_replaced_for_retries() {
    let ctx = RequestCtx::new(Protocol::Http2, None);
    assert!(ctx.target().is_none());
    let t1 = UpstreamTarget {
        instance_id: Arc::from("10.0.0.1:50051"),
        addr: "10.0.0.1:50051".parse().unwrap(),
    };
    let t2 = UpstreamTarget {
        instance_id: Arc::from("10.0.0.2:50051"),
        addr: "10.0.0.2:50051".parse().unwrap(),
    };
    ctx.set_target(t1.clone());
    assert_eq!(ctx.target(), Some(t1));
    // 重试重新选目标必须覆写上一次的选择
    ctx.set_target(t2.clone());
    assert_eq!(ctx.target(), Some(t2));
}

// 验证 auth 的 set-once 语义：首次写入成功，第二次写入返回 Err（重复认证）。
#[test]
fn request_ctx_auth_set_once() {
    let ctx = RequestCtx::new(Protocol::Http1, None);
    assert!(ctx.auth().is_none());
    let auth = AuthContext {
        subject: "alice".into(),
        kind: AuthKind::Jwt,
        claims: HashMap::new(),
    };
    assert!(ctx.set_auth(auth).is_ok());
    assert_eq!(ctx.auth().unwrap().subject, "alice");
    let again = AuthContext {
        subject: "bob".into(),
        kind: AuthKind::ApiKey,
        claims: HashMap::new(),
    };
    assert!(ctx.set_auth(again).is_err());
}

// 验证截止期数学：remaining 从预算值开始、随 tokio 时钟推进递减——
// start_paused + advance 让时间推进完全确定（tokio::time::Instant 的价值）。
#[tokio::test(start_paused = true)]
async fn request_ctx_deadline_math() {
    let mut ctx = RequestCtx::new(Protocol::Http2, None);
    assert!(ctx.remaining().is_none());
    ctx.set_deadline_after(Duration::from_secs(30));
    assert_eq!(ctx.remaining(), Some(Duration::from_secs(30)));
    tokio::time::advance(Duration::from_secs(10)).await;
    assert_eq!(ctx.remaining(), Some(Duration::from_secs(20)));
}
