/**
 * 概览页数据轮询钩子：每 5 秒拉取 /mgmt/v1/stats 与 /metrics，
 * 合并规则与速率差分收在 `overviewState.ts` / `aggregate.ts`（纯函数、
 * 表驱动测试），本钩子只负责取数与三道守卫——在途去重、卸载取消、
 * 两路数据成败解耦。
 */
import { useEffect, useRef, useState } from 'react';
import { ApiClient } from '../api/client';
import type { RateSample } from '../metrics/aggregate';
import {
  foldTail,
  statusDistribution,
  sumCounter,
  totalActiveRequests,
  REQUESTS_TOTAL_METRIC,
} from '../metrics/aggregate';
import { parsePrometheusText } from '../metrics/parse';
import { initialState, reduceTick } from './overviewState';
import type { OverviewState, TickInput } from './overviewState';

export type { OverviewState } from './overviewState';
export { initialState } from './overviewState';

/** 轮询间隔（毫秒） */
export const POLL_INTERVAL_MS = 5000;
/** 状态码分布最大扇区数（超出折叠为「其他」） */
const MAX_SLICES = 6;

/**
 * 概览页数据轮询钩子。
 * 计数器首拍只建立基线不产点；计数器重置（变小）的拍被跳过；
 * 慢请求不叠拍（在途去重），组件卸载后不再 setState。
 */
export function useOverviewData(): OverviewState {
  const [state, setState] = useState<OverviewState>(initialState);
  const stateRef = useRef<OverviewState>(state);
  const baselineRef = useRef<RateSample | null>(null);
  const inFlightRef = useRef(false);
  const clientRef = useRef<ApiClient | null>(null);
  if (clientRef.current === null) {
    clientRef.current = new ApiClient();
  }

  useEffect(() => {
    const client = clientRef.current;
    if (client === null) {
      return undefined;
    }
    let cancelled = false;

    /** 拉取一拍：stats + metrics 并行，各自成败（解耦见 reduceTick） */
    const tick = async (): Promise<void> => {
      if (inFlightRef.current) {
        return; // 在途去重：慢请求不叠拍，避免乱序/重复速率点
      }
      inFlightRef.current = true;
      try {
        const [statsRes, metricsRes] = await Promise.all([
          client.stats(),
          client.metrics(),
        ]);
        if (cancelled) {
          return;
        }
        const error = !statsRes.ok
          ? statsRes.error
          : !metricsRes.ok
            ? metricsRes.error
            : null;
        let metrics: TickInput['metrics'] = null;
        if (metricsRes.ok) {
          const samples = parsePrometheusText(metricsRes.data);
          metrics = {
            total: sumCounter(samples, REQUESTS_TOTAL_METRIC),
            active: totalActiveRequests(samples),
            slices: foldTail(statusDistribution(samples), MAX_SLICES),
          };
        }
        const advanced = reduceTick(
          stateRef.current,
          {
            stats: statsRes.ok ? statsRes.data : null,
            metrics,
            error,
            sampledAt: Date.now(),
          },
          baselineRef.current
        );
        baselineRef.current = advanced.baseline;
        stateRef.current = advanced.state;
        setState(advanced.state);
      } finally {
        inFlightRef.current = false;
      }
    };

    void tick();
    const timer = setInterval(() => {
      void tick();
    }, POLL_INTERVAL_MS);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, []);

  return state;
}
