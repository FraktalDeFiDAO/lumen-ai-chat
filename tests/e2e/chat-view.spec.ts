import { acceptDialogs, conversation, expect, readStorage, seedConversations, test, CONVERSATION_KEY } from './support/fixtures'
import { mockChat, mockHangingChat } from './support/mocks'

/// ChatView is the product: an empty page that invites a first message, and a
/// transcript once there is one. These specs follow that arc against the real
/// gateway, and mock only the states a live provider cannot be asked to produce.

test.describe('ChatView — the empty state', () => {
  test('invites a first message and offers four starting points', async ({ page, consoleErrors }) => {
    await page.goto('/')

    await expect(page).toHaveTitle('Lumen AI')
    await expect(page.getByRole('heading', { name: 'How can I help you today?' })).toBeVisible()
    await expect(page.getByText('Ask a question, explore an idea, or start creating.')).toBeVisible()

    const suggestions = ['Help me write', 'Think through an idea', 'Build something', 'Learn something']
    for (const title of suggestions) await expect(page.getByRole('button', { name: new RegExp(title) })).toBeVisible()
    await expect(page.locator('.suggestion-card')).toHaveCount(4)

    await expect(page.locator('.topbar-title span')).toHaveText('New chat')
    await expect(page.getByRole('button', { name: 'Share' })).toHaveCount(0)
    await expect(page.getByLabel('Message Lumen')).toBeVisible()
    await expect(page.getByText('Lumen can make mistakes. Check important information.')).toBeVisible()
    expect(consoleErrors).toEqual([])
  })

  test('hides the mobile sidebar toggle at desktop width', async ({ page }) => {
    await page.goto('/')
    await expect(page.getByRole('button', { name: 'Open sidebar' })).toBeHidden()
  })

  test('offers no chat options until there is a chat', async ({ page }) => {
    await page.goto('/')
    await page.getByRole('button', { name: 'Chat options' }).click()
    await expect(page.getByRole('menu')).toContainText('No chat options')
    await expect(page.getByRole('menuitem', { name: 'Delete chat' })).toHaveCount(0)
  })
})

test.describe('ChatView — sending', () => {
  test('a suggestion sends its own text and streams a real answer', async ({ page, consoleErrors }) => {
    await page.goto('/')
    await page.getByRole('button', { name: /Help me write/ }).click()

    await expect(page.locator('article.user .message-content'))
      .toHaveText('Draft a thoughtful email to my team')
    await expect(page.locator('article.assistant .message-content'))
      .toContainText('Mock coder response:', { timeout: 15_000 })

    await expect(page).toHaveURL(/\/chat\/[0-9a-f-]{36}$/)
    await expect(page.locator('.history-row')).toHaveCount(1)
    await expect(page.locator('.topbar-title small')).toHaveText('Hermes · Z.AI · GLM 5.2')
    expect(consoleErrors).toEqual([])
  })

  test('Enter sends and Shift+Enter keeps writing', async ({ page }) => {
    mockChat(page, 'Reply to the typed message.')
    await page.goto('/')
    const box = page.getByLabel('Message Lumen')

    await box.fill('First line')
    await box.press('Shift+Enter')
    await box.pressSequentially('second line')
    await expect(box).toHaveValue('First line\nsecond line')

    await box.press('Enter')
    await expect(page.locator('article.user .message-content')).toHaveText('First line\nsecond line')
    await expect(page.locator('article.assistant .message-content')).toHaveText('Reply to the typed message.')
    await expect(box).toHaveValue('')
  })

  test('the send button is inert until there is something to send', async ({ page }) => {
    mockChat(page)
    await page.goto('/')
    const send = page.getByRole('button', { name: 'Send message' })
    await expect(send).toBeDisabled()
    await page.getByLabel('Message Lumen').fill('   ')
    await expect(send).toBeDisabled()
    await page.getByLabel('Message Lumen').fill('Ready now')
    await expect(send).toBeEnabled()
  })

  test('titles the conversation from the first message, truncated to 42 characters', async ({ page }) => {
    mockChat(page)
    const long = 'A question long enough to be cut off by the title rule'
    await page.goto('/')
    await page.getByLabel('Message Lumen').fill(long)
    await page.getByLabel('Message Lumen').press('Enter')

    await expect(page.locator('.topbar-title span')).toHaveText(long.slice(0, 42))
    await expect(page.locator('.history-row')).toContainText(long.slice(0, 42))
  })

  test('shows a thinking indicator while the answer has no text yet', async ({ page }) => {
    mockHangingChat(page)
    await page.goto('/')
    await page.getByLabel('Message Lumen').fill('Waiting on this one')
    await page.getByLabel('Message Lumen').press('Enter')

    await expect(page.getByLabel('Lumen is responding')).toBeVisible()
    await expect(page.getByRole('button', { name: 'Stop generating' })).toBeVisible()
  })

  test('stopping discards an answer that never arrived', async ({ page }) => {
    mockHangingChat(page)
    await page.goto('/')
    await page.getByLabel('Message Lumen').fill('Never mind')
    await page.getByLabel('Message Lumen').press('Enter')

    await page.getByRole('button', { name: 'Stop generating' }).click()
    await expect(page.getByRole('button', { name: 'Send message' })).toBeVisible()
    await expect(page.locator('article.assistant')).toHaveCount(0)
    await expect(page.locator('article.user')).toHaveCount(1)
  })
})

