//! [`crate::metrics`] 的测试：安装语义、各仪表函数的渲染输出与标签基数红线。
//!
//! 期望值全部硬编码（指标名、标签键、标签值、样本值逐字断言），保证断言
//! 独立于实现常量；样本行按「整行 / 标签段」断言，既验证计量值，又验证标签
//! 恰好是有界集合——没有多余标签泄漏进来。

use std::time::Duration;

use crate::ObsError;

use super::*;

/// 在独立的本地 recorder 中执行记录动作，返回渲染出的 Prometheus 文本。
///
/// 为什么用 `metrics::with_local_recorder`（已在 metrics 0.24.6 registry
/// 核实存在）：发射宏先查线程本地 recorder，闭包期间的全部发射被本用例的
/// 隔离 recorder 捕获——既不依赖全局安装状态，也不与其他并行用例互相污染。
fn render_with(f: impl FnOnce()) -> String {
    let recorder = recorder_builder()
        .expect("直方图桶界为非空常量，builder 不会失败")
        .build_recorder();
    metrics::with_local_recorder(&recorder, f);
    recorder.handle().render()
}

/// 取渲染输出中以 `prefix` 开头的首个样本行（跳过 `#` 注释行）。
fn sample_line<'a>(render: &'a str, prefix: &str) -> &'a str {
    render
        .lines()
        .find(|line| !line.starts_with('#') && line.starts_with(prefix))
        .unwrap_or_else(|| panic!("未找到样本行 {prefix}\n---\n{render}"))
}

/// 取样本行的 `{...}` 标签段（含花括号），无标签行返回 `None`。
fn label_section(line: &str) -> Option<&str> {
    let start = line.find('{')?;
    let end = line.find('}')?;
    Some(&line[start..=end])
}

/// 验证：全局 recorder 一次性安装——首次成功、重复安装报错（首个 recorder
/// 保持生效），错误变体与文案可判别。
#[test]
fn install_is_one_shot() {
    let first = install().expect("首次安装成功");
    let err = install().expect_err("重复安装必须报错");
    assert!(
        matches!(err, ObsError::RecorderInstall(_)),
        "错误变体应为 RecorderInstall: {err:?}"
    );
    // 只断言本 crate 自有的错误前缀，不钉死上游库文案——上游换词不应误伤本测试
    assert!(
        err.to_string()
            .contains("failed to install metrics recorder"),
        "错误文案应带本 crate 自有前缀: {err}"
    );
    // 首个 recorder 保持生效：全局发射仍进入首个 handle
    record_route_not_found();
    assert!(
        first.render().contains("rgateway_route_not_found_total 1"),
        "首个 handle 应继续收集指标"
    );
}

/// 验证：`record_request` 计数器输出——指标名、标签键序、status 十进制
/// 字符串、相同序列累计、不同 status 独立成序列。
#[test]
fn record_request_counter_lines() {
    let render = render_with(|| {
        for _ in 0..2 {
            record_request("user-route", "GET", 200, Duration::from_millis(250));
        }
        record_request("order-route", "POST", 503, Duration::from_millis(5));
    });
    // cases 列：期望出现的输出行子串（指标名 + 标签段 + 累计值）
    let cases = [
        "# TYPE rgateway_requests_total counter",
        "rgateway_requests_total{route=\"user-route\",method=\"GET\",status=\"200\"} 2",
        "rgateway_requests_total{route=\"order-route\",method=\"POST\",status=\"503\"} 1",
    ];
    for expected in cases {
        assert!(render.contains(expected), "缺少: {expected}\n---\n{render}");
    }
}

