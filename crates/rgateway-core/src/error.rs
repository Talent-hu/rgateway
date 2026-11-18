//! 网关错误层级与协议感知的状态码映射。
//!
//! 过滤器链的任意一层都可以产出 [`GatewayError`]，由代理（proxy）的错误
//! 渲染器按请求协议渲染：
//!
//! * HTTP/1、HTTP/2 —— HTTP status + JSON body；
//! * 原生 gRPC —— `grpc-status` / `grpc-message` trailer；
//! * gRPC-Web —— trailer 帧（二进制帧或 base64 文本帧）。
//!
//! # 映射总表（唯一权威来源）
//!
//! | 变体 | [`ErrorKind`] | grpc-status | HTTP status | 客户端文案 |
//! |------|---------------|-------------|-------------|------------|
//! | `Unavailable` | `Unavailable` | 14 UNAVAILABLE | 503 | `upstream unavailable` |
//! | `DeadlineExceeded` | `DeadlineExceeded` | 4 DEADLINE_EXCEEDED | 504 | `deadline exceeded` |
//! | `RateLimited` | `RateLimited` | 8 RESOURCE_EXHAUSTED | 429 | `rate limit exceeded` |
//! | `CircuitOpen` | `CircuitOpen` | 14 UNAVAILABLE | 503 | `circuit breaker open` |
//! | `Unauthenticated` | `Unauthenticated` | 16 UNAUTHENTICATED | 401 | `unauthenticated` |
//! | `PermissionDenied` | `PermissionDenied` | 7 PERMISSION_DENIED | 403 | `permission denied` |
//! | `NotFound` | `NotFound` | 12 UNIMPLEMENTED | 404 | `not found` |
//! | `InvalidArgument` | `InvalidArgument` | 3 INVALID_ARGUMENT | 400 | `invalid argument` |
//! | `Internal` | `Internal` | 13 INTERNAL | 500 | `internal error` |
//!
//! # 消息的两条通道（核心契约）
//!
//! * **客户端可见通道**：[`GatewayError::public_message`]（HTTP JSON body）
//!   与 [`GatewayError::grpc_message`]（`grpc-message` /
//!   `grpc-status-details-bin` 文本）。两者都是**泛化的静态文案**，绝不包含
//!   内部细节（上游 host:port、DNS 错误、实例 id、io 错误原文等）。
//! * **日志通道**：`Display` / `Debug`（thiserror 生成），携带完整内部细节，
//!   **仅**写日志，绝不出现在任何响应里。
//!
//! 变体携带的 `String` 一律是内部细节文本：进日志，不进响应。

use http::StatusCode;

/// 网关错误的粗粒度分类（`Copy`，可安全用于指标标签与错误匹配）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// 上游不可达，或重试次数耗尽。
    Unavailable,
    /// 总截止期（Deadline）或单次尝试超时已到。
    DeadlineExceeded,
    /// 被限流（rate limiting）拒绝。
    RateLimited,
    /// 熔断器（Circuit Breaker）处于 Open 状态。
    CircuitOpen,
    /// 缺少或无效的凭据。
    Unauthenticated,
    /// 已认证但未获授权。
    PermissionDenied,
    /// 没有路由匹配，或目标方法未知。
    NotFound,
    /// 请求校验 / 参数绑定失败。
    InvalidArgument,
    /// 意外的内部故障（对外文案统一泛化）。
    Internal,
}

/// 协议无关的网关错误。
///
/// 九个变体与 [`ErrorKind`] 一一对应，携带的 `String` 是内部细节。
///
/// **`Display` 文本是内部细节**：任何要发给客户端的内容都必须走
/// [`GatewayError::public_message`] 或 [`GatewayError::grpc_message`]，
/// 二者保证泛化、不泄露内部信息。
#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    /// 上游不可达，或所有重试尝试均失败。
    #[error("upstream unavailable: {0}")]
    Unavailable(String),
    /// 总截止期（Deadline）或单次尝试超时已到。
    #[error("deadline exceeded: {0}")]
    DeadlineExceeded(String),
    /// 被限流过滤器拒绝。
    #[error("rate limited: {0}")]
    RateLimited(String),
    /// 熔断器（Circuit Breaker）Open，拒绝发往该实例的流量。
    #[error("circuit open: {0}")]
    CircuitOpen(String),
    /// 未通过认证：缺少或无效的凭据。
    #[error("unauthenticated: {0}")]
    Unauthenticated(String),
    /// 已认证但无权限访问目标资源。
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    /// 没有路由匹配，或目标方法未知。
    #[error("not found: {0}")]
    NotFound(String),
    /// 请求校验 / 参数绑定失败。
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    /// 意外的内部故障（对外文案统一泛化为 "internal error"）。
    #[error("internal error: {0}")]
    Internal(String),
}

