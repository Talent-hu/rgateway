# rgateway 架构设计

## 定位与适用读者

本文档描述 rgateway（Rust 实现的 API 网关）的总体架构、核心类型、运行语义
与扩展方式，面向**要理解或参与开发的工程师**。阅读前建议先浏览
`README.md` 了解项目定位与配置速览；本文聚焦"代码为什么这样组织、请求
如何流动、改动应该落在哪里"。

代码标识（crate / 类型 / 函数 / 配置键）与命令保持英文原样；所有断言均可
在源码中指认（关键位置随文标注文件路径）。

> **当前实现进度（如实标注）**
>
> * **P1–P4、P9 已完成**：`rgateway-core`、`rgateway-router`、`rgateway-config`、
>   `rgateway-proxy`、`rgateway-filters`、`rgateway-obs` 与 `rgateway-server`
>   （`rgateway` 二进制）——HTTP 代理数据面、过滤器运行时五件套、可观测性
>   （访问日志 / 指标 / span）均已实装并有测试覆盖。
> * **P10 已完成**：管理面 REST + gRPC（`rgateway-mgmt`，同端口按 content-type 复用）。
> * **P5–P8 规划中**：安全（JWT/TLS）、WebSocket/压缩/路径重写、gRPC 透明
>   代理与 gRPC-Web/转码。
> * 文中凡标注「**规划中**」的行为/组件均不可运行，不代表已可用；未标注
>   的内容均有对应实现与测试。

---

## 1. 总体设计

### 1.1 API 网关职责

rgateway 位于客户端与上游服务之间，承担：

| 职责 | 说明 | 状态 |
|------|------|------|
| 路由匹配 | 谓词（Path/Method/Header/Host/Query）AND 语义，first-match | 已完成 |
| 流量治理 | 限流、熔断、重试、超时、总截止期 | 已完成（P4 五件套；Deadline/Target 内建层） |
| 协议适配 | HTTP/1、HTTP/2、WebSocket、gRPC、gRPC-Web、REST-JSON 转码 | HTTP/1+2 转发与 gRPC-Web 错误渲染已完成；WebSocket/gRPC 转发/转码规划中 |
| 负载均衡与服务发现 | SWRR/轮询/随机 + 可插拔发现提供方 | 已完成（静态发现） |
| 安全 | 认证（JWT/API Key）、IP 过滤、TLS、配置注入防护 | 链槽位就绪；路径包含等已完成；JWT/TLS 规划中 |
| 可观测性 | 访问日志、metrics、tracing、关联 id | 已完成（json/text 访问日志、Prometheus `/metrics`、请求 span） |

### 1.2 对标 Spring Cloud Gateway 的设计取向

配置模型与行为语义刻意对齐 Spring Cloud Gateway（SCG），降低迁移心智成本：

* **谓词 + 过滤器**的路由声明式模型，紧凑 DSL（`"Path=/api/**"`、
  `"Method=GET,POST"`）与结构化 YAML 双形态等价。
* **`order` 小者先匹配**、first-match-wins、`id` 平局仲裁
  （`crates/rgateway-router/src/matcher.rs`）。
* **`defaults.filters` 追加**到每条路由过滤器之后（SCG default filters
  语义，见 §4.2）。
* **Header/Host/Query 正则为全值匹配**，对齐 Java `String.matches` 语义
  （`crates/rgateway-core/src/predicate.rs` 的 `full_match_regex`）。
* **ant 路径模式**（`?` / `*` / `**`）语义与 SCG 兼容
  （`crates/rgateway-router/src/ant_path.rs`）。

差异点：Rust 版把过滤器链在**编译期**一次性组装为 tower 服务链（SCG 为
运行期逐过滤器查找），并用不可变快照承载热重载（§7.1）。

### 1.3 核心设计原则

1. **零拷贝 body**：过滤器链与上游间流动的 body 统一为持有 `bytes::Bytes`
   块的装箱流（`ProxyBody`），数据块是引用计数共享的，转发路径不复制
   字节（`crates/rgateway-core/src/body.rs`）。
2. **不可变快照**：一次配置编译产出整体不可变的 `CompiledConfig`，经
   `arc-swap` 原子换入；请求全程持有一致视图，无读锁（§7.1）。
3. **tower 服务链**：每条路由的过滤器链在编译期组装为可克隆的
   `BoxCloneService` 链，请求处理期零动态过滤器查找、零配置解析
   （`crates/rgateway-config/src/compile_chain.rs`）。