/// 验证：`record_request` 耗时直方图——秒浮点口径、桶界累积计数、
/// `_sum` / `_count` 行齐全。
#[test]
fn record_request_duration_histogram_lines() {
    let render = render_with(|| {
        record_request("user-route", "GET", 200, Duration::from_millis(250));
    });
    // cases 列：期望出现的输出行子串（直方图各类样本行）
    let cases = [
        "# TYPE rgateway_request_duration_seconds histogram",
        "rgateway_request_duration_seconds_bucket{route=\"user-route\",method=\"GET\",le=\"0.1\"} 0",
        "rgateway_request_duration_seconds_bucket{route=\"user-route\",method=\"GET\",le=\"0.25\"} 1",
        "rgateway_request_duration_seconds_bucket{route=\"user-route\",method=\"GET\",le=\"+Inf\"} 1",
        "rgateway_request_duration_seconds_sum{route=\"user-route\",method=\"GET\"} 0.25",
        "rgateway_request_duration_seconds_count{route=\"user-route\",method=\"GET\"} 1",
    ];
    for expected in cases {
        assert!(render.contains(expected), "缺少: {expected}\n---\n{render}");
    }
}

/// 验证：耗时以**秒**记入直方图（不是毫秒/微秒），浮点值渲染正确。
#[test]
fn record_request_duration_is_seconds() {
    // cases 列：请求耗时 | 期望 `_sum` 样本行（整行相等）
    let cases: [(Duration, &str); 3] = [
        (
            Duration::from_millis(250),
            "rgateway_request_duration_seconds_sum{route=\"r\",method=\"GET\"} 0.25",
        ),
        (
            Duration::from_millis(1500),
            "rgateway_request_duration_seconds_sum{route=\"r\",method=\"GET\"} 1.5",
        ),
        (
            Duration::from_millis(5),
            "rgateway_request_duration_seconds_sum{route=\"r\",method=\"GET\"} 0.005",
        ),
    ];
    for (duration, expected) in cases {
        let render = render_with(|| record_request("r", "GET", 200, duration));
        assert!(render.contains(expected), "缺少: {expected}\n---\n{render}");
    }
}

/// 验证（基数红线）：`record_request` 的标签段逐字受控——只有
/// route/method/status（耗时直方图只有 route/method），绝不混入
/// path、query、request_id、上游地址等无界标签。
#[test]
fn record_request_labels_are_bounded() {
    let render = render_with(|| {
        record_request("user-route", "GET", 200, Duration::from_millis(250));
    });
    // cases 列：样本行前缀 | 期望标签段（逐字相等 ⇒ 没有多余标签）
    let cases: [(&str, &str); 3] = [
        (
            "rgateway_requests_total",
            "{route=\"user-route\",method=\"GET\",status=\"200\"}",
        ),
        (
            "rgateway_request_duration_seconds_count",
            "{route=\"user-route\",method=\"GET\"}",
        ),
        (
            "rgateway_request_duration_seconds_sum",
            "{route=\"user-route\",method=\"GET\"}",
        ),
    ];
    for (prefix, expected_labels) in cases {
        let line = sample_line(&render, prefix);
        assert_eq!(label_section(line), Some(expected_labels), "行: {line}");
    }
    // 无界标签键一个都不许出现（防基数爆炸 = 防内存放大攻击面）
    for forbidden in ["path=", "query=", "request_id=", "upstream=", "peer="] {
        assert!(
            !render.contains(forbidden),
            "不应出现标签: {forbidden}\n{render}"
        );
    }
}

/// 验证：`record_upstream_error` 计数器——route/kind 标签与累计值。
#[test]
fn record_upstream_error_lines() {
    let render = render_with(|| {
        record_upstream_error("user-route", "connect_timeout");
        record_upstream_error("user-route", "connect_timeout");
        record_upstream_error("order-route", "tls_error");
    });
    // cases 列：期望出现的输出行子串（指标名 + 标签段 + 累计值）
    let cases = [
        "rgateway_upstream_errors_total{route=\"user-route\",kind=\"connect_timeout\"} 2",
        "rgateway_upstream_errors_total{route=\"order-route\",kind=\"tls_error\"} 1",
    ];
    for expected in cases {
        assert!(render.contains(expected), "缺少: {expected}\n---\n{render}");
    }
}

