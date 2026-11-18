//! JwtAuth 过滤器测试：JWKS 夹具、Bearer 提取、kid 三态查找、
//! 算法白名单（HS*/none 拒）、claim 校验、AuthContext 粒度、双认证、
//! 错误双通道。测试密钥是固定的 P-256 密钥对（只作测试，无安全意义）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use rgateway_core::{empty_body, AuthKind, ErrorKind, Protocol, ProxyBody, RequestCtx, RouteState};
use tower::ServiceExt;

use super::*;
use crate::test_support::{counting_ok, spec_yaml};

/// 固定 P-256 私钥（PKCS#8 PEM——jsonwebtoken 只接受 PKCS#8），
/// 与 [`JWKS_ONE`] 中的公钥同对。
const TEST_EC_PEM: &str = "-----BEGIN PRIVATE KEY-----\n\
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgrUlU6HAEONbmGl+Y\n\
YN3lzrvd87/wB4+/9myGTG7Xp1ChRANCAATlnLl1O1LGsgp3h++5wGvVVu4kvquQ\n\
qKLOURRsGU+rnWEL5aiQxvcI7XBKBYpwYbqP8Ge5aOEbvqqXsYCRhNm/\n\
-----END PRIVATE KEY-----";

/// 单密钥 JWKS（kid = test-key，ES256）。
const JWKS_ONE: &str = r#"{
  "keys": [
    {
      "kty": "EC",
      "crv": "P-256",
      "x": "5Zy5dTtSxrIKd4fvucBr1VbuJL6rkKiizlEUbBlPq50",
      "y": "YQvlqJDG9wjtcEoFinBhuo_wZ7lo4Ru-qpexgJGE2b8",
      "alg": "ES256",
      "kid": "test-key"
    }
  ]
}"#;

/// 双密钥 JWKS（同一把密钥材料挂两个 kid——查找语义只看 kid 匹配与数量）。
const JWKS_TWO: &str = r#"{
  "keys": [
    {
      "kty": "EC",
      "crv": "P-256",
      "x": "5Zy5dTtSxrIKd4fvucBr1VbuJL6rkKiizlEUbBlPq50",
      "y": "YQvlqJDG9wjtcEoFinBhuo_wZ7lo4Ru-qpexgJGE2b8",
      "alg": "ES256",
      "kid": "key-a"
    },
    {
      "kty": "EC",
      "crv": "P-256",
      "x": "5Zy5dTtSxrIKd4fvucBr1VbuJL6rkKiizlEUbBlPq50",
      "y": "YQvlqJDG9wjtcEoFinBhuo_wZ7lo4Ru-qpexgJGE2b8",
      "alg": "ES256",
      "kid": "key-b"
    }
  ]
}"#;

/// 唯一临时目录序号（并行测试各写各的 JWKS 文件）。
static DIR_SEQ: AtomicUsize = AtomicUsize::new(0);