4. **治理状态跨重载存活**：熔断器、限流桶、LB 计数器放在独立的状态
   注册表，配置热重载不清零（§7.2）。

---

## 2. crate 地图

工作区共规划 **8 个 crate**，依赖自底向上单向流动、无环：

```text
rgateway-core      协议无关领域模型（无任何 rgateway 内部依赖）
    ↑
rgateway-router    ant 路径匹配、有序路由表
    ↑
rgateway-config    YAML → 校验 → 编译为不可变快照（+ 热重载）
    ↑
rgateway-filters   具体过滤器实现（RateLimit/CircuitBreaker/Retry/Timeout/BodyLimit）
rgateway-proxy     上游代理/协议处理器（实现 config 的工厂 trait）
rgateway-obs       可观测性导出（访问日志/指标/span）
rgateway-mgmt      管理面 API（REST + gRPC，同端口复用）
    ↑
rgateway-server    可运行二进制（rgateway），装配一切
```

| crate | 职责 | 状态 |
|-------|------|------|
| `rgateway-core` | 领域类型：`RequestCtx`、`Route`、`GatewayError`、`Breaker`、LB、`ServiceDiscovery`、`ProxyBody` | 已完成 |
| `rgateway-router` | `AntPattern`/`PathPredicate`、`RouteTable`（first-match） | 已完成 |
| `rgateway-config` | schema/loader/validate/compile/snapshot/watcher/target/deadline | 已完成 |
| `rgateway-filters` | 过滤器的 tower layer 实装（五件套，其余名字恒等占位） | 已完成 |
| `rgateway-proxy` | 上游转发、协议分发、头治理、gRPC-Web 帧、错误渲染 | 已完成 |
| `rgateway-obs` | 访问日志、Prometheus metrics、tracing 导出 | 已完成 |
| `rgateway-mgmt` | 管理面（熔断重置、快照查询等）HTTP/gRPC 接口 | 已完成（REST + gRPC 同端口复用） |
| `rgateway-server` | 二进制入口：加载配置、装配链、监听端口（数据面 + 管理面） | 已完成 |

两个关键依赖约定（见 `crates/rgateway-config/src/spec.rs`）：

* **依赖倒置**：`FilterFactory` / `RouteHandlerFactory` 这两个 trait 定义在
  `rgateway-config`，由上层（`rgateway-filters`/`rgateway-proxy`）实现并在
  编译期注入，config 反向不依赖任何实现。
* **core 不感知过滤器**：`rgateway-core` 只有协议无关的领域模型，不依赖
  路由语法、配置格式或代理行为。

---

## 3. 请求生命周期

### 3.1 配置侧：加载 → 校验 → 编译

```text
rgateway.yaml
   │  load_from_file / load_from_str        (loader.rs，16 MiB 上限)
   ▼
ConfigFile（Serde schema，纯数据）          (schema.rs)
   │  validate() —— 跨字段语义校验，一次收集全部错误
   ▼
   │  compile(CompileInput)                 (compile.rs)
   │    ├─ parse_server → ServerSettings（compact 时长/大小 → typed）
   │    ├─ build_discovery → StaticDiscovery
   │    ├─ 逐路由 compile_route：谓词 + 过滤器链 + Target + Deadline
   │    └─ 汇总 RouteTable / mgmt / observability 设置
   ▼
CompiledConfig（不可变快照，整体 Arc）
   │  SnapshotStore.swap()（arc-swap 原子换入，版本号 +1）
   ▼
生效配置（在途请求继续持有旧 Arc 到结束）
```

热重载由 `watcher.rs` 驱动：监听配置文件**父目录**（NonRecursive + 文件名
过滤，兼容 Windows 编辑器的"临时文件 + rename"保存方式），事件经 300ms
防抖合并后触发 `snapshot::reload`；编译失败**保旧快照**并计一次
`failed_reloads`，错误完整记日志（§7.3）。

### 3.2 请求侧：匹配 → 链执行 → 上游

1. **协议探测**：分发层从 content-type / upgrade 头探测
   `Protocol`（`Http1`/`Http2`/`WebSocket`/`Grpc`/`GrpcWeb`/
   `GrpcWebText`/`RestJson`），路由可用 `protocol` 字段强制固定
   （`ProtocolHint::Fixed`）。
2. **构造 `RequestCtx`**：`request_id`（uuid v7，对外以 `X-Request-ID`
   传播）、`start`、`peer_addr` 等，以 `Arc<RequestCtx>` 放进 request
   extensions（`crates/rgateway-core/src/route.rs`）。
