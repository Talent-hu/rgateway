# rgateway

rgateway 是用 Rust 实现的 API 网关，对标 Spring Cloud Gateway。设计目标是同时覆盖 HTTP/REST 代理与 gRPC 全套能力：透明代理、gRPC-Web、JSON 转码，以及管理面 gRPC API。配置采用 YAML，路由语义（谓词、过滤器链、order 优先级、defaults 追加）与 Spring Cloud Gateway 对齐。

## 项目状态

可运行的网关二进制（`rgateway`）已交付：HTTP/1.1 与 HTTP/2 明文代理、
过滤器运行时五件套、可观测性（访问日志 / Prometheus 指标 / 请求 span）与
管理面 REST 与 gRPC 管理服务（MgmtService，同端口按 content-type 复用）均已实装。

| 阶段 | 内容 | 状态 |
|------|------|------|
| P1–P2 | 领域模型、路由匹配、配置管线（加载/校验/编译）、热重载、治理核心（SWRR / 熔断）；crate：`rgateway-core` / `rgateway-router` / `rgateway-config` | 已完成 |
| P3 | HTTP 代理数据面 + `rgateway` 二进制（协议探测、并发闸、错误双通道、优雅关闭、热重载装配）；crate：`rgateway-proxy` / `rgateway-server` | 已完成 |
| P4 | 过滤器运行时实装：`RateLimit` / `CircuitBreaker` / `Retry` / `Timeout` / `BodyLimit`；crate：`rgateway-filters` | 已完成（其余 7 个过滤器属 P5/P6，恒等占位） |
| P9 | 可观测性：访问日志（json/text 双格式）、Prometheus 指标（`/metrics`）、请求/上游 span；crate：`rgateway-obs` | 已完成 |
| P10 | 管理面：REST（health / snapshot / routes / breakers / reset / stats + `/metrics`）；crate：`rgateway-mgmt` | 已完成（REST + gRPC 同端口复用） |
| P5–P8 | 认证与安全、WebSocket / 压缩 / 路径重写、gRPC 透明代理与 gRPC-Web / 转码 | 规划中 |

## 特性

### 已实现

以下能力均已在代码中落地，可逐项指认到具体 crate：

| 特性 | 说明 | 位置 |
|------|------|------|
| 路由与谓词 | Path / Host / Header / Query / Method 五种谓词，ant 路径匹配，按 `(order, id)` 排序的路由表，first-match-wins | `rgateway-router`、`rgateway-core` |
| 配置管线 | YAML 加载 → 语义校验 → 编译为不可变快照，错误带精确配置坐标 | `rgateway-config` |
| 热重载 | 目录监听 + 300ms 防抖，失败保留上一份有效配置，治理状态跨重载存活 | `rgateway-config` |
| HTTP 代理 | HTTP/1.1 + HTTP/2 明文转发、代理头治理（`X-Forwarded-*` / hop-by-hop 剥离）、gRPC-Web trailer 帧渲染、协议感知错误渲染 | `rgateway-proxy`、`rgateway-server` |
| 过滤器运行时 | 限流（governor 令牌桶）/ 熔断（实例级三态）/ 重试（退避 + 抖动 + 可重放缓冲）/ 单次尝试超时 / 请求体限长；canonical 链槽位组装 | `rgateway-filters` |
| 可观测性 | 访问日志（JSON / logfmt 双格式，11 键稳定 schema）、Prometheus 指标（请求量/耗时/上游错误/活跃请求/未命中）、请求 span | `rgateway-obs` |
| 管理面 REST + gRPC | health / snapshot / routes / breakers（含幂等重置）/ stats：REST 信封 API 与 MgmtService gRPC 双通道，同端口按 content-type 复用；`/metrics` 指标端点（REST） | `rgateway-mgmt` |
| 负载均衡 | SWRR 平滑加权轮询、RoundRobin、Random 三种算法 | `rgateway-core` |
| 错误映射 | 九种 `GatewayError` 到 HTTP 状态码与 gRPC code 的映射，内外消息双通道 | `rgateway-core` |
| 路径安全 | 配置内相对路径包含规则、16 MiB 配置大小上限、指标路径保留冲突校验 | `rgateway-config` |

### 规划中

以下能力仅在路线图与配置模型中存在，**尚不可用**：

