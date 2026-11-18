//! [`crate::dispatch`] 的测试：分发全流程——404 渲染矩阵、链上错误渲染
//! 矩阵、Content-Length 预检、body 流限长与错误分类、并发闸（含许可生命
//! 周期）、hint 覆写、`X-Request-ID` 治理与 `SECRET_DETAIL` 泄露检测。
//!
//! 路由与快照全部手工构造（`Route` / `CompiledConfig` 字段均 pub）：
//! 假链直接放进 `Route.chain`，不经过配置编译管线，保证被测对象只有
//! `Dispatcher::serve` 本身。测试替身与断言辅助在 `dispatch_tests_util`。

use std::sync::Arc;

use bytes::Bytes;
use http::StatusCode;
use tracing::instrument::WithSubscriber;

use rgateway_core::{empty_body, full_body, Protocol, ProtocolHint, RequestCtx};

#[path = "dispatch_tests_util.rs"]
mod util;
use self::util::*;

// 无路由命中：按探测协议渲染 NotFound（HTTP JSON / grpc-status 12 / gRPC-Web 帧），
// 且携带 X-Request-ID。列含义：(请求, 期望渲染)
#[tokio::test]
async fn not_found_renders_per_detected_protocol() {
    let dispatcher = dispatcher(
        1024,
        16,
        vec![route("r1", ProtocolHint::Auto, ChainBehavior::Ok200)],
    );
    let cases = [
        (
            http_request("/nope"),
            Expect::Http {
                status: 404,
                json_error: "not found",
            },
        ),
        (
            request_with(
                "POST",
                "/nope",
                http::Version::HTTP_2,
                Some("application/grpc"),
                None,
                empty_body(),
            ),
            Expect::Grpc {
                status: "12",
                message: "not found",
            },
        ),
        (
            request_with(
                "POST",
                "/nope",
                http::Version::HTTP_11,
                Some("application/grpc-web+proto"),
                None,
                empty_body(),
            ),
            Expect::GrpcWebFrame {
                status: 12,
                message: "not found",
            },
        ),
    ];
    for (i, (req, want)) in cases.into_iter().enumerate() {
        let resp = dispatcher.serve(req, None).await;
        assert_error_shape(resp, &want, &format!("404 case {i}")).await;
    }
}

// 链上错误（RateLimited）渲染矩阵：HTTP 429 JSON / gRPC grpc-status 8 /
// gRPC-Web 帧。列含义：(请求, 期望渲染)
#[tokio::test]
async fn chain_error_renders_per_final_protocol() {
    let dispatcher = dispatcher(
        1024,
        16,
        vec![route("r1", ProtocolHint::Auto, ChainBehavior::RateLimited)],
    );
    let cases = [
        (
            http_request("/api/x"),
            Expect::Http {
                status: 429,
                json_error: "rate limit exceeded",
            },
        ),
        (
            request_with(
                "POST",
                "/api/x",
                http::Version::HTTP_2,
                Some("application/grpc"),
                None,
                empty_body(),
            ),
            Expect::Grpc {
                status: "8",
                message: "rate limit exceeded",
            },
        ),
        (
            request_with(
                "POST",
                "/api/x",
                http::Version::HTTP_11,
                Some("application/grpc-web+proto"),
                None,
                empty_body(),
            ),
            Expect::GrpcWebFrame {
                status: 8,
                message: "rate limit exceeded",
            },
        ),
    ];
    for (i, (req, want)) in cases.into_iter().enumerate() {
        let resp = dispatcher.serve(req, None).await;
        assert_error_shape(resp, &want, &format!("chain error case {i}")).await;
    }
}