3. **路由匹配**：`RouteTable::find` 按 `(order, id)` 排序顺序扫描，返回
   第一条**全部谓词命中**的路由（AND 语义）；无命中 → `NotFound`
   （404 / grpc-status 12）。
4. **执行过滤器链**：`Route::chain_clone()` 克隆出预编译链（§4），请求
   自外向内穿过各层。
5. **上游代理**：最内层 Handler 由 `RouteHandlerFactory` 构造，按协议转发到
   `RequestCtx::target` 选定的上游并流式透传响应体（HTTP/1+2 已实装；
   WebSocket/gRPC 转发**规划中**）；错误由协议感知的渲染器输出（§8）。

### 3.3 单次上游尝试（已在 config 层实现）

链内层的 Target/Breaker/Timeout 对每次尝试执行：

```text
TargetSelector（target.rs）
  ├─ lb://name：discovery.get → 健康且 weight>0 → 跳过熔断 open 的实例
  │             → LoadBalancer.pick → 写入 ctx.target（覆写上次选择）
  ├─ http(s):// 直连：DNS 解析（spawn_blocking）→ 单一 UpstreamTarget
  └─ 错误分类：无健康实例 → Unavailable；全被熔断跳过 → CircuitOpen
Breaker（core/governance.rs）：实例级熔断判定（§6.1）
Timeout：单次尝试超时（rgateway-filters 已实装）
Handler：协议处理器转发（HTTP/1+2 已实装，WebSocket/gRPC 规划中）
```

---

## 4. 过滤器链语义

### 4.1 权威链序（唯一权威来源：`compile_chain.rs::assemble_chain`）

请求穿过的链为**外 → 内**：

```text
Compression → CORS → RateLimit → Auth → Route → Deadline
  → Retry → Target → Breaker → Timeout → Handler
```

由内向外组装（`assemble_chain`）：先 `handler`，依次包 Timeout、Breaker、
Target、Retry、Deadline、Route、Auth、RateLimit、CORS、Compression。

两个关键次序决策（`compile_chain.rs` 模块注释）：

* **Target 在 Retry 与 Breaker 之间**：每次重试都要重新执行目标选择
  （重新 LB 选实例并跳过熔断打开的实例），把选中实例写入 `ctx.target`；
  实例级熔断判定的是"本次选中的这个实例"，必须随每次尝试重新执行。
* **Deadline 在 Retry 之外**：总预算约束整个请求（含所有重试）；单次
  尝试超时由内侧的 Timeout 槽负责。

`FilterSlot` 枚举（`spec.rs`）固定各过滤器槽位，槽位间顺序由枚举声明序
（`Ord`）决定；**Deadline 不是过滤器槽位**，编译期固定插在 Route 与
Retry 之间：

| 槽位 | 归入的过滤器名（`slot_for`） |
|------|------------------------------|
| `Compression` | `Compression` |
| `Cors` | `CORS` |
| `RateLimit` | `RateLimit` |
| `Auth` | `JwtAuth`、`ApiKeyAuth`、`IpFilter` |
| `Route` | `RewritePath`、`RequestHeader`、`BodyLimit` |
| `Retry` | `Retry` |
| `Breaker` | `CircuitBreaker` |
| `Timeout` | `Timeout` |

> 过滤器**实现**（tower layer 本体）在 `rgateway-filters`（五件套已实装，
> 其余名字恒等占位）；槽位映射、按名参数校验（`validate.rs`）与链组装机制
> 已完成并被测试覆盖。

### 4.2 槽位分组与 `defaults.filters` 追加规则

* 一个路由的「生效过滤器列表」= 路由自身 `filters` **之后追加**
  `defaults.filters`（SCG 语义）。
* 全部条目按 [`FilterSlot`] 归类，**槽内保持 config 顺序**；包装时按
  槽内逆序向外套（`ChainCtx::wrap`），因此**同一槽位内路由声明的过滤器
  包在默认过滤器外侧**，先于默认过滤器执行。
* 未知过滤器名在该条目自身位置报错（`routes[id].filters[i]` /
  `defaults.filters[i]`）。

### 4.3 重试、截止期与链克隆

* **每次重试重新负载均衡**：Retry 槽包在 Target 外侧，重试一轮就重跑
  目标选择并**覆写** `ctx.target`（`RequestCtx::target` 是 `Mutex` 可覆写
  而非 `OnceLock`，正是为重试换实例，见 `route.rs` 设计要点）。