impl GatewayError {
    /// 归类为 [`ErrorKind`]（变体与 kind 一一对应，纯查表、无分配）。
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::Unavailable(_) => ErrorKind::Unavailable,
            Self::DeadlineExceeded(_) => ErrorKind::DeadlineExceeded,
            Self::RateLimited(_) => ErrorKind::RateLimited,
            Self::CircuitOpen(_) => ErrorKind::CircuitOpen,
            Self::Unauthenticated(_) => ErrorKind::Unauthenticated,
            Self::PermissionDenied(_) => ErrorKind::PermissionDenied,
            Self::NotFound(_) => ErrorKind::NotFound,
            Self::InvalidArgument(_) => ErrorKind::InvalidArgument,
            Self::Internal(_) => ErrorKind::Internal,
        }
    }

    /// gRPC status code，按网关错误映射表：
    ///
    /// | 错误变体 | grpc-status |
    /// |----------|-------------|
    /// | Unavailable | 14 UNAVAILABLE |
    /// | DeadlineExceeded | 4 DEADLINE_EXCEEDED |
    /// | RateLimited | 8 RESOURCE_EXHAUSTED |
    /// | CircuitOpen | 14（`grpc-message` 另注明熔断打开） |
    /// | Unauthenticated | 16 UNAUTHENTICATED |
    /// | PermissionDenied | 7 PERMISSION_DENIED |
    /// | NotFound | 12 UNIMPLEMENTED |
    /// | InvalidArgument | 3 INVALID_ARGUMENT |
    /// | Internal | 13 INTERNAL |
    ///
    /// 设计取舍：`CircuitOpen` 与 `Unavailable` 同映射 14，因为客户端视角
    /// 都是"当前无法访问上游"；熔断这一内部状态只在
    /// [`GatewayError::grpc_message`] 里附加说明，不另占 status code。
    pub fn grpc_status(&self) -> i32 {
        match self {
            Self::Unavailable(_) => 14,
            Self::DeadlineExceeded(_) => 4,
            Self::RateLimited(_) => 8,
            Self::CircuitOpen(_) => 14,
            Self::Unauthenticated(_) => 16,
            Self::PermissionDenied(_) => 7,
            Self::NotFound(_) => 12,
            Self::InvalidArgument(_) => 3,
            Self::Internal(_) => 13,
        }
    }

    /// HTTP status，用于 HTTP/1-2 响应与 gRPC-JSON 转码（transcoding）响应：
    ///
    /// | 错误变体 | HTTP status |
    /// |----------|-------------|
    /// | Unavailable | 503 SERVICE_UNAVAILABLE |
    /// | DeadlineExceeded | 504 GATEWAY_TIMEOUT |
    /// | RateLimited | 429 TOO_MANY_REQUESTS |
    /// | CircuitOpen | 503 SERVICE_UNAVAILABLE |
    /// | Unauthenticated | 401 UNAUTHORIZED |
    /// | PermissionDenied | 403 FORBIDDEN |
    /// | NotFound | 404 NOT_FOUND |
    /// | InvalidArgument | 400 BAD_REQUEST |
    /// | Internal | 500 INTERNAL_SERVER_ERROR |
    pub fn http_status(&self) -> StatusCode {
        match self {
            Self::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::DeadlineExceeded(_) => StatusCode::GATEWAY_TIMEOUT,
            Self::RateLimited(_) => StatusCode::TOO_MANY_REQUESTS,
            Self::CircuitOpen(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Unauthenticated(_) => StatusCode::UNAUTHORIZED,
            Self::PermissionDenied(_) => StatusCode::FORBIDDEN,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::InvalidArgument(_) => StatusCode::BAD_REQUEST,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// 泛化的客户端文案（HTTP JSON body 用），返回静态字符串、零分配。
    ///
    /// 契约：内部细节绝不从这里泄露——变体携带的 `String` 不参与返回值。
    pub fn public_message(&self) -> &'static str {
        match self {
            Self::Unavailable(_) => "upstream unavailable",
            Self::DeadlineExceeded(_) => "deadline exceeded",
            Self::RateLimited(_) => "rate limit exceeded",
            Self::CircuitOpen(_) => "circuit breaker open",
            Self::Unauthenticated(_) => "unauthenticated",
            Self::PermissionDenied(_) => "permission denied",
            Self::NotFound(_) => "not found",
            Self::InvalidArgument(_) => "invalid argument",
            Self::Internal(_) => "internal error",
        }
    }

    /// `grpc-message` / `grpc-status-details-bin` 的消息文本。
    ///
    /// 契约：除 [`ErrorKind::CircuitOpen`] 外，每个变体的措辞与
    /// [`Self::public_message`] 完全一致；`CircuitOpen` 额外注明熔断已打开
    /// ——这是两个方法之间**唯一**允许的差异。该文本客户端可见，因此绝不
    /// 能包含内部细节（上游 host:port、DNS 错误、实例 id、io 错误原文等）；
    /// 内部细节只保留给日志，通过 `Display` / `Debug` 实现获取。
    pub fn grpc_message(&self) -> String {
        match self {
            Self::CircuitOpen(_) => "circuit breaker open: circuit is open".to_string(),
            other => other.public_message().to_string(),
        }
    }

    /// 便捷构造器：带格式化内部上下文（内部上下文仅进日志）。
    pub fn unavailable(msg: impl Into<String>) -> Self {
        Self::Unavailable(msg.into())
    }

    /// 便捷构造器，见 [`GatewayError::unavailable`]。
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }

    /// 便捷构造器，见 [`GatewayError::unavailable`]。
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::NotFound(msg.into())
    }

    /// 便捷构造器，见 [`GatewayError::unavailable`]。
    pub fn invalid_argument(msg: impl Into<String>) -> Self {
        Self::InvalidArgument(msg.into())
    }
}

