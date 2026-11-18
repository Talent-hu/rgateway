/**
 * 管理面 REST 客户端：统一信封解析、边界防御式字段提取。
 *
 * 所有方法返回 `ApiResult<T>`：失败走 error 通道（泛化文案），
 * 网络/解析异常绝不抛出。
 */
import {
  asArray,
  asNumber,
  asRecord,
  asString,
  parseEnvelope,
  type ApiResult,
} from './envelope';
import type {
  BreakersData,
  HealthData,
  ResetBreakerRequest,
  ResetData,
  RouteBreakers,
  RoutesData,
  RouteSummary,
  SnapshotData,
  StatsData,
} from './types';

/** 网络异常的客户端文案 */
const NETWORK_ERROR = '无法连接管理面服务';
/** 请求/解析失败的客户端文案 */
const REQUEST_ERROR = '请求失败';
/** 请求参数无效的客户端文案 */
const INVALID_ARGUMENT = '请求参数无效';

/** 默认指标路径（服务端配置 observability.metrics.path 的默认值） */
export const DEFAULT_METRICS_PATH = '/metrics';

/** 客户端构造选项 */
export interface ApiClientOptions {
  /** 基础地址；默认空串（同源 + dev proxy） */
  baseUrl?: string;
  /** 自定义 fetch（便于测试注入） */
  fetchImpl?: typeof fetch;
  /** 指标路径，默认 /metrics */
  metricsPath?: string;
}

/** 判断是否为普通对象（用于过滤畸形数组元素） */
function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

/** 防御式提取路由摘要；非法元素返回 null（由调用方过滤） */
function toRouteSummary(raw: unknown): RouteSummary | null {
  if (!isRecord(raw)) {
    return null;
  }
  return {
    id: asString(raw['id']),
    order: asNumber(raw['order']),
    uri: asString(raw['uri']),
    protocol: asString(raw['protocol'], 'unknown'),
  };
}

/** 防御式提取单路由熔断状态 */
function toRouteBreakers(raw: unknown): RouteBreakers | null {
  if (!isRecord(raw)) {
    return null;
  }
  const breakers = asArray(raw['breakers'])
    .filter(isRecord)
    .map((b) => ({
      instance_id: asString(b['instance_id']),
      state: asString(b['state'], 'closed'),
    }));
  return { route_id: asString(raw['route_id']), breakers };
}

/** 管理面 REST 客户端 */
export class ApiClient {
  private readonly baseUrl: string;
  private readonly metricsPath: string;
  private readonly fetchImpl: typeof fetch;

  constructor(options: ApiClientOptions = {}) {
    this.baseUrl = options.baseUrl ?? '';
    this.metricsPath = options.metricsPath ?? DEFAULT_METRICS_PATH;
    this.fetchImpl =
      options.fetchImpl ?? ((input, init) => fetch(input, init));
  }

  /** GET /health */
  async health(): Promise<ApiResult<HealthData>> {
    const env = await this.getJson('/health');
    if (!env.ok) {
      return env;
    }
    const d = asRecord(env.data);
    return { ok: true, data: { status: asString(d['status'], 'unknown') } };
  }

  /** GET /mgmt/v1/snapshot */
  async snapshot(): Promise<ApiResult<SnapshotData>> {
    const env = await this.getJson('/mgmt/v1/snapshot');
    if (!env.ok) {
      return env;
    }
    const d = asRecord(env.data);
    const routes = asArray(d['routes'])
      .map(toRouteSummary)
      .filter((r): r is RouteSummary => r !== null);
    return {
      ok: true,
      data: {
        version: asNumber(d['version']),
        failed_reloads: asNumber(d['failed_reloads']),
        listen: asString(d['listen']),
        route_count: asNumber(d['route_count'], routes.length),
        routes,
      },
    };
  }

  /** GET /mgmt/v1/routes（按路由匹配顺序） */
  async routes(): Promise<ApiResult<RoutesData>> {
    const env = await this.getJson('/mgmt/v1/routes');
    if (!env.ok) {
      return env;
    }
    const d = asRecord(env.data);
    const routes = asArray(d['routes'])
      .map(toRouteSummary)
      .filter((r): r is RouteSummary => r !== null);
    return { ok: true, data: { routes } };
  }