// Content-Length 预检：超过 max_request_body 直接 400，边界值放行进链。
#[tokio::test]
async fn content_length_precheck_rejects_oversized() {
    let dispatcher = dispatcher(
        64,
        16,
        vec![route("r1", ProtocolHint::Auto, ChainBehavior::EchoBody)],
    );
    // 列含义：(Content-Length 头值, 期望渲染) —— 均超过 64 字节上限
    let cases = [
        (
            "65",
            Expect::Http {
                status: 400,
                json_error: "invalid argument",
            },
        ),
        (
            "1000000",
            Expect::Http {
                status: 400,
                json_error: "invalid argument",
            },
        ),
    ];
    for (cl, want) in cases {
        let req = request_with(
            "POST",
            "/api/x",
            http::Version::HTTP_11,
            None,
            Some(cl),
            empty_body(),
        );
        let resp = dispatcher.serve(req, None).await;
        assert_error_shape(resp, &want, cl).await;
    }
    // 边界：刚好等于上限放行，链回显请求体
    let req = request_with(
        "POST",
        "/api/x",
        http::Version::HTTP_11,
        None,
        Some("10"),
        full_body(Bytes::from_static(b"0123456789")),
    );
    let resp = dispatcher.serve(req, None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_bytes(resp).await, b"0123456789");
}

// body 流限长：Content-Length 预检骗过（头写 1），实际体超限时
// Limited 触发 LengthLimitError，映射为 InvalidArgument 渲染 400。
#[tokio::test]
async fn body_stream_over_limit_maps_to_invalid_argument() {
    let dispatcher = dispatcher(
        64,
        16,
        vec![route("r1", ProtocolHint::Auto, ChainBehavior::EchoBody)],
    );
    let big = full_body(Bytes::from(vec![0u8; 100]));
    let req = request_with(
        "POST",
        "/api/x",
        http::Version::HTTP_11,
        None,
        Some("1"),
        big,
    );
    let resp = dispatcher.serve(req, None).await;
    let want = Expect::Http {
        status: 400,
        json_error: "invalid argument",
    };
    assert_error_shape(resp, &want, "mid-stream limit").await;
}

// 并发闸：max_concurrent_requests=1 时第一个请求持锁 pending，
// 第二个请求被 RateLimited 拒绝（429）。
#[tokio::test]
async fn concurrent_gate_rejects_when_saturated() {
    let dispatcher = dispatcher(
        1024,
        1,
        vec![route("r1", ProtocolHint::Auto, ChainBehavior::Pending)],
    );
    let first_dispatcher = Arc::clone(&dispatcher);
    let first =
        tokio::spawn(async move { first_dispatcher.serve(http_request("/api/x"), None).await });
    // 让第一个请求跑到链上 pending 处并持住唯一许可
    for _ in 0..3 {
        tokio::task::yield_now().await;
    }
    let resp = dispatcher.serve(http_request("/api/x"), None).await;
    let want = Expect::Http {
        status: 429,
        json_error: "rate limit exceeded",
    };
    assert_error_shape(resp, &want, "saturated").await;
    assert!(!first.is_finished(), "第一个请求应当仍在链上 pending");
}

// 并发闸许可跟随响应体生命周期：max=1 时首个响应体未耗尽前第二个请求必须
// 429（流式响应不得无界突破并发上限）；未耗尽直接 drop 或正常耗尽后放行。
// 两段分别覆盖许可的两种释放路径：drop 释放与耗尽即刻释放。
#[tokio::test]
async fn concurrency_permit_lives_until_response_body_finishes() {
    let want_429 = Expect::Http {
        status: 429,
        json_error: "rate limit exceeded",
    };
    // 段一：悬挂响应体（永不耗尽）持有许可
    let gate_hanging = dispatcher(
        1024,
        1,
        vec![route("r1", ProtocolHint::Auto, ChainBehavior::HangingBody)],
    );
    let resp1 = gate_hanging.serve(http_request("/api/x"), None).await;
    assert_eq!(resp1.status(), StatusCode::OK, "首个请求应拿到许可");
    let resp2 = gate_hanging.serve(http_request("/api/x"), None).await;
    // 先断状态码再耗响应体：实现有误时第二个响应体是悬挂体，
    // 直接 collect 会挂死而不是快速失败。
    assert_eq!(
        resp2.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "响应体未耗尽必须429"
    );
    assert_error_shape(resp2, &want_429, "响应体未耗尽必须429").await;
    drop(resp1); // 未耗尽直接 drop：许可必须随之释放
    let resp3 = gate_hanging.serve(http_request("/api/x"), None).await;
    assert_eq!(resp3.status(), StatusCode::OK, "drop 响应体后应放行");
    drop(resp3);
    // 段二：有限响应体正常耗尽 → 许可即刻释放 → 下一个放行
    let gate_finite = dispatcher(
        1024,
        1,
        vec![route("r1", ProtocolHint::Auto, ChainBehavior::Ok200)],
    );
    let resp = gate_finite.serve(http_request("/api/x"), None).await;
    assert_eq!(body_bytes(resp).await, b"pong", "耗尽首个响应体");
    let resp = gate_finite.serve(http_request("/api/x"), None).await;
    assert_eq!(resp.status(), StatusCode::OK, "耗尽响应体后应放行");
}

