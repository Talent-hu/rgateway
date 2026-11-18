//! 路由谓词：可组合的请求匹配器。
//!
//! 一条路由的谓词**全部**命中才算匹配（[`AndPredicate`] 在编译期展平成
//! 单一列表）。内置谓词覆盖 method / header / host / query；路径（ant 风格）
//! 匹配在 `rgateway-router` 中实现，因为它依赖路径匹配器。
//!
//! # 全值正则匹配（安全要点）
//!
//! Header / Host / Query 的模式是拿去和**整个**值匹配的正则（对齐
//! Spring / Java `String.matches` 语义）：任何用户模式 `P` 都经
//! [`full_match_regex`] 编译为 `^(?:P)$`。
//!
//! 为什么必须锚定全值：若按子串搜索匹配，`Host=api\.example\.com` 会连
//! `xapi.example.com`、`api.example.com.evil`、`api-example.com` 一起命中
//! ——攻击者可控的 Host / 头 / 查询值就能溜进本不该匹配的路由（路由劫持）。
//! 锚定后这些值一律不匹配。模式本身仍是完整正则：择一不受影响（`a|b`
//! 匹配全值 `a` 或 `b`），`.*example.*` 这类用户正则照常工作——去掉的只是
//! “隐式子串搜索”。host 要按字面匹配时请转义点号（`\.`）。

use std::sync::Arc;

use http::request::Parts;
use http::{HeaderName, Method};
use regex::Regex;

use crate::route::RequestCtx;

/// 判断一个请求是否属于某条路由。
///
/// 实现要求 `Send + Sync`：编译后的谓词被路由快照共享，会在多个 tokio
/// worker 上并发调用；`matches` 应是快速的纯内存检查（不做 I/O）。
pub trait RoutePredicate: Send + Sync {
    /// 请求满足本谓词时返回 `true`。
    fn matches(&self, ctx: &RequestCtx, parts: &Parts) -> bool;

    /// 人类可读描述，用于诊断与管理接口输出。
    fn describe(&self) -> String;
}

/// 匹配所有请求（路由未配置显式谓词时使用）。
#[derive(Debug, Default, Clone, Copy)]
pub struct AlwaysPredicate;

impl RoutePredicate for AlwaysPredicate {
    fn matches(&self, _ctx: &RequestCtx, _parts: &Parts) -> bool {
        true
    }

    fn describe(&self) -> String {
        "Always".to_string()
    }
}

/// `Method=GET,POST` —— 请求方法必须是所列值之一（精确匹配、大小写敏感；
/// 列表为空则永不命中）。
#[derive(Debug, Clone)]
pub struct MethodPredicate {
    methods: Vec<Method>,
}

impl MethodPredicate {
    /// 用给定方法列表构建；不做去重，命中判定用 `contains`。
    pub fn new(methods: Vec<Method>) -> Self {
        Self { methods }
    }
}

impl RoutePredicate for MethodPredicate {
    fn matches(&self, _ctx: &RequestCtx, parts: &Parts) -> bool {
        self.methods.contains(&parts.method)
    }

    fn describe(&self) -> String {
        let names: Vec<&str> = self.methods.iter().map(|m| m.as_str()).collect();
        format!("Method={}", names.join(","))
    }
}

/// 把用户提供的谓词模式编译成**全值**匹配器。
///
/// 模式被包装为 `^(?:P)$`：必须匹配整个值（对齐 Spring / Java
/// `String.matches` 语义），而非任意子串。这是安全修复点——子串匹配会让
/// `api\.example\.com` 命中 `xapi.example.com` / `api.example.com.evil`，
/// 造成路由劫持；锚定后绕过路径被堵死。模式本身仍是完整正则：择一不受
/// 影响（`a|b` 匹配全值 `a` 或 `b`），`.*example.*` 等用户正则照常工作，
/// 去掉的只是隐式子串搜索。host 按字面匹配时请转义点号（`\.`）。
///
/// 这是 Host / Header / Query 谓词正则的唯一构造路径：配置编译与校验都
/// 复用本函数，保证每个使用点都是同样的全值语义。
pub fn full_match_regex(pattern: &str) -> Result<Regex, regex::Error> {
    Regex::new(&format!(r"^(?:{pattern})$"))
}

