# rgateway 使用指南

**定位**：面向要**部署、配置**rgateway 网关的使用者（运维 / 后端工程师），讲清
如何写配置、如何匹配路由、如何配置治理行为、配置热重载时会发生什么、出错怎么排。
不涉及内部实现细节与二次开发（那是开发者文档的范围）。

**阅读前提**：会读写基本 YAML；了解 HTTP / gRPC 与反向代理的常规概念。

> **当前项目状态（务必先读）**：P1–P4、P9 已完成，管理面 REST 已交付
> （P10 已完成）。**可运行的网关二进制 `rgateway` 已交付**：HTTP 代理、
> 过滤器运行时五件套（限流/熔断/重试/超时/请求体限长）、访问日志与
> Prometheus 指标、管理面 REST 均可实际使用。下文凡规划中未实装的功能都会
> 明确标注 **【规划中】**，请勿按已可用对待。

---

## 1. 概述与当前状态

### 1.1 现在能做什么

| 能力 | 状态 | 位置 |
|------|------|------|
| YAML 配置加载、语义校验、编译为运行时结构 | ✅ 已实装 | `rgateway-config` |
| 路由匹配（Path / Method / Header / Host / Query 谓词，AND 组合） | ✅ 已实装 | `rgateway-core`、`rgateway-router` |
| ant 风格路径匹配（`?` / `*` / `**`） | ✅ 已实装 | `rgateway-router` |
| 负载均衡（`round_robin` / `random` / `smooth_wrr`） | ✅ 已实装 | `rgateway-core/src/lb.rs` |
| 熔断器引擎（滑动窗口错误率三态机） | ✅ 已实装 | `rgateway-core/src/governance.rs` |
| 目标选择（服务发现 + LB + 跳过熔断实例） | ✅ 已实装 | `rgateway-config/src/target.rs` |
| 总超时预算（Deadline 层） | ✅ 已实装 | `rgateway-config/src/deadline.rs` |
| 错误到 HTTP / gRPC 状态码映射 | ✅ 已实装 | `rgateway-core/src/error.rs` |
| 配置文件热重载（监听 + 防抖 + 失败保旧） | ✅ 已实装 | `rgateway-config/src/watcher.rs` |
| 治理状态（熔断窗口、限流桶、LB 计数）跨热重载保留 | ✅ 已实装 | `rgateway-core/src/governance.rs` |
| 过滤器链骨架（按规范槽位组装） | ✅ 已实装 | `rgateway-config/src/compile_chain.rs` |
| 12 种过滤器的配置模型与参数校验 | ✅ 已实装 | `rgateway-config/src/{spec,validate}.rs` |
| 网关二进制（监听端口、HTTP/1+2 代理转发、优雅关闭） | ✅ 已实装 | `rgateway-server`（`rgateway` 二进制） |
| 过滤器运行时：限流 / 熔断 / 重试 / 单次尝试超时 / 请求体限长 | ✅ 已实装 | `rgateway-filters` |
| 访问日志（json/text）/ Prometheus 指标 / 请求 span | ✅ 已实装 | `rgateway-obs` |
| 管理面 REST（health/snapshot/routes/breakers/stats + `/metrics`） | ✅ 已实装 | `rgateway-mgmt` |
| 管理面 gRPC API | ✅ 已实装 | `rgateway-mgmt`（P10） |
| TLS 监听、gRPC 转码、WebSocket、JWT/API Key 认证 | **【规划中】** | P5–P8 |

### 1.2 现在还不能做什么

- **`tls.enabled` 监听**：TLS 属 P5，开启必须启动失败（绝不静默降级明文）。
- **WebSocket 升级代理 / 压缩 / `RewritePath` 运行时**：属 P6，配置可加载
  但运行时为恒等占位。
- **gRPC 透明转发与 JSON 转码**：属 P7/P8；gRPC 流量当前会被准入闸拒绝
  （Unavailable，见 §6.4 渲染形态）。
- **`JwtAuth` / `ApiKeyAuth` / `IpFilter` 认证**：属 P5，恒等占位。

### 1.3 crate 分工

| crate | 职责 |
|-------|------|
| `rgateway-core` | 错误模型与协议映射、路由/谓词抽象、负载均衡、治理状态（熔断器） |
| `rgateway-router` | ant 路径模式、有序路由表（first-match-wins） |
| `rgateway-config` | schema / 校验 / DSL / 编译管线 / 热重载 watcher / 快照存储 |
| `rgateway-filters` | 过滤器运行时（限流 / 熔断 / 重试 / 超时 / 请求体限长） |
| `rgateway-proxy` | HTTP 上游转发、头治理、gRPC-Web 帧、错误渲染 |
| `rgateway-obs` | 访问日志 / Prometheus 指标 / 请求 span |
| `rgateway-mgmt` | 管理面 REST + gRPC API |
| `rgateway-server` | `rgateway` 二进制：装配、分发、监听（数据面 + 管理面） |

测试分布（`cargo test --workspace` 全绿）：8 个 crate 的库测试 + `rgateway-server`
的端到端集成测试（`tests/` 目录）。

