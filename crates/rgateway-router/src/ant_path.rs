//! Ant 风格路径模式匹配（`?`、`*`、`**`）与 Path 谓词。
//!
//! # 通配语义（与 Spring Cloud Gateway 兼容，大小写敏感）
//!
//! 模式与路径都按 `/` 切分为「段」（segment），通配符语义限定在段这一层：
//!
//! * `?`：段内恰好匹配一个字符
//! * `*`：段内匹配零个或多个字符，永不过段边界（`/`）
//! * `**`：仅当独占整个段时生效，匹配零个或多个完整段（可以跨 `/`）
//! * 其余字符一律按字面量匹配
//!
//! 大小写敏感：`/health` 不匹配 `/Health`，也不做 Unicode 归一化，比较
//! 是精确的字符串/字符比较。
//!
//! # 尾部斜杠行为
//!
//! 模式与路径在忽略一个前导 `/` 后按 `/` 切分为段，尾部 `/` 会留下一个
//! *空段*。因此 `/api/*` 匹配 `/api/`（恰好一个空段）但不匹配 `/api`；
//! `/api/**` 两者都匹配——`**` 也可以匹配零个段。
//!
//! ```text
//! /api/*     匹配 /api/x、/api/          不匹配 /api、/api/x/y
//! /api/**    匹配 /api、/api/、/api/a/b  不匹配 /v1、/apix
//! /users/?   匹配 /users/1               不匹配 /users/10、/users/
//! /a/**/b    匹配 /a/b、/a/x/y/b         不匹配 /a/x/y/c
//! ```
//!
//! # 设计要点
//!
//! * **匹配零分配**：模式在构造期一次性编译成段匹配器（`Segment`/`Token`
//!   结构，不引入正则），匹配全程借用请求路径切片、不分配 `String`。
//!   路由匹配位于每请求的热路径上，把成本全部压到构造期，可以消除每请求
//!   的堆分配。
//! * **构造期折叠连续 `**`**：`/**/**/x` 与 `/**/x` 匹配完全相同的路径集
//!   （连续 `**` 联合吞并的段序列集合恰等于单个 `**` 的吞并集合），编译
//!   时合并为一个，语义不变，还减少比较与回溯次数。
//! * **贪心 + 单回溯点，最坏 `O(n * m)`**：段序列匹配（`match_segments`）
//!   与段内字符匹配（`wildcard_match`）都采用经典贪心 glob 算法，只记录
//!   最近一个 `**`/`*` 作为唯一回溯点。相比为每个 `**` 枚举吞并段数组合
//!   的暴力回溯（k 个 `**` 最坏 `O(n^k)`，攻击者可控的长路径可触发指数
//!   爆炸），单回溯点把上界压到 `O(n * m)`（n、m 为路径与模式的段数或
//!   字符数）；逐步推导见两个匹配函数的注释。

use http::request::Parts;
use rgateway_core::{RequestCtx, RoutePredicate};

/// 裸 `**` 速记：[`AntPattern::new`] 额外接受的入口，等价于 `/**`，
/// 匹配一切路径。
const CATCH_ALL: &str = "**";

/// 段内单个通配 token。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token {
    /// 字面量字符（`?`、`*` 之外的一切字符）。
    Lit(char),
    /// `?`——恰好匹配一个字符（按字符计数，与 UTF-8 宽度无关）。
    One,
    /// `*`——零个或多个字符，永不过段边界（`/`）。
    Star,
}

/// 编译后的一个模式段。
#[derive(Debug, Clone)]
enum Segment {
    /// 纯字面量段（不含通配符）：匹配走整段字符串比较的快路径。
    Literal(String),
    /// 混合 `*` / `?` / 字面量的段：按 token 序列逐字符匹配。
    Wild(Vec<Token>),
    /// `**`——零个或多个完整段。
    AnySegments,
}

impl Segment {
    /// 把原始段编译为匹配器。
    ///
    /// 含通配符才走 token 化；纯字面量段用 `Literal` 快路径——整段字符串
    /// 相等即可判定，免去逐 token 比较，而字面量段正是请求路径上最常见的
    /// 段形态。
    fn compile(raw: &str) -> Self {
        if raw == CATCH_ALL {
            return Self::AnySegments;
        }
        if !raw.contains('*') && !raw.contains('?') {
            return Self::Literal(raw.to_string());
        }
        Self::Wild(tokenize(raw))
    }
}

