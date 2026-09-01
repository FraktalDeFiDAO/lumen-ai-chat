<script setup lang="ts">
import { onErrorCaptured, ref } from 'vue'
import { RouterView, useRouter } from 'vue-router'

/// The last resort.
///
/// An uncaught error while rendering unmounts the tree: the page goes blank,
/// with nothing said and nothing to click. Whatever went wrong, that is worse
/// than the thing that went wrong -- a conversation is still on disk, and the
/// user has no way to know that.
///
/// This catches it, says so, and offers the two things that actually recover:
/// going back to the chat list, and reloading.
const failure = ref('')
const router = useRouter()

onErrorCaptured(error => {
  failure.value = error instanceof Error ? error.message : 'Something went wrong.'
  // Stop it propagating: the point is that the tree survives.
  return false
})

async function backToChats() {
  failure.value = ''
  await router.push('/')
}

function reload() {
  window.location.reload()
}
</script>

<template>
  <div v-if="failure" class="app-failure" role="alert">
    <div>
      <h1>Something went wrong on this page</h1>
      <p>Your conversations are safe — this is a fault in the interface, not in what it was showing.</p>
      <p class="app-failure-detail">{{ failure }}</p>
      <div class="app-failure-actions">
        <button type="button" class="primary-button" @click="backToChats">Back to your chats</button>
        <button type="button" class="secondary-button" @click="reload">Reload</button>
      </div>
    </div>
  </div>
  <RouterView v-else />
</template>
