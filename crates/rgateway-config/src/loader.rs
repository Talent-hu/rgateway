//! 从文件或字符串加载并解析 YAML 配置。
//!
//! 本模块只做到 schema 层（反序列化为 [`ConfigFile`]），语义校验在
//! `validate`、编译在 `compile`。两条防线贯穿所有入口：
//!
//! 1. **路径包含（containment）**：配置引用的相对路径（TLS 证书/私钥、
//!    gRPC descriptor 等）以配置文件所在目录为基准解析，且必须留在该
//!    目录内。威胁模型：配置文件可能被不受控的来源提供，借绝对路径读取
//!    任意系统文件（`/etc/passwd`、`C:\Windows\...`），或用 `..` 跳出
//!    部署目录——因此绝对引用（POSIX 根、Windows 盘符、UNC）与向上逃逸
//!    的 `..` 一律拒绝，含尾随点/空格或 `:` 的段（Win32 别名/ADS 混淆
//!    向量）在所有平台同样拒绝，并返回带 [`Location`] 的 [`ConfigError`]。错误
//!    消息只回显输入引用本身、绝不回显解析后的目标路径，避免向配置作者
//!    泄露目录布局。解析是纯词法的：`.` 与 `..` 归一化不触碰文件系统。
//! 2. **大小上限**：无论来自文件还是内存字符串，输入都受
//!    [`MAX_CONFIG_BYTES`]（16 MiB）限制，且在完整读取/解析之前拒绝，
//!    防止超大输入耗尽内存。热重载的读取路径（[`read_capped`]）同受此限。

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use crate::error::{ConfigError, Location};
use crate::schema::ConfigFile;

/// 配置大小上限：16 MiB。
pub const MAX_CONFIG_BYTES: usize = 16 * 1024 * 1024;

/// 把 YAML 文本解析为原始 [`ConfigFile`]（仅 schema 层，不做语义校验）。
///
/// 超过 [`MAX_CONFIG_BYTES`] 的输入在解析前即被拒绝。
///
/// # 错误
///
/// 超限或 YAML 语法错误时返回 [`ConfigError`]。
pub fn load_from_str(yaml: &str) -> Result<ConfigFile, ConfigError> {
    parse_with_cap(yaml, MAX_CONFIG_BYTES)
}

/// 读取并解析 YAML 配置文件。
///
/// 超过 [`MAX_CONFIG_BYTES`] 的文件在完整读取前即被拒绝
/// （见 [`read_capped`] 的双重检查）。
///
/// # 错误
///
/// I/O 失败、超限或 YAML 语法错误时返回 [`ConfigError`]。
pub fn load_from_file(path: &Path) -> Result<ConfigFile, ConfigError> {
    load_from_file_capped(path, MAX_CONFIG_BYTES)
}

/// 把配置里的相对文件引用解析成以配置文件目录为基准的完整路径。
///
/// 引用必须留在配置目录内（威胁模型见模块文档）：绝对引用与会逃逸的
/// `..` 被拒绝，返回 [`ConfigError::value`]，其 [`Location`] 就是出错的
/// 引用文本。引用先做纯词法归一化（不触碰文件系统），再拼接到配置文件
/// 的父目录上。
///
/// # 错误
///
/// 绝对引用、`..` 逃逸或非 UTF-8 段时返回 [`ConfigError`]。
pub fn resolve_path(config_file: &Path, relative: &str) -> Result<PathBuf, ConfigError> {
    let segments = normalize_inside(relative)?;
    let base = config_file.parent().unwrap_or_else(|| Path::new(""));
    Ok(join_segments(base, &segments))
}

/// 带显式大小上限的解析（测试用小上限验证边界行为）。
fn parse_with_cap(yaml: &str, max_bytes: usize) -> Result<ConfigFile, ConfigError> {
    if yaml.len() > max_bytes {
        return Err(too_large(max_bytes));
    }
    Ok(serde_yaml::from_str(yaml)?)
}

/// 带显式大小上限的读取 + 解析（测试用小上限验证边界行为）。
fn load_from_file_capped(path: &Path, max_bytes: usize) -> Result<ConfigFile, ConfigError> {
    let yaml = read_capped(path, max_bytes)?;
    parse_with_cap(&yaml, max_bytes)
}

/// 读文件，但任何时候持有的内容不超过 `max_bytes`：先用 metadata 拒绝
/// 超大文件，再做有界读取兜底——metadata 可能撒谎（特殊文件、读取期间
/// 并发增长），多读 1 字节即可判定「刚好超限」。
pub(crate) fn read_capped(path: &Path, max_bytes: usize) -> Result<String, ConfigError> {
    let file = std::fs::File::open(path)?;
    if file.metadata()?.len() > max_bytes as u64 {
        return Err(too_large(max_bytes));
    }
    let mut buf = String::new();
    let mut limited = file.take(max_bytes as u64 + 1);
    limited.read_to_string(&mut buf)?;
    if buf.len() > max_bytes {
        return Err(too_large(max_bytes));
    }
    Ok(buf)
}

