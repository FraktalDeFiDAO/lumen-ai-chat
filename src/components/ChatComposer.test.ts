import { beforeEach, describe, expect, it, vi } from 'vitest'
import { mount } from '@vue/test-utils'
import ChatComposer from './ChatComposer.vue'
import type { ChatModel } from '@/types/chat'

const models: ChatModel[] = [
  { id: 'm1', key: 'hermes/z.ai/glm-5.2', name: 'GLM 5.2', provider: 'z.ai', provider_name: 'Z.AI', ai_coder: 'hermes', ai_coder_name: 'Hermes' },
  { id: 'm2', key: 'hermes/z.ai/glm-5.1', name: 'GLM 5.1', provider: 'z.ai', provider_name: 'Z.AI', ai_coder: 'hermes', ai_coder_name: 'Hermes' },
  { id: 'm3', key: 'codex/codex/gpt-5.6', name: 'GPT 5.6', provider: 'codex', provider_name: 'Codex', ai_coder: 'codex', ai_coder_name: 'Codex' },
]

function mountComposer(props: Record<string, unknown> = {}) {
  return mount(ChatComposer, { props: { models, model: 'hermes/z.ai/glm-5.2', ...props } })
}

/// jsdom implements `File` without `Blob.text()`, which every browser has and
/// the composer relies on, so the method is supplied here rather than mocking
/// the component's own reading logic away.
function file(name: string, content: string, type = ''): File {
  const value = new File([content], name, { type })
  if (typeof value.text !== 'function') {
    Object.defineProperty(value, 'text', { value: () => Promise.resolve(content) })
  }
  return value
}

/// `setInputFiles` has no jsdom equivalent, so the file list is installed on the
/// real input and the component's own change handler does the rest.
async function attach(wrapper: ReturnType<typeof mountComposer>, files: File[]) {
  const input = wrapper.find('input[type="file"]')
  Object.defineProperty(input.element, 'files', { value: files, writable: true, configurable: true })
  await input.trigger('change')
  await new Promise(resolve => setTimeout(resolve, 0))
  await wrapper.vm.$nextTick()
}

