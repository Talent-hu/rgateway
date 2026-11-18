//! [`crate::schema`] 的反序列化测试：golden 样例、双形态谓词/过滤器、
//! 未知字段拒绝、缺省段默认值。

use super::*;

/// 验证：项目计划中的完整示例 YAML 必须能整体反序列化（golden schema 测试，
/// 锁定配置文件的对外形状），且各段关键字段落到期望值。
#[test]
fn plan_example_yaml_parses() {
    let yaml = r#"
server:
  listen: "0.0.0.0:8080"
  http2: { max_concurrent_streams: 1024, initial_stream_window_size: 1MB }
  limits: { max_request_body: 10MB, max_concurrent_requests: 4096 }
  timeouts: { connect: 3s, request_default: 30s, graceful_shutdown: 30s, ws_handshake: 10s }
  tls: { enabled: false, cert: "", key: "", client_auth: none }
discovery:
  providers:
    - type: static
      services:
        user-svc: [{ address: "127.0.0.1:50051", weight: 100 }]
defaults:
  filters: [{ name: CircuitBreaker, sliding_window: 20, failure_rate: 50%, min_calls: 10,
               open_duration: 5s, half_open_max_calls: 3 }]
routes:
  - id: user-http
    order: 1
    uri: "lb://user-svc"
    predicates:
      - "Path=/api/users/**, /api/orders/**"
      - "Method=GET,POST"
      - "Header=X-Tenant, \\d+"
    filters:
      - { name: RewritePath, regex: "/api/(?<segment>.*)", replacement: "/${segment}" }
      - "RateLimit=key:client_ip,rate:100,period:1s"
      - { name: Retry, retries: 3, methods: [GET, HEAD], statuses: [502, 503, 504],
          grpc_statuses: [14], backoff: { initial: 50ms, max: 2s, multiplier: 2, jitter: full } }
      - { name: JwtAuth, issuer: "https://idp", audience: gw, jwks_file: ./jwks.json }
  - id: user-grpc
    uri: "lb://user-svc"
    protocol: auto
    predicates: [ "Path=/helloworld.Greeter/*" ]
  - id: user-rest
    uri: "lb://user-svc"
    predicates: [ "Path=/v1/**" ]
    transcoding:
      descriptor: "./proto/descriptor.bin"
      rules: [{ selector: "helloworld.Greeter/SayHello", post: "/v1/echo", body: "*" }]
observability:
  access_log: { enabled: true, format: json }
  metrics: { enabled: true, path: /metrics }
mgmt: { listen: "127.0.0.1:9090", grpc: true }
"#;
    let cfg: ConfigFile = serde_yaml::from_str(yaml).expect("plan YAML must parse");
    assert_eq!(cfg.server.listen, "0.0.0.0:8080");
    assert_eq!(cfg.server.http2.max_concurrent_streams, 1024);
    assert_eq!(cfg.server.limits.max_request_body, "10MB");
    assert_eq!(cfg.server.timeouts.connect, "3s");
    assert_eq!(cfg.discovery.providers.len(), 1);
    assert_eq!(cfg.routes.len(), 3);
    assert_eq!(cfg.routes[0].id, "user-http");
    assert_eq!(cfg.routes[0].predicates.len(), 3);
    assert_eq!(cfg.routes[0].filters.len(), 4);
    assert!(cfg.routes[2].transcoding.is_some());
    assert_eq!(cfg.defaults.filters.len(), 1);
    assert_eq!(cfg.mgmt.listen, "127.0.0.1:9090");
    assert!(cfg.mgmt.grpc);
}

/// 验证：同一列表里紧凑 DSL 字符串与结构化映射两种形态能同时反序列化，
/// 并分别落到 Compact / Structured 变体，结构化过滤器的参数收进 params。
#[test]
fn dual_form_predicates_and_filters() {
    let yaml = r#"
routes:
  - id: r
    uri: http://127.0.0.1:1
    predicates:
      - "Method=GET"
      - { name: Host, args: ["api.example.com"] }
    filters:
      - "RateLimit=key:client_ip,rate:100,period:1s"
      - { name: Retry, retries: 2 }
"#;
    let cfg: ConfigFile = serde_yaml::from_str(yaml).unwrap();
    let r = &cfg.routes[0];
    assert!(matches!(r.predicates[0], PredicateConfig::Compact(_)));
    assert!(matches!(r.predicates[1], PredicateConfig::Structured(_)));
    match &r.filters[1] {
        FilterConfig::Structured(s) => {
            assert_eq!(s.name, "Retry");
            assert!(s.params.contains_key("retries"));
        }
        _ => panic!("expected structured filter"),
    }
}

/// 验证：deny_unknown_fields 生效——拼错的字段名令整个文件反序列化失败，
/// 而不是被静默忽略。
#[test]
fn unknown_fields_are_rejected() {
    let yaml = r#"
server: { listen: "0.0.0.0:1", nope: true }
"#;
    assert!(serde_yaml::from_str::<ConfigFile>(yaml).is_err());
}

/// 验证：空文档 `{}` 下各配置段整体缺省，回落到内置默认值。
#[test]
fn defaults_apply_when_sections_missing() {
    let cfg: ConfigFile = serde_yaml::from_str("{}").unwrap();
    assert_eq!(cfg.server.listen, "0.0.0.0:8080");
    assert!(cfg.routes.is_empty());
    assert_eq!(cfg.mgmt.listen, "127.0.0.1:9090");
}