/// 输入超过大小上限的定位错误（[`Location::root`]）。
fn too_large(max_bytes: usize) -> ConfigError {
    ConfigError::value(
        Location::root(),
        format!("config exceeds the {max_bytes} byte size limit"),
    )
}

/// 把引用纯词法归一化为留在基准目录内的段序列：丢弃 `.`，遇 `..` 弹出
/// 一段；弹空后仍有 `..` 即为逃逸。绝对引用与根/盘符组件直接拒绝。
fn normalize_inside(reference: &str) -> Result<Vec<&str>, ConfigError> {
    if is_absolute_reference(reference) {
        return Err(reject(
            reference,
            "must be relative to the config directory",
        ));
    }
    let mut kept: Vec<&str> = Vec::new();
    for component in Path::new(reference).components() {
        match component {
            Component::Normal(segment) => match segment.to_str() {
                Some(segment) => {
                    // Win32 会剥掉尾随点/空格、把 ':' 解释为 ADS 流分隔符，
                    // 同一逻辑名可指向不同文件，产生别名混淆向量；为跨平台
                    // 行为一致，所有平台一律拒绝这类段。
                    if segment.ends_with('.') || segment.ends_with(' ') || segment.contains(':') {
                        return Err(reject(
                            reference,
                            "has a trailing dot/space or ':' (unsafe filename)",
                        ));
                    }
                    kept.push(segment)
                }
                None => return Err(reject(reference, "contains invalid UTF-8")),
            },
            Component::CurDir => {}
            Component::ParentDir => {
                // 弹出上一段；已到基准仍要上跳 = 逃逸
                if kept.pop().is_none() {
                    return Err(reject(reference, "escapes the config directory"));
                }
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err(reject(
                    reference,
                    "must be relative to the config directory",
                ));
            }
        }
    }
    Ok(kept)
}

