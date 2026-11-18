# rgateway 配置参考手册（`rgateway.yaml`）

本手册是 `rgateway.yaml` 的逐键参考，面向需要精确了解每个配置键的类型、默认值、
取值范围与校验规则的使用者（网关运维、配置作者、二次开发者）。

- **阅读前提**：了解 YAML 基础语法与 HTTP/gRPC 常用概念即可，不要求读过源码。
- **事实来源**：全部条目逐字段对照实现核对，出处为
  `crates/rgateway-config/src/` 下的 `schema.rs`（数据模型）、`spec.rs`
  （谓词/过滤器归一化）、`dsl.rs`（值语法）、`validate.rs`（语义校验）、
  `loader.rs`（加载与路径安全）、`compile.rs` / `compile_chain.rs` /
  `compile_settings.rs`（编译管线），以及 `crates/rgateway-core/src/predicate.rs`
  （谓词匹配语义）、`crates/rgateway-router/src/ant_path.rs`（ant 路径匹配）。
- **实现进度（如实标注）**：P1–P4、P9 已完成（crate：`rgateway-core`、
  `rgateway-router`、`rgateway-config`、`rgateway-proxy`、`rgateway-filters`、
  `rgateway-obs`、`rgateway-mgmt`、`rgateway-server`；可运行二进制 `rgateway`
  已交付）。配置的**解析、校验、编译**管线与 HTTP 代理、过滤器运行时五件套
  （`RateLimit`/`CircuitBreaker`/`Retry`/`Timeout`/`BodyLimit`）、可观测性
  （访问日志/指标/请求 span）、管理面 REST 均已实装并有测试覆盖；未实装的
  过滤器（`JwtAuth` 等 7 个）保持恒等占位语义。凡涉及运行时行为的描述，
  本手册都会显式标注状态，不会写成已可用。
- **代码标识、命令、配置键保持英文原样**；正则、值语法等均按代码原样给出。

---

## 1. 配置文件总体结构

配置文件是单个 YAML 文档，根节点为顶层映射，共 6 个顶层键，**全部可省略**
（缺省取内置默认值，空文件 `{}` 合法）；出现未知顶层键立即报错
（`deny_unknown_fields`）。

| 顶层键 | 类型 | 必填 | 说明 |
|--------|------|------|------|
| `server` | 映射 | 可选 | 数据面监听与 HTTP/2、限额、超时、TLS 设置 |
| `discovery` | 映射 | 可选 | 服务发现提供者列表（当前仅 `static`） |
| `defaults` | 映射 | 可选 | 全局默认：负载均衡、超时预算、追加过滤器 |
| `routes` | 列表 | 可选 | 路由表；`order` 数值小者先匹配 |
| `observability` | 映射 | 可选 | 访问日志、Prometheus 指标、tracing 过滤 |
| `mgmt` | 映射 | 可选 | 管理面监听设置（REST + 可选 gRPC） |

要点：

- **拼错的键立即报错**，不会被静默忽略（唯一例外：结构化过滤器的参数表
  天然收集任意键，见 §4）。
- 缺失的键取内置默认值，配置文件只需写与默认不同的部分。
- 时长/大小等值在 schema 层保持**字符串形态**，编译阶段才解析，以便报错
  定位到精确坐标（见 §6）。
- 配置热重载：配置文件所在目录被监听（防抖窗口 `DEFAULT_DEBOUNCE` =
  300ms）；重载失败**保留旧快照继续生效**（`snapshot.rs`：失败保旧），
  治理状态（限流桶、熔断器、LB 计数器）跨重载保留。
- 配置输入（文件或内存字符串）有 **16 MiB 大小上限**，超限在解析前拒绝
  （见 §6）。

最小合法配置（全部取默认值）：

```yaml
{}
```

---

## 2. 逐段字段参考

字段表列含义：**类型**为 YAML 形态；**必填**指反序列化或校验层面的要求；
**校验**列给出会被拒绝的取值（报错时机见 §6）。

### 2.1 `server` — 数据面设置

结构体 `ServerConfig`。整体可省略。

#### `server` 直接字段

| 字段 | 类型 | 必填 | 默认值 | 取值范围 / 校验 | 示例 |
|------|------|------|--------|-----------------|------|
| `listen` | string | 可选 | `"0.0.0.0:8080"` | 必须能解析为 `ip:port` socket 地址 | `listen: "127.0.0.1:8080"` |

#### `server.http2` — HTTP/2 协议参数（`Http2Config`）

| 字段 | 类型 | 必填 | 默认值 | 取值范围 / 校验 | 示例 |
|------|------|------|--------|-----------------|------|
| `max_concurrent_streams` | u32 | 可选 | `1024` | 无额外校验 | `max_concurrent_streams: 1024` |
| `initial_stream_window_size` | size 字符串 | 可选 | `"1MB"` | 须为合法大小语法（§5） | `initial_stream_window_size: 1MB` |
| `initial_connection_window_size` | size 字符串 | 可选 | `"64KB"` | 须为合法大小语法（§5） | `initial_connection_window_size: 64KB` |

#### `server.limits` — 请求限制（`LimitsConfig`）

| 字段 | 类型 | 必填 | 默认值 | 取值范围 / 校验 | 示例 |
|------|------|------|--------|-----------------|------|
| `max_request_body` | size 字符串 | 可选 | `"10MB"` | 须为合法大小语法（§5） | `max_request_body: 10MB` |
| `max_concurrent_requests` | u32 | 可选 | `4096` | 必须 > 0 | `max_concurrent_requests: 4096` |
| `max_retry_buffer` | size 字符串 | 可选 | `"1MB"` | 须为合法大小语法（§5）；重试重放请求体的缓冲上限 | `max_retry_buffer: 1MB` |

#### `server.timeouts` — 各阶段超时（`TimeoutsConfig`）

全部字段为紧凑时长字符串（§5），且**必须 > 0**（`0s` 是配置错误）。

