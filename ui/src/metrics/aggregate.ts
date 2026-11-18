import type { MetricSample } from './parse';

/**
 * 指标聚合辅助：为概览页提供计数器求和、速率差分与状态码分布。
 * 纯函数，便于表驱动测试。
 */

/** 状态码分布条目 */
export interface StatusCount {
  status: string;
  count: number;
}

/** 请求总数计数器指标名 */
export const REQUESTS_TOTAL_METRIC = 'rgateway_requests_total';
/** 活跃请求 gauge 指标名 */
export const ACTIVE_REQUESTS_METRIC = 'rgateway_active_requests';
/** 状态分布折叠用的「其他」标签 */
export const OTHER_STATUS = '其他';

/**
 * 汇总指定计数器指标的所有序列值（忽略非有限值）。
 *
 * @param samples 解析后的指标样本
 * @param metric 计数器指标名
 */
export function sumCounter(samples: MetricSample[], metric: string): number {
  let total = 0;
  for (const s of samples) {
    if (s.name === metric && Number.isFinite(s.value)) {
      total += s.value;
    }
  }
  return total;
}

/** 读取 rgateway_active_requests gauge 的当前值；缺失或非有限返回 0 */
export function totalActiveRequests(samples: MetricSample[]): number {
  for (const s of samples) {
    if (s.name === ACTIVE_REQUESTS_METRIC && Number.isFinite(s.value)) {
      return s.value;
    }
  }
  return 0;
}

/**
 * 计数器增量差分：(当前 - 上一拍) / 间隔秒 = 每秒速率。
 * 首拍无基线、计数器重置（变小）、非法间隔均返回 null（跳过该点）。
 *
 * 已知局限（朴素差分固有，不作修复）：进程重启后若一个采样间隔内计数器
 * 爬回超过旧值（如 100 → 重启 → 150），差分会把 50 当增量、算出偏低的
 * 速率——没有 scrape 时间戳/元数据则原理上不可判。
 */
export function computeRate(
  prev: number | null,
  curr: number,
  intervalSeconds: number
): number | null {
  if (prev === null || !Number.isFinite(prev) || !Number.isFinite(curr)) {
    return null;
  }
  if (!Number.isFinite(intervalSeconds) || intervalSeconds <= 0) {
    return null;
  }
  if (curr < prev) {
    return null;
  }
  return (curr - prev) / intervalSeconds;
}

/** 速率采样基线：上一拍的计数器值与采样时间戳（毫秒）。 */
export interface RateSample {
  total: number;
  atMs: number;
}

/**
 * 时间戳基线推进：用**真实 elapsed** 做差分分母（浏览器后台节流、慢响应
 * 下按固定间隔算会等比例虚高速率），并维护基线。首拍、计数器重启、
 * elapsed ≤ 0（含时钟回拨）只建基线不产点。
 */
export function advanceRate(
  prev: RateSample | null,
  total: number,
  atMs: number
): { point: number | null; baseline: RateSample } {
  const baseline: RateSample = { total, atMs };
  if (prev === null) {
    return { point: null, baseline };
  }
  const elapsedSeconds = (atMs - prev.atMs) / 1000;
  if (elapsedSeconds <= 0) {
    return { point: null, baseline };
  }
  return {
    point: computeRate(prev.total, total, elapsedSeconds),
    baseline,
  };
}

/**
 * 按 status 标签聚合 rgateway_requests_total，
 * 缺 status 标签归入 unknown，结果按状态码升序。
 */
export function statusDistribution(samples: MetricSample[]): StatusCount[] {
  const counts = new Map<string, number>();
  for (const s of samples) {
    if (s.name !== REQUESTS_TOTAL_METRIC || !Number.isFinite(s.value)) {
      continue;
    }
    const status = s.labels['status'] ?? 'unknown';
    counts.set(status, (counts.get(status) ?? 0) + s.value);
  }
  return [...counts.entries()]
    .map(([status, count]) => ({ status, count }))
    .sort((a, b) => (a.status < b.status ? -1 : a.status > b.status ? 1 : 0));
}

/**
 * 尾部折叠：条目超过 limit 时保留前 max(1, limit-1) 条，
 * 其余聚合成「其他」；不超过上限则原样返回（新数组）。
 */
export function foldTail(items: StatusCount[], limit: number): StatusCount[] {
  if (items.length <= limit) {
    return [...items];
  }
  const keep = Math.max(1, limit - 1);
  const head = items.slice(0, keep);
  let tailCount = 0;
  for (const item of items.slice(keep)) {
    tailCount += item.count;
  }
  return [...head, { status: OTHER_STATUS, count: tailCount }];
}
