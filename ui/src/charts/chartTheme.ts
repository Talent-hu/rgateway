/**
 * 图表设计令牌（来自 dataviz 参考调色板，已通过 validate_palette.js 六项检查：
 * 亮度带 / 彩度下限 / CVD 相邻分离 / 正常视觉下限 / 对比度）。
 *
 * 配色按「实体」固定取槽，绝不按排名轮换；
 * 对比度 WARN 槽位（aqua/yellow/magenta）由图例 + 表格视图承担补偿（relief rule）。
 */

/** 分类色槽（固定顺序，最多 6 槽，超出折叠为「其他」） */
export const SERIES = [
  '#2a78d6', // 1 蓝
  '#eb6834', // 2 橙
  '#1baf7a', // 3 青绿
  '#eda100', // 4 黄
  '#e87ba4', // 5 品红
  '#008300', // 6 绿
] as const;

/** 图表表面色（标记间 2px 间隙 / 圆点表面环用） */
export const SURFACE = '#fcfcfb';
/** 页面底色 */
export const PAGE_PLANE = '#f9f9f7';
/** 网格线（发丝线、实线、退让） */
export const GRID = '#e1e0d9';
/** 基线 / 坐标轴 */
export const AXIS = '#c3c2b7';
/** 轴刻度文字（哑色） */
export const MUTED = '#898781';
/** 主文字 */
export const TEXT_PRIMARY = '#0b0b0b';
/** 次级文字 */
export const TEXT_SECONDARY = '#52514e';

/** 按槽位取分类色；越界回退到第一槽 */
export function seriesColor(index: number): string {
  return SERIES[index] ?? SERIES[0];
}

/** 格式化时间戳为 HH:mm:ss（图表横轴用） */
export function formatTime(ts: number): string {
  const d = new Date(ts);
  const pad = (n: number): string => String(n).padStart(2, '0');
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}
