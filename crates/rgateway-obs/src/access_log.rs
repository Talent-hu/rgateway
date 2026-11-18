//! 访问日志记录与单行渲染（JSON / text 两种格式）。
//!
//! # 为什么收在这里
//!
//! 网关 dispatch 层每请求收尾时输出一行访问日志；格式化逻辑与「哪些字段、
//! 什么键名、缺失怎么表达」的 schema 约定集中在本模块，调用方只负责把
//! 事实填进 [`AccessRecord`]，两种格式共用同一份脱敏策略（防日志注入）。
//!
//! # 注入防护（安全硬性）
//!
//! 路径、方法、peer、request_id、上游地址等全部由外部输入派生，可能携带
//! CR/LF/NUL 等控制字符。两种渲染在输出前一律把控制符替换为 `?`——覆盖
//! ASCII 控制符（0x00–0x1F、0x7F，含 CR/LF/NUL/DEL/Tab）、C1 控制符
//! （0x80–0x9F，含 NEL 0x85）以及 Unicode 行/段分隔符（U+2028/U+2029）；
//! 替换而非删除，避免 `a\r\nb` 被拼接成可信的 `ab` 前缀。text 格式因此绝无
//! 换行逃逸，JSON 格式另有 serde_json 字符串转义兜底。
//!
//! # 键名口径（与 dispatch 旧内联格式的迁移映射）
//!
//! 本模块的键名是访问日志的**唯一权威口径**；dispatch 早期的内联
//! `tracing::info` 行使用过 `proto` / `latency_ms` 与路由缺失哨兵 `"-"`，
//! 与本 schema 的映射关系如下，P9 接入后 dispatch 统一改用
//! [`AccessRecord`]，管道侧只消费一套键名：
//!
//! | 旧键（dispatch 内联） | 本 schema |
//! |----------------------|-----------|
//! | `proto` | `protocol` |
//! | `latency_ms` | `duration_ms` |
//! | `route_id="-"`（哨兵） | `route_id: null`（缺失） |

use std::time::Duration;

use serde_json::{json, Value};

/// 一条访问日志记录：请求级事实的不可变快照。
///
/// 字段全部为原语类型（本 crate 不依赖任何 rgateway_* crate，调用方传什么
/// 记什么），构造后按只读使用：渲染函数只接受 `&AccessRecord`，本类型不提供
/// 任何变更 API。字段全 `pub` 便于调用方用结构体字面量一次构造。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessRecord {
    /// 请求关联 id（`X-Request-ID`）。
    pub request_id: String,
    /// 客户端对端地址（`ip:port`），缺失为 `None`。
    pub peer: Option<String>,
    /// HTTP 方法。
    pub method: String,
    /// 请求路径（不含查询串）。
    pub path: String,
    /// 命中的路由 id，未命中为 `None`。
    pub route_id: Option<String>,
    /// 协议标签（如 `http`、`grpc`、`ws`）。
    pub protocol: String,
    /// 响应状态码。
    pub status: u16,
    /// 上游实例地址（`host:port`），无上游为 `None`。
    pub upstream: Option<String>,
    /// 错误分类（日志通道可记内部细节），无错误为 `None`。
    pub error_kind: Option<String>,
    /// 请求总耗时（调用方传入，测试确定性由调用方保证）。
    pub duration: Duration,
}

/// 渲染为单行 JSON 对象。
///
/// 字段键名 snake_case 恒定；`Option` 字段缺失时输出 `null` 且**键不省略**，
/// 保证日志管道的 schema 稳定。`ts` 由调用方传入已格式化的 RFC3339 UTC
/// 字符串（不在此处取系统时间，保证测试确定性）。
pub fn render_json(record: &AccessRecord, ts: &str) -> String {
    let value = json!({
        "ts": sanitize_text(ts),
        "request_id": sanitize_text(&record.request_id),
        "peer": json_opt(&record.peer),
        "method": sanitize_text(&record.method),
        "path": sanitize_text(&record.path),
        "route_id": json_opt(&record.route_id),
        "protocol": sanitize_text(&record.protocol),
        "status": record.status,
        "upstream": json_opt(&record.upstream),
        "error_kind": json_opt(&record.error_kind),
        "duration_ms": duration_millis(record.duration),
    });
    value.to_string()
}

/// 渲染为单行 `key=value` 文本（logfmt 风格）。
///
/// 与 [`render_json`] 同一字段集与脱敏策略；值含空白、`=`、引号、反斜杠
/// 或为空时加双引号并转义，字面量 `null` 也加引号——让裸 `null` 专表示
/// 缺失字段，两种格式的 null 语义一致。
pub fn render_text(record: &AccessRecord, ts: &str) -> String {
    let pairs: [(&str, String); 11] = [
        ("ts", render_token(&sanitize_text(ts))),
        (
            "request_id",
            render_token(&sanitize_text(&record.request_id)),
        ),
        ("peer", render_opt_token(&record.peer)),
        ("method", render_token(&sanitize_text(&record.method))),
        ("path", render_token(&sanitize_text(&record.path))),
        ("route_id", render_opt_token(&record.route_id)),
        ("protocol", render_token(&sanitize_text(&record.protocol))),
        ("status", record.status.to_string()),
        ("upstream", render_opt_token(&record.upstream)),
        ("error_kind", render_opt_token(&record.error_kind)),
        ("duration_ms", duration_millis(record.duration).to_string()),
    ];
    pairs
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// 日志注入脱敏：控制字符逐字符替换为 `?`。
///
/// 覆盖范围（比 `rgateway-proxy` 的 `header_value` 更严，连 Tab 一起清）：
/// ASCII 控制符、C1 控制符（`char::is_control()` = Unicode Cc 类，含
/// NEL 0x85）、以及 Zl/Zp 类的 U+2028/U+2029 行/段分隔符——部分日志
/// 管道与浏览器 JS 把 U+2028 当换行处理，留着就是换行逃逸残留。其余
/// 非 ASCII（普通 Unicode 文本）原样输出，JSON 通道由 serde_json 转义兜底。
pub(crate) fn sanitize_text(input: &str) -> String {
    input
        .chars()
        .map(|c| {
            if c.is_control() || c == '\u{2028}' || c == '\u{2029}' {
                '?'
            } else {
                c
            }
        })
        .collect()
}

/// `Option` 字段 → JSON 值：`None` 恒为 [`Value::Null`]（键保留，schema 稳定）。
fn json_opt(value: &Option<String>) -> Value {
    match value {
        Some(v) => Value::String(sanitize_text(v)),
        None => Value::Null,
    }
}

/// `Option` 字段 → text token：`None` 输出裸 `null`，`Some` 脱敏后按需引号。
fn render_opt_token(value: &Option<String>) -> String {
    match value {
        Some(v) => render_token(&sanitize_text(v)),
        None => "null".to_string(),
    }
}

/// 把单个值渲染成 text token：含空白/`=`/引号/反斜杠、为空或字面量 `null`
/// 时加双引号，引号内转义 `\` 与 `"`（控制字符已在 [`sanitize_text`] 阶段
/// 清掉，不可能出现换行逃逸）。
fn render_token(value: &str) -> String {
    let needs_quotes = value.is_empty()
        || value == "null"
        || value
            .chars()
            .any(|c| c == ' ' || c == '=' || c == '"' || c == '\\');
    if !needs_quotes {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// [`Duration`] → 毫秒（`u64`，饱和封顶）：`as_millis()` 是 `u128`，
/// 超界时封顶而不是 panic（日志渲染绝不拖垮请求路径）。
fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[path = "access_log_tests.rs"]
mod tests;
