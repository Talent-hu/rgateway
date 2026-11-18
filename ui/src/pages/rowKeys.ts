/**
 * 表格行稳定 key 生成：供路由页与熔断器页的行数据转换共用。
 * 纯函数，保证 key 在空 id / 重复 id 下仍然唯一，便于表驱动测试。
 */

/**
 * 为表格行生成稳定且唯一的 key：
 * - 空/空白 id 回退 `row-{下标}`；
 * - 重复 id 仅首个保留原值，其余追加 `#{下标}` 后缀；
 * - 非字符串输入按空 id 处理（防御式，不信任外部数据）。
 *
 * @param ids 原始 id 列表（与行数据一一对应）
 */
export function toKeys(ids: string[]): string[] {
  const seen = new Map<string, number>();
  return ids.map((raw, index) => {
    const id = typeof raw === 'string' ? raw.trim() : '';
    if (id === '') {
      return `row-${index}`;
    }
    const count = seen.get(id) ?? 0;
    seen.set(id, count + 1);
    return count === 0 ? id : `${id}#${index}`;
  });
}
