/**
 * 全局布局：Arco Layout + 侧边菜单（概览/路由/熔断器/系统）+ react-router。
 */
import { Layout, Menu } from '@arco-design/web-react';
import {
  IconDashboard,
  IconList,
  IconSettings,
  IconThunderbolt,
} from '@arco-design/web-react/icon';
import type { ReactNode } from 'react';
import { useLocation, useNavigate } from 'react-router-dom';

/** 菜单项定义：key 即路由路径 */
const MENU_ITEMS = [
  { key: '/', label: '概览', icon: <IconDashboard /> },
  { key: '/routes', label: '路由', icon: <IconList /> },
  { key: '/breakers', label: '熔断器', icon: <IconThunderbolt /> },
  { key: '/system', label: '系统', icon: <IconSettings /> },
] as const;

interface AppLayoutProps {
  /** 页面内容 */
  children: ReactNode;
}

/** 全局布局组件 */
export function AppLayout({ children }: AppLayoutProps): JSX.Element {
  const navigate = useNavigate();
  const location = useLocation();

  return (
    <Layout style={{ minHeight: '100vh' }}>
      <Layout.Sider width={200} collapsible breakpoint="xl">
        <div
          style={{
            height: 48,
            display: 'flex',
            alignItems: 'center',
            justifyContent: 'center',
            color: 'var(--color-text-1)',
            fontWeight: 600,
            fontSize: 16,
          }}
        >
          rgateway 管理面板
        </div>
        <Menu
          selectedKeys={[location.pathname]}
          onClickMenuItem={(key) => navigate(key)}
        >
          {MENU_ITEMS.map((item) => (
            <Menu.Item key={item.key}>
              {item.icon}
              {item.label}
            </Menu.Item>
          ))}
        </Menu>
      </Layout.Sider>
      <Layout>
        <Layout.Content
          style={{ padding: 20, background: 'var(--color-bg-1)', overflow: 'auto' }}
        >
          {children}
        </Layout.Content>
      </Layout>
    </Layout>
  );
}
