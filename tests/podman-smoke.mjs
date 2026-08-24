import assert from 'node:assert/strict'
import { chromium } from 'playwright-core'

const baseURL = process.env.BASE_URL ?? 'http://127.0.0.1:41789'
const executablePath = process.env.CHROMIUM_PATH ?? '/usr/bin/chromium'
const browser = await chromium.launch({ executablePath, headless: true })
try {
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 } })
  const page = await context.newPage()
  const errors = []
  page.on('console', message => { if (message.type() === 'error') errors.push(message.text()) })
  page.on('pageerror', error => errors.push(error.message))

  await page.goto(baseURL, { waitUntil: 'networkidle' })
  assert.equal(await page.title(), 'Lumen AI')
  await page.getByRole('heading', { name: 'How can I help you today?' }).waitFor()

  const selector = page.getByLabel('AI coder, provider, and model')
  assert.equal(await selector.inputValue(), 'hermes/auto/default')
  assert.equal(await selector.locator('optgroup').count(), 5)

  await page.getByPlaceholder('Message Lumen...').fill('Reply with exactly PODMAN_UI_OK. Do not use tools or modify files.')
  await page.getByRole('button', { name: 'Send message' }).click()
  await page.getByText('PODMAN_UI_OK', { exact: true }).waitFor({ timeout: 120_000 })

  await page.locator('.profile-row').click()
  await page.getByRole('button', { name: 'AI coders' }).click()
  for (const name of ['Hermes', 'OpenCode', 'KiloCode', 'Claude Code', 'Codex']) {
    await page.getByText(name, { exact: true }).waitFor()
  }
  assert.equal(await page.getByText('Installed', { exact: true }).count(), 5)
  await page.screenshot({ path: '/tmp/lumen-podman.png', fullPage: true })
  assert.deepEqual(errors, [])
  console.log('Podman browser smoke passed: production UI, Hermes stream, and five-coder status')
} finally {
  await browser.close()
}
