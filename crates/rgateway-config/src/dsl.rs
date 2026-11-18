//! 紧凑 DSL 解析：谓词 `Name=arg1,arg2`、过滤器 `Name=k1:v1,k2:v2`，以及时长
//! （"500ms"、"3s"）与大小（"10MB"、"64KB"）字符串。
//!
//! # 语法说明
//!
//! ```text
//! named   := Name '=' arg (',' arg)*          # 谓词，如 "Method=GET,POST"
//! filter  := Name '=' kv (',' kv)*            # 过滤器，如 "RateLimit=key:client_ip,rate:100,period:1s"
//! kv      := key ':' value                    # value 内允许再出现 ':'（按第一个 ':' 切分）
//! ```
//!
//! * 转义：`\,` 是参数内的字面逗号，`\\` 是字面反斜杠；其他 `\x` 组合按
//!   原样保留（反斜杠不丢失）。分隔发生在**未转义的逗号**上，且先切分
//!   再反转义（见 `split_unescaped`）；输入以孤立 `\` 结尾是错误。
//! * 时长（[`parse_duration`]）：`500ms`、`3s`、`2m`、`1h`、`1d`、`us`
//!   微秒；纯数字按毫秒计；允许小数秒（`1.5s`）。
//! * 大小（[`parse_size`]）：`512B`、`64KB`、`10MB`、`1GB`、`1MiB`；
//!   纯数字按字节计；**均按 1024 进制**（KB=1024、MB=1024²、GB=1024³）。
//! * 百分数：过滤器参数里可写 `50%`（如 `failure_rate:50%`）。紧凑形态下
//!   值只是字符串透传，由 [`crate::spec::FilterSpec::param_f64`] 换算：
//!   带 `%` 的换算成小数（`50%` → 0.5），不带 `%` 的按原值
//!   （`50` → 50.0），由调用方决定语义。
//!
//! 本模块只做词法层面的切分与取值解析，返回 `Result<_, String>`；错误
//! 定位（[`crate::error::Location`]）由调用方包装成 [`crate::error::ConfigError`]。

use std::collections::BTreeMap;
use std::time::Duration;

use crate::schema::ParamValue;

/// 在未转义的逗号上切分，再把 `\,`→`,`、`\\`→`\` 反转义。
///
/// 单趟扫描：遇到 `\` 就消费下一个字符（`,`/`\` 按转义表解释，其余原样
/// 保留反斜杠），遇到未转义 `,` 才产出一段。因此转义与切分必须同时完成
/// ——若先反转义再按逗号切分，转义出来的字面逗号就无法与分隔符区分。
/// 每段做 trim；末尾的孤立 `\` 是悬空转义，报错。
fn split_unescaped(input: &str) -> Result<Vec<String>, String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(',') => current.push(','),
                Some('\\') => current.push('\\'),
                Some(other) => {
                    // 未知转义（如正则里的 \d）：保留反斜杠，别破坏正则语义
                    current.push('\\');
                    current.push(other);
                }
                None => return Err("dangling escape at end of input".to_string()),
            },
            ',' => {
                parts.push(current.trim().to_string());
                current = String::new();
            }
            other => current.push(other),
        }
    }
    parts.push(current.trim().to_string());
    Ok(parts)
}

/// 解析 `Name=arg1,arg2` → `(name, args)`。
///
/// 按**第一个** `=` 切出名字（后续 `=` 属于参数值），name 与各 arg 均
/// trim。错误：没有 `=`、name 为空、悬空转义。
pub fn parse_named_args(input: &str) -> Result<(String, Vec<String>), String> {
    let (name, rest) = input
        .split_once('=')
        .ok_or_else(|| format!("expected Name=arg form in {input:?}"))?;
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err(format!("empty name in {input:?}"));
    }
    let args = split_unescaped(rest)?;
    Ok((name, args))
}

/// 解析 `Name=k1:v1,k2:v2` → `(name, params)`，值统一为字符串
/// （数字/时长/百分数由 [`crate::spec::FilterSpec`] 的参数访问器按需转换）。
///
/// 每个 `k:v` 按**第一个** `:` 切分，因此值里可以带 `:`
/// （如 URL `http://a:8080`）。空段（多余逗号）跳过。错误：没有 `=`、
/// name 为空、某段没有 `:`、参数键为空。
pub fn parse_compact_filter(input: &str) -> Result<(String, BTreeMap<String, ParamValue>), String> {
    let (name, rest) = input
        .split_once('=')
        .ok_or_else(|| format!("expected Name=k:v form in {input:?}"))?;
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err(format!("empty name in {input:?}"));
    }
    let mut params = BTreeMap::new();
    for part in split_unescaped(rest)? {
        if part.is_empty() {
            continue;
        }
        let (k, v) = part
            .split_once(':')
            .ok_or_else(|| format!("expected key:value pair in {part:?}"))?;
        let k = k.trim().to_string();
        if k.is_empty() {
            return Err(format!("empty parameter key in {input:?}"));
        }
        params.insert(k, ParamValue::String(v.trim().to_string()));
    }
    Ok((name, params))
}

