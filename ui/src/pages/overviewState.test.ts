/**
 * 概览页状态合并（reduceTick）测试：stats 与 metrics 成败解耦（HIGH 回归）、
 * 速率基线推进、计数器重启跳点——轮询编排的纯逻辑全部在此锁定。
 */
import { describe, expect, it } from 'vitest';
import { initialState, reduceTick } from './overviewState';
import type { OverviewState, TickInput } from './overviewState';
import type { RateSample } from '../metrics/aggregate';
import type { StatsData } from '../api/types';

const T0 = 1_700_000_000_000;

const stats = (routeCount: number): StatsData => ({
  version: 1,
  failed_reloads: 0,
  route_count: routeCount,
  breaker_count: 0,
  breaker_open_count: 0,
});

const metricsTick = (total: number, active = 0): TickInput['metrics'] => ({
  total,
  active,
  slices: [],
});

describe('reduceTick - 概览一拍合并（stats 与 metrics 成败解耦）', () => {
  it('HIGH 回归：metrics 失败不得丢弃已成功拉取的 stats', () => {
    const { state, baseline } = reduceTick(
      initialState(),
      { stats: stats(3), metrics: null, error: '请求失败', sampledAt: T0 },
      null
    );
    expect(state.stats).toEqual(stats(3));
    expect(state.error).toBe('请求失败');
    expect(state.loading).toBe(false);
    expect(state.rateSeries).toEqual([]);
    expect(state.totalRequests).toBe(0);
    expect(baseline).toBeNull();
  });

  it('stats 失败保留旧值，metrics 成功照常更新', () => {
    const prev: OverviewState = { ...initialState(), stats: stats(9), loading: false };
    const { state, baseline } = reduceTick(
      prev,
      { stats: null, metrics: metricsTick(50, 2), error: '请求失败', sampledAt: T0 },
      null
    );
    expect(state.stats).toEqual(stats(9));
    expect(state.totalRequests).toBe(50);
    expect(state.activeRequests).toBe(2);
    expect(state.error).toBe('请求失败');
    expect(state.rateSeries).toEqual([]);
    expect(baseline).toEqual({ total: 50, atMs: T0 });
  });

  it('第二拍按真实 elapsed 产出速率点', () => {
    const first = reduceTick(
      initialState(),
      { stats: stats(1), metrics: metricsTick(10), error: null, sampledAt: T0 },
      null
    );
    const second = reduceTick(
      first.state,
      { stats: stats(1), metrics: metricsTick(20), error: null, sampledAt: T0 + 5000 },
      first.baseline
    );
    expect(second.state.rateSeries).toEqual([{ time: T0 + 5000, rps: 2 }]);
    expect(second.baseline).toEqual({ total: 20, atMs: T0 + 5000 });
  });

  it('计数器重启不产点、基线刷新', () => {
    const first = reduceTick(
      initialState(),
      { stats: stats(1), metrics: metricsTick(100), error: null, sampledAt: T0 },
      null
    );
    const second = reduceTick(
      first.state,
      { stats: stats(1), metrics: metricsTick(4), error: null, sampledAt: T0 + 5000 },
      first.baseline
    );
    expect(second.state.rateSeries).toEqual([]);
    expect(second.state.totalRequests).toBe(4);
    expect(second.baseline).toEqual({ total: 4, atMs: T0 + 5000 });
  });

  it('双失败：error 落盘、既有数据全保留', () => {
    const prev: OverviewState = {
      ...initialState(),
      stats: stats(9),
      totalRequests: 77,
      loading: false,
    };
    const { state, baseline } = reduceTick(
      prev,
      { stats: null, metrics: null, error: '请求失败', sampledAt: T0 },
      null
    );
    expect(state.stats).toEqual(stats(9));
    expect(state.totalRequests).toBe(77);
    expect(state.error).toBe('请求失败');
    expect(baseline).toBeNull();
  });

  it('成功拍自动清除历史错误', () => {
    const prev: OverviewState = {
      ...initialState(),
      error: '请求失败',
      loading: false,
    };
    const { state } = reduceTick(
      prev,
      { stats: stats(1), metrics: metricsTick(1), error: null, sampledAt: T0 },
      null
    );
    expect(state.error).toBeNull();
    expect(state.loading).toBe(false);
  });
});

// RateSample 类型贯穿 baseline 流转，锁定其形状防止重构漂移。
const _baselineShape: RateSample = { total: 1, atMs: T0 };
void _baselineShape;