| 字段 | 类型 | 必填 | 默认值 | 取值范围 / 校验 | 示例 |
|------|------|------|--------|-----------------|------|
| `connect` | duration 字符串 | 可选 | `"3s"` | 合法时长且 > 0；与上游建连超时 | `connect: 3s` |
| `request_default` | duration 字符串 | 可选 | `"30s"` | 合法时长且 > 0；路由与 `defaults` 都未设 `request_timeout` 时的默认请求 Deadline 预算 | `request_default: 30s` |
| `graceful_shutdown` | duration 字符串 | 可选 | `"30s"` | 合法时长且 > 0；优雅关闭等待时长 | `graceful_shutdown: 30s` |
| `ws_handshake` | duration 字符串 | 可选 | `"10s"` | 合法时长且 > 0；WebSocket 握手超时 | `ws_handshake: 10s` |

#### `server.tls` — TLS 监听设置（`TlsConfig`）

| 字段 | 类型 | 必填 | 默认值 | 取值范围 / 校验 | 示例 |
|------|------|------|--------|-----------------|------|
| `enabled` | bool | 可选 | `false` | — | `enabled: true` |
| `cert` | string | 条件必填 | `""` | PEM 证书链路径；`enabled: true` 时不得为空；相对路径须留在配置目录内（§6） | `cert: ./tls/server.pem` |
| `key` | string | 条件必填 | `""` | PEM 私钥路径；`enabled: true` 时不得为空；同上路径规则 | `key: ./tls/server.key` |
| `client_auth` | 枚举 | 可选 | `none` | `none` / `optional` / `required`（小写） | `client_auth: optional` |
| `client_ca` | string | 可选 | `""` | 校验客户端证书的 PEM CA 包路径；空 = 不配置；同上路径规则 | `client_ca: ./tls/ca.pem` |

注：`cert`/`key`/`client_ca` 在文件变更时热重载（schema 注释）。

### 2.2 `discovery` — 服务发现（`DiscoveryConfig`）

| 字段 | 类型 | 必填 | 默认值 | 取值范围 / 校验 | 示例 |
|------|------|------|--------|-----------------|------|
| `providers` | 列表 | 可选 | `[]` | 提供者列表，按 `type` 标签分派；当前唯一取值 `static` | 见下 |

`providers` 列表项（`DiscoveryProvider`，内部标签 `type`）：

| 字段 | 类型 | 必填 | 默认值 | 取值范围 / 校验 | 示例 |
|------|------|------|--------|-----------------|------|
| `type` | 枚举 | 必填 | — | 当前仅 `static` | `type: static` |
| `services` | 映射 | 可选 | `{}` | 服务名 → 实例列表（`BTreeMap`，遍历顺序稳定） | `services: { user-svc: [...] }` |

实例项（`InstanceConfig`）：

| 字段 | 类型 | 必填 | 默认值 | 取值范围 / 校验 | 示例 |
|------|------|------|--------|-----------------|------|
| `address` | string | 必填 | — | 必须能解析为 `ip:port` socket 地址 | `address: "127.0.0.1:50051"` |
| `weight` | u32 | 可选 | `100` | 必须 > 0（摘流走 discovery removal，不允许 `weight: 0` 表达下线） | `weight: 100` |

校验规则：

- 每个服务至少一个实例（否则报 `service has no instances`）。
- 同一个服务名只允许由**一个** provider 声明；跨 provider 重复会在编译期
  报错（`duplicate service name …`），而不是静默覆盖。
- `lb://` 路由引用的服务名必须存在于本表（见 §2.4 `uri`）。

示例：

```yaml
discovery:
  providers:
    - type: static
      services:
        user-svc:
          - { address: "127.0.0.1:50051", weight: 100 }
          - { address: "127.0.0.1:50052", weight: 100 }
```

### 2.3 `defaults` — 全局默认（`DefaultsConfig`）

先于路由级设置套用到每条路由，**路由级同名设置优先**；唯一例外是
`filters`（追加语义，见下）。

| 字段 | 类型 | 必填 | 默认值 | 取值范围 / 校验 | 示例 |
|------|------|------|--------|-----------------|------|
| `filters` | 列表 | 可选 | `[]` | 过滤器列表（双形态，§4）；**追加到每条路由已声明过滤器之后**（Spring Cloud Gateway 语义），并按规范槽位参与链组装 | `filters: [{ name: CircuitBreaker, sliding_window: 20 }]` |
| `request_timeout` | duration 字符串 | 可选 | 不设置 | 合法时长且 > 0；作为请求 Deadline 预算，可被路由级 `request_timeout` 覆盖 | `request_timeout: 30s` |
| `load_balancer` | string | 可选 | 不设置 | `round_robin` / `random` / `smooth_wrr` 之一；可被路由级 `load_balancer` 覆盖；两级都未设时用 `round_robin` | `load_balancer: smooth_wrr` |

生效优先级（编译期解析）：

- 超时预算：`routes[].request_timeout` → `defaults.request_timeout` →
  `server.timeouts.request_default`。
- 负载均衡：`routes[].load_balancer` → `defaults.load_balancer` →
  `round_robin`。非法算法名报 `unknown load balancer … (expected
  round_robin|random|smooth_wrr)`。

### 2.4 `routes` — 路由表（`RouteConfig` 列表）

列表项字段（所有键均不允许未知键）：