---

## 2. 构建与测试

### 2.1 环境要求

| 项 | 要求 | 说明 |
|----|------|------|
| Rust | **1.80+**（`workspace.package.rust-version = "1.80"`，edition 2021） | `rustup` 安装 stable 即可 |
| Windows 工具链 | **MSVC**（`x86_64-pc-windows-msvc`） | 需安装 Visual Studio Build Tools（C++ 生成工具） |
| protoc | protobuf 编译器，在 `PATH` 中 | `rgateway-mgmt` 的 `build.rs` 用 tonic-build 生成管理面 gRPC 代码时调用；另用于生成 gRPC-JSON 转码的 `FileDescriptorSet`（见 §3.3） |

`proto/google/api/` 目录已内置 `annotations.proto` 与 `http.proto`
（`google.api.http` 映射规则的依赖）。

### 2.2 常用命令

```bash
# 构建全部 crate 与网关二进制（target/debug/rgateway）
cargo build --workspace

# 启动网关（数据面 + 管理面）
cargo run -- --config config/rgateway.yaml

# 跑全部测试（库测试 + 端到端集成测试）
cargo test --workspace

# 按 crate 跑
cargo test -p rgateway-server
cargo test -p rgateway-filters

# 发布构建（workspace 已配置 thin LTO）
cargo build --workspace --release
```

生成转码描述符集（供 `routes[].transcoding.descriptor` 引用）：

```bash
protoc --include_imports --descriptor_set_out=proto/descriptor.bin -I proto your_service.proto
```

> `--include_imports` 是必需的：转码运行时要能从描述符集里解析出被 import
> 的类型。`descriptor` 路径写进配置时遵守 §8.2 的路径引用规则。

---

## 3. 配置上手：从零写一份配置

### 3.1 配置文件的基本约定

- 格式为 YAML，顶层有六个配置段：`server`、`discovery`、`defaults`、
  `routes`、`observability`、`mgmt`。**全部可省略**，缺省取内置默认值。
- **拼错的键会直接报错**：所有结构体开启 `deny_unknown_fields`，未知字段不
  会被静默忽略（示例：把 `listen` 写成 `lstten` 会得到 `yaml parse error at
  <root>`，而不是带着错配置启动）。
- 需要只写与默认值不同的部分；内置默认值见 §3.4 速查表。
- **带注释的完整示例是 [`config/rgateway.yaml`](../config/rgateway.yaml)**，
  覆盖全部六段与典型过滤器写法，可直接作为编写起点。

### 3.2 最小可用配置走查

下面是一份可被解析、校验通过的最小配置：

```yaml
server:
  listen: "0.0.0.0:8080"

discovery:
  providers:
    - type: static
      services:
        user-svc:
          - { address: "127.0.0.1:50051" }

routes:
  - id: user-api
    uri: "lb://user-svc"
    predicates:
      - "Path=/api/**"
```

逐段讲解：

**`server:`** —— 数据面监听与限额。`listen` 是 `ip:port`（默认
`0.0.0.0:8080`）。本例显式写出只是为了展示；不写也行。同段还有 `http2:`
（`max_concurrent_streams` 默认 1024、`initial_stream_window_size` 默认
`1MB`、`initial_connection_window_size` 默认 `64KB`）、`limits:`
（`max_request_body` 默认 `10MB`、`max_concurrent_requests` 默认 4096、
`max_retry_buffer` 默认 `1MB`）、`timeouts:`（`connect` 默认 `3s`、
`request_default` 默认 `30s`、`graceful_shutdown` 默认 `30s`、`ws_handshake`
默认 `10s`）、`tls:`（默认关闭；开启时 `cert` / `key` 必填）。

**`discovery:`** —— 服务发现提供者列表，当前唯一提供者是 `type: static`：
手工维护 `服务名 → 实例列表`。每个实例 `{ address, weight }`，`address` 是
`ip:port`，`weight` 默认 100、必须 > 0（摘流走 discovery removal，不允许用
`weight: 0` 表达下线）。服务至少要有一个实例。

**`routes:`** —— 路由表，每条路由 `id` 与 `uri` 必填：

- `id`：全配置唯一，也是错误定位里的坐标名（`routes[user-api]`）。
- `uri`：`lb://<服务名>`（走服务发现 + 负载均衡，服务名必须存在于
  `discovery` 中）或 `http(s)://host:port`（直连）。
- `predicates`：谓词列表，**全部命中**才算匹配本路由（§4）。
- 可选字段：`order`（默认 0，小者先匹配）、`protocol`（默认 `auto`）、
  `filters`（§5）、`request_timeout`、`load_balancer`、`transcoding`。

### 3.3 `defaults` 与 `transcoding`

`defaults:` 提供全局默认，套用到每条路由（路由级同名设置优先）：

```yaml
defaults:
  load_balancer: smooth_wrr        # round_robin | random | smooth_wrr
  request_timeout: 30s             # 全局请求超时预算
  filters:                         # 追加到每条路由过滤器之后
    - { name: CircuitBreaker, sliding_window: 20, failure_rate: 50%,
        min_calls: 10, open_duration: 5s, half_open_max_calls: 3 }
```

