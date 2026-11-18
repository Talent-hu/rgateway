# rgateway 开发指南

本文是面向**贡献代码的工程师**的开发指南：如何搭建环境、通过质量门禁、按约定写测试、
遵守代码规范、按扩展手册加功能，以及提交与排错。项目定位、配置语义与架构总览见根
`README.md`；本篇只回答「怎么把代码改对、改稳、合进来」。

## 0. 当前实现进度（如实标注）

| 阶段 | 状态 | 内容 |
|------|------|------|
| P1–P2 | 已完成 | `rgateway-core`、`rgateway-router`、`rgateway-config`：领域模型、路由匹配、配置管线、热重载、治理核心 |
| P3 | 已完成 | `rgateway-proxy`、`rgateway-server`：HTTP 代理数据面与 `rgateway` 二进制 |
| P4 | 已完成 | `rgateway-filters`：RateLimit / CircuitBreaker / Retry / Timeout / BodyLimit 五个过滤器运行时 |
| P9 | 已完成 | `rgateway-obs`：访问日志（json/text）、Prometheus 指标、请求 span |
| P10 | 已完成 | `rgateway-mgmt`：管理面 REST + gRPC（同端口按 content-type 复用） |
| P5–P8 | 规划中 | 安全、WebSocket/压缩/路径重写、gRPC 透明代理、gRPC-Web/转码 |

可运行的网关二进制 `rgateway`（`rgateway-server` 的 `[[bin]]`）已交付。本文凡涉及未实现
部分均明确标注「规划中」，不要当成已可用的功能。

工作区根 `Cargo.toml`：`members = ["crates/*"]`，resolver 2，version 0.1.0，edition 2021，
`rust-version = "1.80"`（MSRV）。

## 1. 开发环境

### 1.1 工具链要求

| 工具 | 要求 | 说明 |
|------|------|------|
| Rust | 1.80+（edition 2021） | workspace `rust-version = "1.80"`；本仓库在 Windows 10 + MSVC 工具链验证（验证环境 Rust 1.96） |
| protoc | 任意 protoc 3 / 4（验证版本 36.2） | 生成 gRPC transcoding 的 `FileDescriptorSet`（`protoc --include_imports --descriptor_set_out`）；P3+ 的 tonic/prost 构建也依赖它 |
| cargo-llvm-cov | 建议最新（验证 0.9.1） | 覆盖率门禁；需要 `llvm-tools-preview` 组件 |

### 1.2 安装（Windows / rustup）

```powershell
rustup default stable-x86_64-pc-windows-msvc   # MSVC 工具链
rustup component add llvm-tools-preview
cargo install cargo-llvm-cov
```

protoc 用你惯用的包管理器安装并确保在 PATH 上（例如 `choco install protoc` 或
`scoop install protoc`），装好后 `protoc --version` 能输出版本即可。

### 1.3 验证与首次构建

```powershell
cargo build            # 构建全部三个 crate
cargo test             # 全部 145 项测试应当全绿
```

### 1.4 依赖注意事项（新增/升级依赖前必看）

根 `Cargo.toml` 中有几处**有意为之**的约束，改动会踩坑：

- `arc-swap` 固定 1.x（声明为 `arc-swap = "1"`）；
- `metrics-exporter-prometheus` 必须保持 `default-features = false`，否则经
  hyper-rustls 拉入 `aws-lc-sys`，在 Windows 上构建失败；
- `rustls` / `tokio-rustls` 走 `ring` provider（`default-features = false` + `ring` feature）。

## 2. 质量门禁

四条门禁在提交前**全部**要过，按顺序执行（格式 → lint → 测试 → 覆盖率），命令均在
workspace 根目录执行、覆盖所有成员 crate：

| 门禁 | 可复制命令 | 通过标准 |
|------|-----------|----------|
| 格式 | `cargo fmt --check` | 无差异（遵循根 `rustfmt.toml`） |
| Lint | `cargo clippy -- -D warnings` | 零 warning |
| 测试 | `cargo test` | 全部通过（当前 145 项） |
| 覆盖率 | `cargo llvm-cov --fail-under-lines 80` | 全仓行覆盖率 ≥ 80%，否则退出码非 0 |