| 字段 | 类型 | 必填 | 默认值 | 取值范围 / 校验 | 示例 |
|------|------|------|--------|-----------------|------|
| `id` | string | 必填 | — | 非空（trim 后）且全表唯一；同时是错误定位坐标名 `routes[<id>]` | `id: user-http` |
| `order` | i32 | 可选 | `0` | 数值**小者先匹配** | `order: 1` |
| `uri` | string | 必填 | — | `lb://<service>`（走服务发现 + 负载均衡）或 `http(s)://host:port`（直连）；见下方校验 | `uri: "lb://user-svc"` |
| `protocol` | string | 可选 | `"auto"` | 白名单：`auto` / `http1` / `http2` / `grpc` / `grpc-web` / `grpc-web-text` / `rest-json` / `websocket` | `protocol: rest-json` |
| `predicates` | 列表 | 可选 | `[]` | 谓词列表（双形态，§3）；全部命中（AND）才算匹配本路由 | `predicates: ["Path=/api/**"]` |
| `filters` | 列表 | 可选 | `[]` | 过滤器列表（双形态，§4） | `filters: ["RateLimit=key:client_ip,rate:100,period:1s"]` |
| `transcoding` | 映射 | 可选 | 不设置 | gRPC-JSON 转码设置；仅 `protocol` 为 `auto` 或 `rest-json` 时合法 | 见下 |
| `request_timeout` | duration 字符串 | 可选 | 不设置 | 合法时长且 > 0；覆盖 `defaults.request_timeout` | `request_timeout: 30s` |
| `load_balancer` | string | 可选 | 不设置 | `round_robin` / `random` / `smooth_wrr`；覆盖 `defaults.load_balancer` | `load_balancer: round_robin` |

`uri` 校验（`RouteUri::parse`）：

- `lb://` 后必须是非空纯服务名（不含 `/`），且必须已在 `discovery`
  中声明（否则报 `unknown discovery service`）。
- 直连形式必须是 `http://` 或 `https://` scheme 且带 authority
  （`host` 或 `host:port`；端口可省略，建连时按 scheme 取 80/443）。
  其它 scheme、缺 authority、解析失败都报错。

未声明任何谓词的路由匹配**所有**请求（`AlwaysPredicate` 语义）；
多条路由按 `order` 升序逐条匹配。

#### `routes[].transcoding`（`TranscodingConfig`）

| 字段 | 类型 | 必填 | 默认值 | 取值范围 / 校验 | 示例 |
|------|------|------|--------|-----------------|------|
| `descriptor` | string | 必填（出现 transcoding 时） | — | 非空；`protoc --include_imports --descriptor_set_out` 生成的 `FileDescriptorSet` 文件路径；相对路径须留在配置目录内（§6） | `descriptor: ./proto/descriptor.bin` |
| `rules` | 列表 | 可选 | `[]` | 显式 HTTP 映射规则（优先于 `google.api.http` 注解） | 见下 |

#### `routes[].transcoding.rules[]`（`HttpRuleConfig`，对应 `google.api.http` HttpRule）

| 字段 | 类型 | 必填 | 默认值 | 取值范围 / 校验 | 示例 |
|------|------|------|--------|-----------------|------|
| `selector` | string | 必填 | — | `package.Service/Method` 全限定形式（必须含 `/`） | `selector: helloworld.Greeter/SayHello` |
| `get` | string | 五选一 | 不设置 | HTTP GET 的 URI 模板 | `get: /v1/echo` |
| `post` | string | 五选一 | 不设置 | HTTP POST 的 URI 模板 | `post: /v1/echo` |
| `put` | string | 五选一 | 不设置 | HTTP PUT 的 URI 模板 | — |
| `delete` | string | 五选一 | 不设置 | HTTP DELETE 的 URI 模板 | — |
| `patch` | string | 五选一 | 不设置 | HTTP PATCH 的 URI 模板 | — |
| `body` | string | 可选 | 不设置 | `*` 或一个请求字段名，映射到 HTTP body | `body: "*"` |
| `response_body` | string | 可选 | 不设置 | 响应消息中映射到 HTTP body 的字段名 | — |
| `additional_bindings` | 列表 | 可选 | `[]` | 同一方法的附加映射；每条**至少**设置一个 HTTP 方法字段 | — |

校验规则：每条顶层 `rules[]` 的 `get/post/put/delete/patch` 必须**恰好设置
一个**（多了少了都报错）；`additional_bindings` 里每条至少一个。

### 2.5 `observability` — 可观测性（`ObservabilityConfig`）

#### `observability.access_log`（`AccessLogConfig`）

| 字段 | 类型 | 必填 | 默认值 | 取值范围 / 校验 | 示例 |
|------|------|------|--------|-----------------|------|
| `enabled` | bool | 可选 | `true` | — | `enabled: true` |
| `format` | 枚举 | 可选 | `json` | `json` / `text`（小写） | `format: json` |

#### `observability.metrics`（`MetricsConfig`）

| 字段 | 类型 | 必填 | 默认值 | 取值范围 / 校验 | 示例 |
|------|------|------|--------|-----------------|------|
| `enabled` | bool | 可选 | `true` | — | `enabled: true` |
| `path` | string | 可选 | `"/metrics"` | 管理面上暴露指标的路径；必须以 `/` 开头、不含空白/`?`/`#`，且不得与管理面保留路径（`/health`、`/mgmt/v1/...`）冲突，否则加载期报 `observability.metrics.path` 错误 | `path: /metrics` |

#### `observability` 直接字段

| 字段 | 类型 | 必填 | 默认值 | 取值范围 / 校验 | 示例 |
|------|------|------|--------|-----------------|------|
| `tracing_filter` | string | 可选 | 不设置 | tracing 过滤表达式（`RUST_LOG` 语法） | `tracing_filter: rgateway=debug` |

### 2.6 `mgmt` — 管理面（`MgmtConfig`）

| 字段 | 类型 | 必填 | 默认值 | 取值范围 / 校验 | 示例 |
|------|------|------|--------|-----------------|------|
| `listen` | string | 可选 | `"127.0.0.1:9090"` | 必须能解析为 `ip:port` socket 地址；管理面 REST + 可选 gRPC | `listen: "127.0.0.1:9090"` |
| `grpc` | bool | 可选 | `true` | 是否同时提供 gRPC 管理服务 | `grpc: true` |

---

## 3. 谓词参考

谓词决定请求是否匹配一条路由。一条路由的谓词**全部命中（AND）**才算匹配；
未配置谓词的路由匹配所有请求。