// limited_body 错误分类矩阵：客户端请求体错误绝不归 Unavailable（503 是
// 误导），统一归 InvalidArgument（400）；只有内嵌的 ProxyError::Gateway
// 原样透传（保持原错误种类与文案）。列含义：(标签, 请求体错误来源, 期望渲染)
#[tokio::test]
async fn request_body_errors_map_to_invalid_argument() {
    let cases = [
        (
            "upstream-variant",
            BodyFailure::Upstream,
            Expect::Http {
                status: 400,
                json_error: "invalid argument",
            },
        ),
        (
            "io-variant",
            BodyFailure::Io,
            Expect::Http {
                status: 400,
                json_error: "invalid argument",
            },
        ),
        (
            "embedded-gateway",
            BodyFailure::EmbeddedGateway,
            Expect::Http {
                status: 429,
                json_error: "rate limit exceeded",
            },
        ),
    ];
    for (label, failure, want) in cases {
        let dispatcher = dispatcher(
            1024,
            16,
            vec![route("r1", ProtocolHint::Auto, ChainBehavior::EchoBody)],
        );
        let req = request_with(
            "POST",
            "/api/x",
            http::Version::HTTP_11,
            None,
            None,
            failing_body(failure),
        );
        let resp = dispatcher.serve(req, None).await;
        assert_error_shape(resp, &want, label).await;
    }
}

// hint 覆写：渲染协议必须是 hint 覆写后的最终协议，而不是探测协议。
// 列含义：(路由 hint, 请求 content-type, 期望渲染)
#[tokio::test]
async fn protocol_hint_override_decides_error_rendering() {
    let cases = [
        (
            ProtocolHint::Fixed(Protocol::Http1),
            Some("application/grpc"),
            Expect::Http {
                status: 429,
                json_error: "rate limit exceeded",
            },
        ),
        (
            ProtocolHint::Fixed(Protocol::Grpc),
            None,
            Expect::Grpc {
                status: "8",
                message: "rate limit exceeded",
            },
        ),
    ];
    for (i, (hint, content_type, want)) in cases.into_iter().enumerate() {
        let dispatcher = dispatcher(
            1024,
            16,
            vec![route("r1", hint, ChainBehavior::RateLimited)],
        );
        let req = request_with(
            "POST",
            "/api/x",
            http::Version::HTTP_2,
            content_type,
            None,
            empty_body(),
        );
        let resp = dispatcher.serve(req, None).await;
        assert_error_shape(resp, &want, &format!("hint case {i}")).await;
    }
}

