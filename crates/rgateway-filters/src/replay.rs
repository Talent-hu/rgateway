//! 可重放缓冲：请求体的有界收集/重建与响应体 gRPC trailer 探测。
//!
//! # 取舍（为什么要有界）
//!
//! 重试要重放请求体就必须先把它收进内存，收多少就是攻击面。
//! [`FilterBuildCtx::max_retry_buffer`](rgateway_config::FilterBuildCtx::max_retry_buffer)
//! 是可重放缓冲的硬上限：
//!
//! * **已知长度**（Content-Length / 精确 size_hint）超上限的请求一次都
//!   不缓冲，直接单次透传——省掉无谓的拷贝；
//! * **未知长度**有界收集（`max`+1 字节判超，帧边界处零拷贝切片），
//!   超限同样禁用重试，并把已缓冲前缀 + 剩余流按原序拼回透传——
//!   **不丢数据、不无界缓冲**；
//! * 读错误帧原样保留在前缀里随流透传（错误是数据路径的一部分，
//!   不吞错）。
//!
//! 响应侧的 gRPC trailer 探测复用同一上限：trailer 里的 `grpc-status`
//! 要看全响应体才能拿到，超限就放弃判定、按流透传（不重试）。

use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use http_body::{Body as _, Frame};
use http_body_util::BodyExt;
use rgateway_core::{boxed_body, ProxyBody, ProxyError};

use crate::util::{content_length_of, grpc_status_in};

/// 请求体的重放形态。
pub(crate) enum Replay {
    /// 禁用重试：单次透传（与无 Retry 的行为完全一致）。
    Passthrough(http::Request<ProxyBody>),
    /// 可重放：请求头 + 已收齐的完整体字节，每轮重试重建全新 body。
    Buffered {
        /// 保留原请求头/扩展（`Arc<RequestCtx>` 共享，Target 每轮覆写）。
        parts: http::request::Parts,
        /// 收齐的完整请求体。
        body: Bytes,
    },
}

/// 收集请求体以供重放：已知长度超 `max` 直接透传；否则有界收集，
/// 超限/出错时前缀 + 剩余流拼回（见模块文档的取舍）。
pub(crate) async fn prepare(req: http::Request<ProxyBody>, max: usize) -> Replay {
    if known_len(&req).is_some_and(|len| len > max) {
        return Replay::Passthrough(req);
    }
    let (parts, body) = req.into_parts();
    let collected = collect_frames(body, max).await;
    if collected.is_replayable() {
        Replay::Buffered {
            parts,
            body: collected.into_bytes(),
        }
    } else {
        Replay::Passthrough(http::Request::from_parts(parts, collected.into_chain()))
    }
}

/// gRPC trailer 语义探测：有界收集响应体找 trailer 里的 `grpc-status`。
///
/// 返回 `(是否触发重试, 响应)`——响应永远完整可回放（原样或由前缀 +
/// 剩余流重建）；超限/读错误视为不可判定（不重试），按流透传。
pub(crate) async fn peek_trailer(
    resp: http::Response<ProxyBody>,
    grpc_statuses: &[u64],
    max: usize,
) -> (bool, http::Response<ProxyBody>) {
    let (parts, body) = resp.into_parts();
    let collected = collect_frames(body, max).await;
    let retryable = !collected.overflowed()
        && collected
            .grpc_trailer_status()
            .is_some_and(|code| grpc_statuses.contains(&code));
    let resp = http::Response::from_parts(parts, collected.into_chain());
    (retryable, resp)
}

/// 一次有界收集的结果：前缀帧 + 剩余流 + 结束原因。
pub(crate) struct Collected {
    /// 已缓冲帧（顺序保真，含 trailer/错误帧）。
    frames: Vec<Result<Frame<Bytes>, ProxyError>>,
    /// 收齐时合并完整字节用（与 `frames` 共享底层 Bytes，零拷贝）。
    data_parts: Vec<Bytes>,
    /// 未消耗的剩余流（超限时头部含切剩的半个 data 帧）。
    rest: ProxyBody,
    /// 因 `max`+1 判超而中止。
    overflow: bool,
    /// 流自然结束（此路径下前缀只含 data 帧）。
    clean_end: bool,
}

impl Collected {
    /// 流干净结束（全部收齐）时可重放。
    pub(crate) fn is_replayable(&self) -> bool {
        self.clean_end
    }

