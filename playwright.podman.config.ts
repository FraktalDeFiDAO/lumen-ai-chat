import { defineConfig } from '@playwright/test'

/// The smoke suite for the Compose deployment. Separate from the main config
/// because it addresses a deployment that is already running rather than a
/// stack a test run built, and because a real coder invocation is minutes
/// rather than milliseconds.
const baseURL = process.env.BASE_URL ?? 'http://127.0.0.1:41789'
const executablePath = process.env.CHROMIUM_PATH ?? '/usr/bin/chromium'

export default defineConfig({
  testDir: './tests/podman',
  fullyParallel: false,
  workers: 1,
  retries: 0,
  timeout: 180_000,
  expect: { timeout: 30_000 },
  reporter: [['list']],
  outputDir: 'test-results/podman',
  use: {
    baseURL,
    viewport: { width: 1440, height: 900 },
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
    launchOptions: { executablePath },
  },
})