| 特性 | 说明 | 状态 |
|------|------|------|
| 认证与安全 | JWT、API Key、IP 名单、TLS | 规划（P5） |
| WebSocket / 压缩 / 路径重写 | 升级代理、编解码压缩、RewritePath 运行时 | 规划（P6） |
| CORS | 跨域策略过滤器 | 规划 |
| gRPC 透明代理 | unary 与 streaming 转发 | 规划（P7） |
| gRPC 四件套其余部分 | gRPC-Web 代理、JSON 转码 | 规划（P8） |

## 架构

crate 分层自底向上单向依赖：`rgateway-core` ← `rgateway-router` ← `rgateway-config` ← `rgateway-filters` / `rgateway-proxy` / `rgateway-obs` / `rgateway-mgmt` ← `rgateway-server`（二进制装配层），下层永不依赖上层。

| crate | 职责 | 状态 |
|-------|------|------|
| `rgateway-core` | 协议无关领域模型：`RequestCtx`、`Route` / `RoutePredicate`、`LoadBalancer` / `Instance`、`ServiceDiscovery`、治理状态（熔断器等）、`GatewayError`、`ProxyBody` / `BoxedService` | 已有 |
| `rgateway-router` | ant 通配路径模式（`AntPattern`、`PathPredicate`）与有序路由表 `RouteTable` | 已有 |
| `rgateway-config` | YAML schema、加载、语义校验、编译（不可变快照 + 预编译过滤器链）、热重载 | 已有 |
| `rgateway-filters` | 内置过滤器的运行时实装（RateLimit / CircuitBreaker / Retry / Timeout / BodyLimit） | 已有 |
| `rgateway-proxy` | HTTP 代理与协议处理（upstream client、头治理、gRPC-Web 帧、错误渲染），`RouteHandlerFactory` 的注入方 | 已有 |
| `rgateway-obs` | 指标、访问日志、追踪 | 已有 |
| `rgateway-mgmt` | 管理面 REST + gRPC API | 已有（REST + gRPC） |
| `rgateway-server` | 网关二进制入口（`rgateway`）：分发、装配、热重载、管理面监听 | 已有 |

关键依赖（均声明于 `workspace.dependencies`）：hyper 1、tower 0.5、tonic 0.13（配 prost-reflect 0.14）、governor 0.8、rustls 0.23、notify 8、arc-swap 1、thiserror 2。其中 gRPC 与 TLS 相关依赖已声明，随 P7/P8 与 P5 进入实际依赖图。

## 快速开始

### 环境要求

- Rust 1.80+（`rust-version = "1.80"` 声明的 MSRV；仓库无 CI，该下限未在 CI 中验证）、edition 2021
- Windows + MSVC 已验证可用
- gRPC 管理服务构建需要 `protoc`（tonic-build 生成代码）

### 命令

```bash
cargo build               # 产出库与网关二进制 target/debug/rgateway
cargo run -- --config config/rgateway.yaml   # 启动网关（数据面 + 管理面）
cargo test --workspace    # 全部测试
cargo fmt --check         # 格式检查（rustfmt.toml：max_width = 100）
cargo clippy -- -D warnings
cargo llvm-cov            # 覆盖率（全仓 80% 目标）
```

## 配置

配置文件为 YAML（示例见 `config/rgateway.yaml`），由 `serde_yaml` 解析。顶层由六个配置段组成，全部可省略（缺省时整体取内置默认值）；出现未知顶层键直接报错。除结构化过滤器外，所有结构体带 `deny_unknown_fields` 配合 `serde(default)`：拼错的键立即报错，缺失的键取内置默认值，配置文件只需写与默认不同的部分。

| 配置段 | 作用 | 缺省要点 |
|--------|------|----------|
| `server` | 数据面监听、HTTP/2、限额、超时、TLS | `listen: "0.0.0.0:8080"`，`request_default: "30s"` 等 |
| `discovery` | 服务发现 providers（当前唯一实现 `type: static`） | 空 |
| `defaults` | 全局过滤器、默认超时预算、默认负载均衡算法 | `load_balancer` 可选 `round_robin` / `random` / `smooth_wrr` |
| `routes` | 路由列表（`id` / `uri` 必填） | 空列表 |
| `observability` | 访问日志、指标、`tracing_filter` | `access_log.enabled: true`、`format: json`、`metrics.path: "/metrics"` |
| `mgmt` | 管理面监听与 gRPC 开关 | `listen: "127.0.0.1:9090"`、`grpc: true` |