* **Deadline 只约束响应头**：`DeadlineLayer`（`deadline.rs`）用
  `tokio::time::timeout(budget, …)` 约束**拿到响应 headers** 的时刻；预算
  内完成的调用，其响应体**流式透传**、不受截止期截断（SSE/大文件下载
  不被误杀）。超时映射 `DeadlineExceeded`（504 / grpc-status 4）。
  总预算优先级：路由 `request_timeout` → `defaults.request_timeout` →
  `server.timeouts.request_default`（默认 30s）。
* **`Route::chain_clone` 与 `Mutex` 包装**：`BoxedService`
  （`BoxCloneService`）是 `Send` 但**非 `Sync`**，而 `Route` 要以
  `Arc<Route>` 跨线程共享；把链放进 `Mutex` 可把非 `Sync` 服务包装成
  `Sync` 字段。锁**只在克隆服务的一瞬间持有、绝不跨 await**；锁中毒
  视为内部错误（`route.rs::chain_clone`）。

---

## 5. 核心类型

### 5.1 body 与服务（`rgateway-core/src/body.rs`）

| 类型 | 定义 | 要点 |
|------|------|------|
| `ProxyBody` | `UnsyncBoxBody<Bytes, ProxyError>` | 零拷贝 `Bytes` 块；刻意选 **Unsync** 变体（链上只要求 `Send`，动态分发更轻），只能按值传递 |
| `BoxedService` | `BoxCloneService<Request<ProxyBody>, Response<ProxyBody>, GatewayError>` | 编译期组装、运行期廉价 box 克隆；`Send` 非 `Sync` |
| `ProxyError` | `Upstream` / `Io` / `Gateway` | **数据路径**错误（body 流转发中），与控制路径的 `GatewayError` 刻意区分；`into_gateway_error()` 归一（`Upstream`/`Io` → `Unavailable`） |

### 5.2 请求上下文（`route.rs`）

`RequestCtx` 贯穿整条链，两个晚绑定字段的并发语义不同：

* `auth: OnceLock<AuthContext>` —— **set-once**：认证只应执行一次，重复
  写入返回 `Err`（调用方按重复认证错误处理）。
* `target: Mutex<Option<UpstreamTarget>>` —— **可覆写**：每次重试重新
  选实例（§4.3），`set_target` 有意覆盖上一次的选择。

其余字段：`request_id`（uuid v7，时间有序）、`protocol`、`start` /
`deadline`（`tokio::time::Instant`，测试可用 `time::advance` 精确驱动）、
`peer_addr`、`route_id`、`path_params`（路由/转码捕获的模板变量）。

`UpstreamTarget { instance_id: Arc<str>, addr: SocketAddr }`：一次尝试选定
的上游实例；`instance_id` 与 `Instance::id` 同源（默认 `ip:port`），是
熔断器等治理状态的键。

### 5.3 路由与快照

* **`Route`**：`id`、`order`（小者先）、`uri`（`RouteUri::Lb(name)` 或
  `Direct(uri)`）、`protocol_hint`、`predicates`（AND）、`state`
  （`Arc<RouteState>`）、`chain`（`Mutex<BoxedService>`）。
* **`RouteTable`**：按 `(order, id)` 排序，`find` 返回首条全谓词命中者；
  `id` 唯一使排序成全序，热重载后匹配序列确定（`matcher.rs`）。
* **`CompiledConfig`**（`compile.rs`）：`server: ServerSettings`、
  `routes: RouteTable`、`discovery: Arc<dyn ServiceDiscovery>`、
  `states: Arc<StateRegistry>`，以及 mgmt / access_log / metrics /
  tracing_filter 设置。整体不可变、包 `Arc`。
* **`SnapshotStore`**（`snapshot.rs`）：`ArcSwap<CompiledConfig>` 守护
  当前快照，`load()` 是无锁原子读；`swap()` 换入新快照（版本号从 1 起
  单调 +1）；`failed_reloads` 计数失败重载。失败保旧：只在编译**完全
  成功**后才换，绝无"半新半旧"。

### 5.4 服务发现（`discovery.rs`）

```rust
pub trait ServiceDiscovery: Send + Sync {
    fn get(&self, service: &str) -> Vec<Instance>;          // 未知服务 → 空 vec
    fn watch(&self, service: &str) -> Option<watch::Receiver<Vec<Instance>>>;
    fn services(&self) -> Vec<String>;                      // 字典序稳定输出
}
```

