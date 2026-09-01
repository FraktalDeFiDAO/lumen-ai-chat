<script setup lang="ts">
import { computed, nextTick, ref } from 'vue'
import { ArrowUp, FileText, Paperclip, Square, X } from 'lucide-vue-next'
import type { Attachment, ChatModel } from '@/types/chat'

const MAX_FILE_SIZE = 256 * 1024
const allowedExtensions = new Set(['txt', 'md', 'markdown', 'json', 'csv', 'ts', 'tsx', 'js', 'jsx', 'vue', 'css', 'scss', 'html', 'xml', 'yaml', 'yml', 'py', 'rs', 'go', 'java', 'c', 'cpp', 'h', 'sh', 'sql', 'toml'])
const props = withDefaults(defineProps<{ generating?: boolean; models?: ChatModel[]; model?: string; modelsError?: string }>(), { models: () => [] })
const emit = defineEmits<{ send: [value: string]; stop: []; 'update:model': [value: string] }>()
const text = ref('')
const textarea = ref<HTMLTextAreaElement | null>(null)
const fileInput = ref<HTMLInputElement | null>(null)
const attachments = ref<Attachment[]>([])
const attachmentError = ref('')
const canSend = computed(() => !!text.value.trim() || attachments.value.length > 0 || props.generating)
const groupedModels = computed(() => {
  const groups = new Map<string, { label: string; models: ChatModel[] }>()
  for (const model of props.models) {
    const key = `${model.ai_coder}/${model.provider}`
    const group = groups.get(key) ?? { label: `${model.ai_coder_name} · ${model.provider_name}`, models: [] }
    group.models.push(model)
    groups.set(key, group)
  }
  return [...groups.entries()].map(([key, group]) => ({ key, ...group }))
})

function resize() { if (textarea.value) { textarea.value.style.height = 'auto'; textarea.value.style.height = `${Math.min(textarea.value.scrollHeight, 180)}px` } }
function submit() {
  if (props.generating) { emit('stop'); return }
  if (!canSend.value) return
  const sections = attachments.value.map(file => `--- Attached file: ${file.name} ---\n${file.content}\n--- End of ${file.name} ---`)
  if (text.value.trim()) sections.unshift(text.value.trim())
  emit('send', sections.join('\n\n'))
  text.value = ''; attachments.value = []; attachmentError.value = ''; void nextTick(resize)
}
function keydown(event: KeyboardEvent) { if (event.key === 'Enter' && !event.shiftKey) { event.preventDefault(); submit() } }
function extension(name: string) { return name.includes('.') ? name.split('.').pop()!.toLowerCase() : '' }
async function chooseFiles(event: Event) {
  attachmentError.value = ''
  const input = event.target as HTMLInputElement
  for (const file of Array.from(input.files ?? [])) {
    if (file.size > MAX_FILE_SIZE) { attachmentError.value = `${file.name} is larger than 256 KB.`; continue }
    if (!(file.type.startsWith('text/') || allowedExtensions.has(extension(file.name)))) { attachmentError.value = `${file.name} is not a supported text or code file.`; continue }
    try {
      attachments.value.push({ id: crypto.randomUUID(), name: file.name, content: await file.text(), size: file.size })
    } catch { attachmentError.value = `Could not read ${file.name}.` }
  }
  input.value = ''
}
</script>

<template>
  <div class="composer-shell">
    <p v-if="modelsError && models.length" class="model-warning" role="status">{{ modelsError }}</p>
    <div v-if="attachments.length" class="attachment-list" aria-label="Attachments">
      <span v-for="file in attachments" :key="file.id" class="attachment-chip"><FileText :size="14" /><span>{{ file.name }}</span><button type="button" :aria-label="`Remove ${file.name}`" @click="attachments = attachments.filter(item => item.id !== file.id)"><X :size="13" /></button></span>
    </div>
    <p v-if="attachmentError" class="composer-error" role="alert">{{ attachmentError }}</p>
    <textarea ref="textarea" v-model="text" rows="1" aria-label="Message Lumen" placeholder="Message Lumen..." @input="resize" @keydown="keydown" />
    <div class="composer-actions">
      <div class="flex items-center gap-1">
        <input ref="fileInput" class="sr-only" type="file" aria-label="Choose text or code files to attach" multiple accept="text/*,.md,.json,.csv,.ts,.tsx,.js,.jsx,.vue,.css,.html,.yaml,.yml,.py,.rs,.go,.java,.c,.cpp,.h,.sh,.sql,.toml" @change="chooseFiles" />
        <button type="button" class="composer-icon" aria-label="Attach text or code files" @click="fileInput?.click()"><Paperclip :size="18" /></button>
        <label class="model-select-label"><span class="sr-only">AI coder, provider, and model</span><select :value="model" :disabled="!models.length" aria-label="AI coder, provider, and model" @change="emit('update:model', ($event.target as HTMLSelectElement).value)"><option v-if="!models.length" value="">{{ modelsError ? 'Models unavailable' : 'Loading models…' }}</option><optgroup v-for="group in groupedModels" :key="group.key" :label="group.label"><option v-for="item in group.models" :key="item.key" :value="item.key">{{ item.key }}</option></optgroup></select></label>
      </div>
      <button type="button" :disabled="!canSend" :class="['send-button', { ready: canSend }]" :aria-label="generating ? 'Stop generating' : 'Send message'" @click="submit"><Square v-if="generating" :size="13" fill="currentColor" /><ArrowUp v-else :size="18" /></button>
    </div>
  </div>
</template>
