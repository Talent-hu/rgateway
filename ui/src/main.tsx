/**
 * 应用入口：挂载 React 根节点并引入全局样式。
 */
import '@arco-design/web-react/dist/css/arco.css';
import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { App } from './App';
import './index.css';

const container = document.getElementById('root');
if (container === null) {
  // 挂载点缺失属于页面结构错误，直接失败并给出清晰提示
  throw new Error('挂载点 #root 不存在');
}
createRoot(container).render(
  <StrictMode>
    <App />
  </StrictMode>
);