`StaticDiscovery` 由 YAML `discovery.providers[type=static]` 驱动：
`watch` 恒为 `None`（静态配置无变更通知）；动态注册中心（Nacos/Consul 等，
**规划中**）实现本 trait 并让 `watch` 返回更新流即可接入，调用方无需改动。
同名服务被多个 provider 重复声明时编译报错（不静默覆盖）。

---

## 6. 治理子系统

### 6.1 熔断状态机（`rgateway-core/src/governance.rs`）

实例级 `Breaker`：滑动窗口错误率三态机。`BreakerConfig` 参数与内置默认：

| 参数 | 含义 | 默认值 |
|------|------|--------|
| `sliding_window` | 滑动窗口（按已完成调用计数） | 20 |
| `failure_rate` | 触发熔断的失败率阈值 `[0,1]` | 0.5 |
| `min_calls` | 窗口内最少完成调用数（防小样本误判） | 10 |
| `open_duration` | Open 持续多久后允许 HalfOpen 探测 | 5s |
| `half_open_max_calls` | HalfOpen 并发在途探测上限 | 3 |

状态与转移（`check()` 取许可、`BreakerPermit::record()` 报结果）：

* **Closed**：放行；结果压入滑动窗口（超长挤掉最旧）；样本 ≥ `min_calls`
  且失败率 ≥ `failure_rate` → **Open**。
* **Open**：`check()` 直接 `Err(CircuitOpen)`；`open_duration` 到期后首次
  check 转 **HalfOpen** 并占用第一个探测槽。
* **HalfOpen**：最多 `half_open_max_calls` 个探测并发，超限仍
  `Err(CircuitOpen)`；一次成功 → **Closed**（清空窗口），一次失败 →
  **Open**（重启计时）。
* `BreakerPermit` 未 record 即被 drop（取消/panic）**按失败计入**——安全侧
  默认，也防探测槽泄漏。转移时打点 `breaker_transitions_total` /
  `breaker_state`（Closed=0 / Open=1 / HalfOpen=2）。

### 6.2 负载均衡（`rgateway-core/src/lb.rs`）

三种实现共用可用性过滤 `usable()`：**只在 `healthy && weight > 0` 的实例中
选择**，一个可用实例都没有时返回 `None`。

* **`RoundRobin`**（默认，配置名 `round_robin`）：`AtomicU64` 无锁计数
  对可用列表取模。
* **`RandomBalancer`**（`random`）：splitmix64 混合 `计数器 ⊕ request_id`，
  同一请求的重试也倾向落到不同实例；不引入 `rand` 依赖（只要求散布均匀，
  不要求不可预测性）。
* **`SmoothWeightedRoundRobin`**（`smooth_wrr`）：nginx SWRR——每轮
  `current_weight += 权重`、选最大、命中者 `-= 总权重`；一个周期内命中
  次数严格按权重比且被打散（权重 4:2:1 的序列是 `a b a c a b a`）。
  三步需在同一临界区，故用 `Mutex<HashMap>`。

### 6.3 实例可用性判定与 LB/熔断配合（`rgateway-config/src/target.rs`）

`TargetSelector::select_lb` 的过滤顺序：**先 healthy/weight，再熔断，最后
才轮到 LB**：

1. 候选 = `healthy && weight > 0`（`weight == 0` 是运维主动摘流，归入
   `Unavailable` 而非 `CircuitOpen`）；
2. 剔除 `state.breaker_open(id)` 的实例（熔断 Open **且 open 窗口未过**）；
3. `LoadBalancer::pick` 只在可用集上均衡。

错误分类：无健康在轮转实例 → `Unavailable`；有候选但全被"仍在 open 窗口内"
的熔断跳过 → `CircuitOpen`；`pick` 返回 `None` → `Unavailable`。

**时间感知的恢复规则**（`RouteState::breaker_open` / `open_not_elapsed`）：
`open_duration` 一到，即便状态仍是 Open，实例也**重新放回可选集**——链的
Breaker 层靠后续流量发起半开探测。没有这条"到期即重新可选"规则，被跳过的
实例永远没人探测，熔断器永远无法恢复。

`RouteState` 按实例 id 存 `Breaker`（`DashMap`），`breaker_for` 已存在时
只 `reconfigure` 阈值、保留窗口与状态（热重载语义）；`retain_instances`
裁掉已下线实例的熔断器。

---

