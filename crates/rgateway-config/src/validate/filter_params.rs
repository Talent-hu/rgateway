//! 过滤器条目的参数级校验（`validate` 的子模块）：归一化后按名检查
//! 「必填缺失 / 有值但非法」，可选参数的缺省留给运行时默认值。
//!
//! 拆出独立文件只为保持 `validate.rs` 体量在红线内；校验语义与
//! `validate.rs` 的其他段一致——全部错误一次性收集，不遇错即停。

use crate::error::{ConfigError, Location};
use crate::schema::FilterConfig;
use crate::spec::{parse_ip_net, slot_for, FilterSpec};

/// 校验一组过滤器：归一化后按名做参数校验；未知名报 `UnknownFilter`，
/// 解析失败把定位改写到条目处。`at` 生成每个条目的配置定位。
pub(super) fn validate_filter_list(
    filters: &[FilterConfig],
    at: &dyn Fn(usize) -> Location,
    errors: &mut Vec<ConfigError>,
) {
    for (i, f) in filters.iter().enumerate() {
        let floc = at(i);
        match FilterSpec::from_config(f) {
            Ok(spec) => {
                if slot_for(&spec.name).is_none() {
                    errors.push(ConfigError::UnknownFilter {
                        location: floc,
                        name: spec.name,
                    });
                    continue;
                }
                validate_filter_params(&spec, &floc, errors);
            }
            Err(e) => errors.push(super::relocate(e, &floc)),
        }
    }
}

/// 按过滤器名分派参数级校验（各检查见 `check_*`）：只报「必填缺失/有值但
/// 非法」，可选参数的缺省留给运行时默认值。
pub(super) fn validate_filter_params(
    spec: &FilterSpec,
    floc: &Location,
    errors: &mut Vec<ConfigError>,
) {
    match spec.name.as_str() {
        "RateLimit" => check_rate_limit(spec, floc, errors),
        "Retry" => check_retry(spec, floc, errors),
        "CircuitBreaker" => check_circuit_breaker(spec, floc, errors),
        "BodyLimit" => {
            if spec.param_size("max_body").is_err() {
                push(errors, floc, "BodyLimit requires max_body");
            }
        }
        "RewritePath" => check_rewrite_path(spec, floc, errors),
        "JwtAuth" => check_jwt_auth(spec, floc, errors),
        "ApiKeyAuth" => check_api_key_auth(spec, floc, errors),
        "IpFilter" => check_ip_filter(spec, floc, errors),
        // per_attempt 与 timeout 二选一：仅当两者都缺失/非法才报错
        "Timeout"
            if spec.param_duration("per_attempt").is_err()
                && spec.param_duration("timeout").is_err() =>
        {
            push(errors, floc, "Timeout requires per_attempt (or timeout)");
        }
        _ => {}
    }
}

/// 追加一条带条目定位的校验错误。
fn push(errors: &mut Vec<ConfigError>, floc: &Location, message: impl Into<String>) {
    errors.push(ConfigError::value(floc.clone(), message.into()));
}

/// `RateLimit`：key/rate 必填；period 若给出必须 > 0。
fn check_rate_limit(spec: &FilterSpec, floc: &Location, errors: &mut Vec<ConfigError>) {
    if spec.param_str("key").is_err() {
        push(errors, floc, "RateLimit requires key (e.g. key:client_ip)");
    }
    if spec.param_u64("rate").is_err() {
        push(errors, floc, "RateLimit requires rate");
    }
    if let Ok(d) = spec.param_duration("period") {
        if d.is_zero() {
            push(errors, floc, "RateLimit period must be > 0");
        }
    }
}

/// `Retry`：retries 上限 10（防重试风暴）；backoff.initial 若给出必须 > 0。
fn check_retry(spec: &FilterSpec, floc: &Location, errors: &mut Vec<ConfigError>) {
    if let Ok(n) = spec.param_u64("retries") {
        if n > 10 {
            push(errors, floc, "Retry retries must be <= 10");
        }
    }
    if let Ok(backoff) = spec.param_map("backoff") {
        // 把 backoff 子 map 包装成临时 FilterSpec，复用类型化访问器
        let tmp = FilterSpec {
            name: spec.name.clone(),
            params: backoff,
            source: spec.source.clone(),
        };
        if let Ok(Some(d)) = tmp.param_duration_opt("initial") {
            if d.is_zero() {
                push(errors, floc, "Retry backoff.initial must be > 0");
            }
        }
    }
}

