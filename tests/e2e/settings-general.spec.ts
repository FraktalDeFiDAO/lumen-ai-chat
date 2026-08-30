import { CONVERSATION_KEY, SETTINGS_KEY, acceptDialogs, conversation, expect, readStorage, seedConversations, seedSettings, test } from './support/fixtures'
import { mockChat } from './support/mocks'

/// SettingsView exists so a user can change how Lumen behaves and see the change
/// take effect. Each section is asserted on its own terms, and the preferences
/// that shape a request are followed all the way onto the wire.

interface StoredSettings {
  displayName: string
  email: string
  responseStyle: string
  language: string
  theme: string
  notifications: boolean
  temperature: number
  maxTokens: number
  systemPrompt: string
  agentMode: boolean
}

test.describe('SettingsView — navigation', () => {
  test('opens on General and marks the section it is showing', async ({ page, consoleErrors }) => {
    await page.goto('/settings')

    await expect(page.getByRole('heading', { name: 'General', level: 1 })).toBeVisible()
    await expect(page.getByText('Manage your profile and response preferences.')).toBeVisible()

    const nav = page.getByRole('navigation', { name: 'Settings sections' })
    await expect(nav.getByRole('button')).toHaveText([
      'General', 'AI coders', 'Agent harness', 'Appearance', 'Notifications', 'Data controls',
    ])
    await expect(nav.getByRole('button', { name: 'General' })).toHaveAttribute('aria-current', 'page')

    await nav.getByRole('button', { name: 'Appearance' }).click()
    await expect(nav.getByRole('button', { name: 'Appearance' })).toHaveAttribute('aria-current', 'page')
    await expect(nav.getByRole('button', { name: 'General' })).not.toHaveAttribute('aria-current', 'page')
    expect(consoleErrors).toEqual([])
  })

  test('the back button returns where the user came from', async ({ page }) => {
    await page.goto('/')
    await page.locator('.profile-row').click()
    await expect(page).toHaveURL(/\/settings$/)

    await page.getByRole('button', { name: 'Go back' }).click()
    await expect(page).toHaveURL(/\/$/)
    await expect(page.getByRole('heading', { name: 'How can I help you today?' })).toBeVisible()
  })
})

test.describe('SettingsView — profile', () => {
  test('shows the stored profile and its initials', async ({ page }) => {
    await seedSettings(page, { displayName: 'Alex Morgan', email: 'alex@example.com' })
    await page.goto('/settings')

    await expect(page.locator('.avatar.large')).toHaveText('AM')
    await expect(page.locator('.profile-card strong')).toHaveText('Alex Morgan')
    await expect(page.locator('.profile-card p')).toHaveText('alex@example.com')
  })

  test('edits the profile and shows it everywhere at once', async ({ page }) => {
    await page.goto('/')
    await page.locator('.profile-row').click()
    await page.getByRole('button', { name: 'Edit profile' }).click()

    await page.getByLabel('Display name').fill('Test User')
    await page.getByLabel('Email').fill('test@example.com')
    await page.getByRole('button', { name: 'Save' }).click()

    await expect(page.locator('.profile-card strong')).toHaveText('Test User')
    await expect(page.locator('.avatar.large')).toHaveText('TU')

    const stored = await readStorage<StoredSettings>(page, SETTINGS_KEY)
    expect(stored).toMatchObject({ displayName: 'Test User', email: 'test@example.com' })

    await page.getByRole('button', { name: 'Go back' }).click()
    await expect(page.locator('.profile-row strong')).toHaveText('Test User')
  })

  test('cancelling an edit changes nothing', async ({ page }) => {
    await seedSettings(page, { displayName: 'Alex Morgan' })
    await page.goto('/settings')

    await page.getByRole('button', { name: 'Edit profile' }).click()
    await page.getByLabel('Display name').fill('Discarded')
    await page.getByRole('button', { name: 'Cancel' }).click()

    await expect(page.locator('.profile-card strong')).toHaveText('Alex Morgan')
  })

  test('refuses to save an empty display name', async ({ page }) => {
    await page.goto('/settings')
    await page.getByRole('button', { name: 'Edit profile' }).click()
    await page.getByLabel('Display name').fill('')
    await page.getByRole('button', { name: 'Save' }).click()

    await expect(page.getByLabel('Display name')).toBeVisible()
    await expect(page.locator('.profile-card strong')).toHaveCount(0)
  })

  test('shows a placeholder when no email is set', async ({ page }) => {
    await seedSettings(page, { displayName: 'No Email', email: '' })
    await page.goto('/settings')
    await expect(page.locator('.profile-card p')).toHaveText('No email set')
  })
})