/// 把段内字符逐个映射为 token。按 `char`（Unicode 标量值）迭代，
/// 与 `wildcard_match` 的字符步进粒度保持一致。
fn tokenize(raw: &str) -> Vec<Token> {
    raw.chars()
        .map(|c| match c {
            '*' => Token::Star,
            '?' => Token::One,
            c => Token::Lit(c),
        })
        .collect()
}

/// 一份编译完成的 ant 路径模式。
///
/// 构造后只读，可跨请求共享（`Send`/`Sync`）；[`matches`](Self::matches)
/// 在请求热路径上执行且零堆分配。原始模式字符串被保留下来，用于
/// [`pattern`](Self::pattern) 与谓词 `describe` 原样回显用户输入。
#[derive(Debug, Clone)]
pub struct AntPattern {
    /// 用户书写的原始模式（保留裸 `**` 速记形式，便于日志与错误定位）。
    pattern: String,
    /// 编译后的段匹配器序列（连续 `**` 已折叠）。
    segments: Vec<Segment>,
}

impl AntPattern {
    /// 把模式字符串编译为段匹配器。
    ///
    /// 入口比 [`validate`] 略宽松：裸 `**` 作为 `/**` 的速记被接受，
    /// 其余模式必须满足 [`validate`] 的全部语法约束。速记只此一种，
    /// 相对模式（如 `api/*`）依旧被拒绝——宽松不会掩盖笔误。
    ///
    /// # 错误
    ///
    /// 模式为空、不以 `/` 开头或 `**` 未独占整段时返回 `Err(String)`，
    /// 消息可直接展示给配置作者。
    pub fn new(pattern: &str) -> Result<Self, String> {
        let segments = if pattern == CATCH_ALL {
            // 裸 `**` 速记：等价 `/**`，跳过 validate 对前导 '/' 的要求。
            vec![Segment::AnySegments]
        } else {
            validate(pattern)?;
            compile_segments(pattern)
        };
        Ok(Self {
            pattern: pattern.to_string(),
            segments,
        })
    }

    /// 判断请求路径是否匹配本模式（零堆分配）。
    ///
    /// 路径与模式按段比较；双方各忽略一个前导 `/`（[`PathPredicate`] 传入
    /// 的 `parts.uri.path()` 恒以 `/` 开头）。尾部斜杠语义见模块文档。
    ///
    /// 最坏 `O(n * m)`（n、m 为路径段数与模式段数，段内再按字符比较），
    /// 不会随 `**` 数量指数增长。
    pub fn matches(&self, path: &str) -> bool {
        let path = path.strip_prefix('/').unwrap_or(path);
        match_segments(&self.segments, path)
    }

    /// 返回原始模式字符串（保留用户书写形式，含裸 `**` 速记）。
    pub fn pattern(&self) -> &str {
        &self.pattern
    }
}

/// 按 `/` 切分并编译各段，连续 `**` 折叠为一个。
///
/// 折叠是语义保持的：`/**/**/x` 与 `/**/x` 匹配完全相同的路径集（一段
/// 连续 `**` 联合吞并的段序列集合恰等于单个 `**` 的吞并集合），因此合并
/// 不改变任何匹配结果，只减少匹配期的比较与回溯。
fn compile_segments(pattern: &str) -> Vec<Segment> {
    let body = pattern.strip_prefix('/').unwrap_or(pattern);
    let mut segments: Vec<Segment> = Vec::new();
    for raw in body.split('/') {
        let compiled = Segment::compile(raw);
        if matches!(compiled, Segment::AnySegments)
            && matches!(segments.last(), Some(Segment::AnySegments))
        {
            // 前一段已是 `**`：折叠跳过（语义不变，理由见函数文档）。
            continue;
        }
        segments.push(compiled);
    }
    segments
}

