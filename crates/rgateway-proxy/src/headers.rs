//! hop-by-hop 头治理与代理（forwarding）头维护。
//!
//! 代理转发两侧的语义差异集中在这里处理：
//!
//! * **hop-by-hop 头**（RFC 9110 §7.6.1）描述的是「这一跳」的连接属性
//!   （`Connection`、`Transfer-Encoding`、`Upgrade` 等），跨代理转发必须剥离，
//!   否则上游会把上一跳的连接语义误当成自己的；
//!   `Connection` 头里列出的 token 是逐请求**动态**的 hop-by-hop 头，一并剥离。
//! * **代理头**（`X-Request-ID` / `X-Forwarded-*`）逐跳覆写或追加，让上游能
//!   还原客户端链路；原始 `Host` 头**保留不动**（对齐 Spring Cloud Gateway：
//!   上游按原 Host 做虚拟主机路由）。

use std::net::SocketAddr;

use http::{HeaderMap, HeaderName, HeaderValue};

/// hop-by-hop 头固定清单（RFC 9110 §7.6.1 + 常见代理扩展），与
/// `Connection` token 动态清单合并生效。`proxy-connection` 是事实标准
/// （非 RFC，但业界代理一律剥离），一并列入。
const HOP_BY_HOP: [&str; 9] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// `X-Request-ID` 头名（关联 id，日志与链路追踪用）。
const X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");
/// `X-Forwarded-Proto` 头名。
const X_FORWARDED_PROTO: HeaderName = HeaderName::from_static("x-forwarded-proto");
/// `X-Forwarded-Host` 头名。
const X_FORWARDED_HOST: HeaderName = HeaderName::from_static("x-forwarded-host");
/// `X-Forwarded-For` 头名。
const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");
/// RFC 7239 的 `Forwarded` 头名（客户端可伪造，出站一律剥离）。
const FORWARDED: HeaderName = HeaderName::from_static("forwarded");

/// 把文本安全装入 [`HeaderValue`]。
///
/// 正常来源（uuid、泛化静态文案、host）都是合法字节；这里仍做防御性脱敏——
/// 控制字符在 HTTP 头里非法，替换为 `?` 后重新装箱，**绝不 panic**，
/// 也绝不把非法字节悄悄丢成空值。
pub(crate) fn header_value(text: &str) -> HeaderValue {
    if let Ok(value) = HeaderValue::from_str(text) {
        return value;
    }
    let sanitized: Vec<u8> = text
        .bytes()
        .map(|b| {
            if b.is_ascii_control() && b != b'\t' {
                b'?'
            } else {
                b
            }
        })
        .collect();
    // 脱敏后不含非法字节，转换必成功；兜底分支防御性返回空值（不可达）。
    HeaderValue::from_bytes(&sanitized).unwrap_or_else(|_| HeaderValue::from_static(""))
}

/// 剥离全部 hop-by-hop 头：固定清单 + `Connection` 头里列出的 token
/// （逗号分隔、trim、大小写不敏感，多条 `Connection` 头合并处理）。
///
/// token 是客户端可控输入：非法头名（如含空格）安全忽略而不是 panic。
pub fn strip_hop_by_hop(headers: &mut HeaderMap) {
    // 必须先收集 token 再移除：移除 Connection 头后就读不到清单了。
    let tokens = connection_tokens(headers);
    for name in HOP_BY_HOP {
        headers.remove(name);
    }
    for token in tokens {
        if let Ok(name) = HeaderName::from_bytes(token.as_bytes()) {
            headers.remove(name);
        }
    }
}

/// 收集 `Connection` 头列出的全部 token（已 trim + 小写归一，空 token 丢弃）。
fn connection_tokens(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(http::header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

/// 准备发往上游的请求头。
///
/// 语义（对齐 Spring Cloud Gateway）：
///
/// 1. 先 [`strip_hop_by_hop`]；
/// 2. `X-Request-ID` **覆写**为本次请求的关联 id（不信客户端传入的值）；
/// 3. `X-Forwarded-Proto` **覆写**为客户端接入本网关的 scheme（`http` /
///    `https`，由监听形态决定）——不信客户端传入的值；
/// 4. `X-Forwarded-Host` **无条件覆写**为本次请求的原始 Host（与
///    `X-Request-ID` / `X-Forwarded-Proto` 同语义，不信客户端传入的值）；
///    `original_host` 为 `None` 时**移除**客户端值——没有原始 Host 就绝不
///    转发任何 `X-Forwarded-Host`；
/// 5. RFC 7239 的 `Forwarded` 头**无条件剥离**：它与 `X-Forwarded-*` 承载
///    同类链路信息，客户端可伪造，逐跳覆写语义下没有保留的余地；
/// 6. `X-Forwarded-For` 追加 `peer` 的 **IP**（不含端口）：已有值用 `", "`
///    逗号追加保持既有链，无 `peer` 则完全不动；
/// 7. `Host` 头：有值时保留不动（上游按原 Host 做虚拟主机路由）；**缺失时
///    用 `original_host` 补写**——HTTP/2 入站的 authority 走 `:authority`
///    伪头、没有 `Host`，而出站已归一为 HTTP/1.1 必须携带 `Host`，不补写
///    的话上游只能看到 `ip:port`、虚拟主机路由全部失效。`original_host`
///    也为 `None` 时不伪造。
pub fn prepare_upstream(
    headers: &mut HeaderMap,
    peer: Option<SocketAddr>,
    original_host: Option<&str>,
    request_id: &str,
    client_scheme: &str,
) {
    strip_hop_by_hop(headers);
    headers.remove(&FORWARDED);
    headers.insert(X_REQUEST_ID, header_value(request_id));
    headers.insert(X_FORWARDED_PROTO, header_value(client_scheme));
    match original_host {
        Some(host) => {
            headers.insert(X_FORWARDED_HOST, header_value(host));
            if !headers.contains_key(http::header::HOST) {
                headers.insert(http::header::HOST, header_value(host));
            }
        }
        None => {
            headers.remove(&X_FORWARDED_HOST);
        }
    };
    if let Some(addr) = peer {
        append_forwarded_for(headers, addr.ip().to_string());
    }
}

/// 向 `X-Forwarded-For` 追加一跳 IP：多条既有头先合并成一条链再追加，
/// 保证链序就是经过顺序。
fn append_forwarded_for(headers: &mut HeaderMap, ip: String) {
    let existing: Vec<&str> = headers
        .get_all(&X_FORWARDED_FOR)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    let chain = if existing.is_empty() {
        ip
    } else {
        format!("{}, {}", existing.join(", "), ip)
    };
    headers.insert(X_FORWARDED_FOR, header_value(&chain));
}

/// 准备发回下游的响应头：剥离 hop-by-hop 头后覆写 `X-Request-ID` 为本次请求
/// 的关联 id（响应侧不改写 `X-Forwarded-*`——那是请求方向的链路信息）。
pub fn finalize_response(headers: &mut HeaderMap, request_id: &str) {
    strip_hop_by_hop(headers);
    headers.insert(X_REQUEST_ID, header_value(request_id));
}

#[cfg(test)]
#[path = "headers_tests.rs"]
mod tests;