describe('ChatComposer', () => {
  let counter = 0
  beforeEach(() => {
    counter = 0
    vi.stubGlobal('crypto', { randomUUID: () => `file-${++counter}` })
  })

  it('cannot send an empty message', async () => {
    const composer = mountComposer()
    expect(composer.find('.send-button').attributes('disabled')).toBeDefined()

    await composer.find('textarea').setValue('   ')
    expect(composer.find('.send-button').attributes('disabled')).toBeDefined()

    await composer.find('textarea').setValue('Ready')
    expect(composer.find('.send-button').attributes('disabled')).toBeUndefined()
  })

  it('sends the trimmed message and clears the box', async () => {
    const composer = mountComposer()
    await composer.find('textarea').setValue('  Hello there  ')
    await composer.find('.send-button').trigger('click')

    expect(composer.emitted('send')).toEqual([['Hello there']])
    expect((composer.find('textarea').element as HTMLTextAreaElement).value).toBe('')
  })

  it('sends on Enter and keeps writing on Shift+Enter', async () => {
    const composer = mountComposer()
    await composer.find('textarea').setValue('One line')

    await composer.find('textarea').trigger('keydown', { key: 'Enter', shiftKey: true })
    expect(composer.emitted('send')).toBeUndefined()

    await composer.find('textarea').trigger('keydown', { key: 'Enter' })
    expect(composer.emitted('send')).toEqual([['One line']])
  })

  it('turns into a stop button while an answer is streaming', async () => {
    const composer = mountComposer({ generating: true })
    const button = composer.find('.send-button')
    expect(button.attributes('aria-label')).toBe('Stop generating')
    expect(button.attributes('disabled')).toBeUndefined()

    await button.trigger('click')
    expect(composer.emitted('stop')).toHaveLength(1)
    expect(composer.emitted('send')).toBeUndefined()
  })

  it('groups the catalog by coder and provider', () => {
    const composer = mountComposer()
    const groups = composer.findAll('optgroup')
    expect(groups.map(group => group.attributes('label'))).toEqual(['Hermes · Z.AI', 'Codex · Codex'])
    expect(groups[0].findAll('option').map(option => option.text())).toEqual(['GLM 5.2', 'GLM 5.1'])
  })

  /// The group already says which coder and provider a model belongs to, so
  /// repeating them in every option leaves the one thing the user is choosing
  /// -- the model -- buried at the end of a long route key.
  it('names the model rather than repeating its route', () => {
    const options = mountComposer().findAll('option')
    expect(options.map(option => option.text())).not.toContain('hermes/z.ai/glm-5.2')
    expect(options.map(option => option.attributes('value'))).toContain('hermes/z.ai/glm-5.2')
  })

  it('reports the route the user picked', async () => {
    const composer = mountComposer()
    await composer.find('select').setValue('codex/codex/gpt-5.6')
    expect(composer.emitted('update:model')).toEqual([['codex/codex/gpt-5.6']])
  })

  it('disables the picker when there is no catalog, and says why', () => {
    const loading = mountComposer({ models: [] })
    expect(loading.find('select').attributes('disabled')).toBeDefined()
    expect(loading.find('option').text()).toBe('Loading models…')

    const failed = mountComposer({ models: [], modelsError: 'Unable to load models (503)' })
    expect(failed.find('option').text()).toBe('Models unavailable')
    expect(failed.find('.model-warning').exists()).toBe(false)
  })

  it('warns about degraded providers only when it still has models to offer', () => {
    const composer = mountComposer({ modelsError: '1 provider unavailable; fallback models are shown.' })
    expect(composer.find('.model-warning').text()).toBe('1 provider unavailable; fallback models are shown.')
    expect(composer.find('.model-warning').attributes('role')).toBe('status')
  })

  it('accepts a text file, shows it, and lets it be removed', async () => {
    const composer = mountComposer()
    await attach(composer, [file('notes.txt', 'Attachment body', 'text/plain')])

    expect(composer.findAll('.attachment-chip')).toHaveLength(1)
    expect(composer.find('.attachment-chip').text()).toContain('notes.txt')
    expect(composer.find('.send-button').attributes('disabled')).toBeUndefined()

    await composer.find('[aria-label="Remove notes.txt"]').trigger('click')
    expect(composer.findAll('.attachment-chip')).toHaveLength(0)
  })

  it('wraps each attachment so the model can tell where it starts and ends', async () => {
    const composer = mountComposer()
    await composer.find('textarea').setValue('Review this')
    await attach(composer, [file('app.ts', 'const x = 1')])
    await composer.find('.send-button').trigger('click')

    expect(composer.emitted('send')![0][0]).toBe(
      ['Review this', '--- Attached file: app.ts ---\nconst x = 1\n--- End of app.ts ---'].join('\n\n'),
    )
    expect(composer.findAll('.attachment-chip')).toHaveLength(0)
  })

  it('refuses a file larger than 256 KB', async () => {
    const composer = mountComposer()
    await attach(composer, [file('big.txt', 'x'.repeat(256 * 1024 + 1), 'text/plain')])

    expect(composer.find('.composer-error').text()).toBe('big.txt is larger than 256 KB.')
    expect(composer.findAll('.attachment-chip')).toHaveLength(0)
  })

  it('refuses a file that is neither text nor a known code extension', async () => {
    const composer = mountComposer()
    await attach(composer, [file('app.bin', ' ', 'application/octet-stream')])

    expect(composer.find('.composer-error').text()).toBe('app.bin is not a supported text or code file.')
    expect(composer.findAll('.attachment-chip')).toHaveLength(0)
  })

  it('keeps the good files when one in the batch is rejected', async () => {
    const composer = mountComposer()
    await attach(composer, [
      file('good.rs', 'ok'),
      file('bad.exe', 'no', 'application/octet-stream'),
    ])

    expect(composer.findAll('.attachment-chip')).toHaveLength(1)
    expect(composer.find('.attachment-chip').text()).toContain('good.rs')
    expect(composer.find('.composer-error').text()).toBe('bad.exe is not a supported text or code file.')
  })

  it('accepts a file with no extension when the browser calls it text', async () => {
    const composer = mountComposer()
    await attach(composer, [file('LICENSE', 'plain', 'text/plain')])
    expect(composer.findAll('.attachment-chip')).toHaveLength(1)
  })
})
