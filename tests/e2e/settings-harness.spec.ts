import { SETTINGS_KEY, expect, readStorage, seedSettings, test, toggleSwitch } from './support/fixtures'
import { mockAgent, mockFailure, mockJson } from './support/mocks'

/// The AI coders and Agent harness sections report what the server will actually
/// do. They are asserted against the live gateway wherever the answer is real,
/// and against a scripted one only for states a healthy server will not produce.

const openSection = async (page: import('@playwright/test').Page, name: string) => {
  await page.goto('/')
  await page.locator('.profile-row').click()
  await page.getByRole('navigation', { name: 'Settings sections' }).getByRole('button', { name }).click()
}

test.describe('SettingsView — AI coders', () => {
  test('lists every configured coder with its route count and status', async ({ page, consoleErrors }) => {
    await openSection(page, 'AI coders')

    await expect(page.getByRole('heading', { name: 'AI coders', level: 1 })).toBeVisible()
    await expect(page.getByText('Native coding agents available to this gateway.')).toBeVisible()

    const rows = page.locator('.coder-status')
    await expect(rows).toHaveCount(5)
    for (const name of ['Hermes', 'OpenCode', 'KiloCode', 'Claude Code', 'Codex']) {
      await expect(rows.filter({ hasText: name })).toHaveCount(1)
    }
    await expect(page.locator('.status-pill.installed_unverified')).toHaveCount(5)
    await expect(rows.first()).toContainText('1 configured route')
    expect(consoleErrors).toEqual([])
  })

  test('matches what the gateway reports', async ({ page }) => {
    const payload = await (await page.request.get('/api/coders')).json() as { coders: Array<{ name: string; route_count: number }>; allow_writes: boolean }
    await openSection(page, 'AI coders')

    for (const coder of payload.coders) {
      await expect(page.locator('.coder-status').filter({ hasText: coder.name }))
        .toContainText(`${coder.route_count} configured route${coder.route_count === 1 ? '' : 's'}`)
    }
    await expect(page.locator('.coder-safety'))
      .toContainText(payload.allow_writes ? 'Write access explicitly enabled' : 'Read-only')
  })

  test('reports a coder list it could not load', async ({ page }) => {
    await page.route('**/api/coders', route => route.fulfill({ status: 500, contentType: 'application/json', body: '{}' }))
    await openSection(page, 'AI coders')

    await expect(page.locator('.coder-error')).toHaveText('Unable to load AI coders (500)')
    await expect(page.locator('.coder-status')).toHaveCount(0)
  })

  test('says read-only when writes are off and otherwise says they are on', async ({ page }) => {
    await mockJson(page, '**/api/coders', {
      coders: [{ id: 'hermes', name: 'Hermes', status: 'unavailable', route_count: 0 }],
      allow_writes: true,
    })
    await openSection(page, 'AI coders')

    await expect(page.locator('.status-pill.unavailable')).toHaveText('Unavailable')
    await expect(page.locator('.coder-status')).toContainText('0 configured routes')
    await expect(page.locator('.coder-safety')).toContainText('Write access explicitly enabled')
  })
})

