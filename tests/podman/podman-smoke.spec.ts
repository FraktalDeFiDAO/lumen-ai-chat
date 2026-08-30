import { expect, test } from '../e2e/support/fixtures'

/// A smoke test against the real Compose deployment: production images, the
/// hardened containers, and a real Hermes invocation rather than a mock.
///
/// Everything else in this repository runs against mocks, which is what makes
/// them fast and deterministic. This is the one suite that answers whether the
/// thing that actually ships works, so it asserts the few claims that only a
/// real deployment can settle and nothing more.

test.describe('the Compose deployment', () => {
  test('serves the production build with the deployment default route', async ({ page, consoleErrors }) => {
    await page.goto('/', { waitUntil: 'networkidle' })

    await expect(page).toHaveTitle('Lumen AI')
    await expect(page.getByRole('heading', { name: 'How can I help you today?' })).toBeVisible()

    // No AI_CODER_ROUTES_JSON in the deployment, so every coder contributes its
    // own default route and Hermes leads.
    const selector = page.getByLabel('AI coder, provider, and model')
    await expect(selector).toHaveValue('hermes/auto/default')
    await expect(selector.locator('optgroup')).toHaveCount(5)
    expect(consoleErrors).toEqual([])
  })

  test('streams an answer from the real Hermes CLI', async ({ page }) => {
    test.slow()
    await page.goto('/', { waitUntil: 'networkidle' })

    await page.getByLabel('Message Lumen')
      .fill('Reply with exactly PODMAN_UI_OK. Do not use tools or modify files.')
    await page.getByRole('button', { name: 'Send message' }).click()

    await expect(page.getByText('PODMAN_UI_OK', { exact: true })).toBeVisible({ timeout: 120_000 })
  })

  test('reports all five coders as installed inside the container', async ({ page, consoleErrors }) => {
    await page.goto('/settings', { waitUntil: 'networkidle' })
    await page.getByRole('navigation', { name: 'Settings sections' }).getByRole('button', { name: 'AI coders' }).click()

    for (const name of ['Hermes', 'OpenCode', 'KiloCode', 'Claude Code', 'Codex']) {
      await expect(page.locator('.coder-status').filter({ hasText: name })).toHaveCount(1)
    }
    await expect(page.locator('.status-pill.installed_unverified')).toHaveCount(5)
    expect(consoleErrors).toEqual([])
  })
})
