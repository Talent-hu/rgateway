/**
 * 请求速率折线图：单序列趋势（面积为 10% 色调淡washes，2px 线）。
 * 单序列不需要图例（标题已说明内容）；十字准线 + 悬停提示默认开启。
 */
import {
  Area,
  AreaChart,
  CartesianGrid,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
} from 'recharts';
import {
  AXIS,
  GRID,
  MUTED,
  SERIES,
  SURFACE,
  TEXT_PRIMARY,
  TEXT_SECONDARY,
  formatTime,
} from './chartTheme';

/** 单个速率采样点 */
export interface RatePoint {
  /** 采样时间戳（毫秒） */
  time: number;
  /** 每秒请求数 */
  rps: number;
}

interface RequestRateChartProps {
  /** 滚动窗口内的速率点 */
  data: RatePoint[];
}

interface TooltipEntry {
  value?: number | string;
  payload?: RatePoint;
}

/** 自定义悬停提示：文字用文本令牌，颜色仅作标记点缀 */
function RateTooltip(props: {
  active?: boolean;
  payload?: TooltipEntry[];
}): JSX.Element | null {
  const { active, payload } = props;
  const first = payload?.[0];
  if (!active || first === undefined) {
    return null;
  }
  const ts = first.payload?.time ?? 0;
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
      <div style={{ color: TEXT_SECONDARY }}>{formatTime(ts)}</div>
      <div style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
        <span
          style={{
            width: 8,
            height: 8,
            borderRadius: 4,
            background: SERIES[0],
            display: 'inline-block',
          }}
        />
        <span>请求速率：</span>
        <span style={{ fontVariantNumeric: 'tabular-nums' }}>
          {typeof first.value === 'number' ? first.value.toFixed(2) : first.value} rps
        </span>
      </div>
    </div>
  );
}

/** 请求速率折线图组件 */
export function RequestRateChart({ data }: RequestRateChartProps): JSX.Element {
  return (
    <ResponsiveContainer width="100%" height={260}>
      <AreaChart data={data} margin={{ top: 12, right: 16, bottom: 4, left: 0 }}>
        <CartesianGrid stroke={GRID} vertical={false} />
        <XAxis
          dataKey="time"
          type="number"
          domain={['dataMin', 'dataMax']}
          tickFormatter={formatTime}
          stroke={AXIS}
          tick={{ fill: MUTED, fontSize: 12 }}
          tickLine={{ stroke: AXIS }}
        />
        <YAxis
          stroke={AXIS}
          tick={{ fill: MUTED, fontSize: 12 }}
          tickLine={{ stroke: AXIS }}
          tickFormatter={(v: number) => String(Math.round(v * 100) / 100)}
          width={48}
        />
        <Tooltip
          content={<RateTooltip />}
          cursor={{ stroke: AXIS, strokeWidth: 1 }}
        />
        <Area
          type="monotone"
          dataKey="rps"
          name="请求速率"
          stroke={SERIES[0]}
          strokeWidth={2}
          fill={SERIES[0]}
          fillOpacity={0.1}
          dot={false}
          activeDot={{ r: 4, strokeWidth: 2, stroke: SURFACE, fill: SERIES[0] }}
          isAnimationActive={false}
        />
      </AreaChart>
    </ResponsiveContainer>
  );
}