覆盖率目标：全仓 **80%**；核心模块 **90%**（framing / breaker / retry / dsl）。截至
P1–P2，四个核心模块中已落地的两个是：

| 目标模块 | 对应文件 | 状态 |
|----------|----------|------|
| breaker | `crates/rgateway-core/src/governance.rs` | 已实现 |
| dsl | `crates/rgateway-config/src/dsl.rs` | 已实现 |
| framing | `crates/rgateway-proxy/src/framing.rs` | 已实现（gRPC-Web trailer 帧，golden 测试） |
| retry | `crates/rgateway-filters/src/retry.rs`、`backoff.rs` | 已实现（退避/抖动表驱动测试） |

逐文件覆盖率直接看 `cargo llvm-cov` 的终端输出（含每文件行 + TOTAL 行，见第 7 节）；
需要可视化时用 `cargo llvm-cov --html`，报告生成在 `target/llvm-cov/html/`。

## 3. 测试约定

### 3.1 TDD 先行

每个功能/修复遵循 测试先行（RED）→ 最小实现（GREEN）→ 重构（IMPROVE）。先写会失败的
测试，再写刚好让它通过的实现，最后在测试保护下改进结构。这是计划的强制工作流，不是建议。

### 3.2 表格驱动测试

多组「输入 → 期望」用 `cases` 数组 + 循环断言表达，数组上方注释写清列含义。参考
`crates/rgateway-config/src/dsl.rs` 的 `duration_table`、`crates/rgateway-core/src/predicate.rs`
的 `host_predicate_full_value_table`：

```rust
// 列含义：(输入, 期望毫秒数)
let cases = [
    ("500ms", 500),
    ("3s", 3000),
    ("1.5s", 1500),
];
for (input, want_ms) in cases {
    let got = parse_duration(input).unwrap();
    assert_eq!(got, Duration::from_millis(want_ms), "{input}");
}
```

### 3.3 时间敏感测试：tokio 时间暂停/推进

禁用真实 sleep 与墙钟依赖。时间相关用例统一写成 `#[tokio::test(start_paused = true)]`，
用 `tokio::time::advance` 精确推进虚拟时钟：

```rust
#[tokio::test(start_paused = true)]
async fn half_open_after_open_duration() {
    let start = tokio::time::Instant::now();
    tokio::time::advance(Duration::from_secs(4)).await;
    assert_eq!(start.elapsed(), Duration::from_secs(4));
}
```

前提是 dev-dependency 带 `test-util`：`tokio = { workspace = true, features = ["test-util"] }`
（`rgateway-core` 与 `rgateway-config` 已如此声明）。实例见
`crates/rgateway-core/src/governance_tests.rs`（熔断时序）与
`crates/rgateway-config/src/deadline.rs`（截止期语义）。

### 3.4 帧字节 golden 测试

framing（协议帧编解码，`rgateway-proxy/src/framing.rs`）配 golden 测试：把期望字节序列
写成常量，双向逐字节断言（encode 输出与 golden 完全相等、decode 还原原值），防止重构
悄悄改变线上格式。新增帧形态时先推导 golden 字节再写代码。注意 `http_body::Frame` 只是
HTTP body 的数据帧类型，与协议帧编解码无关。

### 3.5 集成测试

跨 crate 的端到端用例在 `crates/rgateway-server/tests/`（`proxy_http.rs`、`error_render.rs`、
`gate_matrix.rs`、`reload.rs`、`graceful_shutdown.rs`、`concurrency_gate.rs` 等，共用
`tests/common/` 的上游替身与裸 HTTP 客户端）；管理面与 gRPC 的端到端另见
`rgateway-mgmt` 集成测试。`rgateway-config` 的编译管线测试（`compile_tests.rs` 一族）
继续承担「准集成」覆盖（测试替身 `compile_tests_util.rs`：假 handler 工厂、假发现、假过滤器）。