test.describe('SettingsView — preferences and generation', () => {
  test('persists every preference across a reload', async ({ page }) => {
    await page.goto('/settings')

    await page.getByLabel('Response style').selectOption('Concise')
    await page.getByLabel('Language').selectOption('Spanish')
    await page.getByLabel('Temperature').fill('0.4')
    await page.getByLabel('Maximum output tokens').fill('2048')
    await page.getByLabel('Custom system instruction').fill('Prefer actionable answers.')

    await expect(page.locator('.generation-settings output')).toHaveText('0.4')

    await page.reload()
    await expect(page.getByLabel('Response style')).toHaveValue('Concise')
    await expect(page.getByLabel('Language')).toHaveValue('Spanish')
    await expect(page.getByLabel('Temperature')).toHaveValue('0.4')
    await expect(page.getByLabel('Maximum output tokens')).toHaveValue('2048')
    await expect(page.getByLabel('Custom system instruction')).toHaveValue('Prefer actionable answers.')
  })

  test('those preferences are what the next request carries', async ({ page }) => {
    await seedSettings(page, {
      responseStyle: 'Concise',
      language: 'Spanish',
      temperature: 0.3,
      maxTokens: 512,
      systemPrompt: 'Prefer actionable answers.',
    })
    const chat = mockChat(page, 'Respuesta.')
    await page.goto('/')
    await page.getByLabel('Message Lumen').fill('Hola')
    await page.getByLabel('Message Lumen').press('Enter')
    await expect(page.locator('article.assistant .message-content')).toHaveText('Respuesta.')

    expect(chat.requests[0]).toMatchObject({ temperature: 0.3, max_tokens: 512 })
    expect((chat.requests[0] as { messages: Array<{ role: string; content: string }> }).messages[0]).toEqual({
      role: 'system',
      content: 'Prefer actionable answers.\n\nRespond in Spanish. Use a concise level of detail.',
    })
  })

  test('sends only the built-in instruction when no custom one is set', async ({ page }) => {
    const chat = mockChat(page)
    await page.goto('/')
    await page.getByLabel('Message Lumen').fill('Hello')
    await page.getByLabel('Message Lumen').press('Enter')
    await expect(page.locator('article.assistant .message-content')).toHaveText('Mocked assistant reply.')

    expect((chat.requests[0] as { messages: Array<{ content: string }> }).messages[0]?.content)
      .toBe('Respond in English. Use a balanced level of detail.')
  })

  test('keeps the temperature output in step with the slider', async ({ page }) => {
    await page.goto('/settings')
    await expect(page.locator('.generation-settings output')).toHaveText('0.7')
    await page.getByLabel('Temperature').fill('1.5')
    await expect(page.locator('.generation-settings output')).toHaveText('1.5')
    await page.getByLabel('Temperature').fill('0')
    await expect(page.locator('.generation-settings output')).toHaveText('0.0')
  })
})

