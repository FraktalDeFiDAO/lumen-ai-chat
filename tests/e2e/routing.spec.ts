import { conversation, expect, seedConversations, test } from './support/fixtures'

/// Four routes, and the rule that anything unknown lands on the chat.

test.describe('Routing', () => {
  test.beforeEach(async ({ page }) => {
    await seedConversations(page, [conversation('Routed chat', { id: 'routed' })])
  })

  test('serves the chat at the root', async ({ page, consoleErrors }) => {
    await page.goto('/')
    await expect(page.getByRole('heading', { name: 'How can I help you today?' })).toBeVisible()
    expect(consoleErrors).toEqual([])
  })

  test('serves a conversation deep link', async ({ page }) => {
    await page.goto('/chat/routed')
    await expect(page.locator('.topbar-title span')).toHaveText('Routed chat')
  })

  test('serves settings deep link', async ({ page }) => {
    await page.goto('/settings')
    await expect(page.getByRole('heading', { name: 'General', level: 1 })).toBeVisible()
  })

  test('redirects an unknown path to the chat', async ({ page }) => {
    await page.goto('/does/not/exist')
    await expect(page).toHaveURL(/\/$/)
    await expect(page.getByRole('heading', { name: 'How can I help you today?' })).toBeVisible()
  })

  test('follows browser history back and forward', async ({ page }) => {
    await page.goto('/')
    await page.locator('.history-open').filter({ hasText: 'Routed chat' }).click()
    await expect(page).toHaveURL(/\/chat\/routed$/)

    await page.locator('.profile-row').click()
    await expect(page).toHaveURL(/\/settings$/)

    await page.goBack()
    await expect(page).toHaveURL(/\/chat\/routed$/)
    await expect(page.locator('.topbar-title span')).toHaveText('Routed chat')

    await page.goForward()
    await expect(page).toHaveURL(/\/settings$/)
    await expect(page.getByRole('heading', { name: 'General', level: 1 })).toBeVisible()
  })

})

test.describe('Routing — between conversations', () => {
  test('switching between conversations swaps the transcript', async ({ page }) => {
    await seedConversations(page, [
      conversation('Routed chat', { id: 'routed', updatedAt: 2_000 }),
      conversation('Other chat', { id: 'other', updatedAt: 1_000 }),
    ])
    await page.goto('/chat/routed')
    await expect(page.locator('article.user .message-content')).toHaveText('Routed chat')

    await page.locator('.history-open').filter({ hasText: 'Other chat' }).click()
    await expect(page).toHaveURL(/\/chat\/other$/)
    await expect(page.locator('article.user .message-content')).toHaveText('Other chat')
  })
})