### 3.6 测试文件拆分模式

测试与被测代码放同一 `src/` 目录，用 `#[path]` 引入：

```rust
// src/spec.rs 末尾
#[cfg(test)]
#[path = "spec_tests.rs"]
mod tests;
```

理由与规则：

- **单文件 500 行硬限**：`spec.rs` 本体 495 行，若把 100 多行测试内联进去就超限；
  拆到 `spec_tests.rs` 后测试仍在同一编译单元（`use super::*` 访问私有项无碍），
  不必建独立 `tests/` 目录。
- 测试规模小时可以内联（`dsl.rs`）或放兄弟模块（`compile.rs` 里的 `discovery_tests`）；
  规模大或成族时拆 `xxx_tests.rs`（如 `watcher_tests.rs`、`target_tests.rs`）。
- 共享测试替身放 `xxx_tests_util.rs`（如 `compile_tests_util.rs`），由测试模块经
  `#[path]` 引入。
- 同一手法也用于拆**实现**模块：`compile.rs` 以 `#[path]` 挂 `compile_chain.rs`
  （`mod chain`）与 `compile_settings.rs`（`mod settings`）。

运行单个测试（名字过滤，子串匹配）：

```powershell
cargo test -p rgateway-config duration_table
cargo test -p rgateway-core full_match_regex -- --nocapture   # 需要看输出时
```

## 4. 代码规范

### 4.1 文件与函数

- 单文件 **500 行硬限**（目标 200–400 行）；函数 **< 50 行**。接近上限就按 3.6 节
  `#[path]` 拆模块或拆测试。
- 已知债务：`validate.rs` 的路由校验函数超过 50 行（代码注释已注明「既有债务」）。
  新逻辑不要往里堆；改到该函数时顺手分段。
- 深度嵌套不超过 4 层；命名清晰、可读性优先。

### 4.2 优先不可变

- 构造后不再变化的值用不可变绑定；跨线程共享的编译产物整体 `Arc` 化（`CompiledConfig`
  即是，热重载时原子换快照，请求路径始终看一致视图）。
- 确需可变状态时用明确的并发原语（`Mutex`、`DashMap`、`arc-swap`），不做隐式共享可变。
  治理状态（熔断器、LB 计数器）走 `RouteState`，跨热重载存活，不自己开全局单例。

### 4.3 错误处理

- **库 crate 用 `thiserror`** 定义类型化错误：`ConfigError`（配置管线，携带 `Location`
  定位）、`GatewayError`（网关错误层次）。错误必须带足上下文。
- **二进制 crate 用 `anyhow`** 汇总顶层错误——`rgateway-server`（`rgateway` 二进制）即如此。
- **非测试代码禁止 `unwrap()` / `expect()`**。配置值是外部输入，`NaN`、溢出等必须以
  错误形式上报而不是 panic（参考 `dsl.rs`：用 `Duration::try_from_secs_f64` 而非会
  panic 的 `from_secs_f64`）。当前三个 crate 的生产代码无 `unwrap`/`expect`，请保持。
- 错误消息面向使用者要友好、可行动；服务端日志记详尽上下文；错误信息不泄露敏感数据
  （`loader` 只回显用户输入的引用、绝不回显解析后的绝对路径）。

### 4.4 输入在边界校验

外部输入（YAML 配置、上游响应、管理面请求）在系统边界先校验、后处理，快速失败并给
清晰消息：

- schema 结构体一律 `#[serde(deny_unknown_fields)]`（唯一例外 `StructuredFilter`，
  `flatten` 本就要收集未知键），拼错的键立即报错而非被静默忽略；
- 语义校验集中在 `validate.rs`，**一次收集全部错误**（`Vec<ConfigError>`）——一次配置
  重载报出所有问题，而不是「改一处、报一处」；
- 时长/大小在 schema 层保持紧凑字符串（`"3s"`、`"10MB"`），编译期才经
  `dsl::parse_duration` / `parse_size` 解析——这样解析失败能报出精确配置坐标；