### 最小示例

只写与默认值不同的部分即可：

```yaml
routes:
  - id: api
    uri: "http://127.0.0.1:9000"
    predicates:
      - "Path=/api/**"
```

`uri` 两种形式：`lb://<service>`（走服务发现 + 负载均衡，服务名必须在 `discovery` 中声明）或 `http(s)://host:port`（直连）。

仓库中的 `config/rgateway.yaml` 展示了更完整的**目标形态**（含 RewritePath / RateLimit / Retry / JwtAuth 等过滤器与 gRPC 转码路由）：其中所有键在配置层均可解析与校验；`RateLimit` / `Retry` / `CircuitBreaker` / `Timeout` / `BodyLimit` 已有运行时行为，其余过滤器与 gRPC 转码尚未实装（见路线图）。

### 配置文件安全规则

- 相对路径引用（TLS 证书、transcoding descriptor 等）以配置文件所在目录为基准做词法归一化，必须留在该目录内：绝对路径（POSIX 根、`\` 开头）、盘符（`C:`）、UNC、`..` 上跳逃逸一律被拒。
- 配置文件大小上限 16 MiB，在读取与解析之前生效。
- 错误消息只回显引用文本本身，不回显解析后的目标路径。

## 路由与谓词

一条路由可声明多个谓词，全部命中（逻辑与）路由才适用；未声明谓词时匹配所有请求。

| 谓词 | 参数 | 语义 |
|------|------|------|
| `Path` | 一个或多个 ant 模式（任一命中即整体命中） | 只匹配 `uri.path()`，查询串不参与 |
| `Method` | 方法列表（任一命中即命中） | 精确匹配、大小写敏感 |
| `Header` | `name, regex` | 同名头任一值全值匹配 `regex` 即命中 |
| `Host` | `regex`（多个模式按 `\|` 拼接取或） | 请求 authority 全值匹配；无 Host 头时回退 `uri.authority()` |
| `Query` | `name, regex` | 查询参数 `name` 任一值全值匹配 `regex` 即命中 |

### ant 路径语义

- `?` 匹配段内恰好一个字符（按 Unicode char 计）
- `*` 匹配段内零个或多个字符，永不过段边界 `/`
- `**` 匹配跨段的零个或多个完整段，且必须独占一个 `/` 分段（`/a**/b` 非法，`/a/**/b` 合法）
- 模式必须非空且以 `/` 开头；匹配大小写敏感，不做 Unicode 归一化
- 匹配为贪心 + 单回溯点的线性扫描（失配时反复回到最近的 `**` / `*` 让它多吞一段 / 一个字符，回溯最多 O(n) 次），最坏 O(n·m)，不随 `**` 数量指数增长，天然防 ReDoS

### 正则全值匹配

Host / Header / Query 的正则一律编译为锚定形式 `^(?:P)$`（与 Java `String.matches` 一致），只做全值匹配、绝不做子串匹配——否则 `Host=api\.example\.com` 会连 `xapi.example.com`、`api.example.com.evil` 一起命中，造成路由劫持。字面 host 写成正则时必须转义点号（`\.`）。

### 多路由匹配

路由表按 `(order, id)` 排序：`order` 升序（数值小者先匹配，Spring Cloud Gateway 语义），`order` 相同按 `id` 字典序，first-match-wins 取第一条谓词全命中的路由。重叠模式靠 `order` 显式仲裁优先级。

## 过滤器 DSL

谓词与过滤器均支持紧凑 DSL 字符串与结构化映射两种写法，编译后完全等价。

```text
# 谓词：Name=arg1,arg2（按第一个 '=' 切名字，参数按未转义逗号切分）
Method=GET,POST
Path=/api/users/**, /api/orders/**
Header=X-Tenant,\d+

# 过滤器：Name=k1:v1,k2:v2（每个 k:v 按第一个 ':' 切分，值内可含 ':'）
RateLimit=key:client_ip,rate:100,period:1s

# 转义：\, = 字面逗号，\\ = 字面反斜杠；其他 \x（如正则 \d、\.）原样保留
Header=X-A,foo\,bar\\x

# 时长：500ms / 3s / 2m / 1h / 1d（后缀 ms/us/s/m/h/d，支持小数秒 1.5s）
#       纯数字按毫秒计；负数、NaN、inf、溢出一律报配置错误
# 大小：512B / 64KB / 10MB / 1GB / 1MiB（B/KB/MB/GB/KiB/MiB/GiB，1024 进制）
#       纯数字按字节计
# 百分数：50% 换算为 0.5（由过滤器参数访问器换算）
```

结构化写法保留 YAML 原生类型（数字、布尔、嵌套 map、序列）：

```yaml
filters:
  - { name: Retry, retries: 3, methods: [GET, HEAD], statuses: [502, 503],
      backoff: { initial: 50ms, max: 2s, multiplier: 2, jitter: full } }
```

要点：

- 列表类参数（如 `methods` / `statuses` / `grpc_statuses`）需要 YAML 序列，只能用结构化写法——紧凑 DSL 的值一律是字符串。
- `defaults.filters` 不是覆盖路由过滤器，而是**追加**到每条路由过滤链之后（Spring Cloud Gateway 语义）；同一槽位内路由声明的过滤器在外、默认过滤器在内，槽内保持配置书写顺序。

## 过滤器链

链的顺序是 canonical 固定的（外 → 内），与配置书写顺序无关；配置里的过滤器按槽位自动分组：

```text
Compression → CORS → RateLimit → Auth → Route(rewrite/header/limit)
  → Deadline(total) → Retry → Target(LB) → Breaker → Timeout → Handler
```

语义要点：

- 每次重试都重新执行负载均衡选实例，并跳过熔断仍处 Open 窗口内的实例；`Target` 位于 `Retry` 与实例级 `Breaker` 之间正为此设计。
- `Deadline` 约束的是**响应头到达时刻**（总预算包含全部重试）；响应体流式透传不受其限制——SSE、大文件下载不会被截止期截断。单次尝试的超时由内侧 `Timeout` 槽负责。
- `Deadline` 与 `Target` 是编译期固定层，不是用户可配置的过滤器。

## 治理

### 负载均衡

三种实现，共用可用性过滤（只在 `healthy && weight > 0` 的实例中选择）：

- `round_robin`：无锁计数器取模轮询
- `random`：splitmix64 伪随机（同一请求的重试倾向落到不同实例）
- `smooth_wrr`：nginx 语义的平滑加权轮询——每轮各实例 `current_weight += 自身权重`，取最大者命中后 `current_weight -= 总权重`，权重比在一个周期内严格成立（4:2:1 → a b a c a b a）

算法选择优先级：路由 `load_balancer` → `defaults.load_balancer` → 内置默认 `round_robin`。

### 熔断

实例级三态状态机，默认参数：`sliding_window = 20`、`failure_rate = 0.5`、`min_calls = 10`、`open_duration = 5s`、`half_open_max_calls = 3`：

- **Closed**：放行，结果压入滑动窗口；样本数 ≥ `min_calls` 且失败率 ≥ `failure_rate` 转 Open。
- **Open**：直接拒绝（`CircuitOpen`）；`open_duration` 到期后实例回到候选集（probe-due），下一次真实调用触发 Open → HalfOpen 转移。
- **HalfOpen**：最多 `half_open_max_calls` 个探测并发；一次成功立即回 Closed 并清空窗口，一次失败立即回 Open 并重启计时。
- 探测许可未上报即被 drop（请求取消 / panic）按失败计入，防探测槽泄漏。

实例可用性判定统一为 `healthy && weight > 0 && 熔断未打开`：

- 无任何健康在轮转实例（空 / 全不健康 / `weight == 0` 摘流）→ `Unavailable`
- 有健康候选但全被 Open 窗口内熔断跳过 → `CircuitOpen`（HTTP 503 / gRPC 14）

`weight: 0` 是运维主动摘流（out-of-rotation）而非后端故障，不允许用它表达下线——摘流走 discovery removal。全部候选被熔断时返回 `CircuitOpen`，客户端与 `Unavailable` 同码，只能靠消息文案区分。

### 限流 / 重试 / 超时

- 限流：配置模型已定义（`RateLimit` 过滤器参数校验），运行时基于 governor 实现（规划中）。
- 重试：`retries` 上限 10（防重试风暴）；`backoff.initial` 必须 > 0；每次重试重新选目标。
- 超时：总预算优先级为路由 `request_timeout` → `defaults.request_timeout` → `server.timeouts.request_default`（缺省 `"30s"`）；单次尝试用 `Timeout` 的 `per_attempt` / `timeout`。
- 治理状态（熔断窗口、LB 计数器、限流桶）跨配置热重载保留，同名路由的槽位复用；陈旧路由 / 实例状态被裁剪。

## 错误映射

| 错误 | HTTP | gRPC code |
|------|------|-----------|
| Unauthenticated | 401 | 16 |
| PermissionDenied | 403 | 7 |
| InvalidArgument | 400 | 3 |
| NotFound | 404 | 12 (UNIMPLEMENTED) |
| RateLimited | 429 | 8 |
| DeadlineExceeded | 504 | 4 |
| Unavailable | 503 | 14 |
| CircuitOpen | 503 | 14 |
| Internal | 500 | 13 |

对客户端只暴露通用消息（如 `upstream unavailable`、`internal error`），内部细节（上游地址、实例 id、io 错误原文等）仅进日志，绝不回显在响应里。HTTP/1、HTTP/2 返回状态码 + JSON body；原生 gRPC 用 `grpc-status` / `grpc-message` trailer；gRPC-Web 用 trailer 帧。注意 `NotFound` 映射 gRPC 12 而非 5；`CircuitOpen` 与 `Unavailable` 在 HTTP 与 gRPC 上完全同码。

## 热重载

- 监听配置文件所在目录（非递归）+ 文件名过滤，兼容 Windows 编辑器「写临时文件 + rename 替换」的保存方式。
- 300ms 防抖合并突发变更，安静窗口内无新事件才触发恰好一次重载。
- notify 事件队列有界（1024），溢出置标志位、防抖泵按事件处理——突发风暴里最后一次触发绝不会丢。
- 重载失败（解析 / 校验 / 编译出错）保留上一份有效配置并记录错误，快照只在完全编译成功后才原子换入，在途请求不受影响。
- 重载时保留路由级治理状态：同名 LB 槽复用、计数器与熔断窗口跨重载存活；新配置中已消失的路由 / 实例的陈旧状态被清理。

## 安全设计

- **路径包含**：配置内相对文件引用做纯词法归一化（不触碰文件系统），必须留在配置目录内；绝对路径、盘符、UNC、`..` 上跳逃逸被拒。
- **大小上限**：16 MiB 配置上限在读取 / 解析之前生效。
- **正则全匹配**：Host / Header / Query 锚定为 `^(?:P)$`，防部分匹配绕过路由。
- **防 ReDoS**：ant 匹配器为线性扫描（最坏 O(n·m)，单回溯点），不随 `**` 数量指数增长。
- **错误消息**：不回显解析后的目标路径，防向配置作者泄露目录布局。
- **快速失败**：配置重名（路由 id 全局唯一）、非法值（未知谓词 / 过滤器名、坏正则、非法时长）在加载期即报错，带精确定位。

## 开发

开发流程与约定：

- TDD：先写测试（RED）→ 最小实现（GREEN）→ 重构
- 表格驱动测试；时间敏感测试用 `tokio::time`（test-util 推进时钟）
- 帧字节 golden 测试锁定线上格式
- 非测试代码不得使用 `unwrap` / `expect`；错误全面处理并带上下文
- 单文件不超过 500 行（目标 200–400）；函数小于 50 行

质量门禁（四条，全部通过才算完成）：

1. `cargo fmt --check`
2. `cargo clippy -- -D warnings`
3. `cargo test --workspace`
4. `cargo llvm-cov`：全仓覆盖率不低于 80%（framing / breaker / retry / dsl 不低于 90%）

## 路线图

| 阶段 | 内容 | 状态 |
|------|------|------|
| P3 | HTTP 代理与 `rgateway-server` 骨架 | 已完成 |
| P4 | 过滤器实装（限流 / 熔断 / 重试落地） | 已完成 |
| P5 | 安全（JWT / API Key / IP 名单 / TLS） | 规划中 |
| P6 | WebSocket、压缩、路径重写 | 规划中 |
| P7 | gRPC 透明代理 | 规划中 |
| P8 | gRPC-Web 与 JSON 转码 | 规划中 |
| P9 | 可观测性（指标 / 访问日志 / 追踪） | 已完成 |
| P10 | 管理面 REST + gRPC API | 已完成（REST + gRPC） |
| P11 | 基准测试与收尾 | 规划中 |

## 许可

MIT OR Apache-2.0。
