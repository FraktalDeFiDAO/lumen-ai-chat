import { conversation, expect, seedConversations, seedSettings, test } from './support/fixtures'
import { mockAgent, mockChat, mockFailure } from './support/mocks'

/// MessageBubble carries everything a user can do with an answer, plus the only
/// window into what the agent harness did to produce one.

test.describe('MessageBubble — acting on an answer', () => {
  test.beforeEach(async ({ page }) => {
    await seedConversations(page, [conversation('Copy me', {
      id: 'bubble-chat',
      messages: [
        { role: 'user', content: 'Say something quotable' },
        { role: 'assistant', content: 'Quotable answer.' },
      ],
    })])
  })

  test('copies the answer and says it copied', async ({ page, consoleErrors }) => {
    await page.goto('/chat/bubble-chat')
    await page.getByRole('button', { name: 'Copy response' }).click()

    await expect(page.getByRole('button', { name: 'Response copied' })).toBeVisible()
    expect(await page.evaluate(() => navigator.clipboard.readText())).toBe('Quotable answer.')
    await expect(page.getByRole('button', { name: 'Copy response' })).toBeVisible({ timeout: 5_000 })
    expect(consoleErrors).toEqual([])
  })

  test('records feedback and lets the same choice be withdrawn', async ({ page }) => {
    await page.goto('/chat/bubble-chat')
    const good = page.getByRole('button', { name: 'Good response' })
    const bad = page.getByRole('button', { name: 'Bad response' })

    await good.click()
    await expect(good).toHaveAttribute('aria-pressed', 'true')
    await expect(bad).toHaveAttribute('aria-pressed', 'false')

    await bad.click()
    await expect(bad).toHaveAttribute('aria-pressed', 'true')

    await bad.click()
    await expect(bad).toHaveAttribute('aria-pressed', 'false')
  })

  test('regenerating replaces the answer and keeps the question', async ({ page }) => {
    mockChat(page, 'A second attempt.')
    await page.goto('/chat/bubble-chat')

    await page.getByRole('button', { name: 'Regenerate response' }).click()
    await expect(page.locator('article.assistant .message-content')).toHaveText('A second attempt.')
    await expect(page.locator('article.user .message-content')).toHaveText('Say something quotable')
    await expect(page.locator('article.assistant')).toHaveCount(1)
  })

  test('offers no actions on the message the user wrote', async ({ page }) => {
    await page.goto('/chat/bubble-chat')
    await expect(page.locator('article.user .message-actions')).toHaveCount(0)
    await expect(page.locator('article.assistant .message-actions')).toHaveCount(1)
  })
})

test.describe('MessageBubble — when the request fails', () => {
  test('shows the server’s reason as an alert beside a plain fallback', async ({ page }) => {
    await mockFailure(page, '**/api/chat', 502, 'upstream provider refused the request')
    await page.goto('/')
    await page.getByLabel('Message Lumen').fill('This will fail')
    await page.getByLabel('Message Lumen').press('Enter')

    await expect(page.locator('article.assistant .message-error'))
      .toHaveText('upstream provider refused the request')
    await expect(page.locator('article.assistant .message-error')).toHaveAttribute('role', 'alert')
    await expect(page.locator('article.assistant .message-content')).toHaveText('Unable to generate a response.')
    await expect(page.locator('article.assistant.has-error')).toHaveCount(1)
  })

  test('says so when a stream closes without any content', async ({ page }) => {
    await page.route('**/api/chat', route => route.fulfill({
      status: 200,
      headers: { 'content-type': 'text/event-stream' },
      body: 'data: [DONE]\n\n',
    }))
    await page.goto('/')
    await page.getByLabel('Message Lumen').fill('Empty stream')
    await page.getByLabel('Message Lumen').press('Enter')

    await expect(page.locator('article.assistant .message-error')).toHaveText('The model returned no response.')
  })
})