`transcoding:` 仅在 `protocol` 为 `auto` 或 `rest-json` 时允许，做
REST/JSON ↔ gRPC 转码（**运行时属 P3+，配置模型与校验已就绪**）：

```yaml
    transcoding:
      descriptor: "./proto/descriptor.bin"     # protoc 生成，相对配置目录
      rules:
        - selector: "helloworld.Greeter/SayHello"   # package.Service/Method
          post: "/v1/echo"                           # get/post/put/delete/patch 五选一
          body: "*"                                  # "*" 或请求字段名
```

校验规则：`selector` 必须是 `package.Service/Method` 形式；每条顶层 `rule`
必须**恰好**设置一个 HTTP 方法；`additional_bindings` 里的规则至少一个方法。

### 3.4 六段默认值速查

| 段 | 键 | 默认值 |
|----|----|--------|
| `server` | `listen` | `0.0.0.0:8080` |
| `server.http2` | `max_concurrent_streams` / `initial_stream_window_size` / `initial_connection_window_size` | `1024` / `1MB` / `64KB` |
| `server.limits` | `max_request_body` / `max_concurrent_requests` / `max_retry_buffer` | `10MB` / `4096` / `1MB` |
| `server.timeouts` | `connect` / `request_default` / `graceful_shutdown` / `ws_handshake` | `3s` / `30s` / `30s` / `10s` |
| `server.tls` | `enabled` / `client_auth` | `false` / `none`（可选 `optional`、`required`） |
| `discovery` | `providers` | 空列表 |
| `defaults` | `filters` / `request_timeout` / `load_balancer` | 空 / 无（回退 `server.timeouts.request_default`） / 无（回退 `round_robin`） |
| `routes[]` | `order` / `protocol` | `0` / `auto` |
| `observability.access_log` | `enabled` / `format` | `true` / `json`（可选 `text`） |
| `observability.metrics` | `enabled` / `path` | `true` / `/metrics` |
| `mgmt` | `listen` / `grpc` | `127.0.0.1:9090` / `true` |

`protocol` 可选值：`auto`、`http1`、`http2`、`grpc`、`grpc-web`、
`grpc-web-text`、`rest-json`、`websocket`。写别的值会在校验期报
`unknown protocol`。

> `observability:` 与 `mgmt:` 均已生效：访问日志按 `access_log` 开关与格式
> 输出，指标在管理面 `metrics.path` 暴露，管理面 REST 服务在 `mgmt.listen`
> 监听（gRPC 管理服务同端口复用，`mgmt.grpc: false` 可关闭）。

---

## 4. 路由配置实战

### 4.1 匹配模型

- 一条路由的所有谓词是 **AND**：全部命中才算匹配该路由。
- 路由表按 `(order, id)` 排序（`order` 小者先；平局按 `id` 字典序），
  **第一条匹配的路由胜出**（first-match-wins，与 Spring Cloud Gateway 一致）。
  重叠的模式用 `order` 显式仲裁优先级。
- 无路由命中时按 `NotFound` 处理（HTTP 404 / gRPC 12）。

### 4.2 按匹配类型的配置示例

```yaml
routes:
  - id: by-path
    uri: "lb://user-svc"
    predicates:
      - "Path=/api/users/**, /api/orders/**"    # 多模式任一命中即 Path 命中

  - id: by-method-host
    uri: "lb://user-svc"
    predicates:
      - "Method=GET,POST"                       # 精确匹配、大小写敏感
      - "Host=api\\.example\\.com"               # 正则，全值匹配（见 4.4）

  - id: by-header
    uri: "lb://order-svc"
    predicates:
      - "Header=X-Tenant,\\d+"                  # 恰好 2 参数：(头名, 正则)

  - id: by-query
    uri: "lb://order-svc"
    predicates:
      - "Query=version,v[0-9]+"                 # 恰好 2 参数：(参数名, 正则)
```

参数多时可用结构化写法（与紧凑 DSL 完全等价）：

```yaml
    predicates:
      - name: Method
        args: [GET, POST]
```

各谓词语义要点：

| 谓词 | 参数 | 匹配语义 |
|------|------|----------|
| `Path` | 1 个以上 ant 模式 | 只看 `uri.path()`（不含查询串）；多模式 OR；模式必须以 `/` 开头 |
| `Method` | 1 个以上方法名 | 精确匹配、大小写敏感 |
| `Host` | 1 个以上正则（按 `\|` 拼接成择一） | 对请求 authority **全值**匹配；无 `Host` 头时回退用 `:authority` 伪头 |
| `Header` | 恰好 2 个：`(名, 正则)` | 同名头任一值全值匹配即命中；缺头 / 值非 UTF-8 不命中 |
| `Query` | 恰好 2 个：`(名, 正则)` | 查询串里存在该参数且值全值匹配；无查询串不命中 |

### 4.3 ant 模式速查