/// `CircuitBreaker`：failure_rate 必须落在 `0..=1`；min_calls 不得超过
/// sliding_window（否则窗口内永远凑不齐最小样本数）。
fn check_circuit_breaker(spec: &FilterSpec, floc: &Location, errors: &mut Vec<ConfigError>) {
    // failure_rate 必须落在 0..=1（param_f64 已把 "50%" 折算成 0.5）
    if let Ok(rate) = spec.param_f64("failure_rate") {
        if !(0.0..=1.0).contains(&rate) {
            push(
                errors,
                floc,
                format!("failure_rate must be within 0..=1, got {rate}"),
            );
        }
    }
    if let (Ok(window), Ok(min)) = (
        spec.param_u64("sliding_window"),
        spec.param_u64("min_calls"),
    ) {
        if min > window {
            push(errors, floc, "min_calls must be <= sliding_window");
        }
    }
}

/// `RewritePath`：regex/replacement 必填；正则只校验语法（故意不做全值
/// 锚定——RewritePath 是重写器，允许部分匹配，与 Header/Query 语义不同）。
fn check_rewrite_path(spec: &FilterSpec, floc: &Location, errors: &mut Vec<ConfigError>) {
    let ok = spec.param_str("regex").is_ok() && spec.param_str("replacement").is_ok();
    if !ok {
        push(errors, floc, "RewritePath requires regex and replacement");
        return;
    }
    let pattern = spec.param_str("regex").unwrap_or_default();
    if let Err(m) = regex::Regex::new(pattern) {
        push(errors, floc, format!("RewritePath regex invalid: {m}"));
    }
}

/// `JwtAuth`：issuer / audience / jwks_file 三者必填（缺任一都无法完成
/// 验签：签发者与受众是 claim 校验的锚点，jwks_file 是公钥来源）。
fn check_jwt_auth(spec: &FilterSpec, floc: &Location, errors: &mut Vec<ConfigError>) {
    for (key, label) in [
        ("issuer", "issuer"),
        ("audience", "audience"),
        ("jwks_file", "jwks_file"),
    ] {
        match spec.param_str(key) {
            Ok("") => push(errors, floc, format!("JwtAuth {label} must not be empty")),
            Ok(_) => {}
            Err(_) => push(errors, floc, format!("JwtAuth requires {label}")),
        }
    }
}

/// `ApiKeyAuth`：`keys` 必须是非空 map 且每个 subject 非空；可选 `header`
/// 若给出必须是合法 HTTP 头名（RFC 7230 token）。
fn check_api_key_auth(spec: &FilterSpec, floc: &Location, errors: &mut Vec<ConfigError>) {
    match spec.param_map("keys") {
        Err(_) => push(errors, floc, "ApiKeyAuth requires keys ({secret: subject})"),
        Ok(keys) => {
            if keys.is_empty() {
                push(errors, floc, "ApiKeyAuth keys must not be empty");
            }
            for (secret, subject) in &keys {
                if secret.is_empty() {
                    push(
                        errors,
                        floc,
                        "ApiKeyAuth keys must not contain empty secrets",
                    );
                }
                let subject_ok = subject.as_str().is_some_and(|s| !s.is_empty());
                if !subject_ok {
                    push(
                        errors,
                        floc,
                        format!("ApiKeyAuth subject for key {secret:?} must not be empty"),
                    );
                }
            }
        }
    }
    if let Ok(Some(header)) = spec.param_str_opt("header") {
        if !is_valid_header_name(header) {
            push(
                errors,
                floc,
                format!("ApiKeyAuth header {header:?} is not a valid header name"),
            );
        }
    }
}

/// RFC 7230 token（header 名允许的字符集）：`!#$%&'*+-.^_`|~` 与字母数字。
fn is_valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

/// `IpFilter`：allow/deny 至少一非空；每条必须是合法 IP 或 CIDR
/// （裸 IP 视作单主机网段）。deny 与 allow 同时配置时 deny 优先——
/// 语义在运行时（IpFilter 工厂）执行，这里只做格式把关。
fn check_ip_filter(spec: &FilterSpec, floc: &Location, errors: &mut Vec<ConfigError>) {
    let allow = spec.param_str_list("allow").unwrap_or_default();
    let deny = spec.param_str_list("deny").unwrap_or_default();
    if allow.is_empty() && deny.is_empty() {
        push(
            errors,
            floc,
            "IpFilter requires allow or deny (at least one non-empty)",
        );
    }
    for (list_name, entries) in [("allow", &allow), ("deny", &deny)] {
        for entry in entries.iter() {
            if parse_ip_net(entry).is_none() {
                push(
                    errors,
                    floc,
                    format!("IpFilter {list_name} entry {entry:?} is not a valid IP or CIDR"),
                );
            }
        }
    }
}
