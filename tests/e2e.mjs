import { chromium } from 'playwright-core'
import assert from 'node:assert/strict'

const baseURL = process.env.BASE_URL ?? 'http://127.0.0.1:41789'
const browser = await chromium.launch({ executablePath: '/usr/bin/chromium', headless: true })
try {
  const page = await browser.newPage({ viewport: { width: 1440, height: 900 } })
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
  await page.getByText(/I can help with that|Here’s a concise framework|The key tradeoff/).waitFor({ timeout: 3000 })
  assert.match(page.url(), /\/chat\//)
  assert.ok((await page.locator('.history-row').count()) >= 1)

  const mobile = await browser.newPage({ viewport: { width: 390, height: 844 } })
  await mobile.goto(baseURL, { waitUntil: 'networkidle' })
  await mobile.getByRole('button', { name: 'Open sidebar' }).click()
  await mobile.getByRole('button', { name: 'Close sidebar' }).waitFor()
  await mobile.screenshot({ path: '/tmp/lumen-mobile.png', fullPage: true })
  assert.deepEqual(errors, [])
  console.log('E2E passed: welcome, chat response, history, routing, and mobile navigation')
  console.log('Screenshots: /tmp/lumen-desktop.png, /tmp/lumen-mobile.png')
} finally {
  await browser.close()
}