| 通配符 | 语义 |
|--------|------|
| `?` | 段内恰好 1 个字符（按 Unicode 字符计，`é` 也算 1 个） |
| `*` | 段内 0 个或多个字符，**永不过 `/`** |
| `**` | 必须**独占一整个段**，匹配 0 个或多个完整段（可跨 `/`） |

- 模式必须以 `/` 开头；`**` 混在段中间（如 `/a**/b`）会被校验拒绝。
- **大小写敏感**，不做 Unicode 归一化。
- **尾部斜杠敏感**：尾部 `/` 会留下一个空段。

```text
/api/*     匹配 /api/x、/api/          不匹配 /api、/api/x/y
/api/**    匹配 /api、/api/、/api/a/b  不匹配 /v1、/apix
/users/?   匹配 /users/1               不匹配 /users/10、/users/
/a/**/b    匹配 /a/b、/a/x/y/b         不匹配 /a/x/y/c
```

### 4.4 正则全值匹配注意事项

`Host` / `Header` / `Query` 的正则都是**全值匹配**：你的模式 `P` 会被锚定为
`^(?:P)$`，必须匹配整个值（对齐 Java `String.matches` 语义）。这是防路由
劫持的安全设计——`api\.example\.com` 不会命中 `xapi.example.com`、
`api.example.com.evil`、`api-example.com`。

下表是**正则模式本身**（锚定全值后的语义）：

| 想要 | 正则模式 |
|------|----------|
| 精确主机名 | `api\.example\.com`（**转义点号**，否则 `.` 匹配任意字符） |
| 带端口也想匹配 | `api\.example\.com(:8080)?` |
| 子串包含语义 | `.*example.*`（显式写 `.*`，隐式子串搜索已被去掉） |
| 择一 | `a\|b` 或写成两个参数 `Host=a,b`（按 `\|` 拼接；只匹配完整值 a 或 b） |

注意转义层级：正则里的 `\d`、`\.` 等写进 YAML **双引号**字符串时要翻倍成
`\\d`、`\\.`（先过 YAML 再过 DSL，见 §4.2 示例）。DSL 层不认识的 `\x`
转义原样保留，不会破坏正则语义。

---

## 5. 过滤器速查

### 5.1 两种写法与紧凑 DSL

过滤器支持紧凑 DSL 字符串与结构化 YAML map 两种写法，编译期归一化后完全
等价；参数多、类型丰富时用结构化，一行能写下时用紧凑 DSL：

```yaml
    filters:
      - "RateLimit=key:client_ip,rate:100,period:1s"     # 紧凑：Name=k:v,k:v
      - name: Retry                                       # 结构化：name + 参数
        retries: 3
        methods: [GET, HEAD]
        backoff: { initial: 50ms, max: 2s, multiplier: 2, jitter: full }
```

紧凑 DSL 语法：

- 谓词 `Name=arg1,arg2`；过滤器 `Name=k1:v1,k2:v2`（每段按**第一个** `:`
  切分，值里可以再带 `:`，如 `x-uri:http://a:8080/v`）。
- 转义：`\,` 是字面逗号，`\\` 是字面反斜杠；其他 `\x` 原样保留；末尾孤立
  `\` 是错误。
- **时长**：`500ms`、`3s`、`2m`、`1h`、`1d`、`us`（微秒）；纯数字按**毫秒**；
  允许小数（`1.5s`）；负数 / NaN / inf 一律拒绝。
- **大小**：`512B`、`64KB`、`10MB`、`1GB`、`1MiB`；纯数字按**字节**；
  **全部 1024 进制**（KB=1024、MB=1024²、GB=1024³）。
- **百分数**：`50%` 换算为 0.5（如 `failure_rate:50%`）；不带 `%` 按原值
  （`50` → 50.0）。

### 5.2 过滤器一览（如实标注实装状态）

实装状态：`RateLimit` / `CircuitBreaker` / `Retry` / `Timeout` / `BodyLimit`
五个过滤器**运行时已实装**（实际拦截/改写请求）；其余七个名字已识别、
槽位归类、参数校验就绪，但运行时为恒等占位（**【规划中】**，配置可加载、
零拦截）。校验通过 ≠ 运行时已生效——以本表状态列为准。

| 过滤器 | 链槽位 | 常用参数 | 参数校验（已实装） |
|--------|--------|----------|--------------------|
| `Compression` | Compression | — | 无参数校验 |
| `CORS` | Cors | — | 无参数校验 |
| `RateLimit` | RateLimit | `key`（如 `client_ip`）、`rate`、`period` | `key`/`rate` 必填；`period` 给出时须 > 0 |
| `JwtAuth` | Auth | `issuer`、`audience`、`jwks_file` | 无参数校验 |
| `ApiKeyAuth` | Auth | — | 无参数校验 |
| `IpFilter` | Auth | — | 无参数校验 |
| `RewritePath` | Route | `regex`、`replacement` | 两者必填；`regex` 须能编译（**不**锚定全值——它是重写器不是匹配器） |
| `RequestHeader` | Route | 头名:值（如 `x-uri:http://a:8080/v`） | 无参数校验 |
| `BodyLimit` | Route | `max_body`（如 `1MB`） | 必填 |
| `Retry` | Retry | `retries`、`methods`、`statuses`、`grpc_statuses`、`backoff{initial,max,multiplier,jitter}` | `retries` ≤ 10；`backoff.initial` > 0 |
| `CircuitBreaker` | Breaker | `sliding_window`、`failure_rate`、`min_calls`、`open_duration`、`half_open_max_calls` | `failure_rate` ∈ [0, 1]（`50%` 自动折算）；`min_calls` ≤ `sliding_window` |
| `Timeout` | Timeout | `per_attempt`（或 `timeout`） | 二者至少给一个 |

