/// <reference types="vitest/config" />
import react from '@vitejs/plugin-react';
import { defineConfig } from 'vitest/config';

// 管理面默认监听 127.0.0.1:9090，开发态统一走代理转发
const MGMT_ORIGIN = 'http://127.0.0.1:9090';

export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173,
    proxy: {
      '/health': { target: MGMT_ORIGIN, changeOrigin: true },
      '/mgmt': { target: MGMT_ORIGIN, changeOrigin: true },
      '/metrics': { target: MGMT_ORIGIN, changeOrigin: true },
    },
  },
  test: {
    environment: 'node',
    include: ['src/**/*.test.ts'],
  },
});
