//! ApiKeyAuth 过滤器测试：参数构建表格、头提取与常数时间匹配、
//! AuthContext 写入、双认证、错误映射（Unauthenticated 双通道）。

use std::sync::Arc;

use rgateway_core::{
    empty_body, AuthKind, ErrorKind, Protocol, ProxyBody, RequestCtx, StateRegistry,
};
use tower::ServiceExt;

use super::*;
use crate::test_support::{build_ctx, counting_ok, spec_yaml};

/// 参数构建表格：每行 = (结构化 spec YAML, 期望构建成功?, 失败消息应含)。
#[test]
fn build_param_table() {
    let cases: &[(&str, bool, &str)] = &[
        ("{ name: ApiKeyAuth, keys: { secret-1: alice } }", true, ""),
        (
            "{ name: ApiKeyAuth, header: x-api-key, keys: { s: alice } }",
            true,
            "",
        ),
        (
            "{ name: ApiKeyAuth, header: Authorization, keys: { s: alice, t: bob } }",
            true,
            "",
        ),
        // keys 缺失 / 空 map 拒绝
        ("{ name: ApiKeyAuth }", false, "keys"),
        ("{ name: ApiKeyAuth, keys: {} }", false, "keys"),
        // subject 空 / 非字符串拒绝
        (
            "{ name: ApiKeyAuth, keys: { secret-1: \"\" } }",
            false,
            "subject",
        ),
        // header 非法头名拒绝
        (
            "{ name: ApiKeyAuth, header: \"bad header\", keys: { s: alice } }",
            false,
            "header",
        ),
    ];
    let states = StateRegistry::new();
    let fctx = build_ctx("r1", states.route_state("r1"));
    for (yaml, ok, err) in cases {
        let result = ApiKeyAuthFactory.build(&spec_yaml(yaml), &fctx);
        match result {
            Ok(_) => assert!(ok, "{yaml}: build must fail"),
            Err(e) => {
                assert!(!ok, "{yaml}: build must succeed, got {e}");
                assert!(
                    e.to_string().contains(err),
                    "{yaml}: error {e} must mention {err:?}"
                );
            }
        }
    }
}

/// helper：以给定 spec 构建 ApiKeyAuth 层，包住可计数 inner；
/// 返回 (链, 计数器, 请求, ctx 句柄)——ctx 在请求发出前捕获，供断言
/// AuthContext 写入结果。
fn api_key_chain(
    yaml: &str,
) -> (
    rgateway_core::BoxedService,
    Arc<std::sync::atomic::AtomicUsize>,
    http::Request<ProxyBody>,
    Arc<RequestCtx>,
) {
    let states = StateRegistry::new();
    let fctx = build_ctx("r1", states.route_state("r1"));
    let wrapper = ApiKeyAuthFactory
        .build(&spec_yaml(yaml), &fctx)
        .expect("build");
    let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let chain = wrapper(counting_ok(Arc::clone(&counter)));
    let ctx = Arc::new(RequestCtx::new(Protocol::Http1, None));
    let req = http::Request::new(empty_body());
    let req = {
        let mut req = req;
        req.extensions_mut().insert(Arc::clone(&ctx));
        req
    };
    (chain, counter, req, ctx)
}

/// 命中/未命中的行为矩阵：正确密钥放行并写入 AuthContext，
/// 错误/缺失密钥 401 拒绝且不触达 inner。
#[tokio::test(start_paused = true)]
async fn key_lookup_and_auth_context() {
    // 命中：subject 写入 AuthContext
    let (chain, counter, mut req, ctx) =
        api_key_chain(r#"{ name: ApiKeyAuth, keys: { secret-1: alice, secret-2: bob } }"#);
    req.headers_mut()
        .insert("x-api-key", http::HeaderValue::from_static("secret-2"));
    chain.oneshot(req).await.expect("valid key must pass");
    assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    let auth = ctx.auth().expect("auth must be set");
    assert_eq!(auth.subject, "bob");
    assert_eq!(auth.kind, AuthKind::ApiKey);

    // 错误密钥 → 401，inner 不触达
    let (chain, counter, mut req, _ctx) =
        api_key_chain(r#"{ name: ApiKeyAuth, keys: { secret-1: alice } }"#);
    req.headers_mut()
        .insert("x-api-key", http::HeaderValue::from_static("wrong-key"));
    let err = chain.oneshot(req).await.expect_err("wrong key must fail");
    assert_eq!(err.kind(), ErrorKind::Unauthenticated);
    assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 0);

    // 缺失头 → 401
    let (chain, counter, req, _ctx) =
        api_key_chain(r#"{ name: ApiKeyAuth, keys: { secret-1: alice } }"#);
    let err = chain
        .oneshot(req)
        .await
        .expect_err("missing header must fail");
    assert_eq!(err.kind(), ErrorKind::Unauthenticated);
    assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 0);
}

/// 自定义 header 名生效；头名之外的头不被误认。
#[tokio::test(start_paused = true)]
async fn custom_header_name() {
    let (chain, _counter, mut req, _ctx) =
        api_key_chain(r#"{ name: ApiKeyAuth, header: x-tenant-key, keys: { t-1: tenant-a } }"#);
    // 默认 x-api-key 头里放正确密钥也不算数
    req.headers_mut()
        .insert("x-api-key", http::HeaderValue::from_static("t-1"));
    let err = chain
        .clone()
        .oneshot(req)
        .await
        .expect_err("default header must not count");
    assert_eq!(err.kind(), ErrorKind::Unauthenticated);

    // 自定义头命中
    let (chain, _counter, mut req, ctx) =
        api_key_chain(r#"{ name: ApiKeyAuth, header: x-tenant-key, keys: { t-1: tenant-a } }"#);
    req.headers_mut()
        .insert("x-tenant-key", http::HeaderValue::from_static("t-1"));
    chain.oneshot(req).await.expect("custom header must pass");
    assert_eq!(ctx.auth().expect("auth set").subject, "tenant-a");
}

/// 错误双通道：401 / gRPC 16，客户端文案泛化、密钥值不得出现在
/// public_message 里。
#[tokio::test(start_paused = true)]
async fn error_mapping_keeps_secrets_private() {
    let (chain, _counter, mut req, _ctx) =
        api_key_chain(r#"{ name: ApiKeyAuth, keys: { super-secret-key-42: alice } }"#);
    req.headers_mut().insert(
        "x-api-key",
        http::HeaderValue::from_static("super-secret-key-42-typo"),
    );
    let err = chain.oneshot(req).await.expect_err("must fail");
    assert_eq!(err.http_status(), 401);
    assert_eq!(err.grpc_status(), 16);
    let public = err.public_message();
    assert_eq!(public, "unauthenticated");
    assert!(!public.contains("super-secret-key-42"), "{public}");
}
