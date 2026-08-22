# Lumen AI Chat

A polished ChatGPT/Claude-inspired chatbot interface built with Vue 3, Vue Router, Pinia, TypeScript, Tailwind CSS 4, and Vite.

## Run

```bash
npm install
npm run dev
```

Open the URL printed by Vite. Conversations persist in browser localStorage. The current assistant response is intentionally simulated in the Pinia store; replace `sendMessage` with an API streaming client when a model provider is selected.

## Quality gates

```bash
npm run typecheck
npm test
npm run build
```
