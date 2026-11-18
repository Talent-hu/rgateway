//! [`crate::ant_path`] 的测试：通配语义、尾部斜杠、`**` 折叠、线性时间
//! 上界与模式校验。

use super::*;
use rgateway_core::Protocol;

// 表驱动执行器：cases 列含义 (模式, 请求路径, 期望是否匹配)，逐条编译并断言。
fn run_match_table(cases: &[(&str, &str, bool)]) {
    for &(pattern, path, want) in cases {
        let p = AntPattern::new(pattern).unwrap_or_else(|e| panic!("{pattern:?}: {e}"));
        assert_eq!(p.matches(path), want, "pattern={pattern:?} path={path:?}");
    }
}

// 构造请求 Parts 并调用谓词，模拟路由器看到的 `parts.uri.path()` 输入。
fn pred_matches(pred: &PathPredicate, uri: &str) -> bool {
    let parts = http::Request::builder()
        .uri(uri)
        .body(())
        .unwrap()
        .into_parts()
        .0;
    pred.matches(&RequestCtx::new(Protocol::Http1, None), &parts)
}

// 验证核心通配语义表：`**` 跨零或多段、`*` 仅段内、`?` 恰一字符、
// 字面量大小写敏感以及尾部斜杠行为。
#[test]
fn match_semantics_table() {
    // 列：(模式, 请求路径, 期望是否匹配)。
    let cases: &[(&str, &str, bool)] = &[
        // "/api/**"：零个或多个完整段
        ("/api/**", "/api", true),
        ("/api/**", "/api/", true),
        ("/api/**", "/api/users", true),
        ("/api/**", "/api/users/1", true),
        ("/api/**", "/v1", false),
        ("/api/**", "/apix", false),
        // "/api/*"：恰好一段，永不过 '/'
        ("/api/*", "/api/x", true),
        ("/api/*", "/api/", true),
        ("/api/*", "/api", false),
        ("/api/*", "/api/x/y", false),
        // "/a/**/b"：`**` 在中间，零个或多个段
        ("/a/**/b", "/a/b", true),
        ("/a/**/b", "/a/x/b", true),
        ("/a/**/b", "/a/x/y/b", true),
        ("/a/**/b", "/a/x/y/c", false),
        ("/a/**/b", "/b", false),
        // "/users/?"：恰好一个字符
        ("/users/?", "/users/1", true),
        ("/users/?", "/users/10", false),
        ("/users/?", "/users/", false),
        ("/users/?", "/users", false),
        // "/health"：纯字面量
        ("/health", "/health", true),
        ("/health", "/health/", false),
        ("/health", "/health/x", false),
        ("/health", "/Health", false),
        ("/health", "/x", false),
        // "/**"：匹配一切
        ("/**", "/", true),
        ("/**", "/anything", true),
        ("/**", "/a/b/c", true),
        // "/**/x"
        ("/**/x", "/x", true),
        ("/**/x", "/a/b/x", true),
        ("/**/x", "/a/b/y", false),
    ];
    run_match_table(cases);
}

// 验证裸 `**` 速记与 `/**` 等价，匹配一切路径。
#[test]
fn bare_catch_all_matches_everything() {
    // 裸 `**` 速记编译结果等价 `/**`。
    run_match_table(&[
        ("**", "/", true),
        ("**", "/x", true),
        ("**", "/a/b/c", true),
    ]);
}

// 验证段内通配的边界情形：`*` 可匹配空串、`?` 按字符（而非字节）计数、
// 空段、连续 `**`、字面量与通配混排。
#[test]
fn wildcard_edge_cases() {
    // 列：(模式, 请求路径, 期望是否匹配)。
    let cases: &[(&str, &str, bool)] = &[
        ("/a*", "/a", true),    // `*` 匹配零个字符
        ("/a*", "/abc", true),  // `*` 在段内匹配
        ("/a*", "/a/b", false), // `*` 永不过 '/'
        ("/x/??", "/x/ab", true),
        ("/x/??", "/x/a", false),
        ("/x/??", "/x/abc", false),
        ("/?", "/é", true), // `?` 按字符计数，与字符宽度无关
        ("/?", "/ab", false),
        ("/a/*/b", "/a//b", true), // `*` 可以匹配空段
        ("/**/**/x", "/x", true),  // 连续 `**` 依然有效
        ("/**/**/x", "/a/b/x", true),
        ("/a/**", "/a", true), // `**` 匹配零个段
        ("/a/**", "/a/", true),
        ("//a", "//a", true), // 前导空段被保留
        ("//a", "/a", false),
        ("/a?c", "/abc", true), // 通配符可与字面量混排
        ("/a?c", "/abbc", false),
    ];
    run_match_table(cases);
}

// 验证构造期折叠连续 `**` 语义保持：`/a/**/**/b` 与 `/a/**/b` 对所有
// 路径的匹配结果一致。
#[test]
fn consecutive_star_star_collapse_equivalence() {
    // "/a/**/**/b" 在构造期被折叠为 "/a/**/b"；两者必须在每条路径上
    // 结果一致，并保持文档承诺的语义。
    let collapsed = AntPattern::new("/a/**/b").unwrap();
    let uncollapsed = AntPattern::new("/a/**/**/b").unwrap();
    for path in ["/a/b", "/a/x/b", "/a/x/y/b", "/a/b/", "/ab", "/a", "/"] {
        assert_eq!(
            collapsed.matches(path),
            uncollapsed.matches(path),
            "{path:?}"
        );
    }
    run_match_table(&[
        ("/a/**/**/b", "/a/b", true),
        ("/a/**/**/b", "/a/x/y/b", true),
        ("/a/**/**/b", "/a/x/y", false),
    ]);
}