// 成功响应：透传链的响应体，且 X-Request-ID 覆写客户端传入值、为合法 uuid。
#[tokio::test]
async fn success_response_carries_request_id() {
    let dispatcher = dispatcher(
        1024,
        16,
        vec![route("r1", ProtocolHint::Auto, ChainBehavior::Ok200)],
    );
    let mut req = http_request("/api/x");
    req.headers_mut().insert(
        "x-request-id",
        http::HeaderValue::from_static("client-supplied"),
    );
    let resp = dispatcher.serve(req, None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let headers = resp.headers().clone();
    assert_eq!(body_bytes(resp).await, b"pong");
    let id = header_text(&headers, "x-request-id");
    assert_ne!(id, "client-supplied", "必须覆写为网关生成的关联 id");
    assert_request_id(&headers);
}

// 泄露检测红线：假链错误载荷里的 SECRET_DETAIL 不得出现在任何
// 客户端可见通道（响应头 / 响应体），HTTP 与 gRPC 家族全覆盖。
#[tokio::test]
async fn secret_detail_never_leaks_to_clients() {
    for behavior in [ChainBehavior::Unavailable, ChainBehavior::RateLimited] {
        let dispatcher = dispatcher(1024, 16, vec![route("r1", ProtocolHint::Auto, behavior)]);
        // 列含义：(请求) —— HTTP / gRPC / gRPC-Web 三条客户端可见通道
        let requests = [
            http_request("/api/x"),
            request_with(
                "POST",
                "/api/x",
                http::Version::HTTP_2,
                Some("application/grpc"),
                None,
                empty_body(),
            ),
            request_with(
                "POST",
                "/api/x",
                http::Version::HTTP_11,
                Some("application/grpc-web+proto"),
                None,
                empty_body(),
            ),
        ];
        for req in requests {
            let resp = dispatcher.serve(req, None).await;
            let headers = resp.headers().clone();
            let body = body_bytes(resp).await;
            assert_no_leak(&headers, &body);
        }
    }
}

// ---- 可观测性接线（访问日志 / 指标 / 并发 gauge）----

// 访问日志 JSON 行经 tracing "access" 目标输出：每请求恰好一行，
// schema 键齐全、关键字段落在请求事实上。
#[tokio::test]
async fn access_log_json_line_is_emitted() {
    let capture = CaptureLog::default();
    let dispatcher = dispatcher(
        1024,
        16,
        vec![route("r1", ProtocolHint::Auto, ChainBehavior::Ok200)],
    );
    let resp = dispatcher
        .serve(http_request("/api/x"), None)
        .with_subscriber(capture.clone())
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let lines = capture.lines();
    assert_eq!(lines.len(), 1, "每请求恰好一行: {lines:?}");
    let value: serde_json::Value = serde_json::from_str(&lines[0]).expect("json access line");
    assert_eq!(value["method"], "GET");
    assert_eq!(value["path"], "/api/x");
    assert_eq!(value["route_id"], "r1");
    assert_eq!(value["status"], 200);
    assert_eq!(value["protocol"], "http1");
    assert_eq!(value["error_kind"], serde_json::Value::Null);
    for key in ["ts", "request_id", "peer", "upstream", "duration_ms"] {
        assert!(value.get(key).is_some(), "缺键 {key}: {}", lines[0]);
    }
}

// 访问日志 text 格式（access_log_json=false）：同一事实以 logfmt 键值对输出。
#[tokio::test]
async fn access_log_text_line_when_json_disabled() {
    let capture = CaptureLog::default();
    let dispatcher = dispatcher_access(
        1024,
        16,
        vec![route("r1", ProtocolHint::Auto, ChainBehavior::Ok200)],
        true,
        false,
    );
    let resp = dispatcher
        .serve(http_request("/api/x"), None)
        .with_subscriber(capture.clone())
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let lines = capture.lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    for want in ["status=200", "route_id=r1", "method=GET"] {
        assert!(lines[0].contains(want), "缺 {want}: {}", lines[0]);
    }
}

// 访问日志开关：enabled=false 时一行都不发（含 404）。
#[tokio::test]
async fn access_log_disabled_emits_nothing() {
    let capture = CaptureLog::default();
    let dispatcher = dispatcher_access(
        1024,
        16,
        vec![route("r1", ProtocolHint::Auto, ChainBehavior::Ok200)],
        false,
        true,
    );
    for uri in ["/api/x", "/nope"] {
        let _ = dispatcher
            .serve(http_request(uri), None)
            .with_subscriber(capture.clone())
            .await;
    }
    assert!(capture.lines().is_empty(), "{:?}", capture.lines());
}

// 404 的访问日志：error_kind=not_found、route_id=null（不编造假路由）。
#[tokio::test]
async fn access_log_records_not_found_without_route() {
    let capture = CaptureLog::default();
    let dispatcher = dispatcher(
        1024,
        16,
        vec![route("r1", ProtocolHint::Auto, ChainBehavior::Ok200)],
    );
    let resp = dispatcher
        .serve(http_request("/nope"), None)
        .with_subscriber(capture.clone())
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let lines = capture.lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    let value: serde_json::Value = serde_json::from_str(&lines[0]).expect("json access line");
    assert_eq!(value["status"], 404);
    assert_eq!(value["error_kind"], "not_found");
    assert_eq!(value["route_id"], serde_json::Value::Null);
}

// 指标全流程（本地 recorder 隔离）：路由命中进 requests_total{route,method,status}，
// 未命中进 route_not_found_total，绝不给无路由请求编造 route 标签。
#[test]
fn serve_records_metrics_end_to_end() {
    let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    metrics::with_local_recorder(&recorder, || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let dispatcher = dispatcher(
                1024,
                16,
                vec![route("r1", ProtocolHint::Auto, ChainBehavior::Ok200)],
            );
            let resp = dispatcher.serve(http_request("/api/x"), None).await;
            body_bytes(resp).await;
            let resp = dispatcher.serve(http_request("/nope"), None).await;
            assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        });
    });
    let text = handle.render();
    assert!(text.contains("rgateway_requests_total"), "{text}");
    assert!(text.contains("route=\"r1\""), "{text}");
    assert!(text.contains("status=\"200\""), "{text}");
    assert!(text.contains("rgateway_route_not_found_total"), "{text}");
    assert!(
        !text.contains("route=\"-\""),
        "不得编造假 route 标签: {text}"
    );
}