### 双形态写法

谓词在 YAML 中有两种等价形态（`untagged`，编译期归一化为
`PredicateSpec { name, args }`）：

| 形态 | 写法 | 适用 |
|------|------|------|
| 紧凑 DSL 字符串 | `"Name=arg1,arg2"` | 参数少，一行写完 |
| 结构化映射 | `{ name: Name, args: [arg1, arg2] }` | 参数多、含逗号/特殊字符 |

紧凑串按**第一个** `=` 切出名字；参数在**未转义的逗号**上切分（转义规则见
§5）；名字与各参数均 trim。两种形态解析结果完全等价。

### 内置谓词一览

| 谓词 | 参数 | 匹配语义 |
|------|------|----------|
| `Path` | ≥ 1 个 ant 路径模式 | 任一模式匹配请求路径即命中（模式间 OR）；ant 语法见 §5 |
| `Method` | ≥ 1 个 HTTP 方法名 | 请求方法在列表中（精确匹配、大小写敏感） |
| `Header` | 恰好 2 个：`(name, regex)` | 同名头**任一值**全值匹配正则即命中；头缺失或值非 UTF-8 不命中 |
| `Host` | ≥ 1 个正则 | 请求 authority 全值匹配（多参数按 `|` 拼接为择一） |
| `Query` | 恰好 2 个：`(name, regex)` | 查询串中存在参数 `name` 且其值全值匹配正则 |

未在上表的名字报 `UnknownPredicate`。

### 正则全值匹配（安全语义）

`Header` / `Host` / `Query` 的正则是与**整个值**匹配的正则（对齐
Spring / Java `String.matches` 语义）：任何用户模式 `P` 都被编译为
`^(?:P)$`（锚定全值，配置校验与运行时共享同一构造函数
`rgateway_core::predicate::full_match_regex`）。

- 防路由劫持：`Host=api\.example\.com` 不会命中 `xapi.example.com`、
  `api.example.com.evil`、`api-example.com`。
- 模式本身仍是完整正则：择一（`a|b`）按全值生效，`.*example.*` 照常工作
  ——去掉的只是“隐式子串搜索”。
- host 按字面匹配时请转义点号（`\.`）；`api.example.com` 未转义的 `.` 是
  正则通配符。

对比：`RewritePath` 的正则**不做**全值锚定（它是查找/替换重写器，允许部分
匹配），见 §4。

### 各谓词示例

```yaml
predicates:
  # Path：ant 模式，多个模式 OR；与其他谓词 AND
  - "Path=/api/users/**, /api/orders/**"
  # Method：方法名列表
  - "Method=GET,POST"
  # Header：(头名, 全值正则)；紧凑形态里 \d 原样保留
  - 'Header=X-Tenant,\d+'
  # Host：注意转义点号
  - 'Host=api\.example\.com'
  # Query：(参数名, 全值正则)
  - 'Query=version,v[0-9]+'
```

结构化写法（参数多或含逗号时更清晰）：

```yaml
predicates:
  - { name: Method, args: [GET, POST] }
  - { name: Header, args: [X-Tenant, "\\d+"] }
  - { name: Path, args: ["/api/**"] }
```

匹配语义细节：

- `Method`：列表为空永不命中（但校验要求至少一个参数）。
- `Header`：同名头出现多个值时任一值命中即匹配。
- `Host`：优先取 `Host` 头，缺失时回退 `uri.authority()`（HTTP/2 只带
  `:authority` 伪头的场景）；带端口的 authority（`api.example.com:8080`）
  需要模式显式覆盖才匹配。
- `Query`：按 `&` 切分参数、首个 `=` 前为名其余为值（无 `=` 的片段值视作
  空串）；URI 无查询串不命中；同名多参数任一命中即匹配。
- `Path`：对 `parts.uri.path()` 匹配；ant 语法与尾部斜杠行为见 §5。

---

## 4. 过滤器参考

### 4.1 实装状态总览（如实标注）

过滤器分两层，当前进度不同：

| 层 | 内容 | 状态 |
|----|------|------|
| 配置层（`rgateway-config`，已实装） | 名称识别与槽位归类（`slot_for`）、紧凑/结构化双形态解析、部分参数校验（`validate.rs`）、过滤链槽位组装管线（`compile_chain.rs`） | **P1–P2 已完成** |
| 运行时层（`rgateway-filters` / `rgateway-proxy`） | 各过滤器的 `FilterFactory` tower layer 实现 | **P3/P4 已交付 5 个**：`RateLimit`、`CircuitBreaker`、`Retry`、`Timeout`、`BodyLimit`（`rgateway-server` 启动时以真实工厂覆盖 no-op 占位注册表）；其余（`JwtAuth`、`RewritePath`、`Compression`、`CORS` 等 7 个）属 P5/P6，仍为恒等占位（配置可加载、运行期零拦截） |
| 治理原语（`rgateway-core`，已实装） | 熔断器状态机（`governance.rs`）、负载均衡（`lb.rs`）、跨重载路由状态（`RouteState`） | **P1–P2 已完成** |

即：过滤器**名字与链序始终生效**（写错名字、缺必填参数会在加载配置时报错），
P4 实装的 5 个过滤器在运行期真正拦截/改写请求；未实装的名字保持
「配置可编译、槽位排链、运行期零拦截」的占位语义。下表“含义”凡涉及
尚未实装的运行时行为处仍标 **[运行时：规划中]**，已实装处标注默认值来源。

### 4.2 过滤器链：规范槽位与组装顺序

过滤器不按书写顺序执行，而是按**规范槽位**（canonical slot）固定分组
（外 → 内）：

```text
Compression → CORS → RateLimit → Auth → Route → Deadline(内建) →
Retry → TargetSelect(内建) → CircuitBreaker → Timeout → Handler
```

- 同一槽位内保持配置顺序；路由声明的过滤器排在 `defaults.filters`
  之前，因此包在默认过滤器**外侧**（先执行）。