- 每个校验/编译错误都带 `Location`（如 `routes[user-http].filters[2]`），绝不裸 `String`。

### 4.5 密钥与配置

- 源码中不出现任何硬编码密钥/口令/令牌；TLS 证书、私钥等敏感路径走配置
  （`server.tls`），由部署方提供。
- 配置引用的相对路径（证书、transcoding 描述符）以配置文件所在目录为基准解析，且
  **禁止逃逸**：绝对路径与 `..` 向上跳出一律拒绝（`loader` 的 containment 规则）。
- 默认值写在 schema 的手写 `Default` 里（如 `listen: "0.0.0.0:8080"`），不在业务逻辑里
  散落硬编码。

### 4.6 格式与注释

- 格式化遵循根 `rustfmt.toml`：edition 2021、`max_width = 100`、
  `use_small_heuristics = "Default"`；`cargo fmt` 自动应用，不做风格争论。
- 用中文 doc 注释（`///` / `//!`）讲「为什么」；公共 API 都有文档注释，可失败的函数带
  `# 错误` 段说明错误条件（现有模块的惯例）。

## 5. 扩展手册

### 5.1 新增一个谓词

涉及文件：`rgateway-core/src/predicate.rs`（路径族在 `rgateway-router/src/ant_path.rs`）、
`rgateway-config/src/compile_chain.rs`、`rgateway-config/src/validate.rs`、测试文件。

1. 在 `rgateway-core/src/predicate.rs` 实现 `RoutePredicate`（`matches` + `describe`）。
   `matches` 必须是纯内存检查（不做 I/O）、`Send + Sync`（编译后被多 worker 并发调用）。
   依赖路径匹配器的谓词放 `rgateway-router`（参考 `ant_path.rs` 的 `PathPredicate`）。
2. 正则类参数一律经 `rgateway_core::predicate::full_match_regex` 编译（用户模式被锚定
   为 `^(?:P)$` **全值匹配**，防子串匹配导致路由劫持）。不要绕过它自建 `Regex::new`。
3. 在 `compile_chain.rs` 的 `build_predicate` 中按名字接入、装配参数（参数个数约束参考
   `require_two`）；未知名字自然落到 `ConfigError::UnknownPredicate` 分支。
4. 在 `validate.rs` 的谓词校验段补按名校验：参数个数、正则可编译、ant 语法
   （`dsl::validate_ant_pattern`）等，定位到 `routes[id].predicates[i]`。
5. 紧凑 DSL 解析层无需改动：`dsl.rs` 的 `parse_named_args` 对任意 `Name=arg1,arg2` 通用
   （`\,` 转义逗号、`\\` 转义反斜杠）。
6. 测试：谓词单测（表格驱动，含全值匹配安全语义用例，参考 `predicate.rs` 里
   `host_predicate_full_value_table`）+ 编译装配测试（`compile_chain_tests.rs` /
   `compile_tests.rs`）。`describe()` 输出格式必须稳定（诊断与管理接口输出依赖）。

### 5.2 新增一个过滤器

前置说明：过滤器**实现**归属 `rgateway-filters` crate（已有五个实现：RateLimit /
CircuitBreaker / Retry / Timeout / BodyLimit，可作范本）；扩展点契约在
`FilterFactory` / `FilterRegistry` / `FilterSlot`（`rgateway-config/src/spec.rs`），
链组装在 `compile_chain.rs`。以下步骤按契约写。

1. 实现 `FilterFactory`：`name()` 返回 canonical 名；`build(&FilterSpec, &FilterBuildCtx)`
   解析参数并返回 `ServiceWrapper`（即
   `Box<dyn Fn(BoxedService) -> BoxedService + Send + Sync>`，把该层套在内层链外面）。
   参数取值用 `FilterSpec` 的类型化访问器（`param_str` / `param_u64` / `param_f64` /
   `param_bool_opt` / `param_duration` / `param_size` / `param_str_list` / `param_map` 等），
   它们自动兼容紧凑 DSL 字符串与结构化 YAML 两种来源；`param_*_opt` 严格区分「没配」
   与「配错」。
