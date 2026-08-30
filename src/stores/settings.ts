import { ref, watch } from 'vue'
import { defineStore } from 'pinia'

const STORAGE_KEY = 'lumen-settings-v1'
export type ResponseStyle = 'Balanced' | 'Concise' | 'Detailed'
export type Language = 'English' | 'Spanish' | 'French'
export type Theme = 'System' | 'Light' | 'Dark'

interface Settings {
  displayName: string
  email: string
  responseStyle: ResponseStyle
  language: Language
  theme: Theme
  notifications: boolean
  temperature: number
  maxTokens: number
  systemPrompt: string
  agentMode: boolean
}

const defaults: Settings = { displayName: 'Alex Morgan', email: 'alex@example.com', responseStyle: 'Balanced', language: 'English', theme: 'System', notifications: true, temperature: 0.7, maxTokens: 4096, systemPrompt: '', agentMode: false }
function load(): Settings {
  try {
    const stored = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? '{}') as Partial<Settings> | null
    return stored && typeof stored === 'object' ? { ...defaults, ...stored } : { ...defaults }
  } catch { return { ...defaults } }
}

export const useSettingsStore = defineStore('settings', () => {
  const settings = ref<Settings>(load())
  watch(settings, value => localStorage.setItem(STORAGE_KEY, JSON.stringify(value)), { deep: true })
  const media = typeof window.matchMedia === 'function' ? window.matchMedia('(prefers-color-scheme: dark)') : null
  function applyTheme() {
    const dark = settings.value.theme === 'Dark'
      || (settings.value.theme === 'System' && !!media?.matches)
    document.documentElement.classList.toggle('theme-dark', dark)
    document.documentElement.style.colorScheme = dark ? 'dark' : 'light'
  }
  watch(() => settings.value.theme, applyTheme, { immediate: true })
  media?.addEventListener('change', () => { if (settings.value.theme === 'System') applyTheme() })
  const initials = () => settings.value.displayName.trim().split(/\s+/).map(part => part[0]).join('').slice(0, 2).toUpperCase() || 'U'
  return { settings, initials }
})