/// `Header=Name,regex` —— 同名头至少有一个值能**全值**匹配正则即命中
/// （见 [`full_match_regex`]）；头不存在或值不是合法 UTF-8 时不命中。
#[derive(Debug, Clone)]
pub struct HeaderPredicate {
    name: HeaderName,
    pattern: Regex,
    /// 用户原始模式（仅用于诊断输出）。
    source: String,
}

impl HeaderPredicate {
    /// 从用户模式构建；按全值匹配编译（见 [`full_match_regex`]）。
    pub fn new(name: HeaderName, pattern: &str) -> Result<Self, regex::Error> {
        Ok(Self {
            name,
            pattern: full_match_regex(pattern)?,
            source: pattern.to_string(),
        })
    }
}

impl RoutePredicate for HeaderPredicate {
    fn matches(&self, _ctx: &RequestCtx, parts: &Parts) -> bool {
        parts.headers.get_all(&self.name).iter().any(|value| {
            value
                .to_str()
                .map(|v| self.pattern.is_match(v))
                .unwrap_or(false)
        })
    }

    fn describe(&self) -> String {
        format!("Header={},{}", self.name.as_str(), self.source)
    }
}

/// `Host=regex` —— 请求 authority 必须**全值**匹配正则（见
/// [`full_match_regex`]）；请求没有 `Host` 头时回退用 `uri.authority()`
/// 判定（HTTP/2 等只带 :authority 伪头的场景依赖此回退）。
#[derive(Debug, Clone)]
pub struct HostPredicate {
    pattern: Regex,
    /// 用户原始模式（仅用于诊断输出）。
    source: String,
}

impl HostPredicate {
    /// 从用户模式构建；按全值匹配编译（见 [`full_match_regex`]）。
    pub fn new(pattern: &str) -> Result<Self, regex::Error> {
        Ok(Self {
            pattern: full_match_regex(pattern)?,
            source: pattern.to_string(),
        })
    }
}

impl RoutePredicate for HostPredicate {
    fn matches(&self, _ctx: &RequestCtx, parts: &Parts) -> bool {
        let host = parts
            .headers
            .get(http::header::HOST)
            .and_then(|h| h.to_str().ok())
            .or_else(|| parts.uri.authority().map(|a| a.as_str()));
        host.map(|h| self.pattern.is_match(h)).unwrap_or(false)
    }

    fn describe(&self) -> String {
        format!("Host={}", self.source)
    }
}

/// `Query=name,regex` —— 查询串中存在参数 `name` 且其值**全值**匹配正则
/// （见 [`full_match_regex`]）即命中。参数按 `&` 切分、首个 `=` 前为名、
/// 其后为值（无 `=` 的片段值视为空串）；URI 无查询串时不命中。
#[derive(Debug, Clone)]
pub struct QueryPredicate {
    name: String,
    pattern: Regex,
    /// 用户原始模式（仅用于诊断输出）。
    source: String,
}

impl QueryPredicate {
    /// 从用户模式构建；按全值匹配编译（见 [`full_match_regex`]）。
    pub fn new(name: String, pattern: &str) -> Result<Self, regex::Error> {
        Ok(Self {
            name,
            pattern: full_match_regex(pattern)?,
            source: pattern.to_string(),
        })
    }
}

impl RoutePredicate for QueryPredicate {
    fn matches(&self, _ctx: &RequestCtx, parts: &Parts) -> bool {
        parts
            .uri
            .query()
            .map(|q| {
                q.split('&').any(|pair| {
                    let mut it = pair.splitn(2, '=');
                    let k = it.next().unwrap_or("");
                    let v = it.next().unwrap_or("");
                    k == self.name && self.pattern.is_match(v)
                })
            })
            .unwrap_or(false)
    }

    fn describe(&self) -> String {
        format!("Query={},{}", self.name, self.source)
    }
}

/// 内层谓词的逻辑与（编译期展平，避免嵌套 And 造成深链）。
#[derive(Clone)]
pub struct AndPredicate {
    inner: Vec<Arc<dyn RoutePredicate>>,
}

impl std::fmt::Debug for AndPredicate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("AndPredicate")
            .field(&self.describe())
            .finish()
    }
}

impl AndPredicate {
    /// 用一组子谓词构建；全部命中（`all`）才算匹配，空列表恒为命中。
    pub fn new(inner: Vec<Arc<dyn RoutePredicate>>) -> Self {
        Self { inner }
    }
}

