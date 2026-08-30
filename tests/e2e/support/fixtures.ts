import { test as base, expect, type Page } from '@playwright/test'

export const CONVERSATION_KEY = 'lumen-conversations-v1'
export const SETTINGS_KEY = 'lumen-settings-v1'
export const MODEL_KEY = 'lumen-model-v1'

export interface SeedMessage {
  id: string
  role: 'user' | 'assistant'
  content: string
  createdAt: number
  feedback?: 'up' | 'down' | null
}

export interface SeedConversation {
  id: string
  title: string
  messages: SeedMessage[]
  createdAt: number
  updatedAt: number
}

/// A conversation shaped exactly like the store persists one, so a seeded page
/// is indistinguishable from one the user built by hand.
export function conversation(
  title: string,
  options: { id?: string; updatedAt?: number; messages?: Array<Partial<SeedMessage> & { role: SeedMessage['role']; content: string }> } = {},
): SeedConversation {
  const createdAt = options.updatedAt ?? Date.now()
  const messages = (options.messages ?? [
    { role: 'user' as const, content: title },
    { role: 'assistant' as const, content: `Answer about ${title}` },
  ]).map((message, index) => ({
    id: message.id ?? `${options.id ?? title}-message-${index}`,
    role: message.role,
    content: message.content,
    createdAt: createdAt + index,
    feedback: message.feedback ?? null,
  }))
  return { id: options.id ?? `conversation-${title.toLowerCase().replace(/\W+/g, '-')}`, title, messages, createdAt, updatedAt: options.updatedAt ?? createdAt }
}

/// Seed browser storage before any application script runs. `addInitScript`
/// re-applies on every navigation in the page, which is what makes a reload
/// assertion meaningful: the page reloads, the app reads what it wrote.
export async function seedStorage(page: Page, entries: Record<string, unknown>) {
  await page.addInitScript(values => {
    for (const [key, value] of Object.entries(values as Record<string, unknown>)) {
      if (window.localStorage.getItem(key) === null) {
        window.localStorage.setItem(key, typeof value === 'string' ? value : JSON.stringify(value))
      }
    }
  }, entries)
}

export async function seedConversations(page: Page, items: SeedConversation[]) {
  await seedStorage(page, { [CONVERSATION_KEY]: items })
}

export async function seedSettings(page: Page, settings: Record<string, unknown>) {
  await seedStorage(page, { [SETTINGS_KEY]: settings })
}

export async function readStorage<T>(page: Page, key: string): Promise<T | null> {
  const raw = await page.evaluate(name => window.localStorage.getItem(name), key)
  return raw === null ? null : (JSON.parse(raw) as T)
}

/// Accept every `confirm()` the page raises. The views guard destructive actions
/// behind one, so a test that does not answer it hangs rather than fails.
export async function acceptDialogs(page: Page) {
  page.on('dialog', dialog => void dialog.accept())
}

export async function dismissDialogs(page: Page) {
  page.on('dialog', dialog => void dialog.dismiss())
}

/// The sidebar is always in the DOM; at phone width it is translated off the
/// left edge instead of being removed, so "hidden" means off-screen.
export async function sidebarOnScreen(page: Page): Promise<boolean> {
  const box = await page.locator('aside.sidebar').boundingBox()
  return !!box && box.x >= 0
}

/// The toggles are styled labels: the checkbox itself is transparent and sits
/// under its own track, so a user clicks the label, and so does this.
export async function toggleSwitch(page: Page, label: string) {
  await page.locator('label.switch').filter({ has: page.getByLabel(label) }).click()
}

type Fixtures = {
  /// Console errors and uncaught exceptions seen so far. A spec that exercises
  /// a happy path asserts this is empty; one that provokes a failure asserts
  /// what it expects to find.
  consoleErrors: string[]
}

export const test = base.extend<Fixtures>({
  consoleErrors: async ({ page }, use) => {
    const errors: string[] = []
    page.on('console', message => { if (message.type() === 'error') errors.push(message.text()) })
    page.on('pageerror', error => errors.push(error.message))
    await use(errors)
  },
})

export { expect }
