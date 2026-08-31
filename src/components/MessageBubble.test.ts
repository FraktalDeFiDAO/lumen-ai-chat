import { beforeEach, describe, expect, it, vi } from 'vitest'
import { mount } from '@vue/test-utils'
import MessageBubble from './MessageBubble.vue'
import type { Message } from '@/types/chat'

function message(overrides: Partial<Message> = {}): Message {
  return { id: 'message-1', role: 'assistant', content: 'An answer.', createdAt: 0, feedback: null, ...overrides }
}

function mountBubble(overrides: Partial<Message> = {}, regenerating = false) {
  return mount(MessageBubble, { props: { message: message(overrides), regenerating } })
}

describe('MessageBubble', () => {
  beforeEach(() => {
    vi.restoreAllMocks()
    vi.useRealTimers()
  })

  it('renders the answer and marks its role', () => {
    const bubble = mountBubble()
    expect(bubble.find('.message-content').text()).toBe('An answer.')
    expect(bubble.find('article').classes()).toContain('assistant')
    expect(bubble.find('.assistant-mark').exists()).toBe(true)
  })

  it('gives the user no actions to take on their own message', () => {
    const bubble = mountBubble({ role: 'user', content: 'A question.' })
    expect(bubble.find('article').classes()).toContain('user')
    expect(bubble.find('.message-actions').exists()).toBe(false)
    expect(bubble.find('.assistant-mark').exists()).toBe(false)
  })

  it('copies the answer and says so until the label resets', async () => {
    vi.useFakeTimers()
    const writeText = vi.fn().mockResolvedValue(undefined)
    vi.stubGlobal('navigator', { clipboard: { writeText } })
    const bubble = mountBubble({ content: 'Copy this exactly.' })

    await bubble.find('[aria-label="Copy response"]').trigger('click')
    await vi.waitFor(() => expect(bubble.find('[aria-label="Response copied"]').exists()).toBe(true))
    expect(writeText).toHaveBeenCalledWith('Copy this exactly.')

    vi.advanceTimersByTime(1300)
    await bubble.vm.$nextTick()
    expect(bubble.find('[aria-label="Copy response"]').exists()).toBe(true)
  })

  it('says nothing was copied when the clipboard refuses', async () => {
    vi.stubGlobal('navigator', { clipboard: { writeText: vi.fn().mockRejectedValue(new Error('denied')) } })
    const bubble = mountBubble()

    await bubble.find('[aria-label="Copy response"]').trigger('click')
    await new Promise(resolve => setTimeout(resolve, 0))
    expect(bubble.find('[aria-label="Copy response"]').exists()).toBe(true)
  })

  it('emits feedback for this message and shows which way it went', async () => {
    const bubble = mountBubble({ feedback: 'up' })
    expect(bubble.find('[aria-label="Good response"]').attributes('aria-pressed')).toBe('true')
    expect(bubble.find('[aria-label="Bad response"]').attributes('aria-pressed')).toBe('false')

    await bubble.find('[aria-label="Bad response"]').trigger('click')
    expect(bubble.emitted('feedback')).toEqual([['message-1', 'down']])
  })

  it('asks for a regeneration, and cannot while one is running', async () => {
    const bubble = mountBubble()
    await bubble.find('[aria-label="Regenerate response"]').trigger('click')
    expect(bubble.emitted('regenerate')).toEqual([['message-1']])

    const busy = mountBubble({}, true)
    expect(busy.find('[aria-label="Regenerate response"]').attributes('disabled')).toBeDefined()
  })

  it('shows a failure as an alert beside whatever text arrived', () => {
    const bubble = mountBubble({ error: 'upstream refused', content: 'Unable to generate a response.' })
    const alert = bubble.find('.message-error')
    expect(alert.attributes('role')).toBe('alert')
    expect(alert.text()).toContain('upstream refused')
    expect(bubble.find('article').classes()).toContain('has-error')
  })

  it('counts retrieved records in the singular and the plural', () => {
    expect(mountBubble({ contextRecords: 1 }).find('.harness-context').text())
      .toBe('Retrieved 1 codebase record before answering.')
    expect(mountBubble({ contextRecords: 6 }).find('.harness-context').text())
      .toBe('Retrieved 6 codebase records before answering.')
    expect(mountBubble().find('.harness-context').exists()).toBe(false)
  })

  it('lists each tool the agent ran with its arguments and output', () => {
    const bubble = mountBubble({
      tools: [
        { id: 'call-1', name: 'read_file', arguments: { path: 'src/main.ts' }, output: 'export const app = 1' },
        { id: 'call-2', name: 'write_file', arguments: { path: '/etc/passwd' }, output: 'outside the workspace', failed: true },
        { id: 'call-3', name: 'search_text', arguments: { query: 'TODO' } },
      ],
    })

    const tools = bubble.findAll('.tool-activity details')
    expect(tools).toHaveLength(3)
    expect(tools[0].find('summary strong').text()).toBe('read_file')
    expect(tools[0].find('summary span').text()).toBe('{"path":"src/main.ts"}')
    expect(tools[0].find('pre').text()).toBe('export const app = 1')

    expect(tools[1].classes()).toContain('failed')
    expect(tools[1].find('summary em').text()).toBe('failed')

    expect(tools[2].find('pre').text()).toBe('Running…')
  })

  it('shortens a long argument to one readable line', () => {
    const bubble = mountBubble({
      tools: [{ id: 'call-1', name: 'read_file', arguments: { path: `src/${'deep/'.repeat(60)}file.ts` } }],
    })
    const shown = bubble.find('.tool-activity summary span').text()
    expect(shown).toHaveLength(120)
    expect(shown.endsWith('…')).toBe(true)
  })

  it('renders a string argument as itself and an absent one as nothing', () => {
    expect(mountBubble({ tools: [{ id: 'a', name: 'read_file', arguments: 'src/main.ts' }] })
      .find('.tool-activity summary span').text()).toBe('src/main.ts')
    expect(mountBubble({ tools: [{ id: 'a', name: 'read_file' }] })
      .find('.tool-activity summary span').text()).toBe('')
  })

  it('says when the harness stopped at its step limit', () => {
    const bubble = mountBubble({ incomplete: true })
    expect(bubble.find('.harness-incomplete').text())
      .toBe('The agent reached its step limit, so this answer is incomplete. Ask it to continue.')
    expect(mountBubble().find('.harness-incomplete').exists()).toBe(false)
  })
})
