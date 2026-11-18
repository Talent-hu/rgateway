//! 请求体/响应体（body）与服务（service）的共享类型别名。
//!
//! 代理转发的 body 统一为持有 [`bytes::Bytes`] 块的拥有型（owned）流：
//! hyper 与过滤器链之间传递的是引用计数的 `Bytes` 块，数据路径零拷贝。
//!
//! 两类错误**刻意**保持区分，不允许混用：
//!
//! * [`ProxyError`] —— body 流在**转发过程中**（数据路径）抛出的错误，
//!   作为 `http_body::Body::Error` 随帧一起冒泡；
//! * [`GatewayError`] —— **服务链**（过滤器链）返回的错误（控制路径），
//!   由协议感知的错误渲染器按 HTTP / gRPC / gRPC-Web 渲染成状态码与消息。
//!
//! # 类型契约
//!
//! * [`ProxyBody`] = `UnsyncBoxBody<Bytes, ProxyError>`：刻意选用 **Unsync**
//!   版本的装箱 body。链上的 body 只要求 `Send`（可跨线程移动），并不要求
//!   `Sync`（可共享引用）；`UnsyncBoxBody` 的动态分发更轻，且允许内部缓冲
//!   是非 `Sync` 的。因此 `ProxyBody` 只能按值传递，不能以共享引用跨线程。
//! * [`BoxedService`] = `BoxCloneService<...>`：同样 `Send` 而**非** `Sync`，
//!   克隆是一次廉价的 box 克隆（重新装箱一个内部克隆）。这就是
//!   [`crate::route::Route::chain`] 用 `Mutex` 包裹过滤器链的根本原因：
//!   `Mutex` 能把非 `Sync` 的服务包装成 `Sync` 字段，使 `Arc<Route>` 可
//!   在多线程间安全共享。

use bytes::Bytes;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full, StreamBody};

use crate::error::GatewayError;

/// body 流携带的错误类型（数据路径错误）。
///
/// 与 [`GatewayError`] 的分工：`ProxyError` 出现在响应已经/正在流式转发的
/// 阶段（此时状态码往往已发出，只能中断流或补写 gRPC trailer）；
/// `GatewayError` 出现在请求处理、响应尚未开始的阶段（可以完整渲染错误）。
///
/// 各变体携带的字符串是内部细节，仅供日志使用——对外消息一律经
/// [`ProxyError::into_gateway_error`] 归一成 [`GatewayError`] 后走泛化文案。
#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    /// 上游（upstream）body 上报的失败：连接被重置、响应体中途断开等。
    #[error("upstream error: {0}")]
    Upstream(String),
    /// 本地 I/O 失败（连接重置等），由 `std::io::Error` 自动转换而来。
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// 嵌入 body 流的网关级错误（罕见：错误在流式阶段才被发现时）。
    #[error("{0}")]
    Gateway(#[from] GatewayError),
}

impl ProxyError {
    /// 归一（collapse）为 [`GatewayError`]，交给错误渲染器统一映射状态码。
    ///
    /// 映射规则：`Gateway` 原样透传；`Upstream` / `Io` 归入
    /// [`GatewayError::Unavailable`]（客户端视角都是"上游暂不可用"，
    /// 内部细节保留在消息文本中，仅进日志）。
    pub fn into_gateway_error(self) -> GatewayError {
        match self {
            Self::Gateway(g) => g,
            Self::Upstream(msg) => GatewayError::Unavailable(msg),
            Self::Io(e) => GatewayError::Unavailable(format!("io error: {e}")),
        }
    }
}

impl From<hyper::Error> for ProxyError {
    // hyper 传输层错误（协议错乱、连接断开等）统一降级为 Upstream 字符串：
    // hyper::Error 没有可安全外泄的结构化细节，且本质属于上游数据路径故障。
    fn from(err: hyper::Error) -> Self {
        Self::Upstream(err.to_string())
    }
}

impl From<std::convert::Infallible> for ProxyError {
    // 有些 body（如 `Full`）的错误类型是 `Infallible`（不可能出错）。
    // 此 impl 让 `boxed_body` 的 `B::Error: Into<ProxyError>` 约束对它们
    // 自动成立，调用方无须为无错误 body 写特例。
    fn from(never: std::convert::Infallible) -> Self {
        // `Infallible` 无任何实例（uninhabited），`match never {}` 是穷举，
        // 运行期永远不会真正执行到这里。
        match never {}
    }
}

