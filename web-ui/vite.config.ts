import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'

export default defineConfig({
  plugins: [vue()],
  base: '/static/vue/',
  build: {
    outDir: '../recisdb-proxy/static/vue',
    emptyOutDir: true,
    assetsDir: 'assets',
    // 成果物は内容ハッシュ付きの名前にする。固定名 (app.css) だと、Cloudflare など
    // 前段が付ける max-age のあいだ端末が古い CSS/JS を使い続け、更新が一部の端末
    // (スマホ) にだけ反映されない。index.html は no-cache、assets/* は immutable で配る
    // (recisdb-proxy/src/web/api/statics.rs の cache_control_for)。
    rollupOptions: {
      output: {
        entryFileNames: 'assets/app-[hash].js',
        chunkFileNames: 'assets/[name]-[hash].js',
        assetFileNames: 'assets/[name]-[hash].[ext]',
      },
    },
  },
  server: {
    port: 5173,
    proxy: { '/api': 'http://127.0.0.1:40080', '/logos': 'http://127.0.0.1:40080' },
  },
})