    /// 是否因缓冲超限而中止。
    pub(crate) fn overflowed(&self) -> bool {
        self.overflow
    }

    /// 合并收齐的 data 帧为完整字节（仅可重放路径调用；体已知在上限内）。
    pub(crate) fn into_bytes(self) -> Bytes {
        let mut buf = BytesMut::new();
        for part in &self.data_parts {
            buf.extend_from_slice(part);
        }
        buf.freeze()
    }

    /// 前缀 + 剩余流拼回成完整 body（不丢数据）。
    pub(crate) fn into_chain(self) -> ProxyBody {
        chain_frames(self.frames, self.rest)
    }

    /// trailer 帧里的 `grpc-status`（trailer 语义；缺失返回 `None`）。
    pub(crate) fn grpc_trailer_status(&self) -> Option<u64> {
        for frame in self.frames.iter().flatten() {
            if let Some(trailers) = frame.trailers_ref() {
                return grpc_status_in(trailers);
            }
        }
        None
    }
}

/// 有界收集 body 帧：最多缓冲 `max`+1 字节判超；到 trailer 帧 / 出错 /
/// 超限即停，剩余流保留。
async fn collect_frames(mut body: ProxyBody, max: usize) -> Collected {
    let mut frames = Vec::new();
    let mut data_parts = Vec::new();
    let mut total = 0usize;
    let mut overflow = false;
    let mut clean_end = false;
    loop {
        match body.frame().await {
            None => {
                clean_end = true;
                break;
            }
            Some(Err(err)) => {
                frames.push(Err(err));
                break;
            }
            Some(Ok(frame)) => match frame.into_data() {
                Ok(data) => {
                    let leftover = absorb_data(&mut frames, &mut data_parts, data, &mut total, max);
                    if !leftover.is_empty() || total > max {
                        overflow = true;
                        if !leftover.is_empty() {
                            body = prepend_frame(Frame::data(leftover), body);
                        }
                        break;
                    }
                }
                // trailer 帧：原样保留并停止（请求/响应 trailer 语义）
                Err(frame) => {
                    frames.push(Ok(frame));
                    break;
                }
            },
        }
    }
    Collected {
        frames,
        data_parts,
        rest: body,
        overflow,
        clean_end,
    }
}

/// 把 data 帧按 `max`+1 上限吸收进前缀（Bytes 零拷贝切片）；
/// 返回切剩未吸收的字节（超限部分，由调用方拼回剩余流）。
fn absorb_data(
    frames: &mut Vec<Result<Frame<Bytes>, ProxyError>>,
    data_parts: &mut Vec<Bytes>,
    mut chunk: Bytes,
    total: &mut usize,
    max: usize,
) -> Bytes {
    if chunk.is_empty() {
        frames.push(Ok(Frame::data(chunk)));
        return Bytes::new();
    }
    while !chunk.is_empty() && *total <= max {
        let room = max.saturating_add(1) - *total;
        let take = room.min(chunk.len());
        let head = chunk.slice(..take);
        chunk = chunk.slice(take..);
        frames.push(Ok(Frame::data(head.clone())));
        data_parts.push(head);
        *total += take;
    }
    chunk
}

/// 把一个帧拼到剩余流前面（overflow 切剩部分归位）。
fn prepend_frame(frame: Frame<Bytes>, rest: ProxyBody) -> ProxyBody {
    let stream = futures_util::stream::iter(std::iter::once(Ok(frame)))
        .chain(http_body_util::BodyStream::new(rest));
    boxed_body(http_body_util::StreamBody::new(stream))
}

/// 前缀帧 + 剩余流按原序拼成 body（不丢数据）。
fn chain_frames(frames: Vec<Result<Frame<Bytes>, ProxyError>>, rest: ProxyBody) -> ProxyBody {
    let stream = futures_util::stream::iter(frames).chain(http_body_util::BodyStream::new(rest));
    boxed_body(http_body_util::StreamBody::new(stream))
}

/// 请求体「已知长度」：Content-Length 优先，其次 body 的精确 size_hint。
fn known_len(req: &http::Request<ProxyBody>) -> Option<usize> {
    if let Some(len) = content_length_of(req) {
        return Some(len);
    }
    req.body()
        .size_hint()
        .exact()
        .and_then(|n| usize::try_from(n).ok())
}

#[cfg(test)]
#[path = "replay_tests.rs"]
mod tests;
