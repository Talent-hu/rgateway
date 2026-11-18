import { describe, expect, it } from 'vitest';
import {
  asArray,
  asNumber,
  asRecord,
  asString,
  parseEnvelope,
} from './envelope';

describe('asNumber - 边界防御式取数', () => {
  const cases: Array<{ name: string; input: unknown; expected: number }> = [
    { name: '合法数字原样返回', input: 42, expected: 42 },
    { name: '数字字符串转换', input: '17', expected: 17 },
    { name: 'null 回退 0', input: null, expected: 0 },
    { name: 'undefined 回退 0', input: undefined, expected: 0 },
    { name: '布尔回退 0', input: true, expected: 0 },
    { name: '非数字字符串回退 0', input: 'abc', expected: 0 },
    { name: '对象回退 0', input: {}, expected: 0 },
    { name: 'NaN 回退 0', input: Number.NaN, expected: 0 },
    { name: '浮点保留', input: 1.5, expected: 1.5 },
    { name: '负数保留', input: -3, expected: -3 },
  ];

  it.each(cases)('$name', ({ input, expected }) => {
    expect(asNumber(input)).toBe(expected);
  });
});

describe('asString - 边界防御式取串', () => {
  const cases: Array<{
    name: string;
    input: unknown;
    fallback: string;
    expected: string;
  }> = [
    { name: '合法字符串原样返回', input: 'ok', fallback: '', expected: 'ok' },
    { name: '空串原样返回', input: '', fallback: '', expected: '' },
    { name: 'null 回退空串', input: null, fallback: '', expected: '' },
    { name: '数字回退空串', input: 5, fallback: '', expected: '' },
    { name: '对象回退空串', input: {}, fallback: '', expected: '' },
    { name: '可指定回退值', input: 5, fallback: '未知', expected: '未知' },
  ];

  it.each(cases)('$name', ({ input, fallback, expected }) => {
    expect(asString(input, fallback)).toBe(expected);
  });
});

describe('asArray / asRecord - 防御式容器', () => {
  it('非数组回退空数组', () => {
    expect(asArray(null)).toEqual([]);
    expect(asArray({})).toEqual([]);
    expect(asArray([1, 2])).toEqual([1, 2]);
  });

  it('非对象回退空对象', () => {
    expect(asRecord(null)).toEqual({});
    expect(asRecord([1])).toEqual({});
    expect(asRecord({ a: 1 })).toEqual({ a: 1 });
  });
});

describe('parseEnvelope - 统一信封解析', () => {
  const cases: Array<{
    name: string;
    input: unknown;
    expectOk: boolean;
    expectError?: string;
  }> = [
    {
      name: '合法成功信封',
      input: { success: true, data: { v: 1 }, error: null },
      expectOk: true,
    },
    {
      name: '失败信封透出服务端泛化文案',
      input: { success: false, data: null, error: 'invalid request' },
      expectOk: false,
      expectError: 'invalid request',
    },
    {
      name: '失败信封缺 error 文案时回退通用文案',
      input: { success: false, data: null, error: null },
      expectOk: false,
      expectError: '请求失败',
    },
    {
      name: 'success 非布尔视为失败',
      input: { success: 'yes', data: {}, error: null },
      expectOk: false,
      expectError: '请求失败',
    },
    {
      name: '非对象视为失败',
      input: 'oops',
      expectOk: false,
      expectError: '请求失败',
    },
    {
      name: 'null 视为失败',
      input: null,
      expectOk: false,
      expectError: '请求失败',
    },
    {
      name: '缺 success 字段视为失败',
      input: { data: {}, error: null },
      expectOk: false,
      expectError: '请求失败',
    },
  ];

  it.each(cases)('$name', ({ input, expectOk, expectError }) => {
    const result = parseEnvelope(input);
    expect(result.ok).toBe(expectOk);
    if (expectOk) {
      expect(result.ok && result.data).toEqual((input as { data: unknown }).data);
    } else {
      expect(!result.ok && result.error).toBe(expectError);
    }
  });
});