## 7. 状态与并发

### 7.1 不可变快照

配置编译产物 `CompiledConfig` 整体不可变、包 `Arc`，`SnapshotStore` 用
`arc-swap` 原子换入。读者（`load()`）只是原子指针读、无锁；在途请求继续
持有旧 `Arc` 服务到结束，换快照从不打断进行中的请求。快照只在编译完全
成功后才换（失败保旧，§3.1）。

### 7.2 状态保留（preserved 槽跨重载复用）

`RouteState`（`governance.rs`）与配置快照分离，经 `StateRegistry` 按路由 id
跨重载存活：

* `breakers`：实例级熔断器表——reload 不重置滑动窗口，否则一次重载会把
  仍在熔断期的实例放回流量。
* `extras: DashMap<&'static str, Arc<dyn Any + Send + Sync>>`：按槽名保存的
  类型化状态。**LB 槽**（`compile_chain.rs` 的 `BALANCER_SLOT = "lb"`）用
  `BalancerSlot<M>`（`PhantomData` 编码算法种类）作 preserved holder：
  算法名不变的重载复用同一 balancer 实例（轮询计数器跨重载存活）；换成
  不同算法时 downcast 失配、槽位整体替换。限流桶等过滤器状态同理。
* 重载成功后裁剪孤儿状态（`snapshot.rs::prune_stale_state`）：`retain_routes`
  丢掉新配置已删除/改名路由的状态，`retain_instances` 丢掉已离开服务发现
  的实例熔断器。在途请求持有的是自己的 `Arc`，裁剪不会抽走脚下的状态。

### 7.3 有界通道与溢出标志（`watcher.rs`）

notify 回调跑在 notify 自己的线程上，**绝不能阻塞**：

* 事件队列 `mpsc::channel(EVENT_QUEUE_CAP)`，`EVENT_QUEUE_CAP = 1024`；
* 队列满时 `try_send` 丢弃事件但置位 `overflow: AtomicBool`；
* 防抖泵把 overflow 标记当成一个事件处理——突发风暴里被丢的最后一个触发
  也绝不丢（最多多做一次 reload）；标记在 reload **之前**清零，reload 期间
  新产生的溢出会重新置位、强制再跑一轮。

防抖窗口默认 300ms（`DEFAULT_DEBOUNCE`）：安静窗口内没有新事件才触发
**恰好一次** reload。读（Access）事件被跳过，防 reload 读文件自我触发。

### 7.4 无锁跨 await 纪律

* 所有 `Mutex` 临界区都是短纯内存操作、**绝不跨 await**：`Route::chain_clone`
  （克隆服务瞬间）、`Breaker`（状态机一步）、SWRR（一轮选择）。
* 锁中毒的处理是保守降级而非 panic：breaker 视为 Closed、SWRR 退回第一个
  可用实例、`RequestCtx::target` 视为"未选"——宁可牺牲均衡性也不让请求失败。
* watcher 的防抖泵是普通 `tokio::spawn`（无需 `LocalSet`）：`watcher.rs`
  顶部有编译期 `Send + Sync` 静态断言守护，任何状态退化回线程本地都编译不过。

---

## 8. 错误模型

### 8.1 `GatewayError` 分类与双通道契约（`rgateway-core/src/error.rs`）

九个变体与 `ErrorKind` 一一对应，变体携带的 `String` **一律是内部细节**：

* **客户端可见通道**：`public_message()`（HTTP JSON body）与
  `grpc_message()`（`grpc-message` 文本）——**泛化静态文案**，绝不包含
  上游 host:port、DNS 错误、实例 id、io 错误原文等内部细节。
* **日志通道**：`Display`/`Debug`（thiserror）携带完整内部细节，仅写日志。

测试用 `SECRET_DETAIL` 载荷对两个通道做泄露检测（`error.rs` 测试组）。

### 8.2 HTTP / gRPC 映射表（与代码完全一致）

