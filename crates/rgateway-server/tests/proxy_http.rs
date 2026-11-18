//! 代理转发端到端：请求/响应往返、代理头治理、hop-by-hop 剥离、
//! `X-Forwarded-Host` 覆写、分块请求体、`lb://` round-robin 分布、
//! Path/Method 谓词 first-match。
//!
//! 转发闭环走生产链路（`bootstrap::start` 装配 + `rgateway-proxy` 的
//! `HttpHandlerFactory` / `UpstreamClient`）；断言覆盖的生产代码：
//! 配置编译链、路由匹配、TargetSelector + RoundRobin、
//! `prepare_upstream` / `finalize_response` 头治理、Dispatcher 全流程。

mod common;

use common::*;

/// 起「回显请求头」上游 + 网关（一条 `/api/**` 路由指向该上游）。
/// 返回 (临时目录保活, 网关, 上游)。
async fn start_echo_chain() -> (TestDir, TestGateway, Upstream) {
    let upstream = start_upstream(Behavior::EchoHeaders).await;
    let dir = TestDir::new();
    let yaml = config_with_routes(&route_entry(
        "api",
        "/api/**",
        &format!("http://127.0.0.1:{}", upstream.port),
        None,
        None,
    ));
    let gw = start(dir.path(), &yaml).await;
    (dir, gw, upstream)
}

/// 列含义：(上游收到的头名, 期望值断言见下)——回显 JSON 的 headers 键全是小写。
#[tokio::test]
async fn get_round_trip_carries_proxy_headers() {
    let (_dir, gw, _upstream) = start_echo_chain().await;

    // 伪装的 X-Request-ID 必须被覆写为网关联 id
    let resp = request(
        gw.addr,
        "GET",
        "/api/echo?x=1",
        &[("Host", "example.test"), ("X-Request-ID", "spoofed")],
        BodySpec::Empty,
    )
    .await;

    assert_eq!(resp.status, 200);
    let echoed: serde_json::Value = serde_json::from_str(&resp.body_text()).expect("echo json");
    assert_eq!(echoed["method"], "GET");
    assert_eq!(echoed["path"], "/api/echo?x=1");
    // X-Request-ID：上游收到的值 = 响应头的值，且不信客户端传入的值
    let upstream_id = echoed["headers"]["x-request-id"].as_str().expect("id");
    assert_eq!(
        upstream_id,
        resp.header("x-request-id").expect("response id")
    );
    assert_ne!(upstream_id, "spoofed");
    assert!(
        uuid::Uuid::parse_str(upstream_id).is_ok(),
        "id must be uuid: {upstream_id}"
    );
    // X-Forwarded-* 与 Host
    assert_eq!(echoed["headers"]["x-forwarded-proto"], "http");
    assert!(
        echoed["headers"]["x-forwarded-for"]
            .as_str()
            .expect("xff")
            .contains("127.0.0.1"),
        "xff must contain gateway peer: {}",
        echoed["headers"]["x-forwarded-for"]
    );
    assert_eq!(echoed["headers"]["x-forwarded-host"], "example.test");
    assert_eq!(
        echoed["headers"]["host"], "example.test",
        "Host must be preserved"
    );
}

/// hop-by-hop 头（固定清单 + `Connection` 动态 token）不得转发到上游；
/// 普通自定义头必须转发（正对照）。
#[tokio::test]
async fn hop_by_hop_headers_are_not_forwarded() {
    let (_dir, gw, _upstream) = start_echo_chain().await;

    let resp = request(
        gw.addr,
        "GET",
        "/api/echo",
        &[
            ("Host", "example.test"),
            ("Connection", "close, x-hop-token"),
            ("x-hop-token", "drop-me"),
            ("Keep-Alive", "timeout=5"),
            ("TE", "trailers"),
            ("Upgrade", "h2c"),
            ("Proxy-Authorization", "Basic ZGVhZGJlZWY="),
            ("X-Custom-Keep", "keep-me"),
        ],
        BodySpec::Empty,
    )
    .await;

    assert_eq!(resp.status, 200);
    let echoed: serde_json::Value = serde_json::from_str(&resp.body_text()).expect("echo json");
    // 列含义：(不得出现在上游请求里的头名)——固定 hop-by-hop 清单 + Connection token
    let forbidden = [
        "connection",
        "keep-alive",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "proxy-authorization",
        "x-hop-token",
    ];
    for name in forbidden {
        assert!(
            echoed["headers"].get(name).is_none(),
            "hop-by-hop header {name} leaked upstream: {}",
            echoed["headers"]
        );
    }
    // 正对照：非 hop-by-hop 头照常转发
    assert_eq!(echoed["headers"]["x-custom-keep"], "keep-me");
}

/// 多 chunk 的 POST 请求体经限长 body 转发后往返一致。
#[tokio::test]
async fn post_chunked_body_round_trips() {
    let upstream = start_upstream(Behavior::EchoBody).await;
    let dir = TestDir::new();
    let yaml = config_with_routes(&route_entry(
        "api",
        "/api/**",
        &format!("http://127.0.0.1:{}", upstream.port),
        None,
        None,
    ));
    let gw = start(dir.path(), &yaml).await;

    // 列含义：每个 Vec 是一个 chunk，拼接后是期望的回显体
    let chunks: Vec<Vec<u8>> = vec![
        b"hello ".to_vec(),
        b"chunked ".to_vec(),
        b"request body".to_vec(),
    ];
    let expected: Vec<u8> = chunks.concat();
    let resp = request(
        gw.addr,
        "POST",
        "/api/echo",
        &[("Host", "example.test"), ("Content-Type", "text/plain")],
        BodySpec::Chunked(chunks),
    )
    .await;

    assert_eq!(resp.status, 200);
    assert_eq!(
        resp.body, expected,
        "echoed body must round-trip byte-identical"
    );
}

