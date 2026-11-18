import react from '@vitejs/plugin-react'
import { defineConfig } from 'vite'

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    host: '127.0.0.1',
    port: 1420,
    strictPort: true,
    // 只盯前端源码。盯着 target 会占住 cargo 构建脚本刚删掉的临时目录，它再建同名目录时被系统拒绝访问，编译就失败；
    // 安装包和软件自己的数据目录也和页面无关
    watch: {
      ignored: ['**/target/**', '**/src-tauri/**', '**/crates/**', '**/installers/**', '**/Foxes-watch-data/**'],
    },
  },
  build: {
    target: 'chrome120',
    outDir: 'dist',
    emptyOutDir: true,
    rollupOptions: {
      input: {
        main: 'index.html',
        island: 'island.html',
      },
    },
  },
})
