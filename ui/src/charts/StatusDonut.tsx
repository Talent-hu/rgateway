/**
 * 状态码分布环图：按 status 标签聚合的 part-to-whole。
 *
 * dataviz 规范落地：
 * - 分类色按固定槽位顺序取（不轮换、不生成新色）
 * - 扇区之间留 2px 表面色间隙（stroke=SURFACE）
 * - 图例常在 + 表格视图承载精确数值（对比度 WARN 槽位的补偿手段）
 * - 悬停提示给出 名称 / 数量 / 占比；文字一律使用文本令牌
 */
import { Cell, Legend, Pie, PieChart, ResponsiveContainer, Tooltip } from 'recharts';
import {
  SURFACE,
  TEXT_PRIMARY,
  TEXT_SECONDARY,
  seriesColor,
} from './chartTheme';

/** 状态分布条目（已在上游完成尾部折叠） */
export interface StatusSlice {
  status: string;
  count: number;
}

interface StatusDonutProps {
  /** 折叠后的状态分布 */
  data: StatusSlice[];
}

interface TooltipEntry {
  name?: string;
  value?: number | string;
  payload?: StatusSlice;
}

/** 自定义悬停提示：名称 / 数量 / 占比 */
function DonutTooltip(props: {
  active?: boolean;
  payload?: TooltipEntry[];
}): JSX.Element | null {
  const { active, payload } = props;
  const first = payload?.[0];
  if (!active || first === undefined) {
    return null;
  }
  const total = payload?.reduce(
    (acc, p) => acc + (typeof p.value === 'number' ? p.value : 0),
    0
  );
  const value = typeof first.value === 'number' ? first.value : 0;
  const share = total !== undefined && total > 0 ? ((value / total) * 100).toFixed(1) : '0.0';
  return (
    <div
      style={{
        background: SURFACE,
        border: '1px solid rgba(11,11,11,0.10)',
        borderRadius: 4,
        padding: '6px 10px',
        fontSize: 12,
        color: TEXT_PRIMARY,
      }}
    >
      <div style={{ color: TEXT_SECONDARY }}>{first.name}</div>
      <div style={{ fontVariantNumeric: 'tabular-nums' }}>
        {value} 次 · {share}%
      </div>
    </div>
  );
}

/** 自定义图例：色点 + 名称（文字用文本令牌，身份由色点承担） */
function StatusLegend(props: {
  payload?: Array<{ value?: string; color?: string }>;
}): JSX.Element | null {
  const { payload } = props;
  if (payload === undefined || payload.length === 0) {
    return null;
  }
  return (
    <ul
      style={{
        listStyle: 'none',
        margin: 0,
        padding: 0,
        display: 'flex',
        flexWrap: 'wrap',
        gap: '4px 12px',
      }}
    >
      {payload.map((entry) => (
        <li
          key={entry.value}
          style={{
            display: 'flex',
            alignItems: 'center',
            gap: 6,
            fontSize: 12,
            color: TEXT_SECONDARY,
          }}
        >
          <span
            style={{
              width: 8,
              height: 8,
              borderRadius: 4,
              background: entry.color,
              display: 'inline-block',
            }}
          />
          {entry.value}
        </li>
      ))}
    </ul>
  );
}

/** 状态码分布环图组件 */
export function StatusDonut({ data }: StatusDonutProps): JSX.Element {
  const total = data.reduce((acc, s) => acc + s.count, 0);
  return (
    <div style={{ display: 'flex', gap: 16, alignItems: 'center', flexWrap: 'wrap' }}>
      <div style={{ flex: '1 1 220px', minWidth: 220 }}>
        <ResponsiveContainer width="100%" height={220}>
          <PieChart>
            <Pie
              data={data}
              dataKey="count"
              nameKey="status"
              innerRadius="60%"
              outerRadius="85%"
              paddingAngle={0}
              stroke={SURFACE}
              strokeWidth={2}
              isAnimationActive={false}
            >
              {data.map((slice, index) => (
                <Cell key={slice.status} fill={seriesColor(index)} />
              ))}
            </Pie>
            <Tooltip content={<DonutTooltip />} />
            <Legend content={<StatusLegend />} />
          </PieChart>
        </ResponsiveContainer>
      </div>
      {/* 表格视图：精确数值（同时是低对比度色槽的补偿通道） */}
      <table
        style={{
          flex: '1 1 180px',
          minWidth: 180,
          borderCollapse: 'collapse',
          fontSize: 12,
        }}
      >
        <thead>
          <tr style={{ color: TEXT_SECONDARY, textAlign: 'left' }}>
            <th style={{ padding: '4px 8px', fontWeight: 500 }}>状态码</th>
            <th style={{ padding: '4px 8px', fontWeight: 500, textAlign: 'right' }}>请求数</th>
            <th style={{ padding: '4px 8px', fontWeight: 500, textAlign: 'right' }}>占比</th>
          </tr>
        </thead>
        <tbody>
          {data.map((slice, index) => (
            <tr key={slice.status} style={{ color: TEXT_PRIMARY }}>
              <td style={{ padding: '4px 8px' }}>
                <span
                  style={{
                    width: 8,
                    height: 8,
                    borderRadius: 4,
                    background: seriesColor(index),
                    display: 'inline-block',
                    marginRight: 6,
                  }}
                />
                {slice.status}
              </td>
              <td style={{ padding: '4px 8px', textAlign: 'right', fontVariantNumeric: 'tabular-nums' }}>
                {slice.count}
              </td>
              <td style={{ padding: '4px 8px', textAlign: 'right', fontVariantNumeric: 'tabular-nums' }}>
                {total > 0 ? ((slice.count / total) * 100).toFixed(1) : '0.0'}%
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