- 未列入表中的过滤器名会报 `unknown filter`。
- **运行时已实装**：`RateLimit` / `CircuitBreaker` / `Retry` / `Timeout` /
  `BodyLimit`（见 §6 治理语义）；其余七个为恒等占位（配置可加载、零拦截）。
- `Retry` 的 `methods` / `statuses` / `grpc_statuses` 是重试判定参数
  （缺省空列表 = 不按方法/状态过滤，仅按错误分类重试）。

### 5.3 常用过滤器示例

```yaml
    filters:
      # 限流：每客户端 IP 每秒 100 次
      - "RateLimit=key:client_ip,rate:100,period:1s"
      # 熔断：滑动窗口 20 次调用，失败率过半即断，至少 10 次样本
      - { name: CircuitBreaker, sliding_window: 20, failure_rate: 50%,
          min_calls: 10, open_duration: 5s, half_open_max_calls: 3 }
      # 重试：GET/HEAD 遇 502/503/504 或 gRPC 14 重试 3 次，指数退避
      - { name: Retry, retries: 3, methods: [GET, HEAD], statuses: [502, 503, 504],
          grpc_statuses: [14], backoff: { initial: 50ms, max: 2s, multiplier: 2, jitter: full } }
      # 单次尝试超时
      - { name: Timeout, per_attempt: 5s }
      # JWT 认证
      - { name: JwtAuth, issuer: "https://idp.example.com", audience: gw, jwks_file: ./jwks.json }
      # 路径重写：/api/users/1 -> /users/1
      - { name: RewritePath, regex: "/api/(?<segment>.*)", replacement: "/${segment}" }
      # 请求体上限
      - "BodyLimit=max_body:1MB"
```

### 5.4 过滤器链顺序（对使用者可见的含义）

链顺序由**槽位**固定，与 YAML 里书写的顺序无关（外 → 内）：

```text
Compression → CORS → RateLimit → Auth(JwtAuth/ApiKeyAuth/IpFilter)
  → Route(RewritePath/RequestHeader/BodyLimit) → Deadline(总超时)
  → Retry → Target(LB 选实例) → CircuitBreaker → Timeout → 协议处理器
```

- 按语义分层：表示层最先，准入/治理靠前，重试与熔断贴近每次上游尝试。
- `defaults.filters` 追加到每条路由自身 `filters` **之后**；同一槽位内路由
  声明的过滤器位于外侧（**先于**默认过滤器执行）。
- `Target` 夹在 Retry 与 Breaker 之间：每次重试都重新选实例（跳过熔断打开
  的实例），熔断判定针对本次选中的实例。
- `Deadline` 在 Retry 之外：总超时预算约束**整个请求（含全部重试）**，且只
  约束拿到响应头的时刻——响应体是长流（SSE、大文件）时不会被总预算截断。

---

## 6. 治理行为说明

### 6.1 负载均衡（✅ 已实装）

- 三种算法：`round_robin`（轮询）、`random`（伪随机，按请求 id 与计数器
  播种，重试倾向换实例）、`smooth_wrr`（nginx 风格平滑加权轮询，一个周期内
  命中次数严格按权重比，且高权重实例不连续突发）。
- 选择优先级：路由 `load_balancer` → `defaults.load_balancer` → 内置默认
  `round_robin`。算法名非法时报 `unknown load balancer`。
- 只在 **healthy 且 weight > 0** 的实例中选择；一个可用实例都没有 →
  `Unavailable`（HTTP 503 / gRPC 14）。
- 轮询计数器、SWRR 权重表**跨热重载存活**（换配置不清零）。

### 6.2 熔断（✅ 引擎与过滤器层均已实装）

熔断器引擎是滑动窗口错误率三态机（每实例一个，`rgateway-core` 实现并测试）：

| 状态 | 行为 |
|------|------|
| Closed | 放行全部调用；结果计入滑动窗口。样本数 ≥ `min_calls` 且失败率 ≥ `failure_rate` 转 Open |
| Open | 拒绝（`CircuitOpen`，HTTP 503 / gRPC 14）；`open_duration` 到期后首次尝试转 HalfOpen |
| HalfOpen | 最多 `half_open_max_calls` 个探测并发；一次成功立即恢复 Closed，一次失败重开 Open 并重启计时 |

使用者可见的行为要点：

- **目标选择会跳过**仍在 Open 窗口内的实例（不会白白浪费一次尝试）；
  `open_duration` 一到，实例自动回到候选集接受恢复探测。
