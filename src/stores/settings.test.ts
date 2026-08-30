import { beforeEach, describe, expect, it, vi } from 'vitest'
import { createPinia, setActivePinia } from 'pinia'
import { nextTick } from 'vue'
import { useSettingsStore } from './settings'

const STORAGE_KEY = 'lumen-settings-v1'

function stubMatchMedia(matches: boolean) {
  const listeners: Array<() => void> = []
  const media = {
    matches,
    addEventListener: (_event: string, listener: () => void) => { listeners.push(listener) },
  }
  vi.stubGlobal('matchMedia', () => media)
  return { media, fire: () => listeners.forEach(listener => listener()) }
}

describe('settings store', () => {
  beforeEach(() => {
    localStorage.clear()
    document.documentElement.className = ''
    document.documentElement.style.colorScheme = ''
    setActivePinia(createPinia())
    vi.unstubAllGlobals()
  })

  it('starts from documented defaults', () => {
    const { settings } = useSettingsStore()
    expect(settings).toMatchObject({
      displayName: 'Alex Morgan',
      email: 'alex@example.com',
      responseStyle: 'Balanced',
      language: 'English',
      theme: 'System',
      notifications: true,
      temperature: 0.7,
      maxTokens: 4096,
      systemPrompt: '',
      agentMode: false,
    })
  })

  it('keeps a stored value and fills the rest from the defaults', () => {
    localStorage.setItem(STORAGE_KEY, JSON.stringify({ displayName: 'Riley Chen', agentMode: true }))
    const { settings } = useSettingsStore()
    expect(settings.displayName).toBe('Riley Chen')
    expect(settings.agentMode).toBe(true)
    expect(settings.temperature).toBe(0.7)
  })

  it('falls back to the defaults when storage holds something unreadable', () => {
    localStorage.setItem(STORAGE_KEY, 'not json at all')
    expect(useSettingsStore().settings.displayName).toBe('Alex Morgan')
  })

  it('writes every change back to this browser', async () => {
    const { settings } = useSettingsStore()
    settings.maxTokens = 512
    settings.systemPrompt = 'Be brief.'
    await nextTick()
    expect(JSON.parse(localStorage.getItem(STORAGE_KEY)!)).toMatchObject({ maxTokens: 512, systemPrompt: 'Be brief.' })
  })

  it('builds initials from the display name', () => {
    const store = useSettingsStore()
    expect(store.initials()).toBe('AM')

    store.settings.displayName = 'ada lovelace king'
    expect(store.initials()).toBe('AL')

    store.settings.displayName = 'Cher'
    expect(store.initials()).toBe('C')

    store.settings.displayName = '   '
    expect(store.initials()).toBe('U')
  })

  it('applies a dark theme to the document as soon as it is chosen', async () => {
    const store = useSettingsStore()
    expect(document.documentElement.classList.contains('theme-dark')).toBe(false)

    store.settings.theme = 'Dark'
    await nextTick()
    expect(document.documentElement.classList.contains('theme-dark')).toBe(true)
    expect(document.documentElement.style.colorScheme).toBe('dark')

    store.settings.theme = 'Light'
    await nextTick()
    expect(document.documentElement.classList.contains('theme-dark')).toBe(false)
    expect(document.documentElement.style.colorScheme).toBe('light')
  })

  it('follows the system preference only while the theme is System', async () => {
    const { fire, media } = stubMatchMedia(true)
    const store = useSettingsStore()
    expect(document.documentElement.classList.contains('theme-dark')).toBe(true)

    store.settings.theme = 'Light'
    await nextTick()
    expect(document.documentElement.classList.contains('theme-dark')).toBe(false)

    media.matches = false
    fire()
    expect(document.documentElement.classList.contains('theme-dark')).toBe(false)

    store.settings.theme = 'System'
    await nextTick()
    media.matches = true
    fire()
    expect(document.documentElement.classList.contains('theme-dark')).toBe(true)
  })

  it('treats a missing matchMedia as a light system', () => {
    vi.stubGlobal('matchMedia', undefined)
    useSettingsStore()
    expect(document.documentElement.classList.contains('theme-dark')).toBe(false)
  })
})
