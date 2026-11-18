//! JwtAuth 过滤器：Bearer JWT 验签认证（槽位 `Auth`，准入类）。
//!
//! 参数：
//! - `issuer` / `audience` / `jwks_file`（必填）：iss/aud claim 校验锚点与
//!   公钥来源。`jwks_file` 是配置目录内的相对路径（经
//!   [`FilterBuildCtx::resolve_path`] 做目录包含校验，防配置注入读任意文件）。
//! - `claims`（可选字符串列表）：除 `sub`/`iss`/`aud` 外要带进
//!   [`AuthContext::claims`] 的自定义 claim 名。
//!
//! # 密钥与算法白名单
//!
//! JWKS 在 **build 期**（启动/热重载）读取并预构建 `kid → DecodingKey`
//! 表；坏 JWKS 使编译失败、热重载保旧快照。算法白名单是
//! **RS256/384/512 与 ES256/384**——刻意排除 `HS*` 与 `none`：JWKS 是
//! 公钥集，接受 HS* 就是密钥混淆攻击面（攻击者拿公钥字节当 HMAC 密钥
//! 自签令牌）。白名单在两处生效：JWKS 条目（build 期）与令牌头
//! （请求期 `Validation::algorithms`）。
//!
//! # kid 查找三态
//!
//! 1. 令牌头带 kid → 在 JWKS 中按 kid 查找；
//! 2. 令牌头无 kid 且 JWKS 只有一把 key → 用那把；
//! 3. 其余（kid 未命中 / 无 kid 且多 key）→ 拒绝。
//!
//! # 错误归类
//!
//! 一切验证失败（缺头、非 Bearer、过期、iss/aud 不符、签名错、kid 未
//! 命中）一律 [`GatewayError::Unauthenticated`]（HTTP 401 / gRPC 16）；
//! 令牌内容、kid、底层错误只进日志。双认证（set_auth 撞 set-once）映射
//! [`GatewayError::Internal`]——两个认证过滤器竞争同一请求是配置错误。

use std::collections::HashMap;
use std::sync::Arc;

use jsonwebtoken::jwk::{AlgorithmParameters, EllipticCurve, Jwk, JwkSet, KeyAlgorithm};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use rgateway_config::{
    ConfigError, FilterBuildCtx, FilterFactory, FilterSpec, Location, ServiceWrapper,
};
use rgateway_core::{AuthContext, AuthKind, BoxedService, GatewayError, ProxyBody};
use tower::service_fn;
use tower::util::BoxCloneService;
use tower::ServiceExt;

use crate::util::ctx_of;

/// 算法白名单（非对称签名）。见模块文档「密钥与算法白名单」。
const ALG_WHITELIST: [Algorithm; 5] = [
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::ES256,
    Algorithm::ES384,
];

/// `AuthContext::claims` 条目上限（防指标/日志基数爆炸），超界丢弃。
const MAX_CLAIMS: usize = 8;

/// JwtAuth 过滤器工厂（canonical 名 `"JwtAuth"`，槽 [`rgateway_config::FilterSlot::Auth`]）。
pub struct JwtAuthFactory;

impl FilterFactory for JwtAuthFactory {
    fn name(&self) -> &'static str {
        "JwtAuth"
    }

    fn build(
        &self,
        spec: &FilterSpec,
        ctx: &FilterBuildCtx,
    ) -> Result<ServiceWrapper, ConfigError> {
        let issuer = spec.param_str("issuer")?.to_string();
        let audience = spec.param_str("audience")?.to_string();
        let extra_claims = spec.param_str_list("claims")?;
        let keys = load_jwks(spec, ctx)?;
        let keys = Arc::new(keys);
        Ok(Box::new(move |inner: BoxedService| {
            let keys = Arc::clone(&keys);
            let issuer = issuer.clone();
            let audience = audience.clone();
            let extra_claims = extra_claims.clone();
            BoxCloneService::new(service_fn(move |req: http::Request<ProxyBody>| {
                let inner = inner.clone();
                let keys = Arc::clone(&keys);
                let issuer = issuer.clone();
                let audience = audience.clone();
                let extra_claims = extra_claims.clone();
                async move {
                    let ctx = ctx_of(&req)?;
                    let auth = match verify(req.headers(), &keys, &issuer, &audience, &extra_claims)
                    {
                        Ok(auth) => auth,
                        Err(reason) => {
                            // 令牌内容 / kid / 底层错误只进错误的内部通道（日志），
                            // 客户端只见泛化的 "unauthenticated"。
                            return Err(GatewayError::Unauthenticated(format!(
                                "jwt validation failed: {reason}"
                            )));
                        }
                    };
                    if ctx.set_auth(auth).is_err() {
                        return Err(GatewayError::Internal("auth already set".into()));
                    }
                    inner.oneshot(req).await
                }
            }))
        }))
    }
}