// 验证多个 `**` 交错时单回溯点贪心的正确性——多 `**` 组合路径是最易
// 出错的场景。
#[test]
fn three_interleaved_star_star_tricky_paths() {
    // 列：(模式, 请求路径, 期望是否匹配)。
    let cases: &[(&str, &str, bool)] = &[
        ("/a/**/b/**/c/**/d", "/a/b/c/d", true),
        ("/a/**/b/**/c/**/d", "/a/x/y/b/c/d", true),
        ("/a/**/b/**/c/**/d", "/a/b/x/c/y/d", true),
        ("/a/**/b/**/c/**/d", "/a/x/b/y/c/z/d", true),
        ("/a/**/b/**/c/**/d", "/a/b/c/x/y/d", true),
        ("/a/**/b/**/c/**/d", "/a/b/c/d/e", false),
        ("/a/**/b/**/c/**/d", "/a/b/c", false),
        ("/a/**/b/**/c/**/d", "/a/b/d", false),
        ("/a/**/b/**/c/**/d", "/a/d", false),
        ("/a/**/b/**/c/**/d", "/b/c/d", false),
        ("/a/**/b/**/c/**/d", "/a/x/b/y/c", false),
    ];
    run_match_table(cases);
}

// 验证 k 个 `**` 不会引发指数回溯：对 1000 段的攻击性路径必须在近线性
// 时间内完成匹配。
#[test]
fn many_star_star_segments_match_in_linear_time() {
    // 对抗性用例：k 个 `**` 不得对攻击者可控的长路径做指数回溯。粗粒度
    // 时限只用于排除旧的 O(n^k) 爆炸（秒级），并非精细的性能基准。
    let p = AntPattern::new("/**/**/**/**/api").unwrap();
    let long_tail = "seg/".repeat(999);
    let non_matching = format!("/{long_tail}seg");
    let matching = format!("/{long_tail}api");
    let start = std::time::Instant::now();
    assert!(!p.matches(&non_matching));
    assert!(p.matches(&matching));
    assert!(
        start.elapsed() < std::time::Duration::from_millis(100),
        "matching a 1000-segment path must be near-linear, took {:?}",
        start.elapsed()
    );
}

// 验证 validate 拒绝非法模式：空串、相对路径、段内 `**`、三连 `*`。
#[test]
fn validate_rejects_malformed_patterns() {
    for bad in ["", "api/*", "/a/**b", "/a/b**", "**/x", "/a/***"] {
        assert!(validate(bad).is_err(), "{bad:?} must be rejected");
    }
}

// 验证 validate 接受合法模式：纯字面量、段内通配、整段 `**`。
#[test]
fn validate_accepts_well_formed_patterns() {
    for good in ["/health", "/api/*", "/api/**", "/a/**/b", "/users/?", "/**"] {
        assert!(validate(good).is_ok(), "{good:?} must be accepted");
    }
}

// 验证宽松入口仅限 AntPattern::new：validate 要求前导 '/'，new 额外
// 接受裸 `**` 速记。
#[test]
fn bare_catch_all_is_new_shorthand_only() {
    // `validate` 强制绝对模式（前导 '/'）；`new` 把裸 `**` 当作 `/**`
    // 的速记接受。
    assert!(validate("**").is_err());
    assert!(AntPattern::new("**").is_ok());
}

// 验证 PathPredicate 的 OR 语义：任一模式命中即整体命中。
#[test]
fn path_predicate_matches_any_pattern() {
    let pred = PathPredicate::new(vec!["/api/**".to_string(), "/health".to_string()]).unwrap();
    assert!(pred_matches(&pred, "/api/users/1"));
    assert!(pred_matches(&pred, "/health"));
    assert!(!pred_matches(&pred, "/other"));
}

// 验证匹配只看 uri.path()：查询串里的伪路径不影响匹配结果。
#[test]
fn path_predicate_ignores_query_string() {
    let pred = PathPredicate::new(vec!["/users/1".to_string()]).unwrap();
    assert!(pred_matches(&pred, "/users/1?name=/health"));
    let pred = PathPredicate::new(vec!["/health".to_string()]).unwrap();
    assert!(!pred_matches(&pred, "/users?x=/health"));
}

// 验证空模式集合永不命中（any 对空迭代返回 false）。
#[test]
fn path_predicate_with_no_patterns_matches_nothing() {
    let pred = PathPredicate::new(vec![]).unwrap();
    assert!(!pred_matches(&pred, "/anything"));
}

// 验证任一模式非法时构造快速失败返回 Err。
#[test]
fn path_predicate_rejects_bad_patterns() {
    assert!(PathPredicate::new(vec!["/ok".to_string(), "bad".to_string()]).is_err());
}

// 验证 describe 输出 `Path=<p1>,<p2>` 形式并原样回显模式字符串。
#[test]
fn describe_lists_patterns() {
    let pred = PathPredicate::new(vec!["/a".to_string(), "/b".to_string()]).unwrap();
    assert_eq!(pred.describe(), "Path=/a,/b");
}