- 未上报结果就取消的调用按失败计（安全侧默认，防探测槽泄漏）。
- **跨热重载保留**：滑动窗口与状态不因换配置重置——否则一次 reload 会把
  仍在熔断期的实例放回流量。已删除路由 / 已下线实例的状态会被清理。
- 引擎内置默认参数（`BreakerConfig` 默认值）：`sliding_window: 20`、
  `failure_rate: 0.5`、`min_calls: 10`、`open_duration: 5s`、
  `half_open_max_calls: 3`。`CircuitBreaker` 过滤器把这些参数映射到引擎
  （已实装）；实例级状态可在管理面 `GET /mgmt/v1/breakers` 查看、
  `POST /mgmt/v1/breakers/reset` 重置。

### 6.3 限流 / 重试 / 超时（如实标注）

| 治理项 | 配置面 | 运行时行为 |
|--------|--------|------------|
| 限流 `RateLimit` | ✅ 校验 `key`/`rate`/`period` | ✅ 已实装（governor 令牌桶，按 `key` 维度分桶，跨热重载保留）；被拒时映射 `RateLimited`（HTTP 429 / gRPC 8） |
| 重试 `Retry` | ✅ 校验 `retries` ≤ 10、`backoff.initial` > 0 | ✅ 已实装；每次重试重新选实例、预算与总 Deadline 共享、请求体重放缓冲上限 `server.limits.max_retry_buffer`；`retries` 缺省 3 |
| 单次尝试超时 `Timeout` | ✅ 校验 `per_attempt`（或 `timeout`） | ✅ 已实装（`per_attempt` 优先、`timeout` 兜底）；超时映射 `DeadlineExceeded` |
| 请求体限长 `BodyLimit` | ✅ 校验 `max_body` 必填 | ✅ 已实装（Content-Length 预检 + 流限长双保险）；超限映射 `InvalidArgument` |
| **总超时预算 Deadline** | ✅ `request_timeout` 三级优先 | ✅ 已实装：路由 → defaults → `server.timeouts.request_default`（默认 `30s`）；超时映射 `DeadlineExceeded`（HTTP 504 / gRPC 4）；只约束响应头 |

### 6.4 错误映射表（✅ 已实装，唯一权威）

过滤器链任意一层的错误按请求协议渲染：HTTP/1、HTTP/2 返回状态码 + JSON
body；原生 gRPC 返回 `grpc-status` / `grpc-message` trailer；gRPC-Web 返回
trailer 帧。客户端文案是**泛化静态文案**，绝不包含内部细节（上游地址、
DNS 错误、实例 id 等——那些只进日志）。

| 网关错误 | HTTP status | grpc-status | 客户端文案 |
|----------|-------------|-------------|------------|
| `Unavailable` | 503 | 14 UNAVAILABLE | `upstream unavailable` |
| `DeadlineExceeded` | 504 | 4 DEADLINE_EXCEEDED | `deadline exceeded` |
| `RateLimited` | 429 | 8 RESOURCE_EXHAUSTED | `rate limit exceeded` |
| `CircuitOpen` | 503 | 14 UNAVAILABLE | `circuit breaker open` |
| `Unauthenticated` | 401 | 16 UNAUTHENTICATED | `unauthenticated` |
| `PermissionDenied` | 403 | 7 PERMISSION_DENIED | `permission denied` |
| `NotFound` | 404 | 12 UNIMPLEMENTED | `not found` |
| `InvalidArgument` | 400 | 3 INVALID_ARGUMENT | `invalid argument` |
| `Internal` | 500 | 13 INTERNAL | `internal error` |