2. 在 `spec.rs` 的 `slot_for` 登记 名字 → 槽位。现有槽位（外 → 内）：
   `Compression → Cors → RateLimit → Auth → Route → Retry → Breaker → Timeout`
   （安全/准入类归 `Auth`，请求改写类归 `Route`）。若需要全新槽位，在 `FilterSlot` 枚举中
   按 canonical 链序插入变体（声明序即排序序，`derive Ord`），并同步
   `compile_chain.rs` 的 `assemble_chain`；`Deadline` 与 `Target` 不是过滤器槽位，
   是编译期固定层。
3. 在 `validate.rs` 的过滤器校验段补参数级校验（必填/类型/格式），复用 `FilterSpec`
   访问器（`is_err()` 即不合格），定位到 `routes[id].filters[i]` 或 `defaults.filters[i]`。
4. 启动期把工厂注册进 `FilterRegistry`，经 `CompileInput.registry` 交给 `compile`
   （注册动作在 `rgateway-server` 的 `runtime_registry()` 做：`noop_filters` 占位底座 +
   `rgateway_filters::registry()` 真实工厂覆盖）。
5. 需要跨热重载保留的状态（限流桶、熔断器等）走 `FilterBuildCtx.route_state`
   （`RouteState::preserved`），不要自建全局单例。
6. 测试：`FilterSpec` 参数解析单测（参考 `spec_tests.rs`）；链槽位/顺序/`defaults.filters`
   追加语义的断言参考 `compile_defaults_tests.rs`。

### 5.3 新增配置字段

涉及文件：`schema.rs` → `validate.rs` → `compile.rs` / `compile_settings.rs` /
`compile_chain.rs` → 测试 → `config/rgateway.yaml` 示例。

1. `rgateway-config/src/schema.rs`：在对应结构体加字段，配 `#[serde(default)]`（结构体
   已带 `deny_unknown_fields`）；有意义的出厂默认值**手写** `Default`（derive 会给出
   空串/0 之类的无意义出厂值）。
2. 时长/大小字段保持紧凑字符串形态（`"3s"` / `"10MB"`），编译期才解析（见 4.4 节理由）。
3. `validate.rs`：补语义校验（取值范围、跨字段约束），错误带 `Location`，按批量收集
   风格进 `Vec`。
4. 编译侧把字符串转成 typed 值挂到产物上：server 段走 `compile_settings.rs`
   （`ServerSettings`），路由级走 `compile_chain.rs`，其余汇总在 `compile.rs`
   （`CompiledConfig`）。
5. 测试：`schema_tests.rs`（默认值 + 未知键拒绝）、`compile_settings_tests.rs` /
   `compile_tests.rs`（typed 转换 + 错误定位精确到字段）。
6. 更新 `config/rgateway.yaml` 示例，注释里写清单位与默认值。

### 5.4 新增发现提供方

涉及文件：`rgateway-core/src/discovery.rs`、`rgateway-config/src/schema.rs`、`validate.rs`、
`compile.rs`（`build_discovery`）、测试。

1. `rgateway-core/src/discovery.rs`：实现 `ServiceDiscovery` 三个方法——`get`（未知服务
   返回空 vec，不报错）、`watch`（实例变更流；静态实现返回 `None`）、`services`（排序
   输出，保证管理面输出稳定）。
2. `schema.rs`：给 `DiscoveryProvider` 加变体（`#[serde(tag = "type", rename_all = "lowercase")]`，
   现有 `Static` 对应 `type: static`）+ 对应 provider 结构体。
3. `validate.rs` 的 discovery 校验段补该 provider 的检查（实例地址可解析、`weight > 0`
   等，参考 static 段——摘流走 discovery removal，不允许 `weight: 0` 表达下线）。
4. `compile.rs` 的 `build_discovery` 处理新变体，产出 `Arc<dyn ServiceDiscovery>`。
   现有「同一服务名只允许一个 provider 声明」的去重检查要继续成立（或按新语义明确
   扩展）；`DiscoveryProvider` 是穷尽 match，编译器会强制你处理新变体。