/// 解析时长字符串：`500ms`、`3s`、`2m`、`1h`、`1d`，或纯数字（毫秒）。
/// 允许小数秒（`1.5s`）。非有限值（`NaN`、`inf`）与超出 [`Duration`]
/// 表示范围的量值一律是错误，绝不 panic。
pub fn parse_duration(input: &str) -> Result<Duration, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("empty duration".to_string());
    }
    // 单位表（毫秒倍率）：长后缀必须排在短后缀前（"ms" 先于 "s"），
    // 否则 "5ms" 会被 "s" 抢先切出 "5m" 而解析失败。
    let units = [
        ("ms", 1.0f64),
        ("us", 0.001),
        ("s", 1000.0),
        ("m", 60_000.0),
        ("h", 3_600_000.0),
        ("d", 86_400_000.0),
    ];
    for (suffix, mult) in units {
        if let Some(num) = input.strip_suffix(suffix) {
            let value: f64 = num
                .trim()
                .parse()
                .map_err(|_| format!("invalid duration {input:?}"))?;
            // 负数显式拒绝以给出更准的消息；NaN 走不到这里（比较恒假），
            // 由下面的 try_from_secs_f64 兜底。
            if value < 0.0 {
                return Err(format!("negative duration {input:?}"));
            }
            // 必须用 try_from_secs_f64 而非 from_secs_f64：配置值是外部
            // 输入，"NaN"/"inf"/天文数字都能通过 f64 解析，from_secs_f64
            // 对这些会 panic，try_* 则返回 Err，让配置错误以错误形式上报
            // （安全修复点：拒绝服务向量从 panic 变成配置诊断）。
            return Duration::try_from_secs_f64(value * mult / 1000.0)
                .map_err(|_| format!("invalid duration {input:?}"));
        }
    }
    // 无后缀：按毫秒计
    let millis: f64 = input
        .parse()
        .map_err(|_| format!("invalid duration {input:?} (use ms/s/m/h/d suffix)"))?;
    if millis < 0.0 {
        return Err(format!("negative duration {input:?}"));
    }
    Duration::try_from_secs_f64(millis / 1000.0).map_err(|_| format!("invalid duration {input:?}"))
}

/// 解析大小字符串：`10MB`、`64KB`、`1GB`、`512B`、`1MiB`（全程 1024
/// 进制：KB=1024、MB=1024²、GB=1024³），或纯数字（字节数）。
pub fn parse_size(input: &str) -> Result<usize, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("empty size".to_string());
    }
    // 与 parse_duration 同理：长后缀在前（"KB" 先于 "B"），"B" 收尾。
    let units = [
        ("KB", 1024u64),
        ("MB", 1024 * 1024),
        ("GB", 1024 * 1024 * 1024),
        ("KiB", 1024),
        ("MiB", 1024 * 1024),
        ("GiB", 1024 * 1024 * 1024),
        ("B", 1),
    ];
    for (suffix, mult) in units {
        if let Some(num) = input.strip_suffix(suffix) {
            let value: u64 = num
                .trim()
                .parse()
                .map_err(|_| format!("invalid size {input:?}"))?;
            // saturating_mul 防 u64 乘法溢出 panic；usize::try_from 在
            // 32 位目标上截断时给出 "overflows usize" 错误。
            return usize::try_from(value.saturating_mul(mult))
                .map_err(|_| format!("size {input:?} overflows usize"));
        }
    }
    input
        .parse::<usize>()
        .map_err(|_| format!("invalid size {input:?} (use B/KB/MB/GB suffix)"))
}