/// 过滤器链与上游连接中流动的 body 类型。
///
/// 即 `UnsyncBoxBody<Bytes, ProxyError>`：装箱后的类型擦除 body，错误统一
/// 为 [`ProxyError`]。选用 **Unsync** 变体的原因见模块文档"类型契约"一节。
pub type ProxyBody = UnsyncBoxBody<Bytes, ProxyError>;

/// 预编译、可克隆的过滤器链服务类型。
///
/// 在配置编译期（config compile）一次性组装完成，请求处理时零动态过滤器
/// 查找。类型为 `BoxCloneService`：`Send` 但**非** `Sync`，只能按值在各层
/// 间移动，克隆是廉价的 box 克隆——详见 [`crate::route::Route::chain`] 对
/// `Mutex` 包裹原因的说明。
pub type BoxedService =
    tower::util::BoxCloneService<http::Request<ProxyBody>, http::Response<ProxyBody>, GatewayError>;

/// 把任意 body 装箱为 [`ProxyBody`]。
///
/// 先用 `Into<ProxyError>` 归一错误类型，再 `boxed_unsync` 装箱。要求
/// `B: Send + 'static`（跨线程移动 + 存进装箱对象），数据块必须是
/// [`bytes::Bytes`]（零拷贝共享）。
pub fn boxed_body<B>(body: B) -> ProxyBody
where
    B: http_body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<ProxyError>,
{
    body.map_err(Into::into).boxed_unsync()
}

/// 持有单个数据块的 body（小 JSON 载荷、错误响应体等）。
pub fn full_body(data: Bytes) -> ProxyBody {
    boxed_body(Full::new(data))
}

/// 空 body（204/304 风格的响应）。
pub fn empty_body() -> ProxyBody {
    boxed_body(Full::new(Bytes::new()))
}

/// 把 body 帧（frame）流包装成 [`ProxyBody`]。
///
/// 流的每一项是 `Result<Frame<Bytes>, ProxyError>`：`Err` 会中止整个 body
/// 流并向上传播 [`ProxyError`]。
pub fn stream_body<S>(frames: S) -> ProxyBody
where
    S: futures_util::Stream<Item = Result<http_body::Frame<Bytes>, ProxyError>> + Send + 'static,
{
    boxed_body(StreamBody::new(frames))
}

/// 在保留数据路径的前提下把 body 错误映射为 [`ProxyError`]。
/// 就是 [`boxed_body`] 的别名，仅为调用点可读性而保留。
pub fn map_body_err<B>(body: B) -> ProxyBody
where
    B: http_body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<ProxyError>,
{
    boxed_body(body)
}

/// 测试便捷构造：用静态字符串造一个 body。
#[cfg(test)]
pub fn test_body(s: &str) -> ProxyBody {
    full_body(Bytes::copy_from_slice(s.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    // 验证 full_body 产出的单帧数据与输入字节完全一致（字节往返无损）。
    #[tokio::test]
    async fn full_body_round_trips_bytes() {
        let mut body = full_body(Bytes::from_static(b"hello"));
        let chunk = body.frame().await.unwrap().unwrap();
        assert_eq!(chunk.into_data().unwrap(), Bytes::from_static(b"hello"));
    }

    // 验证 empty_body 不产出任何帧（`frame()` 返回 None 即流结束）。
    #[tokio::test]
    async fn empty_body_has_no_frames() {
        let mut body = empty_body();
        assert!(body.frame().await.is_none());
    }

    // 验证 into_gateway_error 的归一映射：Upstream → Unavailable，
    // 内嵌的 Gateway 错误原样保留其 ErrorKind。
    #[test]
    fn proxy_error_collapses_to_gateway_error() {
        let g = ProxyError::Upstream("boom".into()).into_gateway_error();
        assert_eq!(g.kind(), crate::error::ErrorKind::Unavailable);
        let g = ProxyError::Gateway(GatewayError::RateLimited("x".into())).into_gateway_error();
        assert_eq!(g.kind(), crate::error::ErrorKind::RateLimited);
    }
}