注：`CircuitOpen` 与 `Unavailable` 同映射 gRPC 14（客户端视角都是"当前
无法访问上游"），但 `grpc-message` 会额外注明 `circuit breaker open: circuit
is open`——这是各错误的 gRPC 文案与 HTTP 文案之间唯一允许的措辞差异。

---

## 7. 热重载操作

### 7.1 改配置后发生什么

1. watcher 监听**配置文件所在目录**（非递归），按文件名过滤事件。
   监听目录而非文件本身是刻意设计：Windows 编辑器保存常是"临时文件 +
   rename 替换"，直接盯文件句柄会丢事件。
2. **防抖合并**：事件突发先攒着，安静窗口（默认 **300ms**）内没有新事件才
   触发**恰好一次** reload；窗口内每来一个相关事件就顺延。
3. reload 流程：读文件 → 解析 → 校验 → 编译出全新不可变快照 → **原子换入**。
   版本号从 1 起单调递增，成功时日志打 `config reloaded`（带 `version`）。
4. **在途请求不受影响**：它们继续持有旧快照的引用到结束，换快照从不打断
   进行中的请求，也没有"半新半旧"的中间态。
5. **治理状态跨重载保留**：熔断滑动窗口、限流桶、LB 计数器照常延续；重载
   成功后才清理新配置不再引用的路由状态与已下线实例的熔断器。

### 7.2 失败时旧行为保留

- 校验或编译失败 → **旧快照继续生效**，错误完整记日志
  （`config reload failed; keeping previous snapshot`），失败计数
  （`failed_reloads`）加一。配置永远不会进入半生效状态。
- 热重载路径下校验失败**只报第一个错误**——改完保存后若还有下一个问题，
  会再报一条，逐个收敛即可（一次收集全部错误的 API 用于嵌入式调用场景）。

### 7.3 排错与注意事项

- **每次保存都会触发**（经防抖合并）；连续编辑产生的一串事件只做一次
  reload，不必担心抖动。
- 看日志判定结果：成功 `config reloaded`；失败 `config reload failed; keeping
  previous snapshot`，错误里带精确配置坐标（§8.1）。
- watcher **只响应配置文件本身的变更**。TLS 证书/私钥文件的热重载是设计
  目标（schema 注释所载），**当前未实装**；改证书不会触发 reload。
- 启动监听时配置文件必须已存在，否则报 `io error at watcher`；watcher 需要
  一个正在运行的 tokio runtime（库调用方注意）。
- 事件队列是有界的（1024），突发风暴下事件可能被丢弃，但有溢出标记兜底——
  **最后一个触发绝不会丢**，最多多做一次 reload。

---

## 8. 常见问题（FAQ）与排错

### 8.1 配置校验报错怎么读

错误格式统一为 `<错误类别> at <位置>: <消息>`，位置是精确的配置坐标：

| 错误前缀 | 含义 | 典型坐标 |
|----------|------|----------|
| `io error` | 文件读不到 | `<root>` 或被引用的外部文件 |
| `yaml parse error` | YAML 语法错 / **拼错的键**（`deny_unknown_fields`） | `<root>` |
| `invalid config` | 结构性不合法（缺必填、重复 id 等） | 具体条目 |
| `invalid value` | 取值不合法（格式、越界） | 具体字段 |
| `unknown filter` / `unknown predicate` | 名字不在支持列表里 | 声明该名字的条目 |
| `compile failed` | 编译为运行时结构失败 | 具体条目 |

坐标写法：`<root>`（整文件）、`server.timeouts.connect`（段.字段）、
`routes[user-http].filters[2]`（路由内列表项）、`defaults.filters[1]`、
`discovery.providers[0].services[user-svc][0].weight`。

常见校验规则速查：

| 报错（消息摘录） | 原因 |
|------------------|------|
| `route id must not be empty` / `duplicate route id` | `id` 为空或重复 |
| `unknown discovery service "x"` | `lb://x` 的 `x` 不在 `discovery` 里 |
| `invalid uri` / `unsupported scheme` | `uri` 只接受 `lb://服务名` 或 `http(s)://host:port` |
| `unknown protocol "x"` | `protocol` 不在白名单（§3.4） |
| `service has no instances` | 静态服务列表为空 |
| `weight must be > 0 (use discovery removal to drain)` | 不允许 `weight: 0`，摘流请从 discovery 列表移除实例 |
| `path pattern must start with '/'` | ant 模式必须以 `/` 开头 |
| `'**' must occupy a whole segment` | `/a**/b` 这类混合写法非法 |
| `Header expects exactly 2 arguments (name, regex)` | 参数个数不对 |
| `invalid regex` | 正则编译失败（注意 YAML 转义写 `\\d`） |
| `Retry retries must be <= 10` | 防重试风暴的硬上限 |
| `transcoding requires protocol auto or rest-json` | 其他 protocol 下不允许转码 |

### 8.2 路径引用被拒绝的原因

配置里的相对文件引用（`server.tls.cert` / `key` / `client_ca`、
`routes[].transcoding.descriptor`）以**配置文件所在目录**为基准解析，且必须
留在该目录内（威胁模型：配置可能来自不受控来源，不能借绝对路径读
`/etc/passwd`、`C:\Windows\...`，也不能用 `..` 跳出部署目录）。

一律拒绝的写法：