| 变体 | ErrorKind | grpc-status | HTTP status | 客户端文案 |
|------|-----------|-------------|-------------|------------|
| `Unavailable` | Unavailable | 14 UNAVAILABLE | 503 | `upstream unavailable` |
| `DeadlineExceeded` | DeadlineExceeded | 4 DEADLINE_EXCEEDED | 504 | `deadline exceeded` |
| `RateLimited` | RateLimited | 8 RESOURCE_EXHAUSTED | 429 | `rate limit exceeded` |
| `CircuitOpen` | CircuitOpen | 14 UNAVAILABLE | 503 | `circuit breaker open` |
| `Unauthenticated` | Unauthenticated | 16 UNAUTHENTICATED | 401 | `unauthenticated` |
| `PermissionDenied` | PermissionDenied | 7 PERMISSION_DENIED | 403 | `permission denied` |
| `NotFound` | NotFound | 12 UNIMPLEMENTED | 404 | `not found` |
| `InvalidArgument` | InvalidArgument | 3 INVALID_ARGUMENT | 400 | `invalid argument` |
| `Internal` | Internal | 13 INTERNAL | 500 | `internal error` |

设计取舍：

* `CircuitOpen` 与 `Unavailable` 同映射 14——客户端视角都是"当前无法访问
  上游"；熔断这一内部状态只在 `grpc_message` 里附加说明
  （`"circuit breaker open: circuit is open"`，这是 `grpc_message` 与
  `public_message` 之间**唯一**允许的措辞差异），不另占 status code。
* gRPC 家族（`Grpc`/`GrpcWeb`/`GrpcWebText`）的错误渲染走 `grpc-status` /
  trailer 语义；gRPC-Web 需把 trailer 编码成 body 末尾的特殊帧（文本变体
  还要 base64）——渲染器在 `rgateway-proxy`（`error_render` / `framing`，已实装）。

### 8.3 数据路径错误

`ProxyError`（body 流在转发过程中抛出）经 `into_gateway_error()` 归一：
`Upstream`/`Io` → `Unavailable`（客户端视角"上游暂不可用"），内嵌
`Gateway` 原样透传。与控制路径错误的分工：响应已开始流式转发时只能中断
流或补写 gRPC trailer；响应尚未开始时可完整渲染错误。

---

## 9. 安全设计

### 9.1 配置路径包含（`loader.rs`）

威胁模型：配置文件可能来自不受控来源，借绝对路径读任意系统文件，或用
`..` 跳出部署目录。防线（纯词法归一化，不触碰文件系统）：

* 相对引用以**配置文件所在目录**为基准解析，且必须留在该目录内；
* 绝对引用一律拒绝——POSIX 根（`/…`）、反斜杠开头（`\…`）、Windows
  盘符（`C:/x`、`C:x`、UNC、verbatim）**全平台一致拒绝**；
* `..` 归一化（丢 `.`、遇 `..` 弹一段），弹空后仍有 `..` 即判逃逸拒绝；
* 错误消息**只回显输入引用本身**（`{:?}` 渲染，控制字符无法伪造日志行），
  **绝不回显解析后的目标路径**，避免向配置作者泄露目录布局。

TLS 材料（cert/key/client_ca）与 gRPC transcoding descriptor 均走此路径
（`compile_settings.rs::resolve_in_dir`、`compile_chain.rs::transcoding_info`）。

### 9.2 配置体积上限

`MAX_CONFIG_BYTES = 16 MiB`（16 * 1024 * 1024），文件与内存字符串两条
入口都受限，且在**完整读取/解析之前**拒绝：文件先用 metadata 拒超大，
再做有界读取兜底（`take(max + 1)` 多读 1 字节判定"刚好超限"）——防超大
输入耗尽内存（`loader.rs`）。

### 9.3 正则全匹配防路由劫持（`predicate.rs`）

Header / Host / Query 谓词的用户模式经 `full_match_regex` 编译为
`^(?:P)$`（对齐 Java `String.matches`）：必须匹配**整个值**。若按子串
搜索，`Host=api\.example\.com` 会连 `xapi.example.com`、
`api.example.com.evil` 一起命中——攻击者可控的 Host/头/查询值就能溜进
不该匹配的路由（路由劫持）。锚定后绕过路径被堵死；择一（`a|b`）与
`.*example.*` 等用户正则照常工作，去掉的只是"隐式子串搜索"。该函数是
三个谓词正则的**唯一构造路径**，配置校验（`validate.rs`）与编译复用同一
语义。

### 9.4 ant 匹配器防 ReDoS（`ant_path.rs`）

路径匹配不用正则：模式编译为段匹配器（`Segment`/`Token`），匹配采用
经典贪心 glob 算法、**只记录最近一个 `**`/`*` 作唯一回溯点**。相比为每个
`**` 枚举吞并段数组合的暴力回溯（k 个 `**` 最坏 `O(n^k)`，攻击者可控的长
路径可触发指数爆炸），单回溯点把上界压到 **`O(n * m)`**（n、m 为路径与
模式的段数或字符数）。匹配全程零堆分配，构造期还折叠连续 `**`。

