import { defineConfig } from '@playwright/test'

/// The server-backed deployment: conversations kept by the gateway under an
/// identity a reverse proxy asserted, rather than in the browser.
///
/// Its own config because the store and the identity mode are startup
/// configuration. `tests/run-e2e.mjs` starts a second gateway for it, with a
/// proxy identity header injected the way oauth2-proxy would.
const baseURL = process.env.BASE_URL ?? 'http://127.0.0.1:41790'
const executablePath = process.env.CHROMIUM_PATH ?? '/usr/bin/chromium'

export default defineConfig({
  testDir: './tests/hosted',
  fullyParallel: false,
  workers: 1,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  timeout: 30_000,
  expect: { timeout: 7_000 },
  reporter: process.env.CI ? [['github'], ['list']] : [['list']],
  outputDir: 'test-results/hosted',
  use: {
    baseURL,
    viewport: { width: 1440, height: 900 },
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
    launchOptions: { executablePath },
  },
})