/// 校验 ant 风格路径模式语法：`?`（单字符）、`*`（段内多字符）、`**`
/// （整段多段）。`**` 必须独占一个 `/` 分段。此处只做语法校验，真正
/// 的匹配（贪心 + 单次回溯）在 rgateway-router 的 ant 匹配器里。
pub fn validate_ant_pattern(pattern: &str) -> Result<(), String> {
    if pattern.is_empty() {
        return Err("path pattern must not be empty".to_string());
    }
    if !pattern.starts_with('/') {
        return Err(format!("path pattern must start with '/': {pattern:?}"));
    }
    for seg in pattern.split('/').skip(1) {
        // "**" 只有独占一段才有跨段语义；"/a**/b" 这类混合写法拒绝
        if seg.contains("**") && seg != "**" {
            return Err(format!("'**' must occupy a whole segment: {pattern:?}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 验证：`Name=arg1,arg2` 的名字/参数切分、trim 与常见谓词形态。
    #[test]
    fn named_args_table() {
        // 表列：(输入, 期望 name, 期望 args)
        let cases: &[(&str, &str, &[&str])] = &[
            ("Method=GET,POST", "Method", &["GET", "POST"]),
            ("Path=/api/**", "Path", &["/api/**"]),
            ("Host=api.example.com", "Host", &["api.example.com"]),
            ("Header=X-Tenant,\\d+", "Header", &["X-Tenant", "\\d+"]),
            ("Path=/a, /b", "Path", &["/a", "/b"]),
        ];
        for (input, want_name, want_args) in cases {
            let (name, args) = parse_named_args(input).unwrap();
            assert_eq!(&name, want_name, "{input}");
            assert_eq!(&args, want_args, "{input}");
        }
    }

    /// 验证：`\,` 转义出参数内的字面逗号，`\\` 转义出字面反斜杠，
    /// 未知转义 `\x` 原样保留。
    #[test]
    fn escaped_comma_and_backslash() {
        let (name, args) = parse_named_args(r"Header=X-A,foo\,bar\\x").unwrap();
        assert_eq!(name, "Header");
        assert_eq!(args, vec!["X-A", "foo,bar\\x"]);
    }

    /// 验证：缺 `=`、空 name、悬空转义均报错。
    #[test]
    fn named_args_errors() {
        assert!(parse_named_args("NoEquals").is_err());
        assert!(parse_named_args("=GET").is_err());
        assert!(parse_named_args(r"Method=GET\").is_err());
    }

    /// 验证：紧凑过滤器 `Name=k:v,k:v` 解析出参数表，值为字符串。
    #[test]
    fn compact_filter_table() {
        let (name, params) =
            parse_compact_filter("RateLimit=key:client_ip,rate:100,period:1s").unwrap();
        assert_eq!(name, "RateLimit");
        assert_eq!(params.len(), 3);
        assert_eq!(params["key"], ParamValue::String("client_ip".into()));
        assert_eq!(params["rate"], ParamValue::String("100".into()));
    }

    /// 验证：`k:v` 按第一个 `:` 切分，值内允许再出现 `:`（URL 等）。
    #[test]
    fn compact_filter_key_with_colon_value() {
        let (_, params) = parse_compact_filter("RequestHeader=x-uri:http://a:8080/v").unwrap();
        assert_eq!(
            params["x-uri"],
            ParamValue::String("http://a:8080/v".into())
        );
    }

    /// 验证：缺 `=`、段内缺 `:`、参数键为空均报错。
    #[test]
    fn compact_filter_errors() {
        assert!(parse_compact_filter("NoEquals").is_err());
        assert!(parse_compact_filter("Name=nocolon").is_err());
        assert!(parse_compact_filter("Name=:v").is_err());
    }

    /// 验证：各时长单位换算、纯数字按毫秒、非法输入与非有限浮点
    /// （NaN/inf/超范围）报错而不 panic。
    #[test]
    fn duration_table() {
        // 表列：(输入, 期望毫秒数)
        let cases = [
            ("500ms", 500),
            ("3s", 3000),
            ("1.5s", 1500),
            ("2m", 120_000),
            ("1h", 3_600_000),
            ("1d", 86_400_000),
            ("250", 250),
            ("0s", 0),
        ];
        for (input, want_ms) in cases {
            let got = parse_duration(input).unwrap();
            assert_eq!(got, Duration::from_millis(want_ms), "{input}");
        }
        assert!(parse_duration("").is_err());
        assert!(parse_duration("-5s").is_err());
        assert!(parse_duration("5x").is_err());
        assert!(parse_duration("abc").is_err());
        // 非有限值与超范围浮点必须返回 Err，绝不能 panic（安全回归）。
        for input in [
            "NaN", "NaNs", "inf", "infs", "-infs", "1e300s", "1e300ms", "1e308",
        ] {
            assert!(parse_duration(input).is_err(), "{input}");
        }
    }

    /// 验证：大小单位按 1024 进制换算、纯数字按字节、非法输入报错。
    #[test]
    fn size_table() {
        // 表列：(输入, 期望字节数)
        let cases = [
            ("10MB", 10 * 1024 * 1024),
            ("64KB", 64 * 1024),
            ("1GB", 1024 * 1024 * 1024),
            ("512B", 512),
            ("1MiB", 1024 * 1024),
            ("1024", 1024),
        ];
        for (input, want) in cases {
            assert_eq!(parse_size(input).unwrap(), want, "{input}");
        }
        assert!(parse_size("").is_err());
        assert!(parse_size("10XB").is_err());
        assert!(parse_size("-1B").is_err());
    }
}