/// 平台无关地判定「绝对风格」引用，在所有平台上一律拒绝以保证行为
/// 一致：以 `/` 或 `\` 开头，或带盘符限定（`C:\x`、`C:x`、UNC、verbatim）
/// ——即使是 Windows 下语义为「盘符相对」的 `C:x` 也拒绝。
fn is_absolute_reference(reference: &str) -> bool {
    if reference.starts_with('/') || reference.starts_with('\\') {
        return true;
    }
    let bytes = reference.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// 把归一化后的段依次拼接到基准目录上。
fn join_segments(base: &Path, segments: &[&str]) -> PathBuf {
    let mut out = base.to_path_buf();
    for segment in segments {
        out.push(segment);
    }
    out
}

/// 构造被拒引用的定位错误（[`Location`] 即引用文本）。消息只以 `{:?}`
/// 渲染输入本身——控制字符无法借此伪造日志行，也绝不回显解析后的目标。
fn reject(reference: &str, reason: &str) -> ConfigError {
    ConfigError::value(
        Location::section(reference),
        format!("file reference {reference:?} {reason}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 验证大小上限常量固定为 16 MiB。
    #[test]
    fn max_config_bytes_is_16mib() {
        assert_eq!(MAX_CONFIG_BYTES, 16 * 1024 * 1024);
    }

    /// 验证 resolve_path 的路径包含性：合法相对引用归一化到配置目录内，
    /// 绝对/逃逸引用被拒绝，且错误消息不泄露解析后的目标路径。
    #[test]
    fn resolve_path_containment_table() {
        // 用一个有辨识度的基准目录名，使错误文本里若泄露了解析目标
        // （散文里的 "config directory" 会与朴素的 "conf" 基准撞车）能被
        // 断言捕获。
        let config_file = Path::new("rgwbase/rgateway.yaml");
        let base = Path::new("rgwbase");

        // 表列：(引用, 期望的相对段序列——最终拼在配置目录之下)。
        let ok_cases: &[(&str, &[&str])] = &[
            ("./a.pem", &["a.pem"]),
            ("sub/a.pem", &["sub", "a.pem"]),
            ("a/../b.pem", &["b.pem"]),
            ("a/..", &[]),
            ("", &[]),
        ];
        for &(input, segments) in ok_cases {
            let got = resolve_path(config_file, input)
                .unwrap_or_else(|e| panic!("{input:?} must resolve: {e}"));
            let mut want = base.to_path_buf();
            for &segment in segments {
                want.push(segment);
            }
            assert_eq!(got, want, "{input:?}");
        }

        // 必须拒绝的引用：`..` 逃逸、盘符限定、POSIX 根、反斜杠开头。
        // 注意 `..\\a.pem` 的拒绝依赖平台分隔符语义：Windows 把 `\` 当分隔
        // 符，Path::components 解析出 ParentDir 逃逸而拒绝；非 Windows 平台
        // 上 `\` 不是分隔符，整个引用是单个字面文件名，会被接受（本表按
        // Windows 语义断言）。`\\x.pem` 则因反斜杠开头的字节检查在所有平台
        // 一律拒绝。
        let reject_cases = [
            "../a.pem",
            "sub/../../a.pem",
            "C:/x.pem",
            "C:x.pem",
            "/etc/x.pem",
            "\\x.pem",
            "..\\a.pem",
        ];
        for input in reject_cases {
            let err = resolve_path(config_file, input).expect_err("must be rejected");
            match err {
                ConfigError::Value { location, message } => {
                    assert_eq!(location.to_string(), input, "{input:?}");
                    // 消息以 `{:?}` 渲染输入（控制字符无法伪造日志行），
                    // 断言必须按渲染后的形式出现。
                    assert!(
                        message.contains(&format!("{input:?}")),
                        "{input:?}: {message}"
                    );
                    assert!(
                        !message.contains("rgwbase"),
                        "{input:?}: resolved target leaked: {message}"
                    );
                }
                other => panic!("{input:?}: expected ConfigError::Value, got {other:?}"),
            }
        }
    }

    /// 验证 Win32 别名向量的拒绝：尾随点/空格段与含 `:` 的段在所有
    /// 平台一律被拒（防同名别名与 ADS 流混淆），正常名字不受影响。
    #[test]
    fn resolve_path_rejects_win32_aliasing_segments() {
        let config_file = Path::new("rgwbase/rgateway.yaml");
        for input in ["cert./a.pem", "cert /a.pem", "x.", "sub./x.pem", "sub/a:b"] {
            let err = resolve_path(config_file, input).expect_err("must be rejected");
            match err {
                ConfigError::Value { location, message } => {
                    assert_eq!(location.to_string(), input, "{input:?}");
                    assert!(
                        message.contains(&format!("{input:?}")),
                        "{input:?}: {message}"
                    );
                }
                other => panic!("{input:?}: expected ConfigError::Value, got {other:?}"),
            }
        }
        // 干净的名字与「点开头但不点结尾」的名字不受影响。
        assert!(resolve_path(config_file, "sub/a-b.pem").is_ok());
        assert!(resolve_path(config_file, ".hidden/x.pem").is_ok());
    }

    /// 验证 parse_with_cap 的大小上限：超限输入在解析前被拒绝。
    #[test]
    fn size_cap_table() {
        // 表列：(yaml 文本, 字节上限, 是否应接受)——接受时必须能解析为
        // `ConfigFile`。
        let cases: &[(&str, usize, bool)] = &[
            ("{}", 2, true),
            ("{}", 1, false),
            ("{} # pad", 8, true),
            ("{} # pad", 4, false),
        ];
        for (yaml, cap, accepted) in cases {
            let result = parse_with_cap(yaml, *cap);
            if *accepted {
                assert!(
                    result.is_ok(),
                    "{yaml:?} at cap {cap} must parse: {:?}",
                    result.err()
                );
            } else {
                let err = result.expect_err("input over cap must be rejected");
                match err {
                    ConfigError::Value { location, message } => {
                        assert_eq!(location, Location::root(), "{yaml:?} at cap {cap}");
                        assert!(message.contains(&cap.to_string()), "{message}");
                    }
                    other => panic!("{yaml:?} at cap {cap}: expected Value, got {other:?}"),
                }
            }
        }
    }

    /// 验证文件读取的有界上限：超限文件不做完整读取即被拒绝。
    #[test]
    fn load_from_file_cap_table() {
        // 表列：(文件内容, 字节上限, 是否应接受)。
        let cases: &[(&str, usize, bool)] = &[
            ("{}", 2, true),
            ("{}", 1, false),
            ("{} # pad", 8, true),
            ("{} # pad", 4, false),
        ];
        for (content, cap, accepted) in cases {
            let path = std::env::temp_dir()
                .join(format!("rgateway-loader-cap-{}.yaml", uuid::Uuid::now_v7()));
            std::fs::write(&path, content).expect("write fixture");
            let result = load_from_file_capped(&path, *cap);
            std::fs::remove_file(&path).expect("remove fixture");
            if *accepted {
                assert!(
                    result.is_ok(),
                    "{content:?} at cap {cap} must parse: {:?}",
                    result.err()
                );
            } else {
                let err = result.expect_err("file over cap must be rejected");
                match err {
                    ConfigError::Value { location, message } => {
                        assert_eq!(location, Location::root(), "{content:?} at cap {cap}");
                        assert!(message.contains(&cap.to_string()), "{message}");
                    }
                    other => panic!("{content:?} at cap {cap}: expected Value, got {other:?}"),
                }
            }
        }
    }
}
