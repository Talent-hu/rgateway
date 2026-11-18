/**
 * 应用根组件：react-router 路由表 + 全局布局。
 * 页面：概览 / 路由 / 熔断器 / 系统。
 */
import { BrowserRouter, Navigate, Route, Routes } from 'react-router-dom';
import { AppLayout } from './layout/AppLayout';
import { BreakersPage } from './pages/BreakersPage';
import { OverviewPage } from './pages/OverviewPage';
import { RoutesPage } from './pages/RoutesPage';
import { SystemPage } from './pages/SystemPage';

/** 应用根组件 */
export function App(): JSX.Element {
  return (
    <BrowserRouter>
      <AppLayout>
        <Routes>
          <Route path="/" element={<OverviewPage />} />
          <Route path="/routes" element={<RoutesPage />} />
          <Route path="/breakers" element={<BreakersPage />} />
          <Route path="/system" element={<SystemPage />} />
          <Route path="*" element={<Navigate to="/" replace />} />
        </Routes>
      </AppLayout>
    </BrowserRouter>
  );
}