/// 校验模式语法（配置校验层使用同一套约束）。
///
/// 约束及理由：
///
/// * 非空且以 `/` 开头：匹配目标是 `parts.uri.path()`，它恒以 `/` 开头，
///   相对模式永远不会命中——配置里出现它必是笔误，应在加载期报错，
///   而不是留下一条静默失效（永不匹配）的路由。
/// * `**` 必须独占整个段：段内 `**`（如 `/a/**b`）在 ant 语义中没有
///   独立含义（段内 `*` 已覆盖「零或多字符」），放行只会制造「看起来
///   生效、实际另有含义」的误导。
///
/// [`AntPattern::new`] 在此之上额外接受裸 `**` 速记（等价 `/**`）。
///
/// # 错误
///
/// 违反任一约束时返回携带可读消息的 `Err`，供配置错误报告直接使用。
pub fn validate(pattern: &str) -> Result<(), String> {
    if pattern.is_empty() {
        return Err("path pattern must not be empty".to_string());
    }
    if !pattern.starts_with('/') {
        return Err(format!("path pattern must start with '/': {pattern:?}"));
    }
    for seg in pattern.split('/').skip(1) {
        if seg.contains("**") && seg != "**" {
            return Err(format!("'**' must occupy a whole segment: {pattern:?}"));
        }
    }
    Ok(())
}

/// 取从字节偏移 `pos` 开始的下一个 `/` 分隔段，语义镜像 `str::split('/')`：
/// 尾部 `/` 会产出一个最终空段；段耗尽后返回 `None`。最后一段之后 `pos`
/// 落在 `text.len() + 1` 的哨兵值上，调用方只需比较 `pos > text.len()`
/// 即可判定结束，无需单独特判边界。
fn next_seg(text: &str, pos: usize) -> Option<(&str, usize)> {
    if pos > text.len() {
        return None;
    }
    match text[pos..].find('/') {
        Some(i) => Some((&text[pos..pos + i], pos + i + 1)),
        // 末段：下次调用以 len+1 哨兵越界，返回 None。
        None => Some((&text[pos..], text.len() + 1)),
    }
}

/// 按段匹配整个模式，采用经典贪心 glob 算法：`**` 是通配元，段内匹配器
/// 是元素比较。只记录最近一个 `**` 作为唯一回溯点，最坏 `O(n * m)`
/// （n、m 为路径段数与模式段数），零堆分配。
///
/// 为什么只回溯最近的 `**` 就够、且不会指数爆炸：
///
/// * 贪心策略：遇到 `**` 先假设它吞并零个段（模式游标越过它继续匹配），
///   把段数尽量留给模式后缀；走不通时才让这个 `**` 多吞一个段重试。
///   若存在任何匹配解，就存在一个与每步贪心选择相容的解（交换论证：
///   解里更早 `**` 多吞的段可以挪给更晚的 `**` 而不破坏后续匹配），
///   因此不需要同时枚举多个 `**` 的吞并长度组合。
/// * 复杂度：回溯点 `star_pos` 只前进不后退，每次回溯至少前进一个段
///   （至多 n 次）；两次回溯之间模式游标最多前进 m 步，总工作量
///   `O(n * m)`。而暴力递归枚举每个 `**` 的吞并段数是 `O(n^k)`（k 为
///   `**` 个数），长攻击路径下指数爆炸——这正是改用单回溯点的原因。
fn match_segments(pat: &[Segment], path: &str) -> bool {
    // pi：模式段游标；pos：路径字节偏移。star/star_pos 是唯一回溯点——
    // 最近一个 `**` 的模式下标，与它当前已吞并到的路径段偏移。
    let mut pi = 0usize;
    let mut pos = 0usize;
    let mut star: Option<usize> = None;
    let mut star_pos = 0usize;
    while let Some((seg, next)) = next_seg(path, pos) {
        let advanced = match pat.get(pi) {
            // `**` 先按吞并零个段处理并记下回溯点，段数留给模式后缀。
            Some(Segment::AnySegments) => {
                star = Some(pi);
                star_pos = pos;
                pi += 1;
                continue;
            }
            Some(Segment::Literal(lit)) if seg == lit.as_str() => Some(next),
            Some(Segment::Wild(tokens)) if wildcard_match(tokens, seg) => Some(next),
            _ => None,
        };
        match advanced {
            Some(n) => {
                pi += 1;
                pos = n;
            }
            None => {
                // 失配：让最近的 `**` 多吞一个段，模式从它之后重新对齐。
                // star_pos 前进保证回溯有限，且不必动更早的 `**`。
                let Some(s) = star else { return false };
                pi = s + 1;
                let Some((_, n)) = next_seg(path, star_pos) else {
                    break;
                };
                star_pos = n;
                pos = n;
            }
        }
    }
    // 路径耗尽：模式里只允许剩下 `**`（各自吞并零个段）。
    pat[pi..].iter().all(|p| matches!(p, Segment::AnySegments))
}

