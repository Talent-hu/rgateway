/**
 * 熔断器页展示逻辑：状态到颜色/文案映射、表格行数据、
 * 重置请求体构造（边界校验快速失败）与概要统计。
 * 纯函数模块，便于表驱动测试。
 */
import type { BreakerEntry, RouteBreakers } from '../api/types';
import { toKeys } from './rowKeys';

/** 熔断状态标签颜色（Arco Tag 预设色） */
export type TagColor = 'green' | 'orange' | 'red' | 'gray';

/** 熔断器表格行数据（Table 数据源，key 为稳定行标识） */
export interface BreakerRow {
  /** 稳定唯一行 key */
  key: string;
  /** 实例 ID */
  instance_id: string;
  /** 熔断状态（closed / open / half_open） */
  state: string;
}

/** 重置请求体（instance_id 为 null 表示路由级全部重置） */
export interface ResetPayload {
  /** 路由 ID */
  route_id: string;
  /** 实例 ID；null 表示重置该路由全部实例 */
  instance_id: string | null;
}

/** 熔断器概要统计 */
export interface BreakerSummary {
  /** 路由数 */
  routeCount: number;
  /** 实例数 */
  instanceCount: number;
  /** 处于 open 状态的实例数 */
  openCount: number;
}

/** 状态到 Tag 颜色映射：closed 绿 / half_open 橙 / open 红 */
const STATE_COLORS: Readonly<Record<string, TagColor>> = {
  closed: 'green',
  half_open: 'orange',
  open: 'red',
};

/** 状态到中文文案映射 */
const STATE_LABELS: Readonly<Record<string, string>> = {
  closed: '闭合',
  half_open: '半开',
  open: '打开',
};

/** 未知状态的回退颜色 */
const UNKNOWN_COLOR: TagColor = 'gray';
/** 未知状态的回退文案 */
const UNKNOWN_LABEL = '未知';

/**
 * 熔断状态 → Arco Tag 颜色；未知状态回退灰色。
 *
 * @param state 熔断状态（closed / open / half_open）
 */
export function breakerStateColor(state: string): TagColor {
  return STATE_COLORS[state] ?? UNKNOWN_COLOR;
}

/**
 * 熔断状态 → 中文文案；未知状态回退「未知」。
 *
 * @param state 熔断状态（closed / open / half_open）
 */
export function breakerStateLabel(state: string): string {
  return STATE_LABELS[state] ?? UNKNOWN_LABEL;
}

/**
 * 构造熔断重置请求体，边界校验快速失败：
 * - route_id 空/空白 → null（非法）；
 * - instanceId 为 null → 路由级全部重置（显式 null）；
 * - instanceId 空/空白 → null（非法，与路由级语义区分）。
 *
 * @param routeId 路由 ID
 * @param instanceId 实例 ID；null 表示路由级全部重置
 */
export function buildResetPayload(
  routeId: string,
  instanceId: string | null
): ResetPayload | null {
  const route = routeId.trim();
  if (route === '') {
    return null;
  }
  if (instanceId === null) {
    return { route_id: route, instance_id: null };
  }
  const instance = instanceId.trim();
  if (instance === '') {
    return null;
  }
  return { route_id: route, instance_id: instance };
}

/**
 * 将实例熔断状态列表转换为表格行：
 * - 保持 API 返回顺序；生成稳定唯一行 key；
 * - `instance_id` **保留原始值**（重置请求要与服务端精确匹配），
 *   展示列自行格式化，不覆写标识本身。
 *
 * @param breakers 实例熔断状态列表
 */
export function toBreakerRows(breakers: BreakerEntry[]): BreakerRow[] {
  const keys = toKeys(breakers.map((b) => b.instance_id));
  return breakers.map((entry, index) => ({
    key: keys[index],
    instance_id: entry.instance_id,
    state: entry.state,
  }));
}

/**
 * 重置结果文案：reset > 0 报告数量，否则提示未匹配（幂等返回 0）。
 *
 * @param reset 服务端返回的重置数量
 */
export function resetResultText(reset: number): string {
  return reset > 0
    ? `已重置 ${reset} 个熔断器`
    : '未匹配到可重置的熔断器';
}

/**
 * 熔断器概要统计：路由数 / 实例数 / open 状态实例数。
 *
 * @param routes 按路由分组的熔断状态列表
 */
export function summarizeBreakers(routes: RouteBreakers[]): BreakerSummary {
  let instanceCount = 0;
  let openCount = 0;
  for (const route of routes) {
    for (const breaker of route.breakers) {
      instanceCount += 1;
      if (breaker.state === 'open') {
        openCount += 1;
      }
    }
  }
  return { routeCount: routes.length, instanceCount, openCount };
}