test.describe('SettingsView — appearance', () => {
  test('applies a dark theme immediately and after a reload', async ({ page }) => {
    await page.goto('/settings')
    await page.getByRole('navigation', { name: 'Settings sections' }).getByRole('button', { name: 'Appearance' }).click()

    await page.getByLabel('Theme').selectOption('Dark')
    await expect(page.locator('html')).toHaveClass(/theme-dark/)
    expect(await page.evaluate(() => document.documentElement.style.colorScheme)).toBe('dark')

    await page.reload()
    await expect(page.locator('html')).toHaveClass(/theme-dark/)
  })

  test('a light theme ignores a dark system preference', async ({ page }) => {
    await page.emulateMedia({ colorScheme: 'dark' })
    await seedSettings(page, { theme: 'Light' })
    await page.goto('/settings')

    await expect(page.locator('html')).not.toHaveClass(/theme-dark/)
    expect(await page.evaluate(() => document.documentElement.style.colorScheme)).toBe('light')
  })

  test('the system theme follows the operating system', async ({ page }) => {
    await page.emulateMedia({ colorScheme: 'dark' })
    await seedSettings(page, { theme: 'System' })
    await page.goto('/settings')
    await expect(page.locator('html')).toHaveClass(/theme-dark/)

    await page.emulateMedia({ colorScheme: 'light' })
    await expect(page.locator('html')).not.toHaveClass(/theme-dark/)
  })
})

test.describe('SettingsView — notifications', () => {
  test('toggles and remembers the notification preference', async ({ page }) => {
    await page.goto('/settings')
    await page.getByRole('navigation', { name: 'Settings sections' }).getByRole('button', { name: 'Notifications' }).click()

    const toggle = page.locator('.switch input[type="checkbox"]')
    await expect(toggle).toBeChecked()
    await page.locator('label.switch').click()
    await expect(toggle).not.toBeChecked()

    expect((await readStorage<StoredSettings>(page, SETTINGS_KEY))?.notifications).toBe(false)
    await page.reload()
    await page.getByRole('navigation', { name: 'Settings sections' }).getByRole('button', { name: 'Notifications' }).click()
    await expect(page.locator('.switch input[type="checkbox"]')).not.toBeChecked()
  })
})

test.describe('SettingsView — data controls', () => {
  const openDataControls = async (page: import('@playwright/test').Page) => {
    await page.goto('/')
    await page.locator('.profile-row').click()
    await page.getByRole('navigation', { name: 'Settings sections' }).getByRole('button', { name: 'Data controls' }).click()
  }

  test('cannot delete anything when there is nothing stored', async ({ page }) => {
    await openDataControls(page)
    await expect(page.getByText('Permanently remove 0 saved conversations.')).toBeVisible()
    await expect(page.getByRole('button', { name: 'Delete all' })).toBeDisabled()
  })

  test('counts a single conversation in the singular', async ({ page }) => {
    await seedConversations(page, [conversation('Only one', { id: 'only-one' })])
    await openDataControls(page)
    await expect(page.getByText('Permanently remove 1 saved conversation.')).toBeVisible()
  })

  test('deletes every conversation once confirmed', async ({ page }) => {
    await seedConversations(page, [
      conversation('First', { id: 'first' }),
      conversation('Second', { id: 'second' }),
    ])
    await acceptDialogs(page)
    await openDataControls(page)

    await expect(page.getByText('Permanently remove 2 saved conversations.')).toBeVisible()
    await page.getByRole('button', { name: 'Delete all' }).click()

    await expect(page.getByText('Permanently remove 0 saved conversations.')).toBeVisible()
    await expect(page.getByRole('button', { name: 'Delete all' })).toBeDisabled()
    expect(await readStorage(page, CONVERSATION_KEY)).toEqual([])

    await page.getByRole('button', { name: 'Go back' }).click()
    await expect(page.locator('.empty-history')).toHaveText('Your conversations will appear here.')
  })

  test('keeps everything when the confirmation is dismissed', async ({ page }) => {
    await seedConversations(page, [conversation('Keep me', { id: 'keep-me' })])
    page.on('dialog', dialog => void dialog.dismiss())
    await openDataControls(page)

    await page.getByRole('button', { name: 'Delete all' }).click()
    await expect(page.getByText('Permanently remove 1 saved conversation.')).toBeVisible()
  })
})