/// 写一个 JWKS 文件到全新临时目录，返回目录路径（用作 config_dir）。
fn jwks_dir(jwks_json: &str) -> PathBuf {
    let n = DIR_SEQ.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("rgw-jwt-test-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    std::fs::write(dir.join("jwks.json"), jwks_json).expect("write jwks");
    dir
}

/// 构造 config_dir 指向给定目录的最小 [`FilterBuildCtx`]。
fn ctx_in(dir: PathBuf) -> rgateway_config::FilterBuildCtx {
    rgateway_config::FilterBuildCtx {
        route_id: Arc::from("r1"),
        route_state: Arc::new(RouteState::new("r1")),
        discovery: crate::test_support::empty_discovery(),
        max_retry_buffer: 1024,
        config_dir: Arc::new(dir),
    }
}

/// 用测试私钥签一个 ES256 令牌；`kid` 为 None 时不写 kid 头。
fn sign(claims: serde_json::Value, kid: Option<&str>) -> String {
    let mut header = Header::new(Algorithm::ES256);
    header.kid = kid.map(str::to_string);
    let key = EncodingKey::from_ec_pem(TEST_EC_PEM.as_bytes()).expect("pem");
    encode(&header, &claims, &key).expect("sign")
}

/// 一个 exp 在未来的标准合法 claims。
fn valid_claims() -> serde_json::Value {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64;
    serde_json::json!({
        "sub": "user-1",
        "iss": "https://idp.example.com",
        "aud": "gw",
        "exp": now + 3600,
    })
}

/// helper：以 JWKS 文件内容 + 过滤器 YAML 构建链，返回 (链, 计数器, 请求, ctx)。
fn jwt_chain(
    jwks_json: &str,
    filter_yaml: &str,
) -> (
    rgateway_core::BoxedService,
    Arc<AtomicUsize>,
    http::Request<ProxyBody>,
    Arc<RequestCtx>,
) {
    let dir = jwks_dir(jwks_json);
    let fctx = ctx_in(dir);
    let wrapper = JwtAuthFactory
        .build(&spec_yaml(filter_yaml), &fctx)
        .expect("build");
    let counter = Arc::new(AtomicUsize::new(0));
    let chain = wrapper(counting_ok(Arc::clone(&counter)));
    let ctx = Arc::new(RequestCtx::new(Protocol::Http1, None));
    let mut req = http::Request::new(empty_body());
    req.extensions_mut().insert(Arc::clone(&ctx));
    (chain, counter, req, ctx)
}

const FILTER: &str =
    r#"{ name: JwtAuth, issuer: "https://idp.example.com", audience: gw, jwks_file: ./jwks.json }"#;

/// 命中：合法令牌放行并写入 AuthContext（subject + 自动 claims）。
#[tokio::test]
async fn valid_token_sets_auth_context() {
    let (chain, counter, mut req, ctx) = jwt_chain(JWKS_ONE, FILTER);
    let token = sign(valid_claims(), Some("test-key"));
    req.headers_mut().insert(
        http::header::AUTHORIZATION,
        http::HeaderValue::from_str(&format!("Bearer {token}")).expect("ascii"),
    );
    chain.oneshot(req).await.expect("valid token must pass");
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    let auth = ctx.auth().expect("auth set");
    assert_eq!(auth.subject, "user-1");
    assert_eq!(auth.kind, AuthKind::Jwt);
    assert_eq!(auth.claims.get("sub").map(String::as_str), Some("user-1"));
    assert_eq!(
        auth.claims.get("iss").map(String::as_str),
        Some("https://idp.example.com")
    );
    assert_eq!(auth.claims.get("aud").map(String::as_str), Some("gw"));
}

/// claims 参数：指定的自定义 claim 进入 claims；数组 claim 逗号 join；
/// 总量封顶 8 条，超界丢弃。
#[tokio::test]
async fn custom_claims_extracted_and_capped() {
    let mut claims = valid_claims();
    claims["roles"] = serde_json::json!(["admin", "ops"]);
    for i in 0..12 {
        claims[format!("extra-{i}")] = serde_json::json!(format!("v{i}"));
    }
    let filter = r#"{ name: JwtAuth, issuer: "https://idp.example.com", audience: gw, jwks_file: ./jwks.json, claims: [roles] }"#;
    let (chain, _counter, mut req, ctx) = jwt_chain(JWKS_ONE, filter);
    let token = sign(claims, Some("test-key"));
    req.headers_mut().insert(
        http::header::AUTHORIZATION,
        http::HeaderValue::from_str(&format!("Bearer {token}")).expect("ascii"),
    );
    chain.oneshot(req).await.expect("must pass");
    let auth = ctx.auth().expect("auth set");
    assert_eq!(
        auth.claims.get("roles").map(String::as_str),
        Some("admin,ops")
    );
    assert!(
        auth.claims.len() <= 8,
        "claims must be capped: {}",
        auth.claims.len()
    );
    assert!(auth.claims.contains_key("sub"));
}

/// 未命中表格：过期、iss/aud 不符、缺 sub、缺 Authorization、非 Bearer、
/// kid 未命中、kid 缺失且多密钥——全部 401 且不触达 inner。
#[tokio::test]
async fn rejection_matrix() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64;

    // 列含义：(jwks, 要放进 Authorization 的值, 期望错误种类)
    let expired = {
        let mut c = valid_claims();
        c["exp"] = serde_json::json!(now - 3600);
        sign(c, Some("test-key"))
    };
    let bad_iss = {
        let mut c = valid_claims();
        c["iss"] = serde_json::json!("https://evil.example.com");
        sign(c, Some("test-key"))
    };
    let bad_aud = {
        let mut c = valid_claims();
        c["aud"] = serde_json::json!("other-gw");
        sign(c, Some("test-key"))
    };
    let no_sub = {
        let mut c = valid_claims();
        c.as_object_mut().expect("obj").remove("sub");
        sign(c, Some("test-key"))
    };
    let wrong_kid = sign(valid_claims(), Some("nonexistent"));
    let no_kid = sign(valid_claims(), None);
    let bearer = |t: &str| format!("Bearer {t}");

    // 表列：(jwks, Authorization 值, 标签)
    let cases: &[(JwksCase, String, &str)] = &[
        (JwksCase::One, bearer(&expired), "expired"),
        (JwksCase::One, bearer(&bad_iss), "bad iss"),
        (JwksCase::One, bearer(&bad_aud), "bad aud"),
        (JwksCase::One, bearer(&no_sub), "missing sub"),
        (JwksCase::One, "Basic abc".into(), "non-bearer"),
        (JwksCase::One, bearer(&wrong_kid), "kid not found"),
        (JwksCase::Two, bearer(&no_kid), "kid missing with two keys"),
    ];
    for (jwks, auth_value, label) in cases {
        let json = match jwks {
            JwksCase::One => JWKS_ONE,
            JwksCase::Two => JWKS_TWO,
        };
        let (chain, counter, mut req, _ctx) = jwt_chain(json, FILTER);
        req.headers_mut().insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_str(auth_value).expect("ascii"),
        );
        let err = chain.oneshot(req).await.expect_err(label);
        assert_eq!(err.kind(), ErrorKind::Unauthenticated, "{label}");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "{label}: inner 不应被调用"
        );
    }

    // 缺 Authorization 头
    let (chain, counter, req, _ctx) = jwt_chain(JWKS_ONE, FILTER);
    let err = chain.oneshot(req).await.expect_err("missing header");
    assert_eq!(err.kind(), ErrorKind::Unauthenticated);
    assert_eq!(counter.load(Ordering::SeqCst), 0);
}