5. 测试：trait 行为单测（get/watch/services 契约，参考 `discovery.rs` 内联测试）+
   `compile.rs` 里 `discovery_tests` 风格的编译测试（含重复服务名报错定位）。

## 6. 提交规范

提交信息用 Conventional Commits，格式 `<type>: <description>`，type 取
`feat` / `fix` / `refactor` / `docs` / `test` / `chore` / `perf` / `ci`。**不加归因行**
（attribution 已在全局 settings 禁用）。可选多行 body 说明动机与影响：

```text
feat: add Query predicate with full-value regex matching
fix: reject NaN and inf in compact duration strings
```

提交前安全检查清单（全部勾选才提交）：

- [ ] 无硬编码密钥、口令、令牌
- [ ] 所有外部输入（配置、上游数据、管理面请求）在边界有校验，错误带 `Location`
- [ ] 非测试代码无 `unwrap()` / `expect()`
- [ ] 错误消息不泄露敏感数据（路径布局、内部细节）
- [ ] SQL/XSS/CSRF 不适用范围已确认；新增 HTTP 面（P3+）时逐条复查
- [ ] 新增依赖的 features 符合 1.4 节约束（`arc-swap` 1.x、`metrics-exporter-prometheus`
      `default-features = false`、rustls ring provider）
- [ ] 第 2 节四条质量门禁全绿

## 7. 调试与排错

### 7.1 日志

库代码用 `tracing`（`debug!` / `warn!` / `error!`）记录诊断信息；日志过滤表达式是
RUST_LOG 语法，配置键 `observability.tracing_filter`（如 `rgateway=debug`）。
tracing-subscriber 由 `rgateway-server`（`rgateway` 二进制）在启动期安装；访问日志
（target `access`）经 `rgateway-obs` 渲染成 JSON/logfmt 单行，与诊断日志同管道输出。

### 7.2 单测复现

- 把问题缩成表格驱动单测（3.2 节），用名字过滤运行：`cargo test -p rgateway-core full_match_regex`，
  需要打印时加 `-- --nocapture`。
- 配置报错一律带 `Location`（如 `routes[user-http].filters[2]`、
  `discovery.providers[1].services[user-svc]`），照坐标定位即可。
- `validate` 一次收集**全部**错误；配置重载失败时看完整错误列表，别逐个试错。
- 时间相关 bug 用 `start_paused` + `advance` 写确定性复现（3.3 节），不要靠真实等待。

### 7.3 覆盖率报告解读

```powershell
cargo llvm-cov                      # 终端逐文件表 + TOTAL 行
cargo llvm-cov --show-missing-lines # 直接列出未覆盖行号
cargo llvm-cov --html               # 生成 target/llvm-cov/html/ 可视化报告
```

- 终端表列含 `Lines / Missed / Cover`：门禁看 **Lines**——TOTAL ≥ 80%（可用
  `cargo llvm-cov --fail-under-lines 80` 强制），核心模块（`governance.rs`、`dsl.rs`，
  及落地后的 framing/retry）对应文件行 ≥ 90%。
- HTML 报告逐行标出未覆盖代码，是补测试时的首选视图。
- 区分「没测到」与「测不到」：不可达的防御分支（如 `match` 的兜底 `other =>`）可以
  接受漏测，业务语义分支不行。

### 7.4 环境与依赖排错

- Windows 下若因 TLS 依赖拉入 `aws-lc-sys` 导致构建失败，检查是否动了
  `metrics-exporter-prometheus` 的 features（必须保持 `default-features = false`）
  或换掉了 rustls 的 ring provider。
- 需要确认第三方 crate 的 API 形状而在线文档不可用时，直接 grep 本地 registry 源码
  （`%CARGO_HOME%\registry\src\` 下对应 crate 目录），并用 golden/单测锁定行为。
- 依赖升级后先跑 `cargo test`（145 项须全绿），再处理 clippy 新警告，最后过覆盖率门禁。
