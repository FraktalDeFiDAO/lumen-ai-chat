import { beforeEach, describe, expect, it, vi } from 'vitest'
import { flushPromises, mount } from '@vue/test-utils'
import { createPinia, setActivePinia } from 'pinia'
import { createMemoryHistory, createRouter } from 'vue-router'
import AppSidebar from './AppSidebar.vue'
import { useChatStore } from '@/stores/chat'
import { useSettingsStore } from '@/stores/settings'
import type { Conversation } from '@/types/chat'

function conversation(id: string, title: string, updatedAt: number): Conversation {
  return { id, title, messages: [], createdAt: updatedAt, updatedAt }
}

const router = createRouter({
  history: createMemoryHistory(),
  routes: [
    { path: '/', component: { template: '<div />' } },
    { path: '/chat/:id', component: { template: '<div />' } },
    { path: '/settings', component: { template: '<div />' } },
  ],
})

async function mountSidebar(open = true) {
  await router.push('/')
  await router.isReady()
  const wrapper = mount(AppSidebar, { props: { open }, global: { plugins: [router] } })
  return wrapper
}

describe('AppSidebar', () => {
  beforeEach(() => {
    localStorage.clear()
    setActivePinia(createPinia())
    vi.restoreAllMocks()
  })

  it('says so when there is no history yet', async () => {
    const sidebar = await mountSidebar()
    expect(sidebar.find('.empty-history').text()).toBe('Your conversations will appear here.')
    expect(sidebar.findAll('.history-row')).toHaveLength(0)
  })

  it('lists conversations with the most recently touched first', async () => {
    const store = useChatStore()
    store.conversations = [
      conversation('older', 'Older chat', 1_000),
      conversation('newer', 'Newer chat', 2_000),
    ]
    const sidebar = await mountSidebar()

    expect(sidebar.findAll('.history-open').map(row => row.text())).toEqual(['Newer chat', 'Older chat'])
    expect(sidebar.find('.empty-history').exists()).toBe(false)
  })

  it('filters the list by title, ignoring case', async () => {
    const store = useChatStore()
    store.conversations = [
      conversation('rust', 'Rust error handling', 2_000),
      conversation('vue', 'Vue composition API', 1_000),
    ]
    const sidebar = await mountSidebar()

    await sidebar.find('.search-input').setValue('VUE')
    expect(sidebar.findAll('.history-row')).toHaveLength(1)
    expect(sidebar.find('.history-open').text()).toBe('Vue composition API')

    await sidebar.find('.search-input').setValue('nothing here')
    expect(sidebar.findAll('.history-row')).toHaveLength(0)
  })

  /// Someone looking for the chat where they discussed a thing searches for the
  /// thing, not for whatever the first line happened to be.
  it('finds a conversation by what was said in it, not just its title', async () => {
    const store = useChatStore()
    store.conversations = [
      { ...conversation('a', 'Deployment notes', 2_000), messages: [
        { id: 'm1', role: 'user', content: 'How do I roll back a release?', createdAt: 1, feedback: null },
        { id: 'm2', role: 'assistant', content: 'Tag the previous commit.', createdAt: 2, feedback: null },
      ] },
      { ...conversation('b', 'Unrelated', 1_000), messages: [
        { id: 'm3', role: 'user', content: 'What is for lunch?', createdAt: 1, feedback: null },
      ] },
    ]
    const sidebar = await mountSidebar()

    await sidebar.find('.search-input').setValue('roll back')
    expect(sidebar.findAll('.history-open').map(row => row.text())).toEqual(['Deployment notes'])

    await sidebar.find('.search-input').setValue('lunch')
    expect(sidebar.findAll('.history-open').map(row => row.text())).toEqual(['Unrelated'])
  })

  it('marks the conversation that is open', async () => {
    const store = useChatStore()
    store.conversations = [conversation('a', 'A', 2_000), conversation('b', 'B', 1_000)]
    store.activeId = 'b'
    const sidebar = await mountSidebar()

    const active = sidebar.findAll('.history-row').filter(row => row.classes().includes('active'))
    expect(active).toHaveLength(1)
    expect(active[0].text()).toContain('B')
  })

  it('opens a conversation, routes to it, and closes itself', async () => {
    const store = useChatStore()
    store.conversations = [conversation('chat-a', 'Chat A', 1_000)]
    const sidebar = await mountSidebar()

    await sidebar.find('.history-open').trigger('click')
    await flushPromises()
    expect(store.activeId).toBe('chat-a')
    expect(router.currentRoute.value.path).toBe('/chat/chat-a')
    expect(sidebar.emitted('close')).toHaveLength(1)
  })

  it('starts a new chat from the button and from the brand', async () => {
    const store = useChatStore()
    store.conversations = [conversation('chat-a', 'Chat A', 1_000)]
    store.activeId = 'chat-a'
    const sidebar = await mountSidebar()

    await sidebar.find('.new-chat').trigger('click')
    await flushPromises()
    expect(store.activeId).toBeNull()
    expect(router.currentRoute.value.path).toBe('/')
    expect(sidebar.emitted('close')).toHaveLength(1)

    store.activeId = 'chat-a'
    await sidebar.find('.brand').trigger('click')
    expect(store.activeId).toBeNull()
  })

  it('deletes a conversation by name', async () => {
    const store = useChatStore()
    store.conversations = [conversation('chat-a', 'Chat A', 2_000), conversation('chat-b', 'Chat B', 1_000)]
    const sidebar = await mountSidebar()

    expect(sidebar.find('.row-delete').attributes('aria-label')).toBe('Delete Chat A')
    await sidebar.find('.row-delete').trigger('click')
    expect(store.conversations.map(item => item.id)).toEqual(['chat-b'])
  })

  it('returns to the empty state after deleting the open conversation', async () => {
    const store = useChatStore()
    store.conversations = [conversation('chat-a', 'Chat A', 1_000)]
    store.activeId = 'chat-a'
    await router.push('/chat/chat-a')
    const sidebar = mount(AppSidebar, { props: { open: true }, global: { plugins: [router] } })

    await sidebar.find('.row-delete').trigger('click')
    await flushPromises()
    expect(store.activeId).toBeNull()
    expect(router.currentRoute.value.path).toBe('/')
  })

  it('shows the profile and opens settings', async () => {
    const preferences = useSettingsStore()
    preferences.settings.displayName = 'Riley Chen'
    const sidebar = await mountSidebar()

    expect(sidebar.find('.profile-row .avatar').text()).toBe('RC')
    expect(sidebar.find('.profile-row strong').text()).toBe('Riley Chen')

    await sidebar.find('.profile-row').trigger('click')
    await flushPromises()
    expect(router.currentRoute.value.path).toBe('/settings')
  })

  it('slides off screen when it is closed, and puts a backdrop up when it is open', async () => {
    const closed = await mountSidebar(false)
    expect(closed.find('aside').classes()).toContain('-translate-x-full')
    expect(closed.find('.backdrop-blur-\\[1px\\]').exists()).toBe(false)

    const open = await mountSidebar(true)
    expect(open.find('aside').classes()).toContain('translate-x-0')
    const backdrop = open.find('.backdrop-blur-\\[1px\\]')
    expect(backdrop.exists()).toBe(true)

    await backdrop.trigger('click')
    expect(open.emitted('close')).toHaveLength(1)
  })
})