/// 选择 JWKS 夹具的枚举（表格驱动）。
enum JwksCase {
    One,
    Two,
}

/// kid 三态：kid 命中、kid 缺失且单密钥放行；kid 未命中拒绝。
#[tokio::test]
async fn kid_lookup_semantics() {
    // kid 命中（双密钥里任一）
    let (chain, _c, mut req, _ctx) = jwt_chain(JWKS_TWO, FILTER);
    let token = sign(valid_claims(), Some("key-b"));
    req.headers_mut().insert(
        http::header::AUTHORIZATION,
        http::HeaderValue::from_str(&format!("Bearer {token}")).expect("ascii"),
    );
    chain.oneshot(req).await.expect("kid key-b must pass");

    // kid 缺失且单密钥 → 用那把
    let (chain, _c, mut req, _ctx) = jwt_chain(JWKS_ONE, FILTER);
    let token = sign(valid_claims(), None);
    req.headers_mut().insert(
        http::header::AUTHORIZATION,
        http::HeaderValue::from_str(&format!("Bearer {token}")).expect("ascii"),
    );
    chain
        .oneshot(req)
        .await
        .expect("single key without kid must pass");
}

/// 算法白名单：HS256（密钥混淆攻击面）与 alg:none 一律拒绝。
#[tokio::test]
async fn rejects_hs_and_none_algorithms() {
    // HS256 令牌：以字符串密钥签发，若网关接受则为密钥混淆漏洞
    let hs_token = {
        let claims = valid_claims();
        let key = EncodingKey::from_secret(b"attacker-secret");
        encode(&Header::new(Algorithm::HS256), &claims, &key).expect("hs sign")
    };
    let (chain, counter, mut req, _ctx) = jwt_chain(JWKS_ONE, FILTER);
    req.headers_mut().insert(
        http::header::AUTHORIZATION,
        http::HeaderValue::from_str(&format!("Bearer {hs_token}")).expect("ascii"),
    );
    let err = chain
        .oneshot(req)
        .await
        .expect_err("HS256 must be rejected");
    assert_eq!(err.kind(), ErrorKind::Unauthenticated);
    assert_eq!(counter.load(Ordering::SeqCst), 0);

    // alg:none 令牌（无签名段）
    let none_token = {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs() as i64;
        let payload = serde_json::json!({
            "sub": "user-1", "iss": "https://idp.example.com",
            "aud": "gw", "exp": now + 3600,
        });
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).expect("json"));
        format!("{header}.{payload}.")
    };
    let (chain, counter, mut req, _ctx) = jwt_chain(JWKS_ONE, FILTER);
    req.headers_mut().insert(
        http::header::AUTHORIZATION,
        http::HeaderValue::from_str(&format!("Bearer {none_token}")).expect("ascii"),
    );
    let err = chain.oneshot(req).await.expect_err("none must be rejected");
    assert_eq!(err.kind(), ErrorKind::Unauthenticated);
    assert_eq!(counter.load(Ordering::SeqCst), 0);
}

