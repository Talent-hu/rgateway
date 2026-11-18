/**
 * 路由页展示逻辑：把 API 路由摘要转换为表格行数据。
 * 纯函数模块，便于表驱动测试。
 */
import type { RouteSummary } from '../api/types';
import { toKeys } from './rowKeys';

/** 路由表行数据（Table 数据源，key 为稳定行标识） */
export interface RouteRow {
  /** 稳定唯一行 key */
  key: string;
  /** 路由 ID */
  id: string;
  /** 匹配顺序 */
  order: number;
  /** 匹配 URI */
  uri: string;
  /** 协议 */
  protocol: string;
}

/** 协议字段空白时的回退值（与 API 客户端默认值一致） */
const UNKNOWN_PROTOCOL = 'unknown';

/**
 * 将路由摘要列表转换为表格行：
 * - 保持 API 返回的路由匹配顺序，不做重排；
 * - 生成稳定唯一行 key（空/重复 id 不冲突）；
 * - 字段裁剪空白；order 非有限归 0；协议空白回退 unknown。
 *
 * @param routes 路由摘要列表
 */
export function toRouteRows(routes: RouteSummary[]): RouteRow[] {
  const keys = toKeys(routes.map((r) => r.id));
  return routes.map((route, index) => {
    const protocol = route.protocol.trim();
    return {
      key: keys[index],
      id: route.id.trim(),
      order: Number.isFinite(route.order) ? route.order : 0,
      uri: route.uri.trim(),
      protocol: protocol !== '' ? protocol : UNKNOWN_PROTOCOL,
    };
  });
}