/// 验证：活跃请求 gauge 随 open/close 增减，终值与操作序列一致。
#[test]
fn active_requests_gauge_tracks_concurrency() {
    // cases 列：操作序列（true=+1 开始请求，false=-1 请求结束）| 期望 gauge 值
    let cases: [(Vec<bool>, &str); 4] = [
        (vec![true], "1"),
        (vec![true, true], "2"),
        (vec![true, true, false], "1"),
        (vec![true, false], "0"),
    ];
    for (ops, expected_value) in cases {
        let render = render_with(|| {
            for open in ops {
                if open {
                    record_active_requests_open();
                } else {
                    record_active_requests_close();
                }
            }
        });
        let line = sample_line(&render, "rgateway_active_requests");
        assert_eq!(
            line,
            format!("rgateway_active_requests {expected_value}"),
            "渲染: {render}"
        );
    }
}

/// 验证：`record_route_not_found` 专用计数器——无标签（未命中就没有
/// route id 可用，不允许编造假标签值），按次累计。
#[test]
fn route_not_found_counter_is_label_free() {
    let render = render_with(|| {
        record_route_not_found();
        record_route_not_found();
    });
    let line = sample_line(&render, "rgateway_route_not_found_total");
    assert_eq!(line, "rgateway_route_not_found_total 2", "渲染: {render}");
}

/// 验证（基数红线）：method 白名单归一——9 个标准方法原样保留，域外值
/// （自定义方法、小写、空串、伪造 token）一律收敛为 "OTHER"。
#[test]
fn normalize_method_whitelist_table() {
    // cases 列：输入 method | 期望标签值
    let cases = [
        ("GET", "GET"),
        ("HEAD", "HEAD"),
        ("POST", "POST"),
        ("PUT", "PUT"),
        ("DELETE", "DELETE"),
        ("CONNECT", "CONNECT"),
        ("OPTIONS", "OPTIONS"),
        ("PATCH", "PATCH"),
        ("TRACE", "TRACE"),
        ("PURGE", "OTHER"),
        ("get", "OTHER"),
        ("", "OTHER"),
        ("EVIL\r\nX-Injected", "OTHER"),
        ("GET ", "OTHER"),
    ];
    for (input, want) in cases {
        assert_eq!(normalize_method(input), want, "input={input:?}");
    }
}

/// 验证（基数红线）：kind 格式归一——`[a-z0-9_]{1,64}` 内保留，其余
/// （大写、连字符、空、超长、无界文本）收敛为 "invalid"。
#[test]
fn normalize_kind_format_table() {
    // cases 列：输入 kind | 期望标签值
    let cases = [
        ("connect_timeout", "connect_timeout"),
        ("reset", "reset"),
        ("io", "io"),
        ("", "invalid"),
        ("TLS_Error", "invalid"),
        ("tls-error", "invalid"),
        ("a b", "invalid"),
        ("upstream request failed: refused", "invalid"),
    ];
    for (input, want) in cases {
        assert_eq!(normalize_kind(input), want, "input={input:?}");
    }
    let long = "a".repeat(65);
    assert_eq!(normalize_kind(&long), "invalid", "65 字符超长应归一");
}

/// 验证（基数红线）：伪造 method/kind 进入渲染输出时被归一为受控值——
/// 攻击者无法用自定义方法名制造无限时间序列。
#[test]
fn forged_labels_are_normalized_in_render() {
    let render = render_with(|| {
        record_request("r", "EVIL\r\nX", 200, Duration::from_millis(1));
        record_upstream_error("r", "EVIL-KIND");
    });
    // cases 列：期望出现的输出行子串（归一后的受控标签值）
    let cases = [
        "rgateway_requests_total{route=\"r\",method=\"OTHER\",status=\"200\"} 1",
        "rgateway_upstream_errors_total{route=\"r\",kind=\"invalid\"} 1",
    ];
    for expected in cases {
        assert!(render.contains(expected), "缺少: {expected}\n---\n{render}");
    }
    assert!(
        !render.contains("EVIL"),
        "伪造标签值不得原样进指标输出\n{render}"
    );
}