/// 一把预构建的 JWKS 条目：kid（可无）、验签密钥、白名单算法。
struct KeyEntry {
    kid: Option<String>,
    key: DecodingKey,
    #[allow(dead_code)] // 诊断/未来按 alg 强化校验用；验签实际用令牌头 alg
    alg: Algorithm,
}

/// build 期读取 + 解析 JWKS 并预构建密钥表。错误带条目级上下文。
fn load_jwks(spec: &FilterSpec, ctx: &FilterBuildCtx) -> Result<Vec<KeyEntry>, ConfigError> {
    let raw_ref = spec.param_str("jwks_file")?;
    let path = ctx.resolve_path(raw_ref)?;
    let loc = Location::section(&spec.source);
    let text = std::fs::read_to_string(&path).map_err(|e| {
        ConfigError::value(
            loc.clone(),
            format!("cannot read jwks_file {raw_ref:?}: {e}"),
        )
    })?;
    let set: JwkSet = serde_json::from_str(&text).map_err(|e| {
        ConfigError::value(
            loc.clone(),
            format!("jwks_file {raw_ref:?} is not a valid JWK Set: {e}"),
        )
    })?;
    if set.keys.is_empty() {
        return Err(ConfigError::value(
            loc,
            format!("jwks_file {raw_ref:?} contains no keys"),
        ));
    }
    let mut out = Vec::with_capacity(set.keys.len());
    for jwk in &set.keys {
        let alg = resolve_algorithm(jwk).map_err(|m| {
            ConfigError::value(
                Location::section(&spec.source),
                format!("jwks_file {raw_ref:?} key {:?}: {m}", jwk.common.key_id),
            )
        })?;
        let key = DecodingKey::from_jwk(jwk).map_err(|e| {
            ConfigError::value(
                Location::section(&spec.source),
                format!("jwks_file {raw_ref:?} key {:?}: {e}", jwk.common.key_id),
            )
        })?;
        out.push(KeyEntry {
            kid: jwk.common.key_id.clone(),
            key,
            alg,
        });
    }
    Ok(out)
}

/// 解析 JWK 的算法并过白名单：`alg` 字段给出时按字段，缺省按
/// `kty`/`crv` 推断（RSA→RS256、EC P-256→ES256、P-384→ES384）。
fn resolve_algorithm(jwk: &Jwk) -> Result<Algorithm, String> {
    if let Some(ka) = jwk.common.key_algorithm {
        // 显式白名单匹配即转换——HS*/none/PS*/EdDSA 一律落进兜底拒绝
        return match ka {
            KeyAlgorithm::RS256 => Ok(Algorithm::RS256),
            KeyAlgorithm::RS384 => Ok(Algorithm::RS384),
            KeyAlgorithm::RS512 => Ok(Algorithm::RS512),
            KeyAlgorithm::ES256 => Ok(Algorithm::ES256),
            KeyAlgorithm::ES384 => Ok(Algorithm::ES384),
            other => Err(format!("algorithm {other:?} is not allowed")),
        };
    }
    match &jwk.algorithm {
        AlgorithmParameters::RSA(_) => Ok(Algorithm::RS256),
        AlgorithmParameters::EllipticCurve(p) => match &p.curve {
            EllipticCurve::P256 => Ok(Algorithm::ES256),
            EllipticCurve::P384 => Ok(Algorithm::ES384),
            other => Err(format!("cannot infer algorithm from curve {other:?}")),
        },
        other => Err(format!("cannot infer algorithm from key type {other:?}")),
    }
}