impl RoutePredicate for AndPredicate {
    fn matches(&self, ctx: &RequestCtx, parts: &Parts) -> bool {
        self.inner.iter().all(|p| p.matches(ctx, parts))
    }

    fn describe(&self) -> String {
        let parts: Vec<String> = self.inner.iter().map(|p| p.describe()).collect();
        format!("And({})", parts.join(" && "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::route::Protocol;
    use http::HeaderValue;

    fn parts(method: &str, uri: &str, headers: &[(&str, &str)]) -> Parts {
        let mut builder = http::Request::builder().method(method).uri(uri);
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        builder.body(()).unwrap().into_parts().0
    }

    fn ctx() -> RequestCtx {
        RequestCtx::new(Protocol::Http1, None)
    }

    /// 验证：Method 谓词只命中列出的方法，其余方法一律不匹配。
    #[test]
    fn method_predicate_table() {
        let get_post = MethodPredicate::new(vec![Method::GET, Method::POST]);
        // 列含义：(请求方法, URI, 期望命中)
        let cases = [
            ("GET", "/x", true),
            ("POST", "/x", true),
            ("DELETE", "/x", false),
            ("PUT", "/x", false),
        ];
        for (method, uri, want) in cases {
            let p = parts(method, uri, &[]);
            assert_eq!(get_post.matches(&ctx(), &p), want, "{method} {uri}");
        }
    }

    /// 验证：Header 谓词按全值正则匹配，值为空/不匹配都不命中，缺头也不命中。
    #[test]
    fn header_predicate_table() {
        let pred = HeaderPredicate::new(HeaderName::from_static("x-tenant"), r"\d+").unwrap();
        // 列含义：(请求方法, URI, 头值, 期望命中)
        let cases: &[(&str, &str, &str, bool)] = &[
            ("GET", "/", "42", true),
            ("GET", "/", "abc", false),
            ("GET", "/", "", false),
        ];
        for &(method, uri, value, want) in cases {
            let p = parts(method, uri, &[("x-tenant", value)]);
            assert_eq!(pred.matches(&ctx(), &p), want, "value={value:?}");
        }
        // 缺失该头
        let p = parts("GET", "/", &[]);
        assert!(!pred.matches(&ctx(), &p));
    }

    /// 验证：同名头出现多个值时，任一值命中即匹配（get_all + any 语义）。
    #[test]
    fn header_predicate_requires_match_on_any_value() {
        let pred = HeaderPredicate::new(HeaderName::from_static("x-tag"), "a").unwrap();
        let mut p = parts("GET", "/", &[]);
        p.headers.append("x-tag", HeaderValue::from_static("b"));
        p.headers.append("x-tag", HeaderValue::from_static("a"));
        assert!(pred.matches(&ctx(), &p));
    }

    /// 验证：Host 谓词精确匹配 authority——不匹配子串变体，也不容忍端口。
    #[test]
    fn host_predicate_table() {
        let pred = HostPredicate::new(r"api\.example\.com").unwrap();
        // 列含义：(Host 头值, 期望命中)
        let cases: &[(&str, bool)] = &[
            ("api.example.com", true),
            ("api.example.com:8080", false),
            ("evil.example.com", false),
        ];
        for &(host, want) in cases {
            let p = parts("GET", "/", &[("host", host)]);
            assert_eq!(pred.matches(&ctx(), &p), want, "host={host}");
        }
    }

    /// 验证：Query 谓词按参数名 + 全值正则匹配，缺参/错值/空查询串都不命中。
    #[test]
    fn query_predicate_table() {
        let pred = QueryPredicate::new("v".to_string(), r"\d+").unwrap();
        let cases = [
            ("/x?v=1", true),
            ("/x?v=abc", false),
            ("/x?other=1", false),
            ("/x", false),
            ("/x?v=1&v=2", true),
        ];
        for (uri, want) in cases {
            let p = parts("GET", uri, &[]);
            assert_eq!(pred.matches(&ctx(), &p), want, "uri={uri}");
        }
    }

    /// 验证：And 谓词要求全部子谓词同时命中，任一不满足即整体不匹配。
    #[test]
    fn and_predicate_requires_all() {
        let pred = AndPredicate::new(vec![
            Arc::new(MethodPredicate::new(vec![Method::GET])),
            Arc::new(QueryPredicate::new("v".into(), "1").unwrap()),
        ]);
        assert!(pred.matches(&ctx(), &parts("GET", "/x?v=1", &[])));
        assert!(!pred.matches(&ctx(), &parts("POST", "/x?v=1", &[])));
        assert!(!pred.matches(&ctx(), &parts("GET", "/x?v=2", &[])));
    }

    /// 验证：describe 输出稳定格式（管理接口/诊断输出依赖此格式）。
    #[test]
    fn describe_is_stable() {
        let pred = MethodPredicate::new(vec![Method::GET, Method::POST]);
        assert_eq!(pred.describe(), "Method=GET,POST");
    }

    /// 验证：full_match_regex 把用户模式锚定为全值匹配（安全修复的核心语义）。
    #[test]
    fn full_match_regex_anchors_user_pattern() {
        // 子串匹配已消失：模式必须覆盖整个值——前缀/后缀/扩展变体全部拒绝
        let re = full_match_regex(r"api\.example\.com").unwrap();
        assert!(re.is_match("api.example.com"));
        assert!(!re.is_match("xapi.example.com"));
        assert!(!re.is_match("api.example.com.evil"));
        // 择一保持顶层语义：匹配全值 a 或 b，绝不匹配 "xab" 这类混合值
        let re = full_match_regex("a|b").unwrap();
        assert!(re.is_match("a"));
        assert!(re.is_match("b"));
        assert!(!re.is_match("xab"));
        // 用户正则照常工作，去掉的只是隐式子串搜索
        let re = full_match_regex(".*example.*").unwrap();
        assert!(re.is_match("foexamplebar"));
        assert!(!re.is_match("counterexamp")); // 整个值里根本没有 "example"，全值匹配拒绝
    }

    /// 验证：Host 全值匹配语义——前缀/中缀/后缀/带端口变体一律拒绝。
    #[test]
    fn host_predicate_full_value_table() {
        // 字面 authority 写成正则时要转义点号，让 `.` 匹配字面点而非
        // 正则的“任意字符”通配符
        let pred = HostPredicate::new(r"api\.example\.com").unwrap();
        // 列含义：(Host 头值, 期望命中)
        let cases: &[(&str, bool)] = &[
            ("api.example.com", true),
            ("xapi.example.com", false),
            ("api-example.com", false),
            ("api.example.com.evil", false),
            ("api.example.com:8080", false),
            ("evil.example.com", false),
        ];
        for &(host, want) in cases {
            let p = parts("GET", "/", &[("host", host)]);
            assert_eq!(pred.matches(&ctx(), &p), want, "host={host}");
        }
        // 无 Host 头：回退用 uri.authority() 判定（行为保持不变）
        let p = parts("GET", "http://api.example.com/", &[]);
        assert!(pred.matches(&ctx(), &p), "authority fallback");
    }

    /// 验证：择一模式只匹配完整候选值（a|b 不命中 xab/ax 等部分重叠值）。
    #[test]
    fn host_predicate_alternation_matches_full_values_only() {
        let pred = HostPredicate::new("a|b").unwrap();
        for (host, want) in [("a", true), ("b", true), ("xab", false), ("ax", false)] {
            let p = parts("GET", "/", &[("host", host)]);
            assert_eq!(pred.matches(&ctx(), &p), want, "host={host}");
        }
    }

    /// 验证：Header 全值匹配语义——`\d+` 不命中前后带杂质的值（a42b/42a/空串）。
    #[test]
    fn header_predicate_full_value_table() {
        let pred = HeaderPredicate::new(HeaderName::from_static("x-tenant"), r"\d+").unwrap();
        let cases: &[(&str, bool)] = &[("42", true), ("a42b", false), ("42a", false), ("", false)];
        for &(value, want) in cases {
            let p = parts("GET", "/", &[("x-tenant", value)]);
            assert_eq!(pred.matches(&ctx(), &p), want, "value={value:?}");
        }
    }

    /// 验证：Query 全值匹配语义（`a|b` 不命中 xab/ab），其他参数不顶替。
    #[test]
    fn query_predicate_full_value_table() {
        let pred = QueryPredicate::new("token".to_string(), "a|b").unwrap();
        let cases = [
            ("/x?token=a", true),
            ("/x?token=b", true),
            ("/x?token=xab", false),
            ("/x?token=ab", false),
            ("/x?other=a", false),
        ];
        for (uri, want) in cases {
            let p = parts("GET", uri, &[]);
            assert_eq!(pred.matches(&ctx(), &p), want, "uri={uri}");
        }
    }
}