### 9.5 其他

* 未知配置键报错（`deny_unknown_fields`）：拼错的键立即失败，而不是带
  错误配置上线（`schema.rs`）。
* 错误对外/对内双通道（§8.1）：内部细节不回显客户端。
* 认证/限流等准入过滤器归 Auth/RateLimit 槽，先于路由改写执行（§4.1）。

---

## 10. 扩展点

### 10.1 新增谓词

1. 在 `rgateway-core`（或 router）实现 `RoutePredicate`（`matches` +
   `describe`，要求 `Send + Sync`、纯内存检查）；
2. 在 `rgateway-config/src/compile_chain.rs::build_predicate` 加名字分派；
3. 在 `rgateway-config/src/validate.rs` 的谓词名校验补参数校验。

注意正则参数必须走 `full_match_regex` 全值锚定（§9.3）。路径类谓词放
`rgateway-router`（依赖路径匹配器），其余放 core。

### 10.2 新增过滤器

1. 实现 `FilterFactory`（`spec.rs`）：`name()` 返回 canonical 名，
   `build()` 把 `FilterSpec` + `FilterBuildCtx` 变成 `ServiceWrapper`
   （`BoxedService -> BoxedService` 闭包）；
2. 在 `spec.rs::slot_for` 登记槽位（决定链序位置，§4.1）；
3. 在 `validate.rs` 的按名校验里补参数校验；
4. 在装配处（`rgateway-server::bootstrap::runtime_registry`）`FilterRegistry::register` 注册。

参数读取用 `FilterSpec` 的 `param_*` 访问器族——自动抹平紧凑 DSL（全字符串）
与结构化 YAML（number/bool/sequence/mapping）两种来源；`param_*_opt` 严格
区分"没配"与"配错"。需要跨重载保留的状态（限流桶等）放
`RouteState::preserved` 槽（§7.2）。

### 10.3 新增服务发现提供方

1. 实现 `ServiceDiscovery`（core）：`get`/`watch`/`services`；动态提供方
   让 `watch` 返回 `watch::Receiver<Vec<Instance>>` 更新流，调用方无需改动；
2. 在 `schema.rs::DiscoveryProvider` 加变体（`type` 标签分派）；
3. 在 `compile.rs::build_discovery` 加构建分派（注意同名服务跨 provider
   重复声明要报错的既有语义）。

### 10.4 新增上游协议处理器（proxy 层）

实现 `RouteHandlerFactory`（`spec.rs`）：`build(&RouteCompileInfo)` 返回
最内层 `BoxedService`。`RouteCompileInfo` 提供 `id`/`uri`/`protocol_hint`/
`state`/`transcoding`（descriptor 路径已在编译期解析为配置目录内的绝对
路径）。协议探测结果（`RequestCtx::protocol`）与错误渲染映射（§8.2）是
处理器必须遵守的契约。

---

## 附：关键源码索引

| 主题 | 文件 |
|------|------|
| 领域类型 / 请求上下文 | `crates/rgateway-core/src/route.rs` |
| body 与服务链类型 | `crates/rgateway-core/src/body.rs` |
| 错误层次与映射表 | `crates/rgateway-core/src/error.rs` |
| 熔断 / 治理状态 | `crates/rgateway-core/src/governance.rs` |
| 负载均衡（SWRR 等） | `crates/rgateway-core/src/lb.rs` |
| 服务发现 | `crates/rgateway-core/src/discovery.rs` |
| 谓词与全值正则 | `crates/rgateway-core/src/predicate.rs` |
| ant 路径匹配 | `crates/rgateway-router/src/ant_path.rs` |
| 路由表 | `crates/rgateway-router/src/matcher.rs` |
| 编译管线 | `crates/rgateway-config/src/compile.rs` |
| 过滤器链组装（权威链序） | `crates/rgateway-config/src/compile_chain.rs` |
| 过滤器/谓词规格与工厂契约 | `crates/rgateway-config/src/spec.rs` |
| 目标选择 | `crates/rgateway-config/src/target.rs` |
| 总截止期层 | `crates/rgateway-config/src/deadline.rs` |
| 快照与热重载 | `crates/rgateway-config/src/snapshot.rs` |
| 文件监听与防抖 | `crates/rgateway-config/src/watcher.rs` |
| 配置加载与路径包含 | `crates/rgateway-config/src/loader.rs` |
