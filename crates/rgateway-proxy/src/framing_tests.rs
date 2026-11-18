//! [`crate::framing`] 的测试：golden 字节锁定线上帧格式、文本变体的 base64
//! golden、坏输入表驱动解析。framing 是核心模块，行覆盖率要求 ≥ 90%。

use super::*;

// ---------- golden 常量：完整 trailer 帧的期望字节 ----------
// 帧布局：1 字节 flags（0x80）+ 4 字节大端 payload 长度 + payload。
// payload = "grpc-status: {status}\r\n" + "grpc-message: {message}\r\n"。
// 逐字节硬编码（长度前缀也是字面量），防止重构悄悄改变线上格式。

/// status=14 / "upstream unavailable"：payload 53 字节（0x35）。
const GOLDEN_UNAVAILABLE: &[u8] =
    b"\x80\x00\x00\x00\x35grpc-status: 14\r\ngrpc-message: upstream unavailable\r\n";

/// status=0 / 空消息：payload 32 字节（0x20）；空消息仍输出 "grpc-message: \r\n"。
const GOLDEN_OK_EMPTY: &[u8] = b"\x80\x00\x00\x00\x20grpc-status: 0\r\ngrpc-message: \r\n";

/// status=3 / 含多字节语义的 ASCII 文本（消息内含冒号，考验按首个冒号拆行）：
/// payload 79 字节（0x4f）。
const GOLDEN_MULTIBYTE_ASCII: &[u8] = b"\x80\x00\x00\x00\x4fgrpc-status: 3\r\ngrpc-message: bad request: truncated multibyte utf-8 sequence\r\n";

/// [`GOLDEN_UNAVAILABLE`] 的 gRPC-Web-Text 变体（整帧 base64）。
const GOLDEN_UNAVAILABLE_B64: &str =
    "gAAAADVncnBjLXN0YXR1czogMTQNCmdycGMtbWVzc2FnZTogdXBzdHJlYW0gdW5hdmFpbGFibGUNCg==";

/// [`GOLDEN_OK_EMPTY`] 的 gRPC-Web-Text 变体（整帧 base64）。
const GOLDEN_OK_EMPTY_B64: &str = "gAAAACBncnBjLXN0YXR1czogMA0KZ3JwYy1tZXNzYWdlOiANCg==";

/// [`GOLDEN_MULTIBYTE_ASCII`] 的 gRPC-Web-Text 变体（整帧 base64）。
const GOLDEN_MULTIBYTE_ASCII_B64: &str =
    "gAAAAE9ncnBjLXN0YXR1czogMw0KZ3JwYy1tZXNzYWdlOiBiYWQgcmVxdWVzdDogdHJ1bmNhdGVkIG11bHRpYnl0ZSB1dGYtOCBzZXF1ZW5jZQ0K";

// 验证 encode 输出与 golden 帧逐字节完全相等，并且 decode(golden) 还原原值。
// 列含义：(status, message, golden 帧字节)
#[test]
fn encode_matches_golden_and_decode_round_trips() {
    let cases: [(i32, &str, &[u8]); 3] = [
        (14, "upstream unavailable", GOLDEN_UNAVAILABLE),
        (0, "", GOLDEN_OK_EMPTY),
        (
            3,
            "bad request: truncated multibyte utf-8 sequence",
            GOLDEN_MULTIBYTE_ASCII,
        ),
    ];
    for (status, message, golden) in cases {
        let encoded = encode_trailer_frame(status, message);
        assert_eq!(
            encoded.as_ref(),
            golden,
            "encode mismatch for status {status}"
        );
        assert_eq!(
            decode_trailer_frame(golden).unwrap(),
            (status, message.to_string()),
            "decode mismatch for status {status}"
        );
    }
}

// 验证 gRPC-Web-Text 变体输出与 base64 golden 完全相等。
// 列含义：(status, message, golden base64 文本)
#[test]
fn encode_text_matches_base64_golden() {
    let cases: [(i32, &str, &str); 3] = [
        (14, "upstream unavailable", GOLDEN_UNAVAILABLE_B64),
        (0, "", GOLDEN_OK_EMPTY_B64),
        (
            3,
            "bad request: truncated multibyte utf-8 sequence",
            GOLDEN_MULTIBYTE_ASCII_B64,
        ),
    ];
    for (status, message, golden) in cases {
        assert_eq!(
            encode_trailer_frame_text(status, message),
            golden,
            "text variant mismatch for status {status}"
        );
    }
}

// 验证 decode(golden) 对文本变体同样可还原：base64 解码后再走帧解码。
#[test]
fn decode_restores_from_text_variant_golden() {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(GOLDEN_MULTIBYTE_ASCII_B64)
        .unwrap();
    assert_eq!(
        decode_trailer_frame(&bytes).unwrap(),
        (
            3,
            "bad request: truncated multibyte utf-8 sequence".to_string()
        )
    );
}