// 并发 gauge 与响应体同生命周期：响应体未耗尽前 gauge 停在 1，
// drop 响应体立即归零（与 PermitBody 的许可释放同步）。
#[test]
fn active_request_gauge_lives_with_response_body() {
    let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    metrics::with_local_recorder(&recorder, || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let dispatcher = dispatcher(
                1024,
                16,
                vec![route("r1", ProtocolHint::Auto, ChainBehavior::HangingBody)],
            );
            let resp = dispatcher.serve(http_request("/api/x"), None).await;
            assert_eq!(gauge_value(&handle.render()), 1.0, "响应体悬挂时 gauge=1");
            drop(resp);
            assert_eq!(gauge_value(&handle.render()), 0.0, "drop 响应体后 gauge=0");
        });
    });
}

/// 从 Prometheus 文本取 `rgateway_active_requests` 当前值（缺失返回 NaN）。
fn gauge_value(text: &str) -> f64 {
    text.lines()
        .find(|line| line.starts_with("rgateway_active_requests"))
        .and_then(|line| line.rsplit(' ').next())
        .and_then(|token| token.parse::<f64>().ok())
        .unwrap_or(f64::NAN)
}

// hint 覆写重建 ctx：关联 id 与起算时刻必须保留（span / 访问日志 /
// X-Request-ID 同源，duration 覆盖探测与匹配阶段），只有协议被改写。
#[test]
fn rebuilt_ctx_keeps_request_id_and_start() {
    let peer: std::net::SocketAddr = "10.0.0.5:1234".parse().expect("addr");
    let ctx = RequestCtx::new(Protocol::Grpc, Some(peer));
    let rebuilt = super::rebuild_ctx(&ctx, Protocol::Http1, Some(peer));
    assert_eq!(rebuilt.request_id, ctx.request_id, "关联 id 不得漂移");
    assert_eq!(rebuilt.start, ctx.start, "起算时刻不得重置");
    assert_eq!(rebuilt.protocol, Protocol::Http1, "协议按最终协议改写");
    assert_eq!(rebuilt.peer_addr, Some(peer));
}