  /** GET /mgmt/v1/breakers */
  async breakers(): Promise<ApiResult<BreakersData>> {
    const env = await this.getJson('/mgmt/v1/breakers');
    if (!env.ok) {
      return env;
    }
    const d = asRecord(env.data);
    const routes = asArray(d['routes'])
      .map(toRouteBreakers)
      .filter((r): r is RouteBreakers => r !== null);
    return { ok: true, data: { routes } };
  }

  /** GET /mgmt/v1/stats */
  async stats(): Promise<ApiResult<StatsData>> {
    const env = await this.getJson('/mgmt/v1/stats');
    if (!env.ok) {
      return env;
    }
    const d = asRecord(env.data);
    return {
      ok: true,
      data: {
        version: asNumber(d['version']),
        failed_reloads: asNumber(d['failed_reloads']),
        route_count: asNumber(d['route_count']),
        breaker_count: asNumber(d['breaker_count']),
        breaker_open_count: asNumber(d['breaker_open_count']),
      },
    };
  }

  /**
   * POST /mgmt/v1/breakers/reset（未知路由/实例幂等返回 reset: 0）。
   * 请求体恒含 instance_id：字符串表示实例级重置，
   * null 表示路由级全部重置（与服务端「缺省或 null」语义一致）。
   * 「传了 instance_id 但为空白」是非法输入——绝不静默放大为路由级
   * 全量重置（重置是写操作，范围放大是 footgun）。
   */
  async resetBreakers(req: ResetBreakerRequest): Promise<ApiResult<ResetData>> {
    const routeId = asString(req.route_id).trim();
    if (routeId === '') {
      return { ok: false, error: INVALID_ARGUMENT };
    }
    const rawInstanceId = req.instance_id;
    const instanceId = asString(rawInstanceId).trim();
    if (rawInstanceId != null && instanceId === '') {
      return { ok: false, error: INVALID_ARGUMENT };
    }
    const body: Record<string, unknown> = {
      route_id: routeId,
      instance_id: instanceId !== '' ? instanceId : null,
    };
    const env = await this.postJson('/mgmt/v1/breakers/reset', body);
    if (!env.ok) {
      return env;
    }
    const d = asRecord(env.data);
    return { ok: true, data: { reset: asNumber(d['reset']) } };
  }

  /** GET /metrics：返回 Prometheus 原始文本（非信封） */
  async metrics(): Promise<ApiResult<string>> {
    try {
      const res = await this.fetchImpl(this.baseUrl + this.metricsPath, {
        method: 'GET',
      });
      if (!res.ok) {
        return { ok: false, error: REQUEST_ERROR };
      }
      return { ok: true, data: await res.text() };
    } catch (err) {
      // 网络/运行时细节只进控制台，客户端只拿到泛化文案
      console.error('mgmt metrics request failed:', err);
      return { ok: false, error: NETWORK_ERROR };
    }
  }

  /** GET + JSON 信封解析 */
  private async getJson(path: string): Promise<ApiResult<unknown>> {
    return this.requestJson(path, { method: 'GET' });
  }

  /** POST + JSON 信封解析 */
  private async postJson(
    path: string,
    body: unknown
  ): Promise<ApiResult<unknown>> {
    return this.requestJson(path, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    });
  }

  /** 发送请求并解析信封；所有异常收敛为 error 通道 */
  private async requestJson(
    path: string,
    init: RequestInit
  ): Promise<ApiResult<unknown>> {
    let res: Response;
    try {
      res = await this.fetchImpl(this.baseUrl + path, init);
    } catch (err) {
      console.error('mgmt request failed:', err);
      return { ok: false, error: NETWORK_ERROR };
    }
    if (!res.ok) {
      return { ok: false, error: REQUEST_ERROR };
    }
    try {
      return parseEnvelope(await res.json());
    } catch (err) {
      console.error('mgmt response parse failed:', err);
      return { ok: false, error: REQUEST_ERROR };
    }
  }
}