- `Deadline`（总超时预算）与 `TargetSelect`（目标选择）是编译期固定插入的
  内建层，不是可配置过滤器：Deadline 约束整个请求（含全部重试）拿到响应头
  的时刻；TargetSelect 每次尝试重新选实例并跳过熔断打开的实例。
- `CircuitBreaker` 为**实例级**，位于目标选择之后、单次尝试超时之前。

槽位归属（`slot_for`）：

| 槽位 | 过滤器名 |
|------|----------|
| Compression | `Compression` |
| CORS | `CORS` |
| RateLimit | `RateLimit` |
| Auth | `JwtAuth`、`ApiKeyAuth`、`IpFilter` |
| Route | `RewritePath`、`RequestHeader`、`BodyLimit` |
| Retry | `Retry` |
| Breaker | `CircuitBreaker` |
| Timeout | `Timeout` |

未在此表的名字报 `UnknownFilter`。注意：**`ResponseHeader` 不是已定义的
过滤器名**（代码中没有该名字），配置里写它会报 `UnknownFilter`。

### 4.3 双形态写法与参数类型

过滤器同样双形态（归一化为 `FilterSpec { name, params }`）：

| 形态 | 写法 | 说明 |
|------|------|------|
| 紧凑 DSL 字符串 | `"Name=k1:v1,k2:v2"` | 每段按**第一个** `:` 切 k/v（值内可再含 `:`，如 URL）；参数值一律是字符串 |
| 结构化映射 | `{ name: Name, k1: v1, ... }` | 除 `name` 外的所有键值进入参数表；值保留 YAML 原生类型（数字/布尔/序列/嵌套 map） |

两种形态的参数读取完全等价：类型化访问器先取原生类型，失败再回退字符串解析
（数字字符串、时长/大小字符串、`%` 百分数、布尔词等，见 §5）。类型/格式
不符会在校验或编译期报错，定位到该过滤器条目。

### 4.4 逐过滤器参数表

下表“校验”列标注的是**配置层今天就会执行**的检查；已实装的过滤器
（P4 五件套）在**运行时构建层**还有一道参数校验（见各表“默认”列），
未实装过滤器的语义与默认值仍待实现（标注 **[运行时：规划中]**）。

#### `RateLimit`（RateLimit 槽，已实装）

| 参数 | 类型 | 必填 | 默认 | 校验 / 含义 |
|------|------|------|------|-------------|
| `key` | string | 必填 | — | 限流键维度标识（示例 `key:client_ip`）；缺失报 `RateLimit requires key` |
| `rate` | u64 | 必填 | — | 窗口内允许通过的请求数；缺失报 `RateLimit requires rate` |
| `period` | duration | 可选 | `1s` | 限流窗口时长；若给出必须 > 0（运行时缺省 `DEFAULT_PERIOD = 1s`） |

```yaml
- "RateLimit=key:client_ip,rate:100,period:1s"
```

#### `CircuitBreaker`（Breaker 槽，实例级，已实装）

参数含义对应 `rgateway-core` 的 `BreakerConfig`（滑动窗口错误率三态机，
状态机已实装于 `governance.rs`，跨热重载保留；P4 已接入过滤器链）。

| 参数 | 类型 | 必填 | 默认* | 校验 / 含义 |
|------|------|------|-------|-------------|
| `sliding_window` | u64 | 可选 | `20` | 滑动窗口长度（按已完成调用计数） |
| `failure_rate` | number / 百分数 | 可选 | `0.5` | 失败率阈值，必须落在 `0..=1`（`50%` 折算 0.5；`"200%"` 报错） |
| `min_calls` | u64 | 可选 | `10` | 触发熔断的最小样本数；必须 ≤ `sliding_window` |
| `open_duration` | duration | 可选 | `5s` | Open 状态持续时长，到期转 HalfOpen 探测 |
| `half_open_max_calls` | u64 | 可选 | `3` | HalfOpen 期间并发探测上限 |

\* 默认值取自 `BreakerConfig::default()`（`governance.rs`），过滤器层沿用
同一默认（运行时构建缺省即该表）。

```yaml
- { name: CircuitBreaker, sliding_window: 20, failure_rate: 50%,
    min_calls: 10, open_duration: 5s, half_open_max_calls: 3 }
```

#### `Retry`（Retry 槽，已实装）

| 参数 | 类型 | 必填 | 默认 | 校验 / 含义 |
|------|------|------|------|-------------|
| `retries` | u64 | 可选 | `3` | 重试次数上限（对齐 SCG `DEFAULT_RETRIES=3`）；配置层与运行时都强制 ≤ 10（防重试风暴） |
| `methods` | 字符串列表 | 可选 | 空列表 | 参与重试判定的请求方法（如 `[GET, HEAD]`） |
| `statuses` | u64 列表 | 可选 | 空列表 | 触发重试的 HTTP 状态码（如 `[502, 503, 504]`） |
| `grpc_statuses` | u64 列表 | 可选 | 空列表 | 触发重试的 gRPC 状态码（如 `[14]`） |
| `backoff` | map | 可选 | 见子键默认 | 退避配置，子键见下 |

`backoff` 子键（缺省整组取 `initial: 50ms, max: 2s, multiplier: 2.0, jitter: full`）：

| 子键 | 类型 | 默认 | 校验 / 含义 |
|------|------|------|-------------|
| `initial` | duration | `50ms` | 首次退避时长；若给出必须 > 0（配置层校验） |
| `max` | duration | `2s` | 退避上限（`initial × multiplier^n` 封顶于此） |
| `multiplier` | number | `2.0` | 递增倍率；若给出必须 > 0 |
| `jitter` | string | `full` | 抖动策略；`full` = 在 `[0, 基础退避]` 内均匀取值 |

```yaml
- { name: Retry, retries: 3, methods: [GET, HEAD], statuses: [502, 503, 504],
    grpc_statuses: [14], backoff: { initial: 50ms, max: 2s, multiplier: 2, jitter: full } }
```

