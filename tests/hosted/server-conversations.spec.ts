import { expect, test } from '@playwright/test'

/// The hosted deployment: conversations belong to a person and live on the
/// server, not in a browser.
///
/// Everything here asserts something that is only true in that mode, so the
/// checks are the ones the main suite structurally cannot make: that history
/// survives a browser with nothing in it, that it follows the identity rather
/// than the machine, and that two people never see each other's chats.
///
/// The gateway runs with `AI_IDENTITY_MODE=proxy`, so each context sets the
/// header an identity-aware proxy would have set.

const asUser = (user: string) => ({ extraHTTPHeaders: { 'x-forwarded-user': user } })

/// A conversation created through the UI, returning its id from the URL.
async function startChat(page: import('@playwright/test').Page, message: string) {
  await page.goto('/')
  await page.getByLabel('Message Lumen').fill(message)
  await page.getByLabel('Message Lumen').press('Enter')
  await expect(page.locator('article.assistant .message-content'))
    .toContainText('Mock coder response:', { timeout: 15_000 })
  await expect(page).toHaveURL(/\/chat\/[0-9a-f-]{36}$/)
  return page.url().split('/').pop()!
}

test.describe('server-kept conversations', () => {
  test('a chat survives a browser that remembers nothing', async ({ browser }) => {
    const first = await browser.newContext(asUser('riley'))
    const page = await first.newPage()
    const id = await startChat(page, 'Remember this across browsers')
    // Give the debounced save its moment; the run already flushed, so this only
    // guards against the assertion racing the request.
    await page.waitForTimeout(1_000)
    await first.close()

    // A second context shares no storage with the first: whatever comes back
    // came from the server.
    const second = await browser.newContext(asUser('riley'))
    const fresh = await second.newPage()
    await fresh.goto(`/chat/${id}`)

    await expect(fresh.locator('article.user .message-content'))
      .toHaveText('Remember this across browsers')
    await expect(fresh.locator('article.assistant .message-content'))
      .toContainText('Mock coder response:')
    await expect(fresh.locator('.history-row')).toHaveCount(1)
    await second.close()
  })

  test('the history follows the person, not the machine', async ({ browser }) => {
    const first = await browser.newContext(asUser('morgan'))
    const page = await first.newPage()
    await startChat(page, 'Morgan first question')
    await page.waitForTimeout(1_000)
    await first.close()

    const second = await browser.newContext(asUser('morgan'))
    const elsewhere = await second.newPage()
    await elsewhere.goto('/')

    await expect(elsewhere.locator('.history-row')).toHaveCount(1)
    await expect(elsewhere.locator('.history-open')).toContainText('Morgan first question')
    await second.close()
  })

  /// The property the hosted design rests on, asserted through the browser.
  test('one person never sees another person’s history', async ({ browser }) => {
    const theirs = await browser.newContext(asUser('avery'))
    const page = await theirs.newPage()
    const id = await startChat(page, 'Avery private question')
    await page.waitForTimeout(1_000)
    await theirs.close()

    const stranger = await browser.newContext(asUser('jordan'))
    const other = await stranger.newPage()
    await other.goto('/')
    await expect(other.locator('.empty-history')).toHaveText('Your conversations will appear here.')

    // Even knowing the id is not enough.
    await other.goto(`/chat/${id}`)
    await expect(other.getByRole('heading', { name: 'How can I help you today?' })).toBeVisible()
    await expect(other.locator('article')).toHaveCount(0)
    await stranger.close()
  })

  test('opening an older chat fetches the transcript the list did not carry', async ({ browser }) => {
    const context = await browser.newContext(asUser('sam'))
    const page = await context.newPage()
    await startChat(page, 'Sam first question')
    await page.waitForTimeout(1_000)

    const second = await context.newPage()
    await second.goto('/')
    await expect(second.locator('.history-row')).toHaveCount(1)

    await second.locator('.history-open').click()
    await expect(second.locator('article.user .message-content')).toHaveText('Sam first question')
    await expect(second.locator('article.assistant .message-content')).toContainText('Mock coder response:')
    await context.close()
  })

  test('deleting a chat deletes it for good, not just for this tab', async ({ browser }) => {
    const context = await browser.newContext(asUser('quinn'))
    const page = await context.newPage()
    await startChat(page, 'Quinn disposable question')
    await page.waitForTimeout(1_000)

    page.on('dialog', dialog => void dialog.accept())
    await page.getByRole('button', { name: 'Chat options' }).click()
    await page.getByRole('menuitem', { name: 'Delete chat' }).click()
    await expect(page).toHaveURL(/\/$/)
    await page.waitForTimeout(500)

    const fresh = await (await browser.newContext(asUser('quinn'))).newPage()
    await fresh.goto('/')
    await expect(fresh.locator('.empty-history')).toBeVisible()
    await context.close()
  })

  test('clearing history clears it on the server too', async ({ browser }) => {
    const context = await browser.newContext(asUser('rowan'))
    const page = await context.newPage()
    await startChat(page, 'Rowan first')
    await page.waitForTimeout(1_000)

    page.on('dialog', dialog => void dialog.accept())
    await page.goto('/settings')
    await page.getByRole('navigation', { name: 'Settings sections' })
      .getByRole('button', { name: 'Data controls' }).click()
    await expect(page.getByText('Permanently remove 1 saved conversation.')).toBeVisible()
    await page.getByRole('button', { name: 'Delete all' }).click()
    await page.waitForTimeout(500)

    const fresh = await (await browser.newContext(asUser('rowan'))).newPage()
    await fresh.goto('/')
    await expect(fresh.locator('.empty-history')).toBeVisible()
    await context.close()
  })

  test('the gateway reports who it thinks is asking', async ({ request }) => {
    const response = await request.get('/api/me', {
      headers: { 'x-forwarded-user': 'riley', 'x-forwarded-email': 'riley@example.com' },
    })

    expect(response.ok()).toBe(true)
    const body = await response.json()
    expect(body.user.id).toBe('riley')
    expect(body.user.email).toBe('riley@example.com')
    expect(body.multi_user).toBe(true)
  })

  test('a request with no identity is refused', async ({ request }) => {
    const response = await request.get('/api/conversations')
    expect(response.status()).toBe(401)
    expect((await response.json()).error.type).toBe('unauthenticated')
  })
})
