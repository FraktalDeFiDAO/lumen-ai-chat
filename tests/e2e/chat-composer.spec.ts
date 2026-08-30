import { expect, test, MODEL_KEY } from './support/fixtures'
import { mockChat, mockFailure, mockJson } from './support/mocks'

/// ChatComposer is where every request is shaped: which model answers, what text
/// and files go with it, and whether the run can be stopped.

test.describe('ChatComposer — model selection', () => {
  test('defaults to the configured route and groups every coder and provider', async ({ page, consoleErrors }) => {
    await page.goto('/')
    const select = page.getByLabel('AI coder, provider, and model')

    await expect(select).toHaveValue('hermes/z.ai/glm-5.2')
    await expect(select).toBeEnabled()

    const labels = await select.locator('optgroup').evaluateAll(groups => groups.map(group => (group as HTMLOptGroupElement).label))
    expect(labels).toEqual(expect.arrayContaining([
      'Hermes · Z.AI',
      'OpenCode · OpenRouter',
      'KiloCode · KiloCode',
      'Claude Code · Claude',
      'Codex · Codex',
    ]))
    expect(labels.length).toBe(9)
    expect(consoleErrors).toEqual([])
  })

  test('remembers the chosen route across a reload', async ({ page }) => {
    await page.goto('/')
    const select = page.getByLabel('AI coder, provider, and model')
    const alternative = await select.locator('option').nth(1).getAttribute('value')
    expect(alternative).toBeTruthy()

    await select.selectOption(alternative)
    expect(await page.evaluate(key => window.localStorage.getItem(key), MODEL_KEY)).toBe(alternative)

    await page.reload()
    await expect(page.getByLabel('AI coder, provider, and model')).toHaveValue(alternative!)
  })

  test('sends the selected route with the request', async ({ page }) => {
    const chat = mockChat(page)
    await page.goto('/')
    await page.getByLabel('Message Lumen').fill('Which model answered?')
    await page.getByLabel('Message Lumen').press('Enter')
    await expect(page.locator('article.assistant .message-content')).toHaveText('Mocked assistant reply.')

    expect(chat.requests).toHaveLength(1)
    expect(chat.requests[0]).toMatchObject({ ai_coder: 'hermes', provider: 'z.ai', model: 'hermes/z.ai/glm-5.2' })
  })

  test('disables the picker and says so when the catalog cannot be loaded', async ({ page }) => {
    await mockFailure(page, '**/api/models', 503, 'models offline')
    await page.goto('/')

    const select = page.getByLabel('AI coder, provider, and model')
    await expect(select).toBeDisabled()
    await expect(select.locator('option')).toHaveText(['Models unavailable'])
  })

  test('warns when some providers fell back', async ({ page }) => {
    await mockJson(page, '**/api/models', {
      models: [{ id: 'a', key: 'hermes/z.ai/glm-5.2', name: 'GLM 5.2', provider: 'z.ai', provider_name: 'Z.AI', ai_coder: 'hermes', ai_coder_name: 'Hermes' }],
      default_ai_coder: 'hermes',
      default_provider: 'z.ai',
      provider_errors: [{ provider: 'openai', message: 'unreachable' }],
    })
    await page.goto('/')

    await expect(page.getByRole('status').filter({ hasText: '1 provider unavailable' }))
      .toContainText('1 provider unavailable; fallback models are shown.')
  })
})

test.describe('ChatComposer — attachments', () => {
  test.beforeEach(async ({ page }) => {
    mockChat(page, 'Read the attachment.')
    await page.goto('/')
  })

  test('accepts a text file, shows a chip, and removes it again', async ({ page }) => {
    await page.locator('input[type="file"]').setInputFiles({ name: 'notes.txt', mimeType: 'text/plain', buffer: Buffer.from('Attachment body') })

    const chip = page.locator('.attachment-chip')
    await expect(chip).toHaveCount(1)
    await expect(chip).toContainText('notes.txt')

    await page.getByRole('button', { name: 'Remove notes.txt' }).click()
    await expect(page.locator('.attachment-chip')).toHaveCount(0)
  })

  test('sends an attachment on its own, wrapped in a delimiter the model can read', async ({ page }) => {
    await page.locator('input[type="file"]').setInputFiles({ name: 'plan.md', mimeType: 'text/markdown', buffer: Buffer.from('# Plan\nShip it.') })
    await expect(page.getByRole('button', { name: 'Send message' })).toBeEnabled()
    await page.getByRole('button', { name: 'Send message' }).click()

    await expect(page.locator('article.user .message-content')).toContainText('--- Attached file: plan.md ---')
    await expect(page.locator('article.user .message-content')).toContainText('# Plan')
    await expect(page.locator('article.user .message-content')).toContainText('--- End of plan.md ---')
    await expect(page.locator('.attachment-chip')).toHaveCount(0)
  })

  test('puts the typed message before the attached files', async ({ page }) => {
    await page.getByLabel('Message Lumen').fill('Review this')
    await page.locator('input[type="file"]').setInputFiles({ name: 'a.txt', mimeType: 'text/plain', buffer: Buffer.from('first') })
    await page.getByRole('button', { name: 'Send message' }).click()

    const sent = await page.locator('article.user .message-content').innerText()
    expect(sent.indexOf('Review this')).toBeLessThan(sent.indexOf('--- Attached file: a.txt ---'))
  })

  test('refuses a file over 256 KB', async ({ page }) => {
    await page.locator('input[type="file"]').setInputFiles({ name: 'big.txt', mimeType: 'text/plain', buffer: Buffer.alloc(256 * 1024 + 1, 'x') })

    await expect(page.getByRole('alert')).toHaveText('big.txt is larger than 256 KB.')
    await expect(page.locator('.attachment-chip')).toHaveCount(0)
  })

  test('refuses a file that is neither text nor a known code extension', async ({ page }) => {
    await page.locator('input[type="file"]').setInputFiles({ name: 'app.bin', mimeType: 'application/octet-stream', buffer: Buffer.from([0, 1, 2, 3]) })

    await expect(page.getByRole('alert')).toHaveText('app.bin is not a supported text or code file.')
    await expect(page.locator('.attachment-chip')).toHaveCount(0)
  })

  test('keeps the good files when one in a batch is rejected', async ({ page }) => {
    await page.locator('input[type="file"]').setInputFiles([
      { name: 'good.ts', mimeType: 'application/octet-stream', buffer: Buffer.from('export const x = 1') },
      { name: 'bad.bin', mimeType: 'application/octet-stream', buffer: Buffer.from([9, 9]) },
    ])

    await expect(page.locator('.attachment-chip')).toHaveCount(1)
    await expect(page.locator('.attachment-chip')).toContainText('good.ts')
    await expect(page.getByRole('alert')).toHaveText('bad.bin is not a supported text or code file.')
  })
})

test.describe('ChatComposer — the textarea', () => {
  test('grows with its content and returns to one line after sending', async ({ page }) => {
    mockChat(page)
    await page.goto('/')
    const box = page.getByLabel('Message Lumen')
    const height = async () => (await box.boundingBox())!.height

    const single = await height()
    await box.fill(Array.from({ length: 8 }, (_, index) => `line ${index}`).join('\n'))
    await box.dispatchEvent('input')
    expect(await height()).toBeGreaterThan(single)

    await box.press('Enter')
    await expect(page.locator('article.assistant .message-content')).toHaveText('Mocked assistant reply.')
    expect(await height()).toBeLessThanOrEqual(single + 1)
  })
})