test.describe('ChatView — an existing conversation', () => {
  test.beforeEach(async ({ page }) => {
    await seedConversations(page, [conversation('Deployment plan', { id: 'seeded-chat' })])
  })

  test('restores a seeded conversation from its own URL', async ({ page, consoleErrors }) => {
    await page.goto('/chat/seeded-chat')
    await expect(page.locator('.topbar-title span')).toHaveText('Deployment plan')
    await expect(page.locator('article.user .message-content')).toHaveText('Deployment plan')
    await expect(page.locator('article.assistant .message-content')).toHaveText('Answer about Deployment plan')
    expect(consoleErrors).toEqual([])
  })

  test('falls back to the empty state for a conversation that does not exist', async ({ page }) => {
    await page.goto('/chat/does-not-exist')
    await expect(page.getByRole('heading', { name: 'How can I help you today?' })).toBeVisible()
    await expect(page.locator('article')).toHaveCount(0)
  })

  test('shares the transcript to the clipboard', async ({ page }) => {
    await page.goto('/chat/seeded-chat')
    await page.getByRole('button', { name: 'Share' }).click()

    await expect(page.getByRole('status').filter({ hasText: 'Copied link text' })).toBeVisible()
    const clipboard = await page.evaluate(() => navigator.clipboard.readText())
    expect(clipboard).toBe('You: Deployment plan\n\nLumen: Answer about Deployment plan')
    await expect(page.getByText('Copied link text')).toBeHidden({ timeout: 5_000 })
  })

  test('deletes the open conversation from the overflow menu', async ({ page }) => {
    await acceptDialogs(page)
    await page.goto('/chat/seeded-chat')

    await page.getByRole('button', { name: 'Chat options' }).click()
    await page.getByRole('menuitem', { name: 'Delete chat' }).click()

    await expect(page).toHaveURL(/\/$/)
    await expect(page.getByRole('heading', { name: 'How can I help you today?' })).toBeVisible()
    expect(await readStorage(page, CONVERSATION_KEY)).toEqual([])
  })

  test('keeps the conversation when the delete prompt is dismissed', async ({ page }) => {
    page.on('dialog', dialog => void dialog.dismiss())
    await page.goto('/chat/seeded-chat')

    await page.getByRole('button', { name: 'Chat options' }).click()
    await page.getByRole('menuitem', { name: 'Delete chat' }).click()

    await expect(page).toHaveURL(/\/chat\/seeded-chat$/)
    await expect(page.locator('article')).toHaveCount(2)
  })

  test('Escape closes the overflow menu', async ({ page }) => {
    await page.goto('/chat/seeded-chat')
    await page.getByRole('button', { name: 'Chat options' }).click()
    await expect(page.getByRole('menu')).toBeVisible()
    await page.keyboard.press('Escape')
    await expect(page.getByRole('menu')).toBeHidden()
  })

  test('Control+K starts a new chat from an open one', async ({ page }) => {
    await page.goto('/chat/seeded-chat')
    await page.keyboard.press('Control+k')

    await expect(page).toHaveURL(/\/$/)
    await expect(page.getByRole('heading', { name: 'How can I help you today?' })).toBeVisible()
    await expect(page.locator('.history-row')).toHaveCount(1)
  })

  test('survives a reload, because conversations live in this browser', async ({ page }) => {
    mockChat(page, 'Persisted answer.')
    await page.goto('/')
    await page.getByLabel('Message Lumen').fill('Remember me')
    await page.getByLabel('Message Lumen').press('Enter')
    await expect(page.locator('article.assistant .message-content')).toHaveText('Persisted answer.')

    const url = page.url()
    await page.reload()
    await expect(page).toHaveURL(url)
    await expect(page.locator('article.user .message-content')).toHaveText('Remember me')
    await expect(page.locator('article.assistant .message-content')).toHaveText('Persisted answer.')
  })
})
