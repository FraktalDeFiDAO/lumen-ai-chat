import { defineConfig } from '@playwright/test'

/// The suite drives a stack that `tests/run-e2e.mjs` has already started: a mock
/// provider, the real gateway, and Vite. It never starts servers itself, so the
/// same specs run against a dev stack (`BASE_URL=... npx playwright test`) and
/// against the ephemeral one CI builds.
const baseURL = process.env.BASE_URL ?? 'http://127.0.0.1:41789'

/// Chromium is resolved from the system rather than downloaded. CI exports
/// CHROMIUM_PATH; `npm ci` never pulls a browser (PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD).
const executablePath = process.env.CHROMIUM_PATH ?? '/usr/bin/chromium'

export default defineConfig({
  testDir: './tests/e2e',
  fullyParallel: true,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  workers: process.env.CI ? 2 : 4,
  timeout: 30_000,
  expect: { timeout: 7_000 },
  reporter: process.env.CI ? [['github'], ['list']] : [['list']],
  outputDir: 'test-results',
  use: {
    baseURL,
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
    video: 'off',
    actionTimeout: 10_000,
    launchOptions: { executablePath },
    permissions: ['clipboard-read', 'clipboard-write'],
  },
  projects: [
    {
      name: 'desktop',
      testIgnore: /mobile\./,
      use: { viewport: { width: 1440, height: 900 } },
    },
    {
      name: 'mobile',
      testMatch: /mobile\./,
      use: { viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true },
    },
  ],
})
