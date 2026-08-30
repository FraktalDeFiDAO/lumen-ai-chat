import { conversation, expect, seedConversations, sidebarOnScreen, test } from './support/fixtures'
import { mockChat } from './support/mocks'

/// At phone width the sidebar becomes an overlay, and it is the only way to
/// reach history or settings — so opening and closing it is a real path, not a
/// cosmetic one. This file runs under the `mobile` project.

test.describe('Mobile layout', () => {
  test.beforeEach(async ({ page }) => {
    await seedConversations(page, [conversation('Saved on a phone', { id: 'phone-chat' })])
  })

  test('hides the sidebar behind a toggle', async ({ page, consoleErrors }) => {
    await page.goto('/')
    await expect(page.getByRole('button', { name: 'Open sidebar' })).toBeVisible()
    expect(await sidebarOnScreen(page)).toBe(false)
    await expect(page.getByRole('heading', { name: 'How can I help you today?' })).toBeVisible()
    expect(consoleErrors).toEqual([])
  })

  test('opens and closes the sidebar', async ({ page }) => {
    await page.goto('/')
    await page.getByRole('button', { name: 'Open sidebar' }).click()
    await expect(page.locator('aside.sidebar')).toHaveClass(/translate-x-0/)
    expect(await sidebarOnScreen(page)).toBe(true)
    await expect(page.getByLabel('Search conversations')).toBeVisible()

    await page.getByRole('button', { name: 'Close sidebar' }).click()
    await expect(page.locator('aside.sidebar')).toHaveClass(/-translate-x-full/)
    expect(await sidebarOnScreen(page)).toBe(false)
  })

  test('closes the sidebar by tapping the backdrop', async ({ page }) => {
    await page.goto('/')
    await page.getByRole('button', { name: 'Open sidebar' }).click()
    await page.locator('.fixed.inset-0').click({ position: { x: 380, y: 700 } })
    await expect(page.locator('.fixed.inset-0')).toHaveCount(0)
    expect(await sidebarOnScreen(page)).toBe(false)
  })

  test('opening a conversation closes the sidebar behind it', async ({ page }) => {
    await page.goto('/')
    await page.getByRole('button', { name: 'Open sidebar' }).click()
    await page.locator('.history-open').filter({ hasText: 'Saved on a phone' }).click()

    await expect(page).toHaveURL(/\/chat\/phone-chat$/)
    expect(await sidebarOnScreen(page)).toBe(false)
    await expect(page.locator('article.user .message-content')).toHaveText('Saved on a phone')
  })

  test('starting a new chat closes the sidebar too', async ({ page }) => {
    await page.goto('/chat/phone-chat')
    await page.getByRole('button', { name: 'Open sidebar' }).click()
    await page.getByRole('button', { name: 'New chat' }).click()

    await expect(page).toHaveURL(/\/$/)
    expect(await sidebarOnScreen(page)).toBe(false)
  })

  test('the composer still sends from a phone', async ({ page }) => {
    mockChat(page, 'Answered on a phone.')
    await page.goto('/')
    await page.getByLabel('Message Lumen').fill('Sent from a phone')
    await page.getByRole('button', { name: 'Send message' }).click()

    await expect(page.locator('article.assistant .message-content')).toHaveText('Answered on a phone.')
  })

  test('settings is reachable and readable on a phone', async ({ page }) => {
    await page.goto('/settings')
    await expect(page.getByRole('heading', { name: 'General', level: 1 })).toBeVisible()
    await page.getByRole('navigation', { name: 'Settings sections' }).getByRole('button', { name: 'Appearance' }).click()
    await expect(page.getByLabel('Theme')).toBeVisible()
  })
})
