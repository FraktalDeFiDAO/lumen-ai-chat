import AxeBuilder from '@axe-core/playwright'
import { conversation, expect, seedConversations, seedSettings, test } from './support/fixtures'
import { mockChat } from './support/mocks'

/// Whether the pages can be used by someone not using a mouse or not seeing the
/// screen.
///
/// Every other suite asserts what the app does. This one asserts something the
/// others cannot see at all: nothing in them would fail if a button lost its
/// name, a control lost its label, or the text stopped contrasting with what is
/// behind it.
///
/// Scoped to WCAG 2 A and AA, which is the line most policies draw. Findings are
/// reported with the element and the rule so a failure says what to fix rather
/// than that something, somewhere, is wrong.

const WCAG = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa']

async function audit(page: import('@playwright/test').Page) {
  const results = await new AxeBuilder({ page }).withTags(WCAG).analyze()
  return results.violations.map(violation => ({
    rule: violation.id,
    impact: violation.impact,
    help: violation.help,
    where: violation.nodes.map(node => node.target.join(' ')).slice(0, 4),
  }))
}

test.describe('accessibility', () => {
  test('the empty chat has no violations', async ({ page }) => {
    await page.goto('/')
    await page.getByRole('heading', { name: 'How can I help you today?' }).waitFor()
    expect(await audit(page)).toEqual([])
  })

  test('a conversation with an answer has no violations', async ({ page }) => {
    await seedConversations(page, [conversation('An accessible chat', { id: 'a11y-chat' })])
    await page.goto('/chat/a11y-chat')
    await page.locator('article.assistant').waitFor()
    expect(await audit(page)).toEqual([])
  })

  /// Tool activity, the retrieval note and the incomplete banner are all
  /// generated markup that no other accessibility check would ever reach.
  test('an agent answer with tool activity has no violations', async ({ page }) => {
    await seedConversations(page, [conversation('Agent answer', {
      id: 'agent-chat',
      messages: [
        { role: 'user', content: 'What does main.ts do?' },
        { role: 'assistant', content: 'It creates the app.' },
      ],
    })])
    await page.addInitScript(() => {
      const raw = window.localStorage.getItem('lumen-conversations-v1')
      if (!raw) return
      const all = JSON.parse(raw) as Array<{ messages: Array<Record<string, unknown>> }>
      const answer = all[0]?.messages?.[1]
      if (!answer) return
      answer.contextRecords = 4
      answer.incomplete = true
      answer.tools = [
        { id: 'call-1', name: 'read_file', arguments: { path: 'src/main.ts' }, output: 'export const app = 1' },
        { id: 'call-2', name: 'write_file', arguments: { path: '/etc/passwd' }, output: 'refused', failed: true },
      ]
      window.localStorage.setItem('lumen-conversations-v1', JSON.stringify(all))
    })

    await page.goto('/chat/agent-chat')
    await page.locator('.tool-activity').waitFor()
    expect(await audit(page)).toEqual([])
  })

  test('every settings section has no violations', async ({ page }) => {
    // Six audits in one test, and an axe pass is seconds rather than
    // milliseconds. Splitting them would report better; this is cheaper.
    test.slow()
    await page.goto('/settings')
    const sections = ['General', 'AI coders', 'Agent harness', 'Appearance', 'Notifications', 'Data controls']
    for (const section of sections) {
      await page.getByRole('navigation', { name: 'Settings sections' })
        .getByRole('button', { name: section }).click()
      await page.getByRole('heading', { level: 1, name: section }).waitFor()
      expect(await audit(page), `the ${section} section`).toEqual([])
    }
  })

  test('the dark theme still contrasts', async ({ page }) => {
    // Contrast is the one thing a light-theme pass says nothing about, and the
    // dark palette is a separate set of colours entirely.
    await seedSettings(page, { theme: 'Dark' })
    await seedConversations(page, [conversation('Dark mode chat', { id: 'dark-chat' })])
    await page.goto('/chat/dark-chat')
    await page.locator('article.assistant').waitFor()
    expect(await audit(page)).toEqual([])
  })

  test('the composer and its attachments have no violations', async ({ page }) => {
    mockChat(page)
    await page.goto('/')
    await page.locator('input[type="file"]').setInputFiles({
      name: 'notes.txt', mimeType: 'text/plain', buffer: Buffer.from('Attachment body'),
    })
    await page.locator('.attachment-chip').waitFor()
    expect(await audit(page)).toEqual([])
  })

  /// Somebody navigating by keyboard has to be able to reach the thing that
  /// sends a message, and see where they are while doing it.
  test('the composer is reachable and visible by keyboard alone', async ({ page }) => {
    await page.goto('/')
    await page.getByRole('heading', { name: 'How can I help you today?' }).waitFor()

    const reached = await page.evaluate(() => {
      const focusable = Array.from(
        document.querySelectorAll<HTMLElement>(
          'a[href], button:not([disabled]), input, select, textarea, [tabindex]:not([tabindex="-1"])',
        ),
      ).filter(element => element.offsetParent !== null || element.tagName === 'TEXTAREA')
      const box = document.querySelector('textarea[aria-label="Message Lumen"]')
      return { count: focusable.length, composerIsFocusable: !!box && focusable.includes(box as HTMLElement) }
    })

    expect(reached.composerIsFocusable).toBe(true)
    expect(reached.count).toBeGreaterThan(5)

    await page.getByLabel('Message Lumen').focus()
    const outline = await page.getByLabel('Message Lumen').evaluate(element =>
      getComputedStyle(element).outlineStyle)
    expect(outline).not.toBe('')
  })
})