/// 构建期失败：坏 JWKS、jwks_file 逃逸、缺参数——全部 ConfigError。
#[test]
fn build_rejects_bad_jwks_and_paths() {
    // 坏 JSON
    let dir = jwks_dir("{ not json");
    let fctx = ctx_in(dir);
    let err = JwtAuthFactory
        .build(&spec_yaml(FILTER), &fctx)
        .err()
        .expect("bad json must fail build");
    assert!(err.to_string().contains("jwks"), "{err}");

    // 路径逃逸
    let dir = jwks_dir(JWKS_ONE);
    let fctx = ctx_in(dir);
    let escaping = r#"{ name: JwtAuth, issuer: x, audience: gw, jwks_file: ../evil.json }"#;
    let err = JwtAuthFactory
        .build(&spec_yaml(escaping), &fctx)
        .err()
        .expect("escape must fail build");
    assert!(err.to_string().contains("evil.json"), "{err}");

    // 缺必填参数
    let dir = jwks_dir(JWKS_ONE);
    let fctx = ctx_in(dir);
    let err = JwtAuthFactory
        .build(
            &spec_yaml("{ name: JwtAuth, audience: gw, jwks_file: ./jwks.json }"),
            &fctx,
        )
        .err()
        .expect("missing issuer must fail build");
    assert!(err.to_string().contains("issuer"), "{err}");
}

/// 双认证：第二个认证过滤器试图写入 AuthContext → Internal。
#[tokio::test]
async fn double_auth_is_internal_error() {
    let dir = jwks_dir(JWKS_ONE);
    let fctx = ctx_in(dir);
    let wrapper = JwtAuthFactory
        .build(&spec_yaml(FILTER), &fctx)
        .expect("build");
    let counter = Arc::new(AtomicUsize::new(0));
    // inner 先占住 auth 槽（模拟已有认证过滤器），再过 JwtAuth 会撞 set-once
    let inner = BoxCloneService::new(tower::service_fn(move |_req: http::Request<ProxyBody>| {
        let counter = Arc::clone(&counter);
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok::<http::Response<ProxyBody>, rgateway_core::GatewayError>(http::Response::new(
                empty_body(),
            ))
        }
    }));
    let chain = wrapper(inner);
    let ctx = Arc::new(RequestCtx::new(Protocol::Http1, None));
    ctx.set_auth(rgateway_core::AuthContext {
        subject: "already".into(),
        kind: AuthKind::ApiKey,
        claims: Default::default(),
    })
    .expect("first set");
    let mut req = http::Request::new(empty_body());
    req.extensions_mut().insert(ctx);
    let token = sign(valid_claims(), Some("test-key"));
    req.headers_mut().insert(
        http::header::AUTHORIZATION,
        http::HeaderValue::from_str(&format!("Bearer {token}")).expect("ascii"),
    );
    let err = chain.oneshot(req).await.expect_err("double auth must fail");
    assert_eq!(err.kind(), ErrorKind::Internal);
}

/// 错误双通道：401 / gRPC 16，客户端文案泛化，令牌与 kid 不回显。
#[tokio::test]
async fn error_mapping_keeps_details_private() {
    let (chain, _c, mut req, _ctx) = jwt_chain(JWKS_ONE, FILTER);
    let token = sign(valid_claims(), Some("test-key"));
    let wrong = format!("Bearer {token}x"); // 损坏签名
    req.headers_mut().insert(
        http::header::AUTHORIZATION,
        http::HeaderValue::from_str(&wrong).expect("ascii"),
    );
    let err = chain.oneshot(req).await.expect_err("must fail");
    assert_eq!(err.http_status(), 401);
    assert_eq!(err.grpc_status(), 16);
    let public = err.public_message();
    assert_eq!(public, "unauthenticated");
    assert!(!public.contains("test-key"), "{public}");
    assert!(!public.contains(&token), "{public}");
}
