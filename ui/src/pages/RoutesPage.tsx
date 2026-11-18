/**
 * 路由页：Arco Table 展示 /mgmt/v1/routes（按路由匹配顺序）。
 * 支持 10 秒自动刷新与手动刷新；请求失败时保留上次数据并展示泛化错误。
 */
import {
  Alert,
  Button,
  Card,
  Table,
  type TableColumnProps,
} from '@arco-design/web-react';
import { IconRefresh } from '@arco-design/web-react/icon';
import { useCallback, useEffect, useRef, useState } from 'react';
import { ApiClient } from '../api/client';
import { toRouteRows, type RouteRow } from './routesView';

/** 自动刷新间隔（毫秒） */
export const POLL_INTERVAL_MS = 10000;
/** 自动刷新提示文案 */
const POLL_HINT = `每 ${POLL_INTERVAL_MS / 1000} 秒自动刷新`;

/** 路由表列定义（id / order / uri / protocol） */
const COLUMNS: TableColumnProps<RouteRow>[] = [
  { title: '路由 ID', dataIndex: 'id' },
  { title: '匹配顺序', dataIndex: 'order' },
  { title: 'URI', dataIndex: 'uri' },
  { title: '协议', dataIndex: 'protocol' },
];

/** 路由页组件 */
export function RoutesPage(): JSX.Element {
  const [rows, setRows] = useState<RouteRow[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const clientRef = useRef<ApiClient | null>(null);
  const mountedRef = useRef(true);
  if (clientRef.current === null) {
    clientRef.current = new ApiClient();
  }

  /** 拉取一次路由列表；失败保留上次数据并记录面向客户端的泛化错误。
   *  卸载后不再 setState（mountedRef 守卫）。 */
  const load = useCallback(async (): Promise<void> => {
    const client = clientRef.current;
    if (client === null) {
      return;
    }
    const res = await client.routes();
    if (!mountedRef.current) {
      return;
    }
    if (res.ok) {
      setRows(toRouteRows(res.data.routes));
      setError(null);
    } else {
      setError(res.error);
    }
    setLoading(false);
  }, []);

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

  return (
    <div>
      {error !== null ? (
        <Alert type="error" content={error} style={{ marginBottom: 16 }} />
      ) : null}
      <div
        style={{ color: 'var(--color-text-3)', marginBottom: 12, fontSize: 12 }}
      >
        {POLL_HINT}
      </div>
      <Card
        title="路由列表"
        loading={loading}
        extra={
          <Button size="small" icon={<IconRefresh />} onClick={() => void load()}>
            刷新
          </Button>
        }
      >
        <Table
          columns={COLUMNS}
          data={rows}
          rowKey="key"
          pagination={false}
          noDataElement={<span>暂无路由</span>}
        />
      </Card>
    </div>
  );
}