// 坏输入表驱动：每行给出输入字节与期望错误文案包含的子串，逐一断言 Err。
// 列含义：(用例说明, 输入字节, 期望错误包含的子串)
#[test]
fn decode_rejects_bad_input_table() {
    // 长度前缀正确（payload 16 字节）但 flags 不是 0x80 的帧。
    let wrong_flags: &[u8] = b"\x00\x00\x00\x00\x10grpc-status: 3\r\n";
    // 长度前缀（0xff）与实际 payload 长度不一致。
    let len_mismatch: &[u8] = b"\x80\x00\x00\x00\xffgrpc-status: 3\r\ngrpc-message: x\r\n";
    // payload 只有 grpc-message、缺 grpc-status（payload 17 字节）。
    let no_status: &[u8] = b"\x80\x00\x00\x00\x11grpc-message: x\r\n";
    // grpc-status 的值不是数字（payload 18 字节）。
    let nan_status: &[u8] = b"\x80\x00\x00\x00\x12grpc-status: abc\r\n";
    // payload 里存在不含冒号的畸形行（payload 8 字节）。
    let bad_line: &[u8] = b"\x80\x00\x00\x00\x08no-colon";
    // payload 不是合法 UTF-8。
    let bad_utf8: &[u8] = b"\x80\x00\x00\x00\x01\xff";

    let cases: [(&str, &[u8], &str); 7] = [
        ("空输入", b"", "too short"),
        ("只有 2 字节", b"\x80\x00", "too short"),
        ("flags 错误", wrong_flags, "flags"),
        ("长度不一致", len_mismatch, "length mismatch"),
        ("缺 grpc-status", no_status, "grpc-status"),
        ("status 非数字", nan_status, "grpc-status"),
        ("畸形行", bad_line, "malformed trailer line"),
    ];
    for (name, input, want) in cases {
        let err = decode_trailer_frame(input).expect_err(&format!("{name} 应当解析失败"));
        assert!(
            err.contains(want),
            "{name}: 错误文案 {err:?} 应包含 {want:?}"
        );
    }
    // 非法 UTF-8 单独断言（错误文案走 "UTF-8" 分支）。
    let err = decode_trailer_frame(bad_utf8).expect_err("非 UTF-8 payload 应当解析失败");
    assert!(err.contains("UTF-8"), "错误文案应提及 UTF-8: {err}");
}

// 验证 grpc-message 缺省时按空串还原（协议允许只带 grpc-status 的 trailer）。
#[test]
fn decode_defaults_missing_message_to_empty() {
    let frame: &[u8] = b"\x80\x00\x00\x00\x10grpc-status: 5\r\n";
    assert_eq!(decode_trailer_frame(frame).unwrap(), (5, String::new()));
}

// 验证消息按「冒号 + 一个空格」拆出、内含冒号与前导空格都无损还原
// （decode 不做百分号解码，与 encode 的 ASCII 约定互逆）。
#[test]
fn decode_preserves_colons_and_spaces_in_message() {
    let frame = encode_trailer_frame(2, "  see: docs, part:2");
    assert_eq!(
        decode_trailer_frame(&frame).unwrap(),
        (2, "  see: docs, part:2".to_string())
    );
}

// 验证解析宽容度（文档契约）：未知头行忽略、行序不限（message 行可先于
// status 行），status/message 照常还原（payload 65 字节 = 0x41）。
#[test]
fn decode_tolerates_unknown_lines_and_any_order() {
    let frame: &[u8] = b"\x80\x00\x00\x00\x41grpc-message: hi\r\ngrpc-status-details-bin: AA==\r\ngrpc-status: 7\r\n";
    assert_eq!(decode_trailer_frame(frame).unwrap(), (7, "hi".to_string()));
}

// 注入防护回归：message 里的 CR/LF 与 ASCII 控制符替换为 '?'（与
// header_value 同策略：tab 保留），"x\r\ngrpc-status: 0" 之类载荷绝不能拆出
// 新行、翻转状态码；文本变体与二进制帧同源（整帧 base64）。
// 列含义：(输入 message, 期望脱敏后的 message)
#[test]
fn encode_sanitizes_message_against_trailer_injection() {
    use base64::Engine;
    let cases: [(&str, &str); 5] = [
        ("x\r\ngrpc-status: 0", "x??grpc-status: 0"),
        ("x\ngrpc-message: hi", "x?grpc-message: hi"),
        ("nul\x00byte", "nul?byte"),
        ("del\x7Fbyte", "del?byte"),
        ("tab\there", "tab\there"),
    ];
    for (input, want) in cases {
        let frame = encode_trailer_frame(14, input);
        let (status, message) = decode_trailer_frame(&frame).expect("脱敏后的帧必须可解析");
        assert_eq!(status, 14, "状态码不得被注入翻转: {input:?}");
        assert_eq!(message, want, "脱敏结果不符: {input:?}");
        // 文本变体与二进制帧同源，注入防护自动一致。
        let text = encode_trailer_frame_text(14, input);
        assert_eq!(
            text,
            base64::engine::general_purpose::STANDARD.encode(&frame),
            "文本变体不一致: {input:?}"
        );
    }
}
