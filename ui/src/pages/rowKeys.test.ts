import { describe, expect, it } from 'vitest';
import { toKeys } from './rowKeys';

describe('toKeys - 表格行稳定 key 生成', () => {
  const cases: Array<{ name: string; input: string[]; expected: string[] }> = [
    { name: '空数组返回空数组', input: [], expected: [] },
    {
      name: '普通 id 原样作为 key',
      input: ['a', 'b'],
      expected: ['a', 'b'],
    },
    {
      name: 'id 两侧空白被裁剪',
      input: [' a ', ' b'],
      expected: ['a', 'b'],
    },
    {
      name: '空 id 回退 row-下标',
      input: ['', 'x', '   '],
      expected: ['row-0', 'x', 'row-2'],
    },
    {
      name: '重复 id 仅首个保留原值',
      input: ['a', 'a', 'a'],
      expected: ['a', 'a#1', 'a#2'],
    },
    {
      name: '裁剪后同名视为重复',
      input: ['a', ' a '],
      expected: ['a', 'a#1'],
    },
    {
      name: '非字符串输入按空 id 处理',
      input: [undefined as unknown as string, 'b'],
      expected: ['row-0', 'b'],
    },
  ];

  it.each(cases)('$name', ({ input, expected }) => {
    expect(toKeys(input)).toEqual(expected);
  });
});
