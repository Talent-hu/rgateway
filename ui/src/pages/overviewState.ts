/**
 * 概览页状态合并（纯 reducer）：把一拍拉取结果合并进状态。
 *
 * 与拉取编排解耦的意义：合并规则（尤其是 stats 与 metrics 成败互不牵连）
 * 是最容易写错的部分，收成纯函数后可表驱动锁定，轮询钩子只剩取数与守卫。
 */
import type { StatsData } from '../api/types';
import type { RateSample } from '../metrics/aggregate';
import { advanceRate } from '../metrics/aggregate';
import type { RatePoint } from '../charts/RequestRateChart';
import type { StatusSlice } from '../charts/StatusDonut';

/** 速率图滚动窗口最大点数 */
export const MAX_POINTS = 60;

/** 概览页数据状态 */
export interface OverviewState {
  /** 基础统计（来自 /mgmt/v1/stats） */
  stats: StatsData | null;
  /** 当前活跃请求（gauge） */
  activeRequests: number;
  /** 请求总数（计数器全序列求和） */
  totalRequests: number;
  /** 请求速率时间序列（滚动窗口） */
  rateSeries: RatePoint[];
  /** 状态码分布（已折叠） */
  statusSlices: StatusSlice[];
  /** 面向客户端的错误文案（成功后自动清除） */
  error: string | null;
  /** 首次加载中 */
  loading: boolean;
}

/** 一拍拉取的派生结果：两个数据源独立成败，各自为 null 表示拉取失败。 */
export interface TickInput {
  /** /mgmt/v1/stats 成功时的载荷（失败 null） */
  stats: StatsData | null;
  /** /metrics 成功时的派生值（失败 null） */
  metrics: {
    total: number;
    active: number;
    slices: StatusSlice[];
  } | null;
  /** 面向客户端的错误文案（全部成功为 null） */
  error: string | null;
  /** 本拍采样时间戳（毫秒） */
  sampledAt: number;
}

/** 创建概览页状态（全零初始值） */
export function initialState(): OverviewState {
  return {
    stats: null,
    activeRequests: 0,
    totalRequests: 0,
    rateSeries: [],
    statusSlices: [],
    error: null,
    loading: true,
  };
}

/**
 * 合并一拍结果（纯函数）：
 * - stats 与 metrics **成败解耦**——任一成功即落盘其数据，另一路失败绝不
 *   丢弃已成功的一路（否则 /metrics 404 时 KPI 会永远冻结在初值）；
 * - 速率点由 [`advanceRate`] 按真实 elapsed 产出，基线随返回值流转；
 * - 任一成功即结束 loading；`error` 取本拍错误文案（成功拍清空）。
 */
export function reduceTick(
  prev: OverviewState,
  input: TickInput,
  baseline: RateSample | null
): { state: OverviewState; baseline: RateSample | null } {
  const stats = input.stats ?? prev.stats;
  const loading = false;
  if (input.metrics === null) {
    return {
      state: { ...prev, stats, error: input.error, loading },
      baseline,
    };
  }
  const { total, active, slices } = input.metrics;
  const advanced = advanceRate(baseline, total, input.sampledAt);
  const rateSeries =
    advanced.point === null
      ? prev.rateSeries
      : [...prev.rateSeries, { time: input.sampledAt, rps: advanced.point }].slice(
          -MAX_POINTS
        );
  return {
    state: {
      ...prev,
      stats,
      activeRequests: active,
      totalRequests: total,
      rateSeries,
      statusSlices: slices,
      error: input.error,
      loading,
    },
    baseline: advanced.baseline,
  };
}
