//! server 设置解析的表驱动测试：compact 时长/大小的解析结果、默认值，
//! 以及坏输入的错误位置必须精确到 `server.*` 字段。

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rgateway_core::StateRegistry;

use crate::error::ConfigError;
use crate::spec::FilterRegistry;

use super::util::*;

/// 验证：`server:` 段的 compact 时长/大小解析与零配置默认值（合法表），
/// 以及各类坏输入的错误位置精确到出错的 `server.*` 字段（非法表）。
#[test]
fn server_settings_parse_table() {
    // (yaml, 期望 listen, connect 超时, request_default, max_request_body,
    //  initial_stream_window_size, max_retry_buffer)
    let cases = [
        (
            "{}",
            "0.0.0.0:8080",
            Duration::from_secs(3),
            Duration::from_secs(30),
            10 * 1024 * 1024,
            1024 * 1024,
            1024 * 1024,
        ),
        (
            r#"server:
  listen: "127.0.0.1:8081"
  http2: { initial_stream_window_size: 512KB }
  limits: { max_request_body: 1MiB, max_retry_buffer: 500KB }
  timeouts: { connect: 500ms, request_default: 1.5s }"#,
            "127.0.0.1:8081",
            Duration::from_millis(500),
            Duration::from_millis(1500),
            1024 * 1024,
            512 * 1024,
            500 * 1024,
        ),
    ];
    for (yaml, want_listen, want_connect, want_request, want_body, want_window, want_retry) in cases
    {
        let file = parse_yaml(yaml);
        let states = Arc::new(StateRegistry::new());
        let compiled = compile_ok(
            &file,
            &FilterRegistry::new(),
            &states,
            &SlowHandler {
                latency: Duration::ZERO,
            },
            Path::new("."),
        );
        let server = &compiled.server;
        assert_eq!(server.listen, want_listen.parse::<SocketAddr>().unwrap());
        assert_eq!(server.connect_timeout, want_connect);
        assert_eq!(server.request_default, want_request);
        assert_eq!(server.max_request_body, want_body);
        assert_eq!(server.initial_stream_window_size, want_window);
        assert_eq!(server.max_retry_buffer, want_retry);
        assert_eq!(server.max_concurrent_streams, 1024);
    }

    // (yaml, 期望错误位置)
    let bad_cases = [
        (r#"server: { listen: "oops" }"#, "server.listen"),
        (
            r#"server: { timeouts: { connect: "abc" } }"#,
            "server.timeouts.connect",
        ),
        (
            r#"server: { limits: { max_request_body: "10XB" } }"#,
            "server.limits.max_request_body",
        ),
        (
            r#"server: { http2: { initial_stream_window_size: "" } }"#,
            "server.http2.initial_stream_window_size",
        ),
        (
            r#"server: { timeouts: { request_default: "0s" } }"#,
            "server.timeouts.request_default",
        ),
    ];
    for (yaml, want_loc) in bad_cases {
        let file = parse_yaml(yaml);
        let states = Arc::new(StateRegistry::new());
        let err = compile_err(
            &file,
            &FilterRegistry::new(),
            &states,
            &SlowHandler {
                latency: Duration::ZERO,
            },
            Path::new("."),
        );
        match err {
            ConfigError::Value { location, message } => {
                assert_eq!(location.to_string(), want_loc, "{yaml}");
                assert!(!message.is_empty(), "{yaml}");
            }
            other => panic!("expected ConfigError::Value for {yaml}, got {other:?}"),
        }
    }
}