#### `Timeout`（Timeout 槽，单次尝试超时，已实装）

| 参数 | 类型 | 必填 | 默认 | 校验 / 含义 |
|------|------|------|------|-------------|
| `per_attempt` | duration | 与 `timeout` 二选一 | — | 单次上游尝试的超时时长，**优先于** `timeout`；必须 > 0；两者都缺失/非法时报 `Timeout requires per_attempt (or timeout)` |
| `timeout` | duration | 与 `per_attempt` 二选一 | — | 同上语义的别名（`per_attempt` 缺失时兜底）；必须 > 0 |

两者同时给出时 `per_attempt` 胜出（单次尝试预算的语义更精确）。

```yaml
- "Timeout=per_attempt:5s"
```

#### `RewritePath`（Route 槽）

| 参数 | 类型 | 必填 | 默认 | 校验 / 含义 |
|------|------|------|------|-------------|
| `regex` | string | 必填 | — | 路径重写正则；仅校验语法可编译（`RewritePath regex invalid: …`）；**不做全值锚定**（重写器允许部分匹配，与 Header/Query 的 `^(?:P)$` 语义不同） |
| `replacement` | string | 必填 | — | 替换模板；缺失报 `RewritePath requires regex and replacement` |

```yaml
- { name: RewritePath, regex: "/api/(?<segment>.*)", replacement: "/${segment}" }
```

#### `RequestHeader`（Route 槽）

参数为「请求头名 → 值」的自由键值对（紧凑形态下 key 即头名；值内可含冒号，
`\,` 转义逗号）。配置层不做参数校验；头改写语义 **[运行时：规划中]**。

```yaml
- "RequestHeader=x-uri:http://a:8080/v"
- 'RequestHeader=x-a:foo\,bar,x-b:2'
```

#### `BodyLimit`（Route 槽，已实装）

| 参数 | 类型 | 必填 | 默认 | 校验 / 含义 |
|------|------|------|------|-------------|
| `max_body` | size | 必填 | — | 请求体大小上限（§5 大小语法）；缺失报 `BodyLimit requires max_body`。运行时对 Content-Length 预检 + body 流限长双保险，超限映射 `InvalidArgument`（400 / grpc-status 3） |

```yaml
- { name: BodyLimit, max_body: 10MB }
```

#### `JwtAuth`（Auth 槽）

示例配置（`config/rgateway.yaml`、测试）中出现的参数：

| 参数 | 类型 | 必填 | 默认 | 含义 |
|------|------|------|------|------|
| `issuer` | string | [运行时：规划中] | — | 期望的 JWT 签发者 **[运行时：规划中]** |
| `audience` | string | [运行时：规划中] | — | 期望的受众 **[运行时：规划中]** |
| `jwks_file` | string | [运行时：规划中] | — | JWKS 文件相对路径 **[运行时：规划中]** |

配置层对这些参数**不做校验**（`validate.rs` 无 `JwtAuth` 分支），语义由
规划中的过滤器实现定义。示例：

```yaml
- { name: JwtAuth, issuer: "https://idp.example.com", audience: gw, jwks_file: ./jwks.json }
```

#### `ApiKeyAuth`、`IpFilter`（Auth 槽）

名称已识别（写错名字之外不会报 `UnknownFilter`），但代码中没有参数示例与
参数校验；全部语义 **[运行时：规划中]**。

#### `Compression`（Compression 槽，最外层）、`CORS`（CORS 槽）

名称已识别。测试中出现过 `Compression=min_size:1024`（参数仅出现在测试夹具，
语义 **[运行时：规划中]**）；`CORS` 无参数示例。响应压缩与跨域处理依赖
`tower-http`（规划中）。

---

## 5. 值语法规范

### 5.1 时长（duration 字符串）

| 写法 | 含义 |
|------|------|
| `500ms` | 500 毫秒 |
| `500us` | 500 微秒（0.5 毫秒） |
| `3s` | 3 秒 |
| `1.5s` | 1.5 秒（允许小数） |
| `2m` | 2 分钟 |
| `1h` | 1 小时 |
| `1d` | 1 天 |
| `250`（纯数字） | 250 毫秒 |

- 单位后缀：`ms` / `us` / `s` / `m` / `h` / `d`；**纯数字按毫秒**。
- 负数、空串、未知后缀（`5x`）、非有限值（`NaN`、`inf`）、超 `Duration`
  表示范围的量值一律报错（错误形式上报，绝不 panic）。
- 要求 > 0 的字段（`server.timeouts.*`、`request_timeout`、
  `RateLimit.period`、`Retry.backoff.initial`）写 `0s` / `0` 会报
  `duration must be > 0`。

### 5.2 大小（size 字符串，1024 进制）

| 写法 | 字节数 |
|------|--------|
| `512B` | 512 |
| `64KB` / `64KiB` | 64 × 1024 |
| `10MB` / `10MiB` | 10 × 1024² |
| `1GB` / `1GiB` | 1024³ |
| `1024`（纯数字） | 1024 字节 |

- 后缀：`B` / `KB` / `MB` / `GB` 与 `KiB` / `MiB` / `GiB`（两组等价，
  均按 **1024 进制**：KB=1024、MB=1024²、GB=1024³）；**纯数字按字节**。
- 非法后缀、负数、溢出（超 `usize`）报错。

### 5.3 百分数

浮点类参数（如 `CircuitBreaker.failure_rate`）的字符串形态支持 `%` 后缀：
`"50%"` 折算为 `0.5`，`"50"` 为 `50.0`（是否百分比语义由参数决定，
`failure_rate` 按 0..=1 解释，故推荐写 `50%`）。

### 5.4 布尔词

结构化 YAML 里可直接写 `true` / `false`；字符串形态接受
`true`/`yes`/`on` 与 `false`/`no`/`off`（大小写不敏感）。