test.describe('MessageBubble — agent mode', () => {
  test.beforeEach(async ({ page }) => {
    await seedSettings(page, { agentMode: true })
  })

  test('reports retrieval, each tool it ran, and the tool output', async ({ page, consoleErrors }) => {
    mockAgent(page, [
      { event: 'context', records: 4 },
      { event: 'tool_call', step: 1, id: 'call-1', name: 'read_file', arguments: { path: 'src/main.ts' } },
      { event: 'tool_result', id: 'call-1', output: 'export const app = 1' },
      { event: 'text', step: 2, text: 'The entry point creates the app.' },
      { event: 'outcome', text: 'The entry point creates the app.' },
    ])
    await page.goto('/')
    await page.getByLabel('Message Lumen').fill('What does main.ts do?')
    await page.getByLabel('Message Lumen').press('Enter')

    await expect(page.locator('.harness-context')).toHaveText('Retrieved 4 codebase records before answering.')
    const tool = page.locator('.tool-activity details')
    await expect(tool).toHaveCount(1)
    await expect(tool.locator('summary strong')).toHaveText('read_file')
    await expect(tool.locator('summary span')).toHaveText('{"path":"src/main.ts"}')

    await tool.locator('summary').click()
    await expect(tool.locator('pre')).toHaveText('export const app = 1')
    await expect(page.locator('article.assistant .message-content')).toHaveText('The entry point creates the app.')
    expect(consoleErrors).toEqual([])
  })

  test('uses the singular for a single retrieved record', async ({ page }) => {
    mockAgent(page, [
      { event: 'context', records: 1 },
      { event: 'text', text: 'One record was enough.' },
      { event: 'outcome', text: 'One record was enough.' },
    ])
    await page.goto('/')
    await page.getByLabel('Message Lumen').fill('Just one')
    await page.getByLabel('Message Lumen').press('Enter')

    await expect(page.locator('.harness-context')).toHaveText('Retrieved 1 codebase record before answering.')
  })

  test('marks a failed tool and still shows its output', async ({ page }) => {
    mockAgent(page, [
      { event: 'tool_call', id: 'call-1', name: 'read_file', arguments: { path: '../../etc/passwd' } },
      { event: 'tool_result', id: 'call-1', output: 'path escapes the workspace', failed: true },
      { event: 'text', text: 'That path is outside the workspace.' },
      { event: 'outcome', text: 'That path is outside the workspace.' },
    ])
    await page.goto('/')
    await page.getByLabel('Message Lumen').fill('Read /etc/passwd')
    await page.getByLabel('Message Lumen').press('Enter')

    const tool = page.locator('.tool-activity details.failed')
    await expect(tool).toHaveCount(1)
    await expect(tool.locator('summary em')).toHaveText('failed')
    await tool.locator('summary').click()
    await expect(tool.locator('pre')).toHaveText('path escapes the workspace')
  })

  test('truncates long tool arguments to a single readable line', async ({ page }) => {
    const path = `src/${'deeply/'.repeat(40)}file.ts`
    mockAgent(page, [
      { event: 'tool_call', id: 'call-1', name: 'read_file', arguments: { path } },
      { event: 'tool_result', id: 'call-1', output: 'ok' },
      { event: 'outcome', text: 'Read it.' },
    ])
    await page.goto('/')
    await page.getByLabel('Message Lumen').fill('Read the deep file')
    await page.getByLabel('Message Lumen').press('Enter')

    const summary = page.locator('.tool-activity details summary span')
    const shown = await summary.innerText()
    expect(shown).toHaveLength(120)
    expect(shown.endsWith('…')).toBe(true)
  })

  test('says the answer is incomplete when the harness hit its step limit', async ({ page }) => {
    mockAgent(page, [
      { event: 'text', text: 'I was partway through when I ran out of steps.' },
      { event: 'outcome', text: 'I was partway through when I ran out of steps.', stopped_at_step_limit: true },
    ])
    await page.goto('/')
    await page.getByLabel('Message Lumen').fill('Do something long')
    await page.getByLabel('Message Lumen').press('Enter')

    await expect(page.locator('.harness-incomplete'))
      .toHaveText('The agent reached its step limit, so this answer is incomplete. Ask it to continue.')
  })

  test('keeps a run that only called tools, with a stand-in summary', async ({ page }) => {
    mockAgent(page, [
      { event: 'tool_call', id: 'call-1', name: 'list_directory', arguments: { path: 'src' } },
      { event: 'tool_result', id: 'call-1', output: 'main.ts\nApp.vue' },
      { event: 'outcome' },
    ])
    await page.goto('/')
    await page.getByLabel('Message Lumen').fill('List src')
    await page.getByLabel('Message Lumen').press('Enter')

    await expect(page.locator('article.assistant .message-content'))
      .toHaveText('No summary was returned; see the tool activity above.')
    await expect(page.locator('.tool-activity details')).toHaveCount(1)
  })

  test('shows a tool that is still running as pending', async ({ page }) => {
    mockAgent(page, [
      { event: 'tool_call', id: 'call-1', name: 'search_text', arguments: { query: 'TODO' } },
      { event: 'outcome', text: 'Searching stopped early.' },
    ])
    await page.goto('/')
    await page.getByLabel('Message Lumen').fill('Find TODOs')
    await page.getByLabel('Message Lumen').press('Enter')

    await page.locator('.tool-activity details summary').click()
    await expect(page.locator('.tool-activity details pre')).toHaveText('Running…')
  })

  test('agent mode is what routes the request to the harness', async ({ page }) => {
    const agent = mockAgent(page, [{ event: 'outcome', text: 'From the harness.' }])
    let chatCalls = 0
    await page.route('**/api/chat', route => { chatCalls += 1; return route.fulfill({ status: 200, headers: { 'content-type': 'text/event-stream' }, body: 'data: [DONE]\n\n' }) })

    await page.goto('/')
    await page.getByLabel('Message Lumen').fill('Which endpoint?')
    await page.getByLabel('Message Lumen').press('Enter')
    await expect(page.locator('article.assistant .message-content')).toHaveText('From the harness.')

    expect(agent.requests).toHaveLength(1)
    expect(chatCalls).toBe(0)
  })
})
