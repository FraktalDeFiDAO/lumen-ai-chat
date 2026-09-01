import { describe, expect, it } from 'vitest'
import { flushPromises, mount } from '@vue/test-utils'
import { createMemoryHistory, createRouter } from 'vue-router'
import { defineComponent, h } from 'vue'
import App from './App.vue'

/// A component that throws while rendering, which is what unmounts the tree and
/// leaves a blank page.
const Exploding = defineComponent({
  setup() {
    return () => {
      throw new Error('the view could not be rendered')
    }
  },
})

const Fine = defineComponent({ setup: () => () => h('p', 'the chat') })

function routerWith(component: ReturnType<typeof defineComponent>) {
  return createRouter({ history: createMemoryHistory(), routes: [{ path: '/', component }] })
}

describe('App', () => {
  it('renders the route when nothing is wrong', async () => {
    const router = routerWith(Fine)
    await router.push('/')
    await router.isReady()
    const app = mount(App, { global: { plugins: [router] } })

    expect(app.text()).toContain('the chat')
    expect(app.find('.app-failure').exists()).toBe(false)
  })

  /// Without this the page goes blank: nothing said, nothing to click, and no
  /// way for the user to know their conversations are still there.
  it('shows what went wrong instead of a blank page', async () => {
    const router = routerWith(Exploding)
    await router.push('/')
    await router.isReady()
    const app = mount(App, { global: { plugins: [router] } })
    // The boundary sets state during the failed render; the replacement view is
    // the update that follows it.
    await flushPromises()

    const failure = app.find('.app-failure')
    expect(failure.exists()).toBe(true)
    expect(failure.attributes('role')).toBe('alert')
    expect(failure.text()).toContain('the view could not be rendered')
    expect(failure.text()).toContain('Your conversations are safe')
    expect(app.find('.app-failure-actions').findAll('button')).toHaveLength(2)
  })
})