### 5.5 紧凑 DSL 的转义与切分

紧凑谓词（`Name=arg1,arg2`）与紧凑过滤器（`Name=k1:v1,k2:v2`）共用切分规则：

| 转义 | 含义 |
|------|------|
| `\,` | 字面逗号（不作参数分隔符） |
| `\\` | 字面反斜杠 |
| 其它 `\x` | **原样保留**（反斜杠不丢失——正则 `\d`、`\.` 安全） |

- 切分发生在**未转义的逗号**上，且先切分再反转义（单趟扫描）。
- 每段 trim；空段（多余逗号）在过滤器参数里跳过。
- 输入以孤立 `\` 结尾报 `dangling escape at end of input`。
- 谓词名按**第一个** `=` 切出（后续 `=` 属于参数值）；过滤器 k:v 按
  **第一个** `:` 切分（值内可含 `:`，如 `RequestHeader=x-uri:http://a:8080/v`）。
- 错误：缺 `=`、名字为空、过滤器段内缺 `:`、参数键为空。

紧凑过滤器的值全部是字符串，由类型化访问器按需转换（数字、时长、大小、
百分数、布尔词均见上）。结构化形态直接携带 YAML 原生类型：
`backoff: {initial: 50ms, max: 2s}`、`statuses: [502, 503]` 等；
`u64` 列表元素必须是 YAML number（字符串数字不回退解析）。

### 5.6 ant 路径模式（`Path` 谓词）

模式与路径按 `/` 切分为段，通配符语义限定在段内（与 Spring Cloud Gateway
兼容，**大小写敏感**，不做 Unicode 归一化）：

| 通配符 | 语义 |
|--------|------|
| `?` | 段内恰好匹配一个字符（按字符计数） |
| `*` | 段内匹配零个或多个字符，永不过 `/` |
| `**` | 仅当独占整个段时生效，匹配零个或多个完整段（可跨 `/`） |

配置层校验（`validate_ant_pattern`）：模式非空、必须以 `/` 开头、
`**` 必须独占一个段（`/a**/b` 拒绝）。

尾部斜杠行为（忽略一个前导 `/` 后按 `/` 切段，尾部 `/` 留下一个空段）：

```text
/api/*     匹配 /api/x、/api/          不匹配 /api、/api/x/y
/api/**    匹配 /api、/api/、/api/a/b  不匹配 /v1、/apix
/users/?   匹配 /users/1               不匹配 /users/10、/users/
/a/**/b    匹配 /a/b、/a/x/y/b         不匹配 /a/x/y/c
```

运行时匹配器（`rgateway-router`）构造期把连续 `**` 折叠为单个（语义不变），
采用贪心 + 单回溯点，最坏 O(n × m)（防指数回溯爆炸）。

---

## 6. 校验规则汇总

### 6.1 校验时机与错误收集

加载流水线：**schema 反序列化 → 语义校验（`validate`）→ 编译（`compile`）**。

| 阶段 | 报错内容 | 行为 |
|------|----------|------|
| schema | 未知键（`deny_unknown_fields`）、缺必填（`id`/`uri`/`address`/`type`/`selector` 等）、YAML 语法错误 | 反序列化即失败 |
| validate | 取值/跨字段约束（下表） | **一次性收集全部错误**（避免改一处报一处）；空结果 = 可安全编译 |
| compile | 值解析、服务名重复、LB 算法名、路径引用、过滤器构建 | 带定位失败，不 panic |

`validate` 的主要规则（错误定位见 6.2）：

| 区域 | 规则 |
|------|------|
| `server` | `listen` 可解析为 socket 地址；4 个 size 字段语法合法；4 个 timeout 字段语法合法且 > 0；`tls.enabled` 时 `cert`/`key` 非空；`max_concurrent_requests` > 0 |
| `discovery` | 每服务至少一个实例；`address` 可解析；`weight` > 0 |
| `defaults` | `filters` 逐条做与路由过滤器相同的按名校验 |
| `routes` | `id` 非空且唯一；`uri` 合法且 `lb://` 服务存在；`protocol` 在白名单；`transcoding` 仅 `auto`/`rest-json`，`descriptor` 非空，`rules` 的 `selector` 含 `/`、HTTP 方法恰好一个、`additional_bindings` 至少一个方法；谓词按名校验（参数个数、ant 语法、正则可编译）；过滤器按名校验（未知名、必填参数、范围约束） |
| compile | 服务名跨 provider 重复；`load_balancer` ∈ {`round_robin`,`random`,`smooth_wrr`}；`request_timeout` > 0；TLS/transcoding 相对路径留在配置目录内 |

谓词参数校验细节：`Path`/`Method`/`Header`/`Host`/`Query` 至少一个参数；
`Header`/`Query` 恰好 2 个参数且第 2 个按 `^(?:P)$` 全值形式校验正则可编译；
`Path` 每个模式过 ant 语法校验。过滤器参数校验细节见 §4.4“校验”列。

### 6.2 错误定位（Location）怎么读

每个错误都携带一个配置坐标字符串，格式逐级下钻：

| 形式 | 含义 |
|------|------|
| `<root>` | 整文件级（I/O 失败、YAML 解析失败、超大小上限） |
| `server`、`defaults` | 顶层段 |
| `server.listen`、`server.timeouts.connect` | 段.字段（多级点分，如 `server.http2.initial_stream_window_size`） |
| `routes[user-http]` | 路由条目（方括号内是路由 `id`） |
| `routes[user-http].predicates[0]` | 路由内列表项（方括号内是下标） |
| `routes[user-http].filters[2]` | 路由过滤器第 3 项 |
| `routes[user-http].transcoding.rules[0].selector` | 嵌套字段逐级下钻 |
| `defaults.filters[1]` | 默认过滤器第 2 项 |
| `discovery.providers[0].services[user-svc][1].weight` | 提供者下标 → 服务名 → 实例下标 → 字段 |
| 过滤器原文 | 过滤器参数错误定位为该条目的源文本（紧凑串原文，或结构化形态的名字） |
| 引用文本本身 | 路径引用被拒时，Location 就是被拒的引用字符串 |

