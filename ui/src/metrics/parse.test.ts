import { describe, expect, it } from 'vitest';
import { parsePrometheusText, type MetricSample } from './parse';

describe('parsePrometheusText - 表驱动', () => {
  const cases: Array<{
    name: string;
    input: string;
    expected: MetricSample[];
  }> = [
    {
      name: '空文本返回空数组',
      input: '',
      expected: [],
    },
    {
      name: '只含注释与空行返回空数组',
      input: '# HELP rgateway_up up\n# TYPE rgateway_up gauge\n\n',
      expected: [],
    },
    {
      name: '无标签样本',
      input: 'rgateway_active_requests 3\n',
      expected: [{ name: 'rgateway_active_requests', labels: {}, value: 3 }],
    },
    {
      name: '带标签样本',
      input: 'rgateway_requests_total{route="/api",method="GET",status="200"} 42\n',
      expected: [
        {
          name: 'rgateway_requests_total',
          labels: { route: '/api', method: 'GET', status: '200' },
          value: 42,
        },
      ],
    },
    {
      name: '多行混合：注释、多标签、多序列',
      input: [
        '# HELP rgateway_requests_total 请求总数',
        '# TYPE rgateway_requests_total counter',
        'rgateway_requests_total{route="/api",method="GET",status="200"} 42',
        'rgateway_requests_total{route="/api",method="GET",status="500"} 1',
        'rgateway_upstream_errors_total{route="/api",kind="connect"} 2',
        '',
      ].join('\n'),
      expected: [
        {
          name: 'rgateway_requests_total',
          labels: { route: '/api', method: 'GET', status: '200' },
          value: 42,
        },
        {
          name: 'rgateway_requests_total',
          labels: { route: '/api', method: 'GET', status: '500' },
          value: 1,
        },
        {
          name: 'rgateway_upstream_errors_total',
          labels: { route: '/api', kind: 'connect' },
          value: 2,
        },
      ],
    },
    {
      name: 'histogram 桶行照常解析',
      input:
        'rgateway_request_duration_seconds_bucket{route="/api",method="GET",le="0.1"} 7\n',
      expected: [
        {
          name: 'rgateway_request_duration_seconds_bucket',
          labels: { route: '/api', method: 'GET', le: '0.1' },
          value: 7,
        },
      ],
    },
    {
      name: '浮点与科学计数值',
      input: 'rgateway_x 1.5\nrgateway_y 2e3\nrgateway_z -0.25\n',
      expected: [
        { name: 'rgateway_x', labels: {}, value: 1.5 },
        { name: 'rgateway_y', labels: {}, value: 2000 },
        { name: 'rgateway_z', labels: {}, value: -0.25 },
      ],
    },
    {
      name: '特殊数值 +Inf/-Inf/NaN',
      input: 'm_a +Inf\nm_b -Inf\nm_c NaN\n',
      expected: [
        { name: 'm_a', labels: {}, value: Number.POSITIVE_INFINITY },
        { name: 'm_b', labels: {}, value: Number.NEGATIVE_INFINITY },
        { name: 'm_c', labels: {}, value: Number.NaN },
      ],
    },
    {
      name: '标签值含转义引号与反斜杠',
      input: 'm{path="a\\"b",dir="c\\\\d"} 1\n',
      expected: [{ name: 'm', labels: { path: 'a"b', dir: 'c\\d' }, value: 1 }],
    },
    {
      name: '标签值转义 \\n 还原为换行',
      input: 'm{note="a\\nb"} 1\n',
      expected: [{ name: 'm', labels: { note: 'a\nb' }, value: 1 }],
    },
    {
      name: '非法转义（\\q）的行按畸形跳过',
      input: 'm{note="a\\qb"} 1\nm_ok{x="y"} 2\n',
      expected: [{ name: 'm_ok', labels: { x: 'y' }, value: 2 }],
    },
    {
      name: '标签值含逗号与空格',
      input: 'm{note="a, b"} 1\n',
      expected: [{ name: 'm', labels: { note: 'a, b' }, value: 1 }],
    },
    {
      name: '容错：畸形行被跳过，合法行照常解析',
      input: '这是垃圾行\nm{broken 1\nm_ok{a="b"} 2\nm_bad{} 3\n',
      expected: [
        { name: 'm_ok', labels: { a: 'b' }, value: 2 },
        { name: 'm_bad', labels: {}, value: 3 },
      ],
    },
    {
      name: '容错：缺值的行被跳过',
      input: 'm_no_value{a="b"}\nm_ok 5\n',
      expected: [{ name: 'm_ok', labels: {}, value: 5 }],
    },
    {
      name: '时间戳尾部被忽略',
      input: 'm{a="b"} 5 1700000000000\n',
      expected: [{ name: 'm', labels: { a: 'b' }, value: 5 }],
    },
  ];

  it.each(cases)('$name', ({ input, expected }) => {
    const got = parsePrometheusText(input);
    expect(got).toHaveLength(expected.length);
    for (let i = 0; i < expected.length; i += 1) {
      expect(got[i].name).toBe(expected[i].name);
      expect(got[i].labels).toEqual(expected[i].labels);
      if (Number.isNaN(expected[i].value)) {
        expect(got[i].value).toBeNaN();
      } else {
        expect(got[i].value).toBe(expected[i].value);
      }
    }
  });
});
