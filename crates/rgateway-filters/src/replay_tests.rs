//! 可重放缓冲测试：有界收集（max+1 判超）、超限前缀 + 剩余流拼回、
//! 错误/trailer 帧保真、gRPC trailer 探测与响应重建。

use bytes::Bytes;
use http_body::Frame;
use http_body_util::BodyExt;
use rgateway_core::body::stream_body;
use rgateway_core::{empty_body, ProxyBody, ProxyError};

use super::*;

/// 多帧请求体（未知长度，无 Content-Length）：每项是一帧字节。
fn multi_frame(frames: &[&'static str]) -> ProxyBody {
    let items: Vec<Result<Frame<Bytes>, ProxyError>> = frames
        .iter()
        .map(|f| Ok(Frame::data(Bytes::from_static(f.as_bytes()))))
        .collect();
    stream_body(futures_util::stream::iter(items))
}

/// 收集完整体为字节（断言用）。
async fn collect_bytes(body: ProxyBody) -> Vec<u8> {
    match body.collect().await {
        Ok(collected) => collected.to_bytes().to_vec(),
        Err(err) => panic!("body must collect: {err}"),
    }
}

/// 有界收集表格：每行 = (帧序列, 上限, 是否可重放, 透传后完整字节)。
/// 列含义：frames = 请求体帧；max = max_retry_buffer；replay = 是否收齐
/// 为可重放缓冲；bytes = 无论走哪条路径都必须一字不差出现的完整内容。
#[tokio::test]
async fn bounded_collect_table() {
    // 每行 = (帧序列, max, 可重放?, 完整字节)
    let cases: &[(&[&'static str], usize, bool, &[u8])] = &[
        // 收齐：2 帧 5 字节 <= 5
        (&["abc", "de"], 5, true, b"abcde"),
        // 恰好等于上限：5 字节 = 5
        (&["abcde"], 5, true, b"abcde"),
        // 空体：0 字节恒可重放
        (&[], 5, true, b""),
        // 超限：6 字节 > 5（max+1 判超），前缀 + 剩余流拼回不丢数据
        (&["abc", "def"], 5, false, b"abcdef"),
        // 超限且跨帧：3 帧 9 字节 > 2，前缀 3 字节 + 剩余 6 字节
        (&["abc", "def", "ghi"], 2, false, b"abcdefghi"),
    ];
    for (frames, max, want_replay, want_bytes) in cases {
        let req = http::Request::new(multi_frame(frames));
        match prepare(req, *max).await {
            Replay::Buffered { body, .. } => {
                assert!(want_replay, "frames {frames:?} max={max}: must not buffer");
                assert_eq!(&body[..], *want_bytes, "frames {frames:?} max={max}");
            }
            Replay::Passthrough(req) => {
                assert!(
                    !want_replay,
                    "frames {frames:?} max={max}: must be replayable"
                );
                let got = collect_bytes(req.into_body()).await;
                assert_eq!(
                    &got[..],
                    *want_bytes,
                    "frames {frames:?} max={max}: data loss"
                );
            }
        }
    }
}

/// 已知长度（Content-Length）超上限：一次都不缓冲，请求原样透传。
#[tokio::test]
async fn known_overlong_body_passes_through_untouched() {
    let body = multi_frame(&["abcde"]);
    let mut req = http::Request::new(body);
    req.headers_mut().insert(
        http::header::CONTENT_LENGTH,
        http::HeaderValue::from_static("5"),
    );
    match prepare(req, 4).await {
        Replay::Passthrough(req) => {
            let got = collect_bytes(req.into_body()).await;
            assert_eq!(&got[..], b"abcde");
        }
        Replay::Buffered { .. } => panic!("known overlong body must not be buffered"),
    }
}

/// 读错误帧保真：前缀字节 + 错误按原序透传（禁用重试也不得丢数据/吞错）。
#[tokio::test]
async fn error_frame_is_preserved_in_passthrough() {
    let frames = futures_util::stream::iter(vec![
        Ok(Frame::data(Bytes::from_static(b"ab"))),
        Err(ProxyError::Upstream("boom".to_string())),
    ]);
    let req = http::Request::new(stream_body(frames));
    match prepare(req, 1024).await {
        Replay::Passthrough(req) => {
            let mut body = req.into_body();
            let first = body.frame().await.expect("prefix frame");
            assert_eq!(
                first.expect("ok frame").into_data().expect("data"),
                Bytes::from_static(b"ab")
            );
            let second = body.frame().await.expect("error frame");
            assert!(second.is_err(), "error must surface downstream");
        }
        Replay::Buffered { .. } => panic!("errored body must not be replayable"),
    }
}

/// 请求体 trailer 帧保真：不并入可重放缓冲，原样透传。
#[tokio::test]
async fn trailer_frame_forces_passthrough() {
    let mut trailers = http::HeaderMap::new();
    trailers.insert("x-checksum", http::HeaderValue::from_static("1"));
    let frames = futures_util::stream::iter(vec![
        Ok(Frame::data(Bytes::from_static(b"payload"))),
        Ok(Frame::trailers(trailers)),
    ]);
    let req = http::Request::new(stream_body(frames));
    match prepare(req, 1024).await {
        Replay::Passthrough(req) => {
            let collected = req.into_body().collect().await.expect("collect");
            let got = collected.trailers().expect("trailers kept");
            assert_eq!(got.get("x-checksum").map(|v| v.as_bytes()), Some(&b"1"[..]));
            assert_eq!(collected.to_bytes(), Bytes::from_static(b"payload"));
        }
        Replay::Buffered { .. } => panic!("body with trailers must not be replayable"),
    }
}

/// 按形态名构造探测用响应体（拆出使决策表函数保持 <50 行）。
/// 返回 (响应体, 期望回放前缀)。形态：trailer:<code> / none / overflow / error。
fn shape_body(shape: &str) -> (ProxyBody, &'static [u8]) {
    match shape {
        "trailer:0" | "trailer:13" | "trailer:14" => {
            let code = shape.strip_prefix("trailer:").expect("prefix");
            let mut trailers = http::HeaderMap::new();
            trailers.insert(
                "grpc-status",
                http::HeaderValue::from_str(code).expect("status header"),
            );
            let frames = futures_util::stream::iter(vec![
                Ok(Frame::data(Bytes::from_static(b"payload"))),
                Ok(Frame::trailers(trailers)),
            ]);
            (stream_body(frames), &b"payload"[..])
        }
        "none" => (multi_frame(&["payload"]), &b"payload"[..]),
        "overflow" => (multi_frame(&["12345678"]), &b"12345678"[..]),
        "error" => {
            let frames = futures_util::stream::iter(vec![
                Ok(Frame::data(Bytes::from_static(b"pay"))),
                Err(ProxyError::Upstream("mid-stream".to_string())),
            ]);
            (stream_body(frames), &b"pay"[..])
        }
        other => panic!("unknown shape {other}"),
    }
}

/// trailer 探测表格：每行 = (grpc_statuses, 响应形态, 是否可重试)。
/// 列含义：codes = 配置的 grpc-status 列表；shape = 响应体形态
/// （trailer 码 / 无 trailer / 超限 / 读错误）；retryable = 判定结果。
#[tokio::test]
async fn peek_trailer_decision_table() {
    // 每行 = (codes, 形态: trailer:<code> | none | overflow | error, 可重试?)
    let cases: &[(&[u64], &str, bool)] = &[
        (&[14], "trailer:14", true),
        (&[14], "trailer:0", false),
        (&[14], "trailer:13", false),
        (&[], "trailer:14", false),
        (&[14], "none", false),
        (&[14], "overflow", false),
        (&[14], "error", false),
    ];
    for (codes, shape, want_retryable) in cases {
        let (body, want_prefix) = shape_body(shape);
        let resp = http::Response::new(body);
        // `overflow` 形态刻意超限（上限 4）；其余形态给足缓冲只测判定。
        let cap = if *shape == "overflow" { 4 } else { 1024 };
        let (retryable, resp) = peek_trailer(resp, codes, cap).await;
        assert_eq!(retryable, *want_retryable, "codes={codes:?} shape={shape}");
        // 无论判定如何，响应体必须完整可回放（数据不丢）
        match resp.into_body().collect().await {
            Ok(collected) => assert_eq!(
                &collected.to_bytes()[..want_prefix.len()],
                want_prefix,
                "codes={codes:?} shape={shape}: data lost"
            ),
            Err(_) => assert_eq!(*shape, "error", "only error shape may fail to collect"),
        }
    }
}

/// 探测后重建的响应保留 trailer 语义（grpc-status: 0 可见于重建体）。
#[tokio::test]
async fn peek_trailer_rebuild_keeps_trailers() {
    let mut trailers = http::HeaderMap::new();
    trailers.insert("grpc-status", http::HeaderValue::from_static("0"));
    let frames = futures_util::stream::iter(vec![
        Ok(Frame::data(Bytes::from_static(b"payload"))),
        Ok(Frame::trailers(trailers)),
    ]);
    let resp = http::Response::new(stream_body(frames));
    let (retryable, resp) = peek_trailer(resp, &[0], 1024).await;
    assert!(retryable, "trailer 0 in codes must retry");
    let collected = resp.into_body().collect().await.expect("collect");
    let got = collected.trailers().expect("trailers kept");
    assert_eq!(
        got.get("grpc-status").map(|v| v.as_bytes()),
        Some(&b"0"[..]),
        "rebuild must keep trailer semantics"
    );
    assert_eq!(collected.to_bytes(), Bytes::from_static(b"payload"));
}

/// known_len：Content-Length 优先；缺失时回退精确 size_hint（空体 = 0）。
#[test]
fn known_len_prefers_content_length_then_size_hint() {
    let mut req = http::Request::new(multi_frame(&["abc"]));
    assert_eq!(known_len(&req), None, "stream body has no known length");
    req.headers_mut().insert(
        http::header::CONTENT_LENGTH,
        http::HeaderValue::from_static("7"),
    );
    assert_eq!(known_len(&req), Some(7), "content-length wins");
    let req = http::Request::new(empty_body());
    assert_eq!(known_len(&req), Some(0), "empty body has exact size hint");
}

/// 空体请求照常可重放（无 body 的 GET 是最常见重试场景）。
#[tokio::test]
async fn empty_body_is_replayable() {
    let req = http::Request::new(empty_body());
    match prepare(req, 16).await {
        Replay::Buffered { body, .. } => assert!(body.is_empty()),
        Replay::Passthrough(_) => panic!("empty body must be replayable"),
    }
}

/// 空 data 帧保真：不计入长度、可重放字节不含空帧噪声。
#[tokio::test]
async fn empty_data_frames_stay_replayable() {
    let frames = futures_util::stream::iter(vec![
        Ok(Frame::data(Bytes::new())),
        Ok(Frame::data(Bytes::from_static(b"ab"))),
        Ok(Frame::data(Bytes::new())),
    ]);
    let req = http::Request::new(stream_body(frames));
    match prepare(req, 16).await {
        Replay::Buffered { body, .. } => assert_eq!(&body[..], &b"ab"[..]),
        Replay::Passthrough(_) => panic!("empty frames must stay replayable"),
    }
}
