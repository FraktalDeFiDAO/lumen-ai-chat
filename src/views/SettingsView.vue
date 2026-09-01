<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { ArrowLeft, Bell, Bot, Brush, Database, RefreshCw, Save, TerminalSquare, Trash2, UserRound, X } from 'lucide-vue-next'
import { useRouter } from 'vue-router'
import { useChatStore } from '@/stores/chat'
import { useSettingsStore } from '@/stores/settings'

const router = useRouter(); const chat = useChatStore(); const preferences = useSettingsStore()
interface CoderStatus { id: string; name: string; status: 'installed_unverified' | 'unavailable'; route_count: number }
interface HarnessStatus { workspace: string; writes_enabled: boolean; max_steps: number; tools: string[]; memory: { records: number; retrieved_per_turn: number; stale?: boolean } | null }
const active = ref<'general' | 'coders' | 'harness' | 'appearance' | 'notifications' | 'data'>('general')
const harness = ref<HarnessStatus | null>(null); const harnessError = ref(''); const indexing = ref(''); 
const editing = ref(false); const draftName = ref(''); const draftEmail = ref('')
const coders = ref<CoderStatus[]>([]); const coderError = ref(''); const coderWrites = ref(false)
const sections = [
  { id: 'general' as const, label: 'General', icon: UserRound },
  { id: 'coders' as const, label: 'AI coders', icon: TerminalSquare },
  { id: 'harness' as const, label: 'Agent harness', icon: Bot },
  { id: 'appearance' as const, label: 'Appearance', icon: Brush },
  { id: 'notifications' as const, label: 'Notifications', icon: Bell },
  { id: 'data' as const, label: 'Data controls', icon: Database },
]
function beginEdit() { draftName.value = preferences.settings.displayName; draftEmail.value = preferences.settings.email; editing.value = true }
function saveProfile() {
  if (!draftName.value.trim()) return
  preferences.settings.displayName = draftName.value.trim()
  preferences.settings.email = draftEmail.value.trim()
  editing.value = false
}
function clearHistory() { if (confirm('Delete all conversations? This cannot be undone.')) chat.clearAll() }
async function loadCoders() {
  try {
    const response = await fetch('/api/coders')
    if (!response.ok) throw new Error(`Unable to load AI coders (${response.status})`)
    const payload = await response.json() as { coders?: CoderStatus[]; allow_writes?: boolean }
    coders.value = payload.coders ?? []
    coderWrites.value = payload.allow_writes === true
  } catch (error) { coderError.value = error instanceof Error ? error.message : 'Unable to load AI coders.' }
}
async function loadHarness() {
  harnessError.value = ''
  try {
    const response = await fetch('/api/harness')
    if (!response.ok) throw new Error(`Unable to load the harness (${response.status})`)
    harness.value = await response.json() as HarnessStatus
  } catch (error) { harnessError.value = error instanceof Error ? error.message : 'Unable to load the harness.' }
}
async function rebuildIndex() {
  indexing.value = 'Indexing…'
  try {
    const response = await fetch('/api/harness/index', { method: 'POST' })
    const payload = await response.json() as { files?: number; records?: number; error?: { message?: string } }
    if (!response.ok) throw new Error(payload.error?.message ?? `Indexing failed (${response.status})`)
    indexing.value = `Indexed ${payload.records ?? 0} record${payload.records === 1 ? '' : 's'} from ${payload.files ?? 0} file${payload.files === 1 ? '' : 's'}.`
    await loadHarness()
  } catch (error) { indexing.value = error instanceof Error ? error.message : 'Indexing failed.' }
}
onMounted(() => { void loadCoders(); void loadHarness(); void chat.ensureHistory() })
</script>
<template>
  <div :class="['settings-page', `theme-${preferences.settings.theme.toLowerCase()}`]">
    <header class="settings-header"><button class="icon-button" aria-label="Go back" @click="router.back()"><ArrowLeft :size="19" /></button><strong>Settings</strong></header>
    <div class="settings-layout">
      <nav aria-label="Settings sections"><button v-for="section in sections" :key="section.id" :class="{ active: active === section.id }" :aria-current="active === section.id ? 'page' : undefined" @click="active = section.id"><component :is="section.icon" :size="17" />{{ section.label }}</button></nav>
      <main>
        <template v-if="active === 'general'">
          <h1>General</h1><p class="settings-lead">Manage your profile and response preferences.</p>
          <section class="settings-card profile-card">
            <template v-if="!editing"><div><span class="avatar large">{{ preferences.initials() }}</span><div><strong>{{ preferences.settings.displayName }}</strong><p>{{ preferences.settings.email || 'No email set' }}</p></div></div><button class="secondary-button" @click="beginEdit">Edit profile</button></template>
            <form v-else class="profile-form" @submit.prevent="saveProfile"><label><span>Display name</span><input v-model="draftName" required maxlength="60" autocomplete="name" /></label><label><span>Email</span><input v-model="draftEmail" type="email" maxlength="120" autocomplete="email" /></label><div class="form-actions"><button type="button" class="secondary-button" @click="editing = false"><X :size="14" />Cancel</button><button class="primary-button"><Save :size="14" />Save</button></div></form>
          </section>
          <h2>Preferences</h2><section class="settings-card stacked"><label><span><strong>Response style</strong><small>Choose how detailed Lumen should be.</small></span><select v-model="preferences.settings.responseStyle" aria-label="Response style"><option>Balanced</option><option>Concise</option><option>Detailed</option></select></label><label><span><strong>Language</strong><small>Used for interface and response preferences.</small></span><select v-model="preferences.settings.language" aria-label="Language"><option>English</option><option>Spanish</option><option>French</option></select></label></section>
          <h2>Generation</h2><section class="settings-card stacked generation-settings"><label><span><strong>Temperature</strong><small>Lower is focused; higher is more varied.</small></span><span class="range-control"><input v-model.number="preferences.settings.temperature" aria-label="Temperature" type="range" min="0" max="2" step="0.1" /><output>{{ preferences.settings.temperature.toFixed(1) }}</output></span></label><label><span><strong>Maximum output tokens</strong><small>Upper bound for each model response.</small></span><input v-model.number="preferences.settings.maxTokens" aria-label="Maximum output tokens" class="number-input" type="number" min="1" max="32768" step="128" /></label><label class="system-prompt-row"><span><strong>Custom system instruction</strong><small>Applied before language and response-style preferences.</small></span><textarea v-model="preferences.settings.systemPrompt" aria-label="Custom system instruction" maxlength="4000" placeholder="You are a helpful expert..." /></label></section>
        </template>
        <template v-else-if="active === 'coders'"><h1>AI coders</h1><p class="settings-lead">Native coding agents available to this gateway.</p><p v-if="coderError" class="coder-error">{{ coderError }}</p><section class="settings-card stacked coder-status-list"><div v-for="coder in coders" :key="coder.id" class="coder-status"><span><strong>{{ coder.name }}</strong><small>{{ coder.route_count }} configured route{{ coder.route_count === 1 ? '' : 's' }}</small></span><span :class="['status-pill', coder.status]">{{ coder.status === 'installed_unverified' ? 'Installed' : 'Unavailable' }}</span></div></section><p class="coder-safety">Workspace mode: <strong>{{ coderWrites ? 'Write access explicitly enabled' : 'Read-only' }}</strong>. Authentication is verified by each native CLI when first used.</p></template>
        <template v-else-if="active === 'harness'">
          <h1>Agent harness</h1><p class="settings-lead">Agent mode runs your conversation through a tool loop: the model reads and edits files in the workspace, and each step is shown in the answer. It works with every hosted provider; a native AI coder CLI runs its own loop instead, and the conversation is passed straight to it.</p>
          <p v-if="harnessError" class="coder-error">{{ harnessError }}</p>
          <section class="settings-card"><div><Bot :size="18" /><div><strong>Agent mode</strong><p>Execute tools instead of only describing them.</p></div></div><label class="switch"><input v-model="preferences.settings.agentMode" type="checkbox" aria-label="Agent mode" /><span aria-hidden="true" /></label></section>
          <template v-if="harness">
            <h2>Tools</h2>
            <section class="settings-card stacked"><div class="harness-tools"><code v-for="tool in harness.tools" :key="tool">{{ tool }}</code></div><p class="agent-note">Workspace <strong>{{ harness.workspace }}</strong>, {{ harness.writes_enabled ? 'writable' : 'read-only' }}. Up to {{ harness.max_steps }} model turns per request.</p></section>
            <h2>Codebase index</h2>
            <section class="settings-card"><div><Database :size="18" /><div><strong>{{ harness.memory ? `${harness.memory.records} indexed records` : 'Retrieval unavailable' }}</strong><p>{{ harness.memory ? `${harness.memory.retrieved_per_turn} records are retrieved before each request.` : 'Set AI_HARNESS_MEMORY_DIR to a writable path to enable codebase search.' }}</p><p v-if="harness.memory?.stale" class="index-stale" role="status">The workspace has changed since this was built, so searches may return code that is no longer there. Rebuild to bring it up to date.</p></div></div><button class="secondary-button" :disabled="!harness.memory" @click="rebuildIndex"><RefreshCw :size="14" />Rebuild</button></section>
            <p v-if="indexing" class="agent-note">{{ indexing }}</p>
          </template>
        </template>
        <template v-else-if="active === 'appearance'"><h1>Appearance</h1><p class="settings-lead">Choose how Lumen looks on this device.</p><section class="settings-card stacked"><label><span><strong>Theme</strong><small>Follow your system or choose a theme.</small></span><select v-model="preferences.settings.theme" aria-label="Theme"><option>System</option><option>Light</option><option>Dark</option></select></label></section></template>
        <template v-else-if="active === 'notifications'"><h1>Notifications</h1><p class="settings-lead">Control non-essential interface notifications.</p><section class="settings-card"><div><Bell :size="18" /><div><strong>Activity notifications</strong><p>Show helpful status and product notices.</p></div></div><label class="switch"><input v-model="preferences.settings.notifications" type="checkbox" aria-label="Activity notifications" /><span aria-hidden="true" /></label></section></template>
        <template v-else><h1>Data controls</h1><p class="settings-lead">Manage conversation data stored in this browser.</p><section class="settings-card"><div><Trash2 :size="18" /><div><strong>Delete all chats</strong><p>Permanently remove {{ chat.conversations.length }} saved conversation{{ chat.conversations.length === 1 ? '' : 's' }}.</p></div></div><button class="danger-button" :disabled="!chat.conversations.length" @click="clearHistory">Delete all</button></section></template>
      </main>
    </div>
  </div>
</template>
