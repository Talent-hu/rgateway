//! IpFilter 过滤器测试：allow/deny 决策矩阵、peer 缺失语义、
//! 参数构建表格、错误映射（PermissionDenied 双通道）。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use rgateway_core::{ErrorKind, StateRegistry};
use tower::ServiceExt;

use super::*;
use crate::test_support::{build_ctx, counting_ok, request, spec_yaml};

/// 参数构建表格：每行 = (结构化 spec YAML, 期望构建成功?, 失败消息应含)。
#[test]
fn build_param_table() {
    let cases: &[(&str, bool, &str)] = &[
        ("{ name: IpFilter, allow: [\"10.0.0.0/8\"] }", true, ""),
        ("{ name: IpFilter, deny: [\"192.0.2.1\"] }", true, ""),
        (
            "{ name: IpFilter, allow: [\"10.0.0.0/8\"], deny: [\"2001:db8::/32\"] }",
            true,
            "",
        ),
        // allow/deny 全空 → 拒绝
        ("{ name: IpFilter }", false, "allow or deny"),
        (
            "{ name: IpFilter, allow: [], deny: [] }",
            false,
            "allow or deny",
        ),
        // 非法条目 → 拒绝
        (
            "{ name: IpFilter, allow: [\"not-an-ip\"] }",
            false,
            "not-an-ip",
        ),
        ("{ name: IpFilter, deny: [\"999.1.1.1\"] }", false, "deny"),
    ];
    let states = StateRegistry::new();
    let fctx = build_ctx("r1", states.route_state("r1"));
    for (yaml, ok, err) in cases {
        let result = IpFilterFactory.build(&spec_yaml(yaml), &fctx);
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

/// helper：以给定 spec 构建 IpFilter 层，包住可计数 inner，返回 (链, 计数器)。
fn ip_filtered_chain(yaml: &str) -> (rgateway_core::BoxedService, Arc<AtomicUsize>) {
    let states = StateRegistry::new();
    let fctx = build_ctx("r1", states.route_state("r1"));
    let wrapper = IpFilterFactory
        .build(&spec_yaml(yaml), &fctx)
        .expect("build");
    let counter = Arc::new(AtomicUsize::new(0));
    (wrapper(counting_ok(Arc::clone(&counter))), counter)
}

fn peer(text: &str) -> Option<SocketAddr> {
    Some(text.parse().expect("static addr"))
}

/// 决策矩阵：每行 = (allow 条目, deny 条目, peer IP, 期望放行?)。
/// deny 命中优先于 allow；allow 非空未命中拒绝；peer 缺失时配了 allow
/// 则 fail-closed 拒绝、只配 deny 则放行。
#[tokio::test(start_paused = true)]
async fn decision_matrix() {
    // 列含义：(allow, deny, peer, want_ok)
    let cases: &[(Option<&str>, Option<&str>, Option<&str>, bool)] = &[
        // 只配 allow：命中放行，未命中拒绝
        (Some(r#"["10.0.0.0/8"]"#), None, Some("10.1.2.3:1234"), true),
        (
            Some(r#"["10.0.0.0/8"]"#),
            None,
            Some("192.0.2.1:1234"),
            false,
        ),
        // 只配 deny：命中拒绝，未命中放行
        (
            None,
            Some(r#"["192.0.2.1"]"#),
            Some("192.0.2.1:1234"),
            false,
        ),
        (None, Some(r#"["192.0.2.1"]"#), Some("10.1.2.3:1234"), true),
        // 两者同配：deny 优先（命中 deny 即拒，即便也在 allow 里）
        (
            Some(r#"["10.0.0.0/8"]"#),
            Some(r#"["10.1.1.1"]"#),
            Some("10.1.1.1:1234"),
            false,
        ),
        (
            Some(r#"["10.0.0.0/8"]"#),
            Some(r#"["10.1.1.1"]"#),
            Some("10.2.2.2:1234"),
            true,
        ),
        // 在 allow 之外且不在 deny：allow 非空 → 拒
        (
            Some(r#"["10.0.0.0/8"]"#),
            Some(r#"["192.0.2.1"]"#),
            Some("172.16.0.1:1234"),
            false,
        ),
        // peer 缺失：配了 allow → fail-closed 拒；只配 deny → 放行
        (Some(r#"["10.0.0.0/8"]"#), None, None, false),
        (None, Some(r#"["192.0.2.1"]"#), None, true),
        // 裸 IP 条目（自动归一为单主机网段）
        (Some(r#"["10.1.2.3"]"#), None, Some("10.1.2.3:1234"), true),
        (Some(r#"["10.1.2.3"]"#), None, Some("10.1.2.4:1234"), false),
        // IPv6
        (
            Some(r#"["2001:db8::/32"]"#),
            None,
            Some("[2001:db8::1]:1234"),
            true,
        ),
    ];
    for (allow, deny, addr, want_ok) in cases {
        let mut parts = vec![String::from("{ name: IpFilter")];
        if let Some(a) = allow {
            parts.push(format!(", allow: {a}"));
        }
        if let Some(d) = deny {
            parts.push(format!(", deny: {d}"));
        }
        parts.push(String::from(" }"));
        let yaml = parts.concat();
        let (chain, counter) = ip_filtered_chain(&yaml);
        let req = request(addr.and_then(peer), None);
        let result = chain.oneshot(req).await;
        if *want_ok {
            assert!(result.is_ok(), "{yaml} peer={addr:?} 应放行，却 {result:?}");
            assert_eq!(counter.load(Ordering::SeqCst), 1, "{yaml}: inner 应被调用");
        } else {
            let err = result.expect_err("{yaml} 应拒绝");
            assert_eq!(
                err.kind(),
                ErrorKind::PermissionDenied,
                "{yaml} peer={addr:?}"
            );
            assert_eq!(
                counter.load(Ordering::SeqCst),
                0,
                "{yaml}: inner 不应被调用"
            );
        }
    }
}

/// 错误双通道：403/gRPC 7 映射正确，客户端文案泛化、内部细节（IP 等）
/// 不出现在 public_message 里。
#[tokio::test(start_paused = true)]
async fn error_mapping_keeps_details_private() {
    let (chain, _) = ip_filtered_chain(r#"{ name: IpFilter, deny: ["192.0.2.1"] }"#);
    let req = request(peer("192.0.2.1:1234"), None);
    let err = chain.oneshot(req).await.expect_err("deny 命中应拒");
    assert_eq!(err.http_status(), 403);
    assert_eq!(err.grpc_status(), 7);
    let public = err.public_message();
    assert_eq!(public, "permission denied");
    // 内部细节（被拒的 IP）不得进入客户端可见文案
    assert!(!public.contains("192.0.2.1"), "{public}");
}
