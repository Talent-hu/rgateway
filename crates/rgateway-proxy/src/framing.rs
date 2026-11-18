//! gRPC-Web trailer 帧编解码。
//!
//! gRPC-Web 没有 HTTP/2 的原生 trailer 通道，协议把 trailer 编码成 body 末尾的
//! 一个特殊数据帧：1 字节 flags（trailer 帧 = 0x80）+ 4 字节大端 payload 长度 +
//! payload。gRPC-Web-Text 变体把整个帧再做 base64。payload 是头行序列，本实现
//! 固定输出两行：`grpc-status: {status}\r\n` 与 `grpc-message: {message}\r\n`
//! （消息为空时仍输出后者，值为空串——客户端解析器不必特判缺失行）。
//!
//! # 为什么不做百分号编码
//!
//! gRPC 规范要求 `grpc-message` 做 percent-encoding；本实现**刻意不做**：消息
//! 来源是 [`rgateway_core::GatewayError::grpc_message`] 的泛化静态文案，保证
//! ASCII 且不含 CR/LF（见 `rgateway-core/src/error.rs` 契约），percent-encoding
//! 只会徒增双方负担。即便如此，encode 侧仍做**防御性脱敏**（见
//! [`sanitize_message`]）：CR/LF 与 ASCII 控制符替换为 `?`，杜绝任何来源的
//! 消息翻转帧内 `grpc-status` 行。decode 侧按「冒号 + 一个空格」拆值，与
//! encode 严格互逆，消息里的冒号与前导空格都无损还原。

use base64::Engine;

/// trailer 帧的 flags 字节：置位 0x80 表示该帧携带 HTTP/2 trailer。
const TRAILER_FLAG: u8 = 0x80;

/// 帧头长度：1 字节 flags + 4 字节大端 payload 长度。
const FRAME_HEADER_LEN: usize = 5;

/// 把 (status, message) 编码为 gRPC-Web trailer 帧。
///
/// 帧格式见模块文档。`status` 是 gRPC status code（取
/// [`rgateway_core::GatewayError::grpc_status`]），`message` 先经
/// [`sanitize_message`] 脱敏再写入帧（防 trailer 注入，详见模块文档）。
pub fn encode_trailer_frame(status: i32, message: &str) -> bytes::Bytes {
    let message = sanitize_message(message);
    let payload = format!("grpc-status: {status}\r\ngrpc-message: {message}\r\n");
    let mut frame = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
    frame.push(TRAILER_FLAG);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload.as_bytes());
    bytes::Bytes::from(frame)
}

/// 消息的防御性脱敏：CR/LF 与 ASCII 控制符（tab 除外）替换为 `?`，
/// 与 [`crate::headers::header_value`] 的头值脱敏策略一致。
///
/// 这是 trailer 注入的唯一控制点：`"x\r\ngrpc-status: 0"` 之类的载荷若不脱敏
/// 就能拆出新行、翻转帧内状态码。逐字节替换只碰 ASCII 控制字节，UTF-8
/// 多字节序列（字节均 ≥ 0x80）不受影响，结果必然仍是合法 UTF-8。
fn sanitize_message(message: &str) -> String {
    let sanitized: Vec<u8> = message
        .bytes()
        .map(|b| {
            if b.is_ascii_control() && b != b'\t' {
                b'?'
            } else {
                b
            }
        })
        .collect();
    // 只把 ASCII 控制字节换成 ASCII '?'，UTF-8 合法性不变；lossy 兜底防御。
    String::from_utf8_lossy(&sanitized).into_owned()
}

/// gRPC-Web-Text 变体：把 [`encode_trailer_frame`] 的整帧做 base64
/// （STANDARD 字母表，含 padding），得到可直接放进 body 的文本。
pub fn encode_trailer_frame_text(status: i32, message: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(encode_trailer_frame(status, message))
}

/// 解析 trailer 帧回 (status, message)，与 [`encode_trailer_frame`] 互逆。
///
/// 供往返测试与后续阶段（上游 gRPC-Web 响应的 trailer 还原）复用。解析宽容度：
/// 行序不限、未知头行忽略、`grpc-message` 缺省视为空串；重复的 `grpc-status` /
/// `grpc-message` 以最后一行为准。
///
/// # 错误
///
/// 返回 `Err(内部诊断文案)` 的情况：帧短于 5 字节、flags 不是 0x80、长度前缀
/// 与实际长度不一致、payload 非 UTF-8、存在不含冒号的畸形行、缺 `grpc-status`
/// 行或其值不是 i32。文案是内部细节，仅进日志，绝不渲染给客户端。
pub fn decode_trailer_frame(bytes: &[u8]) -> Result<(i32, String), String> {
    let (flags, payload) = split_frame(bytes)?;
    if flags != TRAILER_FLAG {
        return Err(format!(
            "unexpected flags 0x{flags:02x}: not a gRPC-Web trailer frame"
        ));
    }
    parse_payload(payload)
}

/// 拆出 flags 与 payload，并校验长度前缀与实际长度一致。
fn split_frame(bytes: &[u8]) -> Result<(u8, &[u8]), String> {
    if bytes.len() < FRAME_HEADER_LEN {
        return Err(format!(
            "trailer frame too short: {} bytes (need at least {FRAME_HEADER_LEN})",
            bytes.len()
        ));
    }
    let flags = bytes[0];
    let declared = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]) as usize;
    let payload = &bytes[FRAME_HEADER_LEN..];
    if payload.len() != declared {
        return Err(format!(
            "length mismatch: prefix declares {declared} bytes, payload has {} bytes",
            payload.len()
        ));
    }
    Ok((flags, payload))
}

/// 解析 payload 的头行序列，还原 (status, message)。
fn parse_payload(payload: &[u8]) -> Result<(i32, String), String> {
    let text = std::str::from_utf8(payload)
        .map_err(|_| "trailer payload is not valid UTF-8".to_string())?;
    let (mut status, mut message) = (None, None);
    for line in text.split("\r\n") {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| format!("malformed trailer line: {line:?}"))?;
        match name.trim().to_ascii_lowercase().as_str() {
            "grpc-status" => {
                let raw = value.trim();
                let parsed = raw
                    .parse::<i32>()
                    .map_err(|_| format!("invalid grpc-status value: {raw:?}"))?;
                status = Some(parsed);
            }
            // 消息按「冒号 + 一个空格」拆出（encode 的严格互逆），未知行忽略。
            "grpc-message" => message = Some(strip_one_space(value).to_string()),
            _ => {}
        }
    }
    let status = status.ok_or_else(|| "missing grpc-status in trailer payload".to_string())?;
    Ok((status, message.unwrap_or_default()))
}

/// 去掉值前恰好一个前导空格（encode 在冒号后固定写一个空格）；这样消息自带的
/// 前导空格与内含冒号都能无损往返。
fn strip_one_space(value: &str) -> &str {
    value.strip_prefix(' ').unwrap_or(value)
}

#[cfg(test)]
#[path = "framing_tests.rs"]
mod tests;