impl From<std::io::Error> for GatewayError {
    // io 错误统一归入 Internal：错误原文保留在消息文本里，仅走日志通道。
    fn from(err: std::io::Error) -> Self {
        Self::Internal(format!("io error: {err}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 构造覆盖全部九个变体的样本集。载荷统一用 "SECRET_DETAIL" 伪装成
    // "内部细节"，用于在客户端可见通道里做泄露检测；顺序与映射总表一致。
    fn all_kinds() -> Vec<GatewayError> {
        vec![
            GatewayError::Unavailable("SECRET_DETAIL".into()),
            GatewayError::DeadlineExceeded("SECRET_DETAIL".into()),
            GatewayError::RateLimited("SECRET_DETAIL".into()),
            GatewayError::CircuitOpen("SECRET_DETAIL".into()),
            GatewayError::Unauthenticated("SECRET_DETAIL".into()),
            GatewayError::PermissionDenied("SECRET_DETAIL".into()),
            GatewayError::NotFound("SECRET_DETAIL".into()),
            GatewayError::InvalidArgument("SECRET_DETAIL".into()),
            GatewayError::Internal("SECRET_DETAIL".into()),
        ]
    }

    // 表驱动校验 grpc-status 映射全表。每行是 (期望的 ErrorKind,
    // 期望的 grpc-status 码)，按 all_kinds() 顺序一一配对。
    #[test]
    fn grpc_status_mapping_table() {
        let expected = [
            (ErrorKind::Unavailable, 14),
            (ErrorKind::DeadlineExceeded, 4),
            (ErrorKind::RateLimited, 8),
            (ErrorKind::CircuitOpen, 14),
            (ErrorKind::Unauthenticated, 16),
            (ErrorKind::PermissionDenied, 7),
            (ErrorKind::NotFound, 12),
            (ErrorKind::InvalidArgument, 3),
            (ErrorKind::Internal, 13),
        ];
        for (err, (kind, status)) in all_kinds().into_iter().zip(expected) {
            assert_eq!(err.kind(), kind);
            assert_eq!(err.grpc_status(), status);
        }
    }

    // 表驱动校验 HTTP status 映射全表：单列数值 = 期望的 HTTP 状态码，
    // 按 all_kinds() 顺序一一配对。
    #[test]
    fn http_status_mapping_table() {
        let expected = [503, 504, 429, 503, 401, 403, 404, 400, 500];
        for (err, status) in all_kinds().into_iter().zip(expected) {
            assert_eq!(err.http_status().as_u16(), status);
        }
    }

    // 验证 public_message 文案非空且绝不泄露内部细节（SECRET_DETAIL 不得出现）。
    #[test]
    fn public_messages_are_generic() {
        for err in all_kinds() {
            let msg = err.public_message();
            assert!(!msg.is_empty());
            // 内部细节绝不能从 public_message 泄露
            assert!(!msg.contains("SECRET_DETAIL"), "leaked detail: {msg}");
        }
    }

    // 验证 CircuitOpen 的 grpc_message 额外注明熔断已打开（唯一允许的措辞差异）。
    #[test]
    fn circuit_open_grpc_message_notes_open() {
        let err = GatewayError::CircuitOpen("user-svc".into());
        assert!(err.grpc_message().contains("circuit breaker open"));
    }

    // grpc_message（客户端可见的 grpc-message / grpc-status-details-bin 文本）
    // 的表驱动契约：不得泄露内部细节，且除 CircuitOpen（额外注明熔断打开）
    // 外措辞必须与 public_message 完全一致。
    #[test]
    fn grpc_messages_are_generic() {
        for err in all_kinds() {
            let msg = err.grpc_message();
            assert!(!msg.is_empty());
            // 内部细节绝不能泄露进客户端可见的 grpc-message
            assert!(!msg.contains("SECRET_DETAIL"), "leaked detail: {msg}");
            if err.kind() == ErrorKind::CircuitOpen {
                // 与 public_message 唯一允许的差异：额外注明熔断已打开
                assert!(
                    msg.contains("circuit breaker open"),
                    "missing open-circuit note: {msg}"
                );
            } else {
                assert_eq!(
                    msg,
                    err.public_message(),
                    "grpc wording diverged from public_message for {:?}",
                    err.kind()
                );
            }
        }
    }

    // 验证 Display 作为日志通道必须保留完整内部细节（SECRET_DETAIL 必须出现），
    // 与 grpc_message 的泛化形成对照。
    #[test]
    fn display_keeps_internal_detail_for_logs() {
        for err in all_kinds() {
            let text = err.to_string();
            assert!(
                text.contains("SECRET_DETAIL"),
                "log channel lost detail: {text}"
            );
        }
    }
}