错误消息前缀（`error.rs` 七类）：`io error at …`、`yaml parse error at …`、
`invalid config at …`（结构性）、`invalid value at …`（取值）、
`unknown filter <name> at …`、`unknown predicate <name> at …`、
`compile failed at …`。

示例：

```text
unknown filter Nope at defaults.filters[1]
invalid value at server.timeouts.connect: duration must be > 0
invalid value at routes[user-http].uri: unknown discovery service "user-svc"
invalid value at routes[user-http].predicates[0]: Path needs at least one argument
```

### 6.3 路径引用安全规则

配置引用的文件（`server.tls.cert` / `key` / `client_ca`、
`routes[].transcoding.descriptor` 等）以**配置文件所在目录**为基准解析，
且必须留在该目录内（威胁模型：配置文件可能来自不受控来源，不得借绝对路径
读任意系统文件，也不得用 `..` 跳出部署目录）：

| 被拒引用 | 例 | 原因 |
|----------|----|------|
| POSIX 绝对路径 | `/etc/passwd` | 绝对引用 |
| 反斜杠开头 | `\x.pem` | 绝对风格（所有平台一律拒） |
| 盘符限定 | `C:/x.pem`、`C:x.pem` | 盘符/UNC/verbatim 一律拒 |
| 上跳逃逸 | `../a.pem`、`sub/../../a.pem` | 跳出配置目录 |
| 非 UTF-8 段 | — | 无法安全归一化 |

- 允许：`./a.pem`、`sub/a.pem`、`a/../b.pem`（纯词法归一化后仍留在目录内；
  归一化**不触碰文件系统**）。
- 错误消息只回显输入引用本身（按 `{:?}` 渲染，控制字符无法伪造日志行），
  **绝不回显解析后的目标路径**（不向配置作者泄露目录布局）。
- 空字符串表示「未配置」，不参与解析。

### 6.4 大小上限

- 配置输入（文件或内存字符串）上限 **16 MiB**（`MAX_CONFIG_BYTES` =
  16 × 1024 × 1024）。
- 在完整读取/解析**之前**拒绝：读取有界（metadata 预检 + 最多读
  `上限 + 1` 字节判定「刚好超限」），防止超大输入耗尽内存。
- 超限错误定位 `<root>`：`config exceeds the 16777216 byte size limit`。

---

## 7. 完整示例

带注释的示例配置（基于 `config/rgateway.yaml` 精简；所有相对路径以本文件
所在目录为基准）。第一部分：全局段。

```yaml
# 数据面：监听、限额、超时（未写的键取默认值）
server:
  listen: "0.0.0.0:8080"
  limits: { max_request_body: 10MB, max_concurrent_requests: 4096, max_retry_buffer: 1MB }
  timeouts: { connect: 3s, request_default: 30s, graceful_shutdown: 30s, ws_handshake: 10s }
  # tls: { enabled: true, cert: ./tls/server.pem, key: ./tls/server.key, client_auth: none }

# 服务发现：静态服务列表（lb://user-svc 引用这里声明的服务）
discovery:
  providers:
    - type: static
      services:
        user-svc:
          - { address: "127.0.0.1:50051", weight: 100 }
          - { address: "127.0.0.1:50052", weight: 100 }

# 全局默认：追加到每条路由的过滤链末尾；可被路由级同名字段覆盖
defaults:
  load_balancer: smooth_wrr
  request_timeout: 30s
  filters:
    - { name: CircuitBreaker, sliding_window: 20, failure_rate: 50%, min_calls: 10,
        open_duration: 5s, half_open_max_calls: 3 }
```

第二部分：路由与可观测性。注意过滤链执行顺序是 §4.2 的**规范槽位序**，
与本书写顺序无关。

```yaml
routes:
  # HTTP 路由：改写路径、按客户端限流、重试、JWT 认证
  - id: user-http
    order: 1                       # order 小者先匹配
    uri: "lb://user-svc"           # 走服务发现 + 负载均衡
    predicates:
      - "Path=/api/users/**, /api/orders/**"   # 多模式 OR，ant 语法
      - "Method=GET,POST"
    filters:
      - { name: RewritePath, regex: "/api/(?<segment>.*)", replacement: "/${segment}" }
      - "RateLimit=key:client_ip,rate:100,period:1s"
      - { name: Retry, retries: 3, methods: [GET, HEAD], statuses: [502, 503, 504],
          grpc_statuses: [14], backoff: { initial: 50ms, max: 2s, multiplier: 2, jitter: full } }
      - { name: JwtAuth, issuer: "https://idp.example.com", audience: gw, jwks_file: ./jwks.json }

  # gRPC 透明路由：单次尝试超时 5s（总预算由 request_timeout/defaults 控制）
  - id: user-grpc
    uri: "lb://user-svc"
    protocol: auto
    predicates: [ "Path=/helloworld.Greeter/*" ]
    filters:
      - { name: Timeout, per_attempt: 5s }

  # REST/JSON <-> gRPC 转码（protocol auto 或 rest-json 才可配 transcoding）
  - id: user-rest
    uri: "lb://user-svc"
    predicates: [ "Path=/v1/**" ]
    transcoding:
      descriptor: "./proto/descriptor.bin"
      rules: [{ selector: "helloworld.Greeter/SayHello", post: "/v1/echo", body: "*" }]

observability:
  access_log: { enabled: true, format: json }
  metrics: { enabled: true, path: /metrics }

mgmt: { listen: "127.0.0.1:9090", grpc: true }
```

状态提醒：以上配置的**解析、校验、编译语义**均已实装；其中各过滤器的运行时
行为（拦截、限流、重试、转码执行等）随 `rgateway-server`（P3 起）交付，
当前版本配置它们不会报错，但网关二进制尚不可运行。
