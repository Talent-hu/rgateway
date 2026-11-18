import { describe, expect, it } from 'vitest';
import {
  advanceRate,
  computeRate,
  foldTail,
  statusDistribution,
  sumCounter,
  totalActiveRequests,
} from './aggregate';
import type { MetricSample } from './parse';

const sample = (
  name: string,
  labels: Record<string, string>,
  value: number
): MetricSample => ({ name, labels, value });

describe('sumCounter - 汇总计数器所有序列', () => {
  const cases: Array<{
    name: string;
    samples: MetricSample[];
    metric: string;
    expected: number;
  }> = [
    {
      name: '无匹配序列返回 0',
      samples: [sample('other', {}, 9)],
      metric: 'rgateway_requests_total',
      expected: 0,
    },
    {
      name: '多序列求和',
      samples: [
        sample('rgateway_requests_total', { status: '200' }, 10),
        sample('rgateway_requests_total', { status: '500' }, 5),
        sample('other', {}, 99),
      ],
      metric: 'rgateway_requests_total',
      expected: 15,
    },
    {
      name: '计数器为 0 时返回 0',
      samples: [sample('rgateway_requests_total', {}, 0)],
      metric: 'rgateway_requests_total',
      expected: 0,
    },
  ];

  it.each(cases)('$name', ({ samples, metric, expected }) => {
    expect(sumCounter(samples, metric)).toBe(expected);
  });
});

describe('totalActiveRequests - 活跃请求 gauge', () => {
  it('取 rgateway_active_requests 的当前值', () => {
    const samples = [
      sample('rgateway_active_requests', {}, 7),
      sample('rgateway_requests_total', {}, 100),
    ];
    expect(totalActiveRequests(samples)).toBe(7);
  });

  it('缺失时返回 0', () => {
    expect(totalActiveRequests([])).toBe(0);
  });
});

describe('computeRate - 计数器增量差分', () => {
  const cases: Array<{
    name: string;
    prev: number | null;
    curr: number;
    intervalSeconds: number;
    expected: number | null;
  }> = [
    {
      name: '首拍无基线返回 null',
      prev: null,
      curr: 100,
      intervalSeconds: 5,
      expected: null,
    },
    {
      name: '正常差分：增量/间隔 = rps',
      prev: 100,
      curr: 130,
      intervalSeconds: 5,
      expected: 6,
    },
    {
      name: '零增量返回 0',
      prev: 100,
      curr: 100,
      intervalSeconds: 5,
      expected: 0,
    },
    {
      name: '计数器重置（变小）返回 null',
      prev: 100,
      curr: 10,
      intervalSeconds: 5,
      expected: null,
    },
    {
      name: '非法间隔返回 null',
      prev: 10,
      curr: 20,
      intervalSeconds: 0,
      expected: null,
    },
    {
      name: '负间隔返回 null',
      prev: 10,
      curr: 20,
      intervalSeconds: -5,
      expected: null,
    },
  ];

  it.each(cases)('$name', ({ prev, curr, intervalSeconds, expected }) => {
    expect(computeRate(prev, curr, intervalSeconds)).toBe(expected);
  });
});