test.describe('SettingsView — agent harness', () => {
  test('explains what agent mode does and reports the live harness', async ({ page, consoleErrors }) => {
    const status = await (await page.request.get('/api/harness')).json() as {
      workspace: string
      writes_enabled: boolean
      max_steps: number
      tools: string[]
      memory: { records: number; retrieved_per_turn: number } | null
    }
    await openSection(page, 'Agent harness')

    await expect(page.getByRole('heading', { name: 'Agent harness', level: 1 })).toBeVisible()
    await expect(page.locator('.settings-lead')).toContainText('Agent mode runs your conversation through a tool loop')

    await expect(page.locator('.harness-tools code')).toHaveText(status.tools)
    expect(status.tools).toContain('read_file')
    expect(status.tools).toContain('list_directory')
    expect(status.tools).toContain('search_text')

    const note = page.locator('.agent-note').first()
    await expect(note).toContainText(status.workspace)
    await expect(note).toContainText(status.writes_enabled ? 'writable' : 'read-only')
    await expect(note).toContainText(`Up to ${status.max_steps} model turns per request.`)
    expect(consoleErrors).toEqual([])
  })

  test('a read-only workspace offers no writing tools', async ({ page }) => {
    await openSection(page, 'Agent harness')
    const tools = await page.locator('.harness-tools code').allInnerTexts()
    expect(tools).not.toContain('write_file')
    expect(tools).not.toContain('edit_file')
  })

  test('reports the codebase index and offers to rebuild it', async ({ page }) => {
    await openSection(page, 'Agent harness')

    await expect(page.getByText(/indexed records$/)).toBeVisible()
    await expect(page.getByText(/records are retrieved before each request\./)).toBeVisible()
    await expect(page.getByRole('button', { name: 'Rebuild' })).toBeEnabled()
  })

  test('rebuilding reports what it indexed and refreshes the count', async ({ page }) => {
    await mockJson(page, '**/api/harness', {
      workspace: '/workspace', writes_enabled: false, max_steps: 12,
      tools: ['read_file', 'search_memory'],
      memory: { records: 0, retrieved_per_turn: 6 },
    })
    await openSection(page, 'Agent harness')
    await expect(page.getByText('0 indexed records')).toBeVisible()

    await mockJson(page, '**/api/harness/index', { files: 3, records: 12, skipped: 1 })
    await mockJson(page, '**/api/harness', {
      workspace: '/workspace', writes_enabled: false, max_steps: 12,
      tools: ['read_file', 'search_memory'],
      memory: { records: 12, retrieved_per_turn: 6 },
    })
    await page.getByRole('button', { name: 'Rebuild' }).click()

    await expect(page.getByText('Indexed 12 records from 3 files.')).toBeVisible()
    await expect(page.getByText('12 indexed records')).toBeVisible()
  })

  test('uses the singular for one record from one file', async ({ page }) => {
    await mockJson(page, '**/api/harness', {
      workspace: '/workspace', writes_enabled: false, max_steps: 12,
      tools: ['read_file'], memory: { records: 0, retrieved_per_turn: 6 },
    })
    await openSection(page, 'Agent harness')
    await mockJson(page, '**/api/harness/index', { files: 1, records: 1 })
    await page.getByRole('button', { name: 'Rebuild' }).click()

    await expect(page.getByText('Indexed 1 record from 1 file.')).toBeVisible()
  })

  test('reports a failed rebuild without clearing the page', async ({ page }) => {
    await mockJson(page, '**/api/harness', {
      workspace: '/workspace', writes_enabled: false, max_steps: 12,
      tools: ['read_file'], memory: { records: 4, retrieved_per_turn: 6 },
    })
    await openSection(page, 'Agent harness')

    await mockFailure(page, '**/api/harness/index', 500, 'codebase indexing failed')
    await page.getByRole('button', { name: 'Rebuild' }).click()

    await expect(page.getByText('codebase indexing failed')).toBeVisible()
    await expect(page.getByText('4 indexed records')).toBeVisible()
  })

  test('says retrieval is unavailable when there is no index', async ({ page }) => {
    await mockJson(page, '**/api/harness', {
      workspace: '/workspace', writes_enabled: true, max_steps: 8,
      tools: ['read_file', 'write_file'], memory: null,
    })
    await openSection(page, 'Agent harness')

    await expect(page.getByText('Retrieval unavailable')).toBeVisible()
    await expect(page.getByText('Set AI_HARNESS_MEMORY_DIR to a writable path to enable codebase search.')).toBeVisible()
    await expect(page.getByRole('button', { name: 'Rebuild' })).toBeDisabled()
    await expect(page.locator('.agent-note').first()).toContainText('writable')
  })

  test('reports a harness it could not reach and hides what it cannot describe', async ({ page }) => {
    await page.route('**/api/harness', route => route.fulfill({ status: 503, contentType: 'application/json', body: '{}' }))
    await openSection(page, 'Agent harness')

    await expect(page.locator('.coder-error')).toHaveText('Unable to load the harness (503)')
    await expect(page.locator('.harness-tools')).toHaveCount(0)
    await expect(page.getByRole('button', { name: 'Rebuild' })).toHaveCount(0)
  })

  test('the agent mode switch is off by default, persists, and is what the chat obeys', async ({ page }) => {
    await openSection(page, 'Agent harness')
    const toggle = page.getByLabel('Agent mode')
    await expect(toggle).not.toBeChecked()

    await toggleSwitch(page, 'Agent mode')
    await expect(toggle).toBeChecked()
    expect((await readStorage<{ agentMode: boolean }>(page, SETTINGS_KEY))?.agentMode).toBe(true)

    const agent = mockAgent(page, [{ event: 'outcome', text: 'Ran as an agent.' }])
    await page.goto('/')
    await page.getByLabel('Message Lumen').fill('Use the tools')
    await page.getByLabel('Message Lumen').press('Enter')

    await expect(page.locator('article.assistant .message-content')).toHaveText('Ran as an agent.')
    expect(agent.requests).toHaveLength(1)
  })

  test('agent mode stays on across a reload', async ({ page }) => {
    await seedSettings(page, { agentMode: true })
    await openSection(page, 'Agent harness')
    await expect(page.getByLabel('Agent mode')).toBeChecked()
  })

  /// An index that has quietly stopped matching the tree returns code that is no
  /// longer there. Nothing else would tell the user why an answer looks wrong.
  test('says when the index has fallen behind the workspace', async ({ page }) => {
    await mockJson(page, '**/api/harness', {
      workspace: '/workspace', writes_enabled: false, max_steps: 12,
      tools: ['read_file', 'search_memory'],
      memory: { records: 120, retrieved_per_turn: 6, stale: true },
    })
    await openSection(page, 'Agent harness')

    await expect(page.locator('.index-stale'))
      .toContainText('The workspace has changed since this was built')
    await expect(page.getByRole('button', { name: 'Rebuild' })).toBeEnabled()
  })

  test('says nothing when the index is current', async ({ page }) => {
    await mockJson(page, '**/api/harness', {
      workspace: '/workspace', writes_enabled: false, max_steps: 12,
      tools: ['read_file', 'search_memory'],
      memory: { records: 120, retrieved_per_turn: 6, stale: false },
    })
    await openSection(page, 'Agent harness')

    await expect(page.locator('.index-stale')).toHaveCount(0)
  })
})