/// 请求期验证：Bearer 提取 → 令牌头算法过白名单 → kid 三态找钥 →
/// claim 校验（exp/iss/aud/sub）→ 组装 [`AuthContext`]。
/// 返回 `Err(String)` 为内部日志用的原因。
fn verify(
    headers: &http::HeaderMap,
    keys: &[KeyEntry],
    issuer: &str,
    audience: &str,
    extra_claims: &[String],
) -> Result<AuthContext, String> {
    let token = bearer_token(headers)?;
    let header = decode_header(token).map_err(|e| format!("bad token header: {e}"))?;
    if !ALG_WHITELIST.contains(&header.alg) {
        return Err(format!("algorithm {:?} is not allowed", header.alg));
    }
    let entry = find_key(keys, header.kid.as_deref())?;
    // Validation::new(header.alg) 把 algorithms 钉为单算法——jsonwebtoken
    // 要求 algorithms 全体与密钥同族（混族会误拒），而 header.alg 已在
    // 上面过了白名单，等效于「白名单 ∩ 密钥族」。
    let mut validation = Validation::new(header.alg);
    validation.set_required_spec_claims(&["exp", "sub"]);
    validation.set_issuer(&[issuer]);
    validation.set_audience(&[audience]);
    let data = decode::<serde_json::Value>(token, &entry.key, &validation)
        .map_err(|e| format!("jwt invalid: {e}"))?;
    Ok(build_auth_context(&data.claims, extra_claims))
}

/// 从 `Authorization: Bearer <token>` 提取令牌；scheme 大小写不敏感。
fn bearer_token(headers: &http::HeaderMap) -> Result<&str, String> {
    let value = headers
        .get(http::header::AUTHORIZATION)
        .ok_or_else(|| "missing Authorization header".to_string())?
        .to_str()
        .map_err(|e| format!("Authorization not text: {e}"))?;
    let (scheme, token) = value
        .split_once(' ')
        .ok_or_else(|| "Authorization is not a bearer scheme".to_string())?;
    if !scheme.eq_ignore_ascii_case("bearer") || token.is_empty() {
        return Err(format!("Authorization scheme {scheme:?} is not bearer"));
    }
    Ok(token)
}

/// kid 三态查找（见模块文档）。
fn find_key<'a>(keys: &'a [KeyEntry], kid: Option<&str>) -> Result<&'a KeyEntry, String> {
    match kid {
        Some(kid) => keys
            .iter()
            .find(|e| e.kid.as_deref() == Some(kid))
            .ok_or_else(|| format!("kid {kid:?} not found in JWKS")),
        None if keys.len() == 1 => Ok(&keys[0]),
        None => Err("token has no kid and JWKS has multiple keys".into()),
    }
}

/// 从已验证的 claims 组装 [`AuthContext`]：`sub` 为身份，claims 收
/// `sub`/`iss`/`aud`（数组 join 为逗号串）+ `extra_claims` 指定项，
/// 总量封顶 [`MAX_CLAIMS`]，超界丢弃。
fn build_auth_context(claims: &serde_json::Value, extra_claims: &[String]) -> AuthContext {
    let subject = claims
        .get("sub")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let mut map: HashMap<String, String> = HashMap::new();
    for name in ["sub", "iss", "aud"] {
        if let Some(text) = claim_to_string(claims, name) {
            map.insert(name.to_string(), text);
        }
    }
    for name in extra_claims {
        if map.len() >= MAX_CLAIMS {
            // 超界丢弃：claims 有界是防基数爆炸的硬约束（多余项静默丢，
            // 身份主体 sub 已在前面优先入表）。
            break;
        }
        if let Some(text) = claim_to_string(claims, name) {
            map.insert(name.clone(), text);
        }
    }
    AuthContext {
        subject,
        kind: AuthKind::Jwt,
        claims: map,
    }
}

/// claim 值转字符串：标量直接字符串化，数组逗号 join，对象/空返回 None。
fn claim_to_string(claims: &serde_json::Value, name: &str) -> Option<String> {
    match claims.get(name)? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        serde_json::Value::Array(items) => {
            let parts: Vec<String> = items
                .iter()
                .filter_map(|v| match v {
                    serde_json::Value::String(s) => Some(s.clone()),
                    serde_json::Value::Number(n) => Some(n.to_string()),
                    _ => None,
                })
                .collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join(","))
            }
        }
        _ => None,
    }
}

#[cfg(test)]
#[path = "jwt_auth_tests.rs"]
mod tests;
