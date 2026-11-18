/**
 * 管理面 REST 契约类型（与 rgateway-mgmt/src/rest.rs 的 JSON 形态对应）。
 * 仅描述已实现的纯函数契约，字段命名沿用服务端 snake_case。
 */

/** GET /health → data */
export interface HealthData {
  status: string;
}

/** 路由摘要（snapshot / routes 端点共用） */
export interface RouteSummary {
  id: string;
  order: number;
  uri: string;
  protocol: string;
}

/** GET /mgmt/v1/snapshot → data */
export interface SnapshotData {
  version: number;
  failed_reloads: number;
  listen: string;
  route_count: number;
  routes: RouteSummary[];
}

/** GET /mgmt/v1/routes → data（按路由匹配顺序） */
export interface RoutesData {
  routes: RouteSummary[];
}

/** 单实例熔断状态：closed / open / half_open */
export interface BreakerEntry {
  instance_id: string;
  state: string;
}

/** 单个路由的熔断状态（实例按 id 排序） */
export interface RouteBreakers {
  route_id: string;
  breakers: BreakerEntry[];
}

/** GET /mgmt/v1/breakers → data */
export interface BreakersData {
  routes: RouteBreakers[];
}

/** GET /mgmt/v1/stats → data */
export interface StatsData {
  version: number;
  failed_reloads: number;
  route_count: number;
  breaker_count: number;
  breaker_open_count: number;
}

/** POST /mgmt/v1/breakers/reset → 请求体 */
export interface ResetBreakerRequest {
  route_id: string;
  instance_id?: string | null;
}

/** POST /mgmt/v1/breakers/reset → data（未知路由/实例幂等返回 0） */
export interface ResetData {
  reset: number;
}