- 绝对引用：以 `/` 或 `\` 开头、盘符限定（`C:\x`、`C:x`）、UNC（`\\srv\x`）
  ——**所有平台**行为一致；
- 会逃出配置目录的 `..`（如 `../a.pem`、`sub/../../a.pem`）。

报错示例（消息只回显你写的引用本身，**不回显**解析后的目标路径，避免泄露
目录布局）：

```text
invalid value at ../a.pem: file reference "../a.pem" escapes the config directory
```

合法引用示例：`./a.pem`、`sub/a.pem`、`a/../b.pem`（归一化后仍在目录内）。
过滤器参数里的文件引用（如 `JwtAuth` 的 `jwks_file`）由未来的过滤器实现
处理，同样应遵守此规则（完整示例 `config/rgateway.yaml` 头部注释如此声明）。

### 8.3 超大配置被拒绝

配置输入（文件或内存字符串）有 **16 MiB**（16,777,216 字节）硬上限，在
完整读取/解析**之前**即拒绝，防止超大输入耗尽内存：

```text
invalid value at <root>: config exceeds the 16777216 byte size limit
```

### 8.4 路由不命中排查思路

按顺序检查：

1. **有没有更早的路由抢走了**：路由按 `(order, id)` 排序、首条匹配胜出；
   检查 `order` 更小（或 `order` 相同但 `id` 字典序更小）的路由的谓词是否
   也命中了这个请求。
2. **谓词是 AND**：一条路由的谓词**全部**命中才匹配；任何一条不满足就跳过
   该路由。
3. **Path 细节**：模式必须以 `/` 开头、大小写敏感、尾部斜杠敏感（`/api/*`
   不匹配 `/api`，要用 `/api/**`）；`*` 不跨段。
4. **正则全值匹配**：`Host=api\.example\.com` 不匹配 `api.example.com:8080`
   或 `xapi.example.com`——需要子串语义时显式写 `.*`（§4.4）。
5. **Query 谓词**：URI 无查询串时永远不命中；参数名精确匹配，值按全值正则。
6. **Header 谓词**：头缺失、值非 UTF-8 都不命中；多值头任一值命中即可。
7. 确认配置真的已生效：热重载失败会保留旧配置（§7.2），看日志里有没有
   `config reload failed`。

---

## 9. 可观测性与管理面

### 9.1 访问日志（`observability.access_log`）

每个请求输出**一行**（含 404 与闸门拒绝）到 tracing 管道（target `access`），
`enabled: false` 整体关闭；`format: json`（默认）输出单行 JSON，`format: text`
输出 logfmt 键值对。两种格式字段一致（11 键稳定 schema）：

```json
{"ts":"2026-11-08T04:44:38Z","request_id":"01a1…","peer":"127.0.0.1:5555",
 "method":"GET","path":"/api/x","route_id":"r1","protocol":"http1","status":200,
 "upstream":"127.0.0.1:9000","error_kind":null,"duration_ms":3}
```

字段说明：`route_id` 未命中路由为 `null`；`upstream` 是实际选中的实例 id
（`host:port`）；`error_kind` 是错误分类的小写标签（`rate_limited`、
`unavailable`、`deadline_exceeded`、`circuit_open`、`not_found` 等），无错误
为 `null`。日志行里的字段统一做了 ASCII 控制字符脱敏（防日志注入）。

### 9.2 Prometheus 指标（`observability.metrics`）

指标在**管理面**监听的 `metrics.path`（默认 `/metrics`）暴露，Prometheus
exposition format。指标名与标签：

| 指标 | 类型 | 标签 | 含义 |
|------|------|------|------|
| `rgateway_requests_total` | counter | `route`、`method`、`status` | 命中路由的完成请求数（method 白名单外归一为 `OTHER`） |
| `rgateway_request_duration_seconds` | histogram | `route`、`method` | 请求耗时（秒，5ms–10s 桶） |
| `rgateway_upstream_errors_total` | counter | `route`、`kind` | 最终上游类错误（`unavailable` / `deadline_exceeded`） |
| `rgateway_active_requests` | gauge | — | 在途请求数（响应体耗尽才计出） |
| `rgateway_route_not_found_total` | counter | — | 路由未命中数（无 route 标签，防基数爆炸） |

`metrics.enabled: false` 时 `/metrics` 返回 404（端点不存在）。热重载改
`metrics.path` 立即生效（scrape 配置要跟着改）。请求速率等衍生指标由
Prometheus/查询层计算，网关只出累计量。

### 9.3 管理面 REST（`mgmt.listen`，默认 `127.0.0.1:9090`）

统一信封 `{"success": bool, "data": …, "error": …}`；错误只回显泛化文案。
默认只监听回环地址——**不要**把管理面直接暴露到公网：

| 方法与路径 | data 内容 |
|------------|-----------|
| `GET /health` | `{status: "ok"}` |
| `GET /mgmt/v1/snapshot` | 配置版本、失败重载数、数据面监听、路由摘要 |
| `GET /mgmt/v1/routes` | 路由表（按匹配顺序：id / order / uri / protocol） |
| `GET /mgmt/v1/breakers` | 逐路由逐实例的熔断状态（`closed` / `open` / `half_open`） |
| `POST /mgmt/v1/breakers/reset` | body `{route_id, instance_id?}` → `{reset: n}`（幂等，未知目标返回 0） |
| `GET /mgmt/v1/stats` | 聚合：版本 / 失败重载 / 路由数 / 熔断器总数与打开数 |
| `GET <metrics.path>` | Prometheus 指标文本（§9.2） |

```bash
curl -s http://127.0.0.1:9090/mgmt/v1/stats | jq
curl -s -X POST http://127.0.0.1:9090/mgmt/v1/breakers/reset \
  -d '{"route_id":"api"}' | jq
```

gRPC 管理服务（`rgateway.mgmt.v1.MgmtService`）与 REST 同端口按 content-type
复用，六个 RPC（Health / Snapshot / Routes / Breakers / ResetBreakers / Stats）
与上表 REST 端点一一对应、语义一致；`mgmt.grpc: false` 时 gRPC 调用返回
UNIMPLEMENTED。proto 定义在 `proto/mgmt/v1/mgmt.proto`：

```bash
grpcurl -plaintext 127.0.0.1:9090 rgateway.mgmt.v1.MgmtService/Stats
```
