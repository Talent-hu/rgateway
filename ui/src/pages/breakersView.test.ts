import { describe, expect, it } from 'vitest';
import type { BreakerEntry, RouteBreakers } from '../api/types';
import {
  breakerStateColor,
  breakerStateLabel,
  buildResetPayload,
  resetResultText,
  summarizeBreakers,
  toBreakerRows,
  type BreakerRow,
  type ResetPayload,
} from './breakersView';

describe('breakerStateColor - 状态到标签颜色映射', () => {
  const cases: Array<{
    name: string;
    input: string;
    expected: 'green' | 'orange' | 'red' | 'gray';
  }> = [
    { name: 'closed 映射绿色', input: 'closed', expected: 'green' },
    { name: 'half_open 映射橙色', input: 'half_open', expected: 'orange' },
    { name: 'open 映射红色', input: 'open', expected: 'red' },
    { name: '空串回退灰色', input: '', expected: 'gray' },
    { name: '大小写不符回退灰色', input: 'OPEN', expected: 'gray' },
    { name: '未知状态回退灰色', input: 'whatever', expected: 'gray' },
  ];

  it.each(cases)('$name', ({ input, expected }) => {
    expect(breakerStateColor(input)).toBe(expected);
  });
});

describe('breakerStateLabel - 状态到中文文案映射', () => {
  const cases: Array<{ name: string; input: string; expected: string }> = [
    { name: 'closed 显示闭合', input: 'closed', expected: '闭合' },
    { name: 'half_open 显示半开', input: 'half_open', expected: '半开' },
    { name: 'open 显示打开', input: 'open', expected: '打开' },
    { name: '未知状态显示未知', input: '', expected: '未知' },
    { name: '垃圾状态显示未知', input: 'xyz', expected: '未知' },
  ];

  it.each(cases)('$name', ({ input, expected }) => {
    expect(breakerStateLabel(input)).toBe(expected);
  });
});

describe('buildResetPayload - 重置请求体构造（边界校验快速失败）', () => {
  const cases: Array<{
    name: string;
    routeId: string;
    instanceId: string | null;
    expected: ResetPayload | null;
  }> = [
    {
      name: '实例级重置携带 instance_id',
      routeId: 'r1',
      instanceId: 'i1',
      expected: { route_id: 'r1', instance_id: 'i1' },
    },
    {
      name: '路由级全部重置显式 null',
      routeId: 'r1',
      instanceId: null,
      expected: { route_id: 'r1', instance_id: null },
    },
    {
      name: '两侧空白被裁剪',
      routeId: ' r1 ',
      instanceId: ' i1 ',
      expected: { route_id: 'r1', instance_id: 'i1' },
    },
    {
      name: '路由 id 空串非法',
      routeId: '',
      instanceId: 'i1',
      expected: null,
    },
    {
      name: '路由 id 纯空白非法',
      routeId: '   ',
      instanceId: null,
      expected: null,
    },
    {
      name: '实例 id 空串非法（与路由级语义区分）',
      routeId: 'r1',
      instanceId: '',
      expected: null,
    },
    {
      name: '实例 id 纯空白非法',
      routeId: 'r1',
      instanceId: '  ',
      expected: null,
    },
  ];

  it.each(cases)('$name', ({ routeId, instanceId, expected }) => {
    expect(buildResetPayload(routeId, instanceId)).toEqual(expected);
  });
});

describe('toBreakerRows - 熔断器表行数据', () => {
  const cases: Array<{
    name: string;
    input: BreakerEntry[];
    expected: BreakerRow[];
  }> = [
    {
      name: '空列表返回空数组',
      input: [],
      expected: [],
    },
    {
      name: '实例按 API 顺序映射，key 取实例 id',
      input: [
        { instance_id: 'i1', state: 'closed' },
        { instance_id: 'i2', state: 'open' },
      ],
      expected: [
        { key: 'i1', instance_id: 'i1', state: 'closed' },
        { key: 'i2', instance_id: 'i2', state: 'open' },
      ],
    },
    {
      name: '空/重复实例 id 生成唯一稳定 key（请求保留原始 id）',
      input: [
        { instance_id: '', state: 'closed' },
        { instance_id: 'i1', state: 'open' },
        { instance_id: ' i1 ', state: 'half_open' },
      ],
      expected: [
        { key: 'row-0', instance_id: '', state: 'closed' },
        { key: 'i1', instance_id: 'i1', state: 'open' },
        { key: 'i1#2', instance_id: ' i1 ', state: 'half_open' },
      ],
    },
  ];

  it.each(cases)('$name', ({ input, expected }) => {
    expect(toBreakerRows(input)).toEqual(expected);
  });
});

describe('resetResultText - 重置结果文案', () => {
  const cases: Array<{ name: string; input: number; expected: string }> = [
    { name: '多个实例被重置', input: 3, expected: '已重置 3 个熔断器' },
    { name: '单个实例被重置', input: 1, expected: '已重置 1 个熔断器' },
    {
      name: '零个实例给出未匹配文案',
      input: 0,
      expected: '未匹配到可重置的熔断器',
    },
    {
      name: '负数（异常输入）按零个处理',
      input: -1,
      expected: '未匹配到可重置的熔断器',
    },
  ];

  it.each(cases)('$name', ({ input, expected }) => {
    expect(resetResultText(input)).toBe(expected);
  });
});

describe('summarizeBreakers - 熔断器概要统计', () => {
  const cases: Array<{
    name: string;
    input: RouteBreakers[];
    expected: { routeCount: number; instanceCount: number; openCount: number };
  }> = [
    {
      name: '空列表全零',
      input: [],
      expected: { routeCount: 0, instanceCount: 0, openCount: 0 },
    },
    {
      name: '统计路由/实例/打开数',
      input: [
        {
          route_id: 'r1',
          breakers: [
            { instance_id: 'i1', state: 'closed' },
            { instance_id: 'i2', state: 'open' },
          ],
        },
        {
          route_id: 'r2',
          breakers: [
            { instance_id: 'i3', state: 'open' },
            { instance_id: 'i4', state: 'half_open' },
          ],
        },
      ],
      expected: { routeCount: 2, instanceCount: 4, openCount: 2 },
    },
    {
      name: '无实例路由也计入路由数',
      input: [{ route_id: 'r1', breakers: [] }],
      expected: { routeCount: 1, instanceCount: 0, openCount: 0 },
    },
    {
      name: '未知状态不计入打开数',
      input: [
        {
          route_id: 'r1',
          breakers: [{ instance_id: 'i1', state: 'weird' }],
        },
      ],
      expected: { routeCount: 1, instanceCount: 1, openCount: 0 },
    },
  ];

  it.each(cases)('$name', ({ input, expected }) => {
    expect(summarizeBreakers(input)).toEqual(expected);
  });
});