describe('advanceRate - 时间戳基线推进（真实 elapsed 分母）', () => {
  const at = (offsetMs: number) => 1_700_000_000_000 + offsetMs;
  const cases: Array<{
    name: string;
    prev: { total: number; atMs: number } | null;
    total: number;
    atMs: number;
    expectedPoint: number | null;
  }> = [
    {
      name: '首拍只建基线不产点',
      prev: null,
      total: 10,
      atMs: at(0),
      expectedPoint: null,
    },
    {
      name: '常规 5 秒差分',
      prev: { total: 10, atMs: at(0) },
      total: 20,
      atMs: at(5000),
      expectedPoint: 2,
    },
    {
      name: '节流 60 秒按真实 elapsed 分母（不虚高）',
      prev: { total: 10, atMs: at(0) },
      total: 80,
      atMs: at(60_000),
      expectedPoint: 70 / 60,
    },
    {
      name: '计数器重启归零只建基线',
      prev: { total: 100, atMs: at(0) },
      total: 4,
      atMs: at(5000),
      expectedPoint: null,
    },
    {
      name: 'elapsed 为 0 防除零不产点',
      prev: { total: 10, atMs: at(5000) },
      total: 20,
      atMs: at(5000),
      expectedPoint: null,
    },
    {
      name: '时钟回拨不产点',
      prev: { total: 10, atMs: at(5000) },
      total: 20,
      atMs: at(1000),
      expectedPoint: null,
    },
    {
      name: '重启后一拍内反超按差分算（朴素差分固有局限）',
      prev: { total: 100, atMs: at(0) },
      total: 150,
      atMs: at(5000),
      expectedPoint: 10,
    },
  ];

  it.each(cases)(
    '$name',
    ({ prev, total, atMs, expectedPoint }) => {
      const result = advanceRate(prev, total, atMs);
      if (expectedPoint === null) {
        expect(result.point).toBeNull();
      } else {
        expect(result.point ?? Number.NaN).toBeCloseTo(expectedPoint, 6);
      }
      expect(result.baseline).toEqual({ total, atMs });
    }
  );
});

describe('statusDistribution - 按 status 标签聚合', () => {
  const cases: Array<{
    name: string;
    samples: MetricSample[];
    expected: Array<{ status: string; count: number }>;
  }> = [
    {
      name: '空样本返回空数组',
      samples: [],
      expected: [],
    },
    {
      name: '按 status 求和并按状态码升序',
      samples: [
        sample('rgateway_requests_total', { status: '500' }, 1),
        sample('rgateway_requests_total', { status: '200' }, 10),
        sample('rgateway_requests_total', { status: '200' }, 5),
        sample('rgateway_requests_total', { status: '404' }, 2),
      ],
      expected: [
        { status: '200', count: 15 },
        { status: '404', count: 2 },
        { status: '500', count: 1 },
      ],
    },
    {
      name: '缺 status 标签归入 unknown',
      samples: [sample('rgateway_requests_total', {}, 3)],
      expected: [{ status: 'unknown', count: 3 }],
    },
    {
      name: '忽略非目标指标',
      samples: [
        sample('rgateway_requests_total', { status: '200' }, 4),
        sample('rgateway_upstream_errors_total', { status: '200' }, 9),
      ],
      expected: [{ status: '200', count: 4 }],
    },
  ];

  it.each(cases)('$name', ({ samples, expected }) => {
    expect(statusDistribution(samples)).toEqual(expected);
  });
});

describe('foldTail - 尾部折叠为「其他」', () => {
  const cases: Array<{
    name: string;
    items: Array<{ status: string; count: number }>;
    limit: number;
    expected: Array<{ status: string; count: number }>;
  }> = [
    {
      name: '不超过上限原样返回',
      items: [
        { status: '200', count: 3 },
        { status: '404', count: 1 },
      ],
      limit: 6,
      expected: [
        { status: '200', count: 3 },
        { status: '404', count: 1 },
      ],
    },
    {
      name: '超出上限折叠尾部',
      items: [
        { status: '200', count: 10 },
        { status: '301', count: 8 },
        { status: '404', count: 6 },
        { status: '500', count: 4 },
        { status: '502', count: 2 },
        { status: '503', count: 1 },
        { status: '504', count: 1 },
      ],
      limit: 6,
      expected: [
        { status: '200', count: 10 },
        { status: '301', count: 8 },
        { status: '404', count: 6 },
        { status: '500', count: 4 },
        { status: '502', count: 2 },
        { status: '其他', count: 2 },
      ],
    },
    {
      name: '上限为 1 时只留「其他」聚合？否：保留第一名并折叠其余',
      items: [
        { status: '200', count: 5 },
        { status: '500', count: 2 },
      ],
      limit: 1,
      expected: [
        { status: '200', count: 5 },
        { status: '其他', count: 2 },
      ],
    },
    {
      name: '空数组返回空数组',
      items: [],
      limit: 6,
      expected: [],
    },
  ];

  it.each(cases)('$name', ({ items, limit, expected }) => {
    expect(foldTail(items, limit)).toEqual(expected);
  });
});
