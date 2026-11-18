/**
 * 熔断器页：按路由分组展示各实例熔断状态（closed 绿 / half_open 橙 / open 红），
 * 支持实例级「重置」与路由级「全部重置」（Popconfirm 确认），成功后自动刷新。
 * 数据每 10 秒自动刷新，另有手动刷新按钮；失败仅展示泛化文案。
 */
import {
  Alert,
  Button,
  Card,
  Empty,
  Message,
  Popconfirm,
  Spin,
  Table,
  Tag,
  type TableColumnProps,
} from '@arco-design/web-react';
import { IconRefresh } from '@arco-design/web-react/icon';
import { useCallback, useEffect, useRef, useState } from 'react';
import { ApiClient } from '../api/client';
import type { RouteBreakers } from '../api/types';
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
import { toKeys } from './rowKeys';

/** 自动刷新间隔（毫秒） */
export const POLL_INTERVAL_MS = 10000;
/** 自动刷新提示文案 */
const POLL_HINT = `每 ${POLL_INTERVAL_MS / 1000} 秒自动刷新`;
/** 重置参数非法的客户端文案 */
const INVALID_ARGUMENT = '请求参数无效';
/** 未命名路由/实例的占位文案 */
const UNNAMED = '（未命名）';

/** 重置回调：入参为已校验的请求体，null 表示非法参数 */
type ResetHandler = (payload: ResetPayload | null) => Promise<void>;

/**
 * 构造熔断器表列定义（实例 ID / 状态标签 / 重置操作）。
 *
 * @param routeId 所属路由 ID（构造重置请求体用）
 * @param onReset 重置回调
 */
function breakerColumns(
  routeId: string,
  onReset: ResetHandler
): TableColumnProps<BreakerRow>[] {
  return [
    { title: '实例 ID', dataIndex: 'instance_id' },
    {
      title: '状态',
      dataIndex: 'state',
      render: (_: unknown, row: BreakerRow) => (
        <Tag color={breakerStateColor(row.state)}>
          {breakerStateLabel(row.state)}
        </Tag>
      ),
    },
    {
      title: '操作',
      render: (_: unknown, row: BreakerRow) => (
        <Popconfirm
          title={`确认重置实例 ${row.instance_id || UNNAMED} 的熔断器？`}
          okText="重置"
          cancelText="取消"
          onOk={() => onReset(buildResetPayload(routeId, row.instance_id))}
        >
          <Button size="small" type="text" status="danger">
            重置
          </Button>
        </Popconfirm>
      ),
    },
  ];
}

/** 熔断器页组件 */
export function BreakersPage(): JSX.Element {
  const [routes, setRoutes] = useState<RouteBreakers[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const clientRef = useRef<ApiClient | null>(null);
  const mountedRef = useRef(true);
  if (clientRef.current === null) {
    clientRef.current = new ApiClient();
  }

  /** 拉取一次熔断状态；失败保留上次数据并记录泛化错误。
   *  卸载后不再 setState（mountedRef 守卫，覆盖轮询与重置回调）。 */
  const load = useCallback(async (): Promise<void> => {
    const client = clientRef.current;
    if (client === null) {
      return;
    }
    const res = await client.breakers();
    if (!mountedRef.current) {
      return;
    }
    if (res.ok) {
      setRoutes(res.data.routes);
      setError(null);
    } else {
      setError(res.error);
    }
    setLoading(false);
  }, []);

  /** 执行一次熔断重置：参数非法或失败时 Message 报错，成功后刷新列表 */
  const handleReset = useCallback<ResetHandler>(
    async (payload) => {
      if (payload === null) {
        Message.error(INVALID_ARGUMENT);
        return;
      }
      const client = clientRef.current;
      if (client === null) {
        return;
      }
      const res = await client.resetBreakers(payload);
      if (!res.ok) {
        Message.error(res.error);
        return;
      }
      const text = resetResultText(res.data.reset);
      if (res.data.reset > 0) {
        Message.success(text);
      } else {
        Message.warning(text);
      }
      await load();
    },
    [load]
  );

  useEffect(() => {
    void load();
    const timer = setInterval(() => {
      void load();
    }, POLL_INTERVAL_MS);
    return () => {
      mountedRef.current = false;
      clearInterval(timer);
    };
  }, [load]);

  const summary = summarizeBreakers(routes);
  const groupKeys = toKeys(routes.map((g) => g.route_id));

  return (
    <div>
      {error !== null ? (
        <Alert type="error" content={error} style={{ marginBottom: 16 }} />
      ) : null}
      <div
        style={{
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'space-between',
          marginBottom: 12,
        }}
      >
        <span style={{ color: 'var(--color-text-3)', fontSize: 12 }}>
          {POLL_HINT} · 共 {summary.routeCount} 条路由 · {summary.instanceCount}{' '}
          个实例 · {summary.openCount} 个打开
        </span>
        <Button size="small" icon={<IconRefresh />} onClick={() => void load()}>
          刷新
        </Button>
      </div>
      <Spin loading={loading} style={{ display: 'block' }}>
        {routes.length === 0 && !loading ? (
          <Empty description="暂无熔断器数据" />
        ) : null}
        {routes.map((group, index) => (
          <Card
            key={groupKeys[index]}
            style={{ marginBottom: 16 }}
            title={`路由 ${group.route_id || UNNAMED}（${
              group.breakers.length
            } 个实例）`}
            extra={
              <Popconfirm
                title={`确认重置路由 ${
                  group.route_id || UNNAMED
                } 的全部熔断器？`}
                okText="重置"
                cancelText="取消"
                onOk={() => handleReset(buildResetPayload(group.route_id, null))}
              >
                <Button size="small" status="danger">
                  全部重置
                </Button>
              </Popconfirm>
            }
          >
            <Table
              columns={breakerColumns(group.route_id, handleReset)}
              data={toBreakerRows(group.breakers)}
              rowKey="key"
              pagination={false}
              noDataElement={<span>暂无熔断器实例</span>}
            />
          </Card>
        ))}
      </Spin>
    </div>
  );
}
