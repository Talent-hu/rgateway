/**
 * 系统页：snapshot 信息（version / failed_reloads / listen）+ 路由表。
 */
import { Alert, Card, Descriptions, Table } from '@arco-design/web-react';
import { useEffect, useState } from 'react';
import { ApiClient } from '../api/client';
import type { RouteSummary, SnapshotData } from '../api/types';

/** 路由表列定义 */
const ROUTE_COLUMNS = [
  { title: '路由 ID', dataIndex: 'id' },
  { title: '匹配顺序', dataIndex: 'order' },
  { title: 'URI', dataIndex: 'uri' },
  { title: '协议', dataIndex: 'protocol' },
];

/** 系统页组件 */
export function SystemPage(): JSX.Element {
  const [snapshot, setSnapshot] = useState<SnapshotData | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    let cancelled = false;
    const client = new ApiClient();
    void client.snapshot().then((res) => {
      if (cancelled) {
        return;
      }
      if (res.ok) {
        setSnapshot(res.data);
        setError(null);
      } else {
        setError(res.error);
      }
      setLoading(false);
    });
    return () => {
      cancelled = true;
    };
  }, []);

  const routes: RouteSummary[] = snapshot?.routes ?? [];

  return (
    <div>
      {error !== null ? (
        <Alert type="error" content={error} style={{ marginBottom: 16 }} />
      ) : null}
      <Card title="系统信息" loading={loading}>
        <Descriptions
          column={2}
          border
          data={[
            { label: '版本', value: String(snapshot?.version ?? '-') },
            { label: '重载失败次数', value: String(snapshot?.failed_reloads ?? '-') },
            { label: '数据面监听', value: snapshot?.listen || '-' },
            { label: '路由数', value: String(snapshot?.route_count ?? '-') },
          ]}
        />
      </Card>
      <Card title="路由表" loading={loading} style={{ marginTop: 16 }}>
        <Table
          columns={ROUTE_COLUMNS}
          data={routes}
          rowKey="id"
          pagination={false}
          noDataElement={<span>暂无路由</span>}
        />
      </Card>
    </div>
  );
}
