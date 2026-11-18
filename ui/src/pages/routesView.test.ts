import { describe, expect, it } from 'vitest';
import type { RouteSummary } from '../api/types';
import { toRouteRows, type RouteRow } from './routesView';

describe('toRouteRows - 路由表行数据', () => {
  const cases: Array<{
    name: string;
    input: RouteSummary[];
    expected: RouteRow[];
  }> = [
    {
      name: '空列表返回空数组',
      input: [],
      expected: [],
    },
    {
      name: '保持 API 匹配顺序原样映射',
      input: [
        { id: 'a', order: 2, uri: '/x', protocol: 'http' },
        { id: 'b', order: 1, uri: '/y', protocol: 'grpc' },
      ],
      expected: [
        { key: 'a', id: 'a', order: 2, uri: '/x', protocol: 'http' },
        { key: 'b', id: 'b', order: 1, uri: '/y', protocol: 'grpc' },
      ],
    },
    {
      name: '空/重复 id 生成唯一稳定 key',
      input: [
        { id: '', order: 0, uri: '/a', protocol: 'http' },
        { id: '  ', order: 1, uri: '/b', protocol: 'http' },
        { id: 'r', order: 2, uri: '/c', protocol: 'http' },
        { id: 'r', order: 3, uri: '/d', protocol: 'http' },
      ],
      expected: [
        { key: 'row-0', id: '', order: 0, uri: '/a', protocol: 'http' },
        { key: 'row-1', id: '', order: 1, uri: '/b', protocol: 'http' },
        { key: 'r', id: 'r', order: 2, uri: '/c', protocol: 'http' },
        { key: 'r#3', id: 'r', order: 3, uri: '/d', protocol: 'http' },
      ],
    },
    {
      name: '非有限 order 归 0',
      input: [{ id: 'a', order: Number.NaN, uri: '/x', protocol: 'http' }],
      expected: [{ key: 'a', id: 'a', order: 0, uri: '/x', protocol: 'http' }],
    },
    {
      name: 'id/uri 两侧空白裁剪，协议空白回退 unknown',
      input: [{ id: ' a ', order: 1, uri: ' /x ', protocol: '  ' }],
      expected: [
        { key: 'a', id: 'a', order: 1, uri: '/x', protocol: 'unknown' },
      ],
    },
  ];

  it.each(cases)('$name', ({ input, expected }) => {
    expect(toRouteRows(input)).toEqual(expected);
  });
});