/// `lb://` 路由 + 两个实例：4 次请求 round-robin 恰好各命中 2 次
/// （TargetSelector + RoundRobin 是生产代码，上游各自回显自身端口）。
#[tokio::test]
async fn lb_route_round_robins_two_instances() {
    let up_a = start_upstream(Behavior::SelfIdentify).await;
    let up_b = start_upstream(Behavior::SelfIdentify).await;
    let dir = TestDir::new();
    let yaml = format!(
        "server:\n  listen: \"127.0.0.1:0\"\ndiscovery:\n  providers:\n    - type: static\n      services:\n        echo-svc:\n          - {{ address: \"127.0.0.1:{}\" }}\n          - {{ address: \"127.0.0.1:{}\" }}\nroutes:\n{}",
        up_a.port,
        up_b.port,
        route_entry("lb-api", "/api/**", "lb://echo-svc", None, None)
    );
    let gw = start(dir.path(), &yaml).await;

    let mut hits: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for _ in 0..4 {
        let resp = request(
            gw.addr,
            "GET",
            "/api/ping",
            &[("Host", "example.test")],
            BodySpec::Empty,
        )
        .await;
        assert_eq!(resp.status, 200);
        *hits.entry(resp.body_text()).or_insert(0) += 1;
    }

    // 列含义：(上游端口, 期望命中次数)——round-robin 在两实例上各 2 次
    let expected = [(up_a.port.to_string(), 2), (up_b.port.to_string(), 2)];
    for (port, want) in expected {
        assert_eq!(
            hits.get(&port).copied().unwrap_or(0),
            want,
            "instance {port} hits"
        );
    }
}

/// Path/Method 谓词 + first-match：命中的进 A 上游；只匹配 Path 的进 B；
/// Method 不匹配的落 404（谓词 AND 语义 + 排序后首条命中）。
#[tokio::test]
async fn path_and_method_predicates_route_first_match() {
    let up_a = start_upstream(Behavior::SelfIdentify).await;
    let up_b = start_upstream(Behavior::SelfIdentify).await;
    let dir = TestDir::new();
    // A（order 0，只看 Path）优先于 B（order 1，Path + Method=GET）
    let yaml = config_with_routes(&format!(
        "  - id: a\n    order: 0\n    uri: \"http://127.0.0.1:{}\"\n    predicates: [\"Path=/api/special/**\"]\n  - id: b\n    order: 1\n    uri: \"http://127.0.0.1:{}\"\n    predicates: [\"Path=/api/**\", \"Method=GET\"]\n",
        up_a.port, up_b.port
    ));
    let gw = start(dir.path(), &yaml).await;

    // 列含义：(方法, 路径, 期望状态, 期望响应体)
    let cases = [
        ("GET", "/api/special/x", 200, up_a.port.to_string()),
        ("GET", "/api/other", 200, up_b.port.to_string()),
        (
            "POST",
            "/api/other",
            404,
            "{\"error\":\"not found\"}".to_string(),
        ),
    ];
    for (method, path, want_status, want_body) in cases {
        let resp = request(
            gw.addr,
            method,
            path,
            &[("Host", "example.test")],
            BodySpec::Empty,
        )
        .await;
        assert_eq!(resp.status, want_status, "{method} {path}");
        assert_eq!(resp.body_text(), want_body, "{method} {path}");
    }
}

/// `X-Forwarded-Host` 覆写端到端：客户端塞 `X-Forwarded-Host: evil.example`
/// 与 `Forwarded` 头，上游收到的 `X-Forwarded-Host` 必须是**原始 Host**，
/// `Forwarded` 头绝不达上游（伪造链路信息的防污染语义）。
#[tokio::test]
async fn x_forwarded_host_is_overwritten_and_forwarded_stripped() {
    let upstream = start_upstream(Behavior::EchoHeaders).await;
    let dir = TestDir::new();
    let yaml = config_with_routes(&route_entry(
        "api",
        "/api/**",
        &format!("http://127.0.0.1:{}", upstream.port),
        None,
        None,
    ));
    let gw = start(dir.path(), &yaml).await;

    let resp = request(
        gw.addr,
        "GET",
        "/api/echo",
        &[
            ("Host", "real.example.test"),
            ("X-Forwarded-Host", "evil.example"),
            ("Forwarded", "for=10.9.9.9;host=evil.example"),
        ],
        BodySpec::Empty,
    )
    .await;

    assert_eq!(resp.status, 200);
    let echoed: serde_json::Value = serde_json::from_str(&resp.body_text()).expect("echo json");
    // 上游收到的 X-Forwarded-Host = 原始 Host（客户端伪造值被覆写）
    assert_eq!(
        echoed["headers"]["x-forwarded-host"], "real.example.test",
        "X-Forwarded-Host must be overwritten with the original Host"
    );
    assert_ne!(echoed["headers"]["x-forwarded-host"], "evil.example");
    // Host 保留不变（上游按原 Host 做虚拟主机路由）
    assert_eq!(echoed["headers"]["host"], "real.example.test");
    // Forwarded 头无条件剥离，绝不达上游
    assert!(
        echoed["headers"].get("forwarded").is_none(),
        "Forwarded header leaked upstream: {}",
        echoed["headers"]
    );
}
