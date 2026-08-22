import { chromium } from 'playwright-core'
import assert from 'node:assert/strict'

const baseURL = process.env.BASE_URL ?? 'http://127.0.0.1:41789'
const browser = await chromium.launch({ executablePath: '/usr/bin/chromium', headless: true })
try {
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 }, permissions: ['clipboard-read', 'clipboard-write'] })
  const page = await context.newPage()
  const errors = []
  page.on('console', message => { if (message.type() === 'error') errors.push(message.text()) })
  await page.goto(baseURL, { waitUntil: 'networkidle' })
  assert.equal(await page.title(), 'Lumen AI')
  await page.getByRole('heading', { name: 'How can I help you today?' }).waitFor()
  assert.equal(await page.getByRole('button', { name: 'Open sidebar' }).isVisible(), false)
  assert.equal(await page.getByRole('button', { name: /Help me write/ }).count(), 1)
  await page.screenshot({ path: '/tmp/lumen-desktop.png', fullPage: true })

  await page.getByRole('button', { name: /Help me write/ }).click()
  await page.locator('article.user').getByText('Draft a thoughtful email to my team', { exact: true }).waitFor()
  await page.getByText(/Mock response received:/).waitFor({ timeout: 5000 })
  assert.match(page.url(), /\/chat\//)
  assert.ok((await page.locator('.history-row').count()) >= 1)

  assert.equal(await page.getByLabel('Model').inputValue(), 'lumen-test')
  await page.locator('input[type="file"]').setInputFiles({ name: 'notes.txt', mimeType: 'text/plain', buffer: Buffer.from('Attachment body') })
  await page.getByText('notes.txt', { exact: true }).waitFor()
  await page.getByRole('button', { name: 'Remove notes.txt' }).click()
  assert.equal(await page.getByText('notes.txt', { exact: true }).count(), 0)

  await page.getByRole('button', { name: 'Good response' }).click()
  assert.equal(await page.getByRole('button', { name: 'Good response' }).getAttribute('aria-pressed'), 'true')
  await page.getByRole('button', { name: 'Regenerate response' }).click()
  await page.getByRole('button', { name: 'Send message' }).waitFor({ timeout: 5000 })
  await page.getByRole('button', { name: 'Share' }).click()
  await page.getByText('Copied link text', { exact: true }).waitFor()

  await page.goto(`${baseURL}/settings`, { waitUntil: 'networkidle' })
  await page.getByRole('button', { name: 'Edit profile' }).click()
  await page.getByLabel('Display name').fill('Test User')
  await page.getByRole('button', { name: 'Save' }).click()
  await page.getByText('Test User', { exact: true }).waitFor()
  await page.getByRole('button', { name: 'Appearance' }).click()
  await page.getByLabel('Theme').selectOption('Dark')
  assert.equal(await page.locator('html').evaluate(element => element.classList.contains('theme-dark')), true)

  const mobile = await browser.newPage({ viewport: { width: 390, height: 844 } })
  await mobile.goto(baseURL, { waitUntil: 'networkidle' })
  await mobile.getByRole('button', { name: 'Open sidebar' }).click()
  await mobile.getByRole('button', { name: 'Close sidebar' }).waitFor()
  await mobile.screenshot({ path: '/tmp/lumen-mobile.png', fullPage: true })
  assert.deepEqual(errors, [])
  console.log('E2E passed: streaming chat, model selection, attachments, feedback, regenerate, share, settings, theme, history, routing, and mobile navigation')
  console.log('Screenshots: /tmp/lumen-desktop.png, /tmp/lumen-mobile.png')
} finally {
  await browser.close()
}
