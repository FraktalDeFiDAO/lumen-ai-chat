import { CONVERSATION_KEY, conversation, expect, readStorage, seedConversations, seedSettings, test } from './support/fixtures'
import { mockChat } from './support/mocks'

/// AppSidebar is the only way back to an earlier conversation, so its ordering,
/// search, and delete behaviour are load-bearing rather than decorative.

const seeded = [
  conversation('Rust error handling', { id: 'chat-rust', updatedAt: 3_000 }),
  conversation('Vue composition API', { id: 'chat-vue', updatedAt: 2_000 }),
  conversation('Podman networking', { id: 'chat-podman', updatedAt: 1_000 }),
]

test.describe('AppSidebar — history', () => {
  test.beforeEach(async ({ page }) => {
    await seedConversations(page, seeded)
  })

  test('lists conversations with the most recently touched first', async ({ page, consoleErrors }) => {
    await page.goto('/')
    await expect(page.locator('.history-row')).toHaveCount(3)
    await expect(page.locator('.history-row .history-open')).toHaveText([
      'Rust error handling',
      'Vue composition API',
      'Podman networking',
    ])
    await expect(page.locator('.section-label')).toHaveText('Recent')
    expect(consoleErrors).toEqual([])
  })

  test('filters by title without regard to case', async ({ page }) => {
    await page.goto('/')
    await page.getByLabel('Search conversations').fill('VUE')
    await expect(page.locator('.history-row')).toHaveCount(1)
    await expect(page.locator('.history-row')).toContainText('Vue composition API')

    await page.getByLabel('Search conversations').fill('nothing matches this')
    await expect(page.locator('.history-row')).toHaveCount(0)

    await page.getByLabel('Search conversations').fill('')
    await expect(page.locator('.history-row')).toHaveCount(3)
  })

  test('opens a conversation and marks it active', async ({ page }) => {
    await page.goto('/')
    await page.locator('.history-open').filter({ hasText: 'Vue composition API' }).click()

    await expect(page).toHaveURL(/\/chat\/chat-vue$/)
    await expect(page.locator('.history-row.active')).toHaveCount(1)
    await expect(page.locator('.history-row.active')).toContainText('Vue composition API')
    await expect(page.locator('.topbar-title span')).toHaveText('Vue composition API')
  })

  test('deletes one conversation and leaves the rest', async ({ page }) => {
    await page.goto('/')
    await page.getByRole('button', { name: 'Delete Podman networking' }).click()

    await expect(page.locator('.history-row')).toHaveCount(2)
    const stored = await readStorage<Array<{ id: string }>>(page, CONVERSATION_KEY)
    expect(stored?.map(item => item.id)).toEqual(['chat-rust', 'chat-vue'])
  })

  test('deleting the open conversation returns to the empty state', async ({ page }) => {
    await page.goto('/chat/chat-rust')
    await expect(page.locator('article')).toHaveCount(2)

    await page.getByRole('button', { name: 'Delete Rust error handling' }).click()
    await expect(page).toHaveURL(/\/$/)
    await expect(page.getByRole('heading', { name: 'How can I help you today?' })).toBeVisible()
    await expect(page.locator('.history-row')).toHaveCount(2)
  })

  test('starts a new chat from the button and from the brand', async ({ page }) => {
    await page.goto('/chat/chat-rust')

    await page.getByRole('button', { name: 'New chat' }).click()
    await expect(page).toHaveURL(/\/$/)
    await expect(page.getByRole('heading', { name: 'How can I help you today?' })).toBeVisible()

    await page.goto('/chat/chat-vue')
    await page.locator('.brand').click()
    await expect(page).toHaveURL(/\/$/)
    await expect(page.locator('.history-row.active')).toHaveCount(0)
  })

  test('shows the keyboard hint for a new chat', async ({ page }) => {
    await page.goto('/')
    await expect(page.locator('.new-chat kbd')).toHaveText('⌘ K')
  })
})

test.describe('AppSidebar — empty and profile', () => {
  test('explains that history is empty before there is any', async ({ page }) => {
    await page.goto('/')
    await expect(page.locator('.empty-history')).toHaveText('Your conversations will appear here.')
    await expect(page.locator('.history-row')).toHaveCount(0)
  })

  test('a first message puts a row in the history', async ({ page }) => {
    mockChat(page, 'Now there is history.')
    await page.goto('/')
    await page.getByLabel('Message Lumen').fill('Start a history')
    await page.getByLabel('Message Lumen').press('Enter')

    await expect(page.locator('.history-row')).toHaveCount(1)
    await expect(page.locator('.history-row')).toContainText('Start a history')
  })

  test('shows the profile and opens settings', async ({ page }) => {
    await seedSettings(page, { displayName: 'Riley Chen', email: 'riley@example.com' })
    await page.goto('/')

    await expect(page.locator('.profile-row .avatar')).toHaveText('RC')
    await expect(page.locator('.profile-row strong')).toHaveText('Riley Chen')
    await expect(page.locator('.profile-row small')).toHaveText('Free plan')

    await page.locator('.profile-row').click()
    await expect(page).toHaveURL(/\/settings$/)
    await expect(page.getByRole('heading', { name: 'General' })).toBeVisible()
  })
})
