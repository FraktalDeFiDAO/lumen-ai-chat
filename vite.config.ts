import { fileURLToPath, URL } from 'node:url'
import { defineConfig } from 'vitest/config'
import vue from '@vitejs/plugin-vue'
import tailwindcss from '@tailwindcss/vite'

export default defineConfig({
  plugins: [vue(), tailwindcss()],
  resolve: { alias: { '@': fileURLToPath(new URL('./src', import.meta.url)) } },
  server: {
    proxy: { '/api': { target: process.env.VITE_API_TARGET ?? 'http://127.0.0.1:41788', changeOrigin: true } },
  },
  // Vitest owns the unit tests under src/; the Playwright specs in tests/e2e
  // are also `*.spec.ts` and would otherwise be collected here and fail.
  test: {
    environment: 'jsdom',
    globals: true,
    include: ['src/**/*.test.ts'],
    coverage: {
      provider: 'v8',
      // What ships, not what tests it. Config and entry points are excluded
      // because covering them measures whether the app starts, which the
      // browser suite already answers.
      include: ['src/**/*.ts', 'src/**/*.vue'],
      exclude: ['src/**/*.test.ts', 'src/main.ts', 'src/vite-env.d.ts', 'src/types/**'],
      reporter: ['text-summary', 'html'],
    },
  },
})