/// 经典段内贪心通配匹配：唯一回溯点是最近的 `*`，迭代实现、零堆分配，
/// 最坏 `O(n * m)`（n、m 为文本与 token 的字符数）。算法骨架与
/// `match_segments` 相同，只是把「段」换成「字符」、把 `**` 换成 `*`。
fn wildcard_match(tokens: &[Token], text: &str) -> bool {
    // ti：token 游标；off：字节偏移。star/star_off 是最近 `*` 的回溯点。
    let mut ti = 0usize;
    let mut off = 0usize;
    let mut star: Option<usize> = None;
    let mut star_off = 0usize;
    while off < text.len() {
        let advanced = match tokens.get(ti) {
            // `*` 先按吞零个字符处理并记下回溯点。
            Some(Token::Star) => {
                star = Some(ti);
                star_off = off;
                ti += 1;
                continue;
            }
            Some(Token::One) => next_char(text, off).map(|(_, n)| n),
            Some(Token::Lit(c)) => match next_char(text, off) {
                Some((ch, n)) if ch == *c => Some(n),
                _ => None,
            },
            None => None,
        };
        match advanced {
            Some(n) => {
                ti += 1;
                off = n;
            }
            None => {
                // 失配：让最近的 `*` 多吞一个字符后从它之后重试。
                let Some(s) = star else { return false };
                let Some((_, n)) = next_char(text, star_off) else {
                    return false;
                };
                star_off = n;
                ti = s + 1;
                off = n;
            }
        }
    }
    // 文本耗尽：只允许尾部还剩 `*` token（各自吞零个字符）。
    tokens[ti..].iter().all(|t| matches!(t, Token::Star))
}

/// 取偏移 `off` 处的下一个 Unicode 字符及新的字节偏移。
///
/// `?` 与 `*` 以 `char`（Unicode 标量值）为单位步进而非字节，所以 `?`
/// 能匹配任意宽度的字符（如 `é`），段内比较始终按字符进行。
fn next_char(text: &str, off: usize) -> Option<(char, usize)> {
    text[off..].chars().next().map(|c| (c, off + c.len_utf8()))
}

/// `Path=/api/users/**, /api/orders/**`——多模式 Path 谓词，任一模式命中
/// 即整体命中（OR 语义）。只检查 `parts.uri.path()`，查询串不参与匹配。
#[derive(Debug, Clone)]
pub struct PathPredicate {
    /// 编译好的模式集合；任一匹配即命中。
    patterns: Vec<AntPattern>,
}

impl PathPredicate {
    /// 编译一组模式；任一模式匹配即整体匹配。
    ///
    /// # 错误
    ///
    /// 任一模式语法非法时立即返回 `Err`（快速失败：把问题暴露在构造/
    /// 配置期，而不是留下一条永不命中的谓词）。
    pub fn new(patterns: Vec<String>) -> Result<Self, String> {
        let mut compiled = Vec::with_capacity(patterns.len());
        for p in patterns {
            compiled.push(AntPattern::new(&p)?);
        }
        Ok(Self { patterns: compiled })
    }
}

impl RoutePredicate for PathPredicate {
    /// 取 `parts.uri.path()` 与各模式逐一比对，任一命中即返回 `true`。
    /// 请求上下文不参与路径判断。
    fn matches(&self, _ctx: &RequestCtx, parts: &Parts) -> bool {
        let path = parts.uri.path();
        self.patterns.iter().any(|p| p.matches(path))
    }

    /// 返回 `Path=<模式1>,<模式2>` 形式的描述（原样回显模式字符串），
    /// 用于路由转储与错误信息。
    fn describe(&self) -> String {
        let list: Vec<&str> = self.patterns.iter().map(|p| p.pattern()).collect();
        format!("Path={}", list.join(","))
    }
}

#[cfg(test)]
#[path = "ant_path_tests.rs"]
mod tests;
