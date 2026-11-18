/**
 * 概览页：KPI 统计卡片 + 请求速率折线图 + 状态码分布环图。
 * 数据每 5 秒轮询一次（速率图为计数器增量差分）。
 */
import { Alert, Card, Grid, Spin, Statistic } from '@arco-design/web-react';
import { RequestRateChart } from '../charts/RequestRateChart';
import { StatusDonut } from '../charts/StatusDonut';
import { POLL_INTERVAL_MS, useOverviewData } from './useOverviewData';

/** 轮询提示文案 */
const POLL_HINT = `每 ${POLL_INTERVAL_MS / 1000} 秒自动刷新`;

/** 概览页组件 */
export function OverviewPage(): JSX.Element {
  const {
    stats,
    activeRequests,
    totalRequests,
    rateSeries,
    statusSlices,
    error,
    loading,
  } = useOverviewData();

  return (
    <Spin loading={loading} style={{ display: 'block' }}>
      {error !== null ? (
        <Alert type="error" content={error} style={{ marginBottom: 16 }} />
      ) : null}
      <div style={{ color: 'var(--color-text-3)', marginBottom: 12, fontSize: 12 }}>
        {POLL_HINT}
      </div>
      <Grid.Row gutter={16}>
        <Grid.Col span={6} xs={12} sm={12} md={6}>
          <Card>
            <Statistic title="路由数" value={stats?.route_count ?? 0} />
          </Card>
        </Grid.Col>
        <Grid.Col span={6} xs={12} sm={12} md={6}>
          <Card>
            <Statistic title="打开的熔断器" value={stats?.breaker_open_count ?? 0} />
          </Card>
        </Grid.Col>
        <Grid.Col span={6} xs={12} sm={12} md={6}>
          <Card>
            <Statistic title="当前活跃请求" value={activeRequests} />
          </Card>
        </Grid.Col>
        <Grid.Col span={6} xs={12} sm={12} md={6}>
          <Card>
            <Statistic title="总请求数" value={totalRequests} />
          </Card>
        </Grid.Col>
      </Grid.Row>
      <Grid.Row gutter={16} style={{ marginTop: 16 }}>
        <Grid.Col span={14} xs={24} sm={24} md={14}>
          <Card title="请求速率（每秒）">
            <RequestRateChart data={rateSeries} />
          </Card>
        </Grid.Col>
        <Grid.Col span={10} xs={24} sm={24} md={10}>
          <Card title="状态码分布">
            <StatusDonut data={statusSlices} />
          </Card>
        </Grid.Col>
      </Grid.Row>
    </Spin>
  );
}
