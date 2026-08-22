import { spawn } from 'node:child_process'

const cwd = new URL('..', import.meta.url).pathname
const children = []
const logs = new Map()

function start(command, args, env = {}) {
  const child = spawn(command, args, {
    cwd,
    detached: true,
    env: { ...process.env, ...env },
    stdio: ['ignore', 'pipe', 'pipe'],
  })
  children.push(child)
  const lines = []
  logs.set(child, lines)
  for (const stream of [child.stdout, child.stderr]) stream.on('data', data => {
    lines.push(data.toString())
    if (lines.join('').length > 20_000) lines.shift()
  })
  return child
}

async function waitFor(url, child, timeoutMs = 90_000) {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    if (child.exitCode !== null) throw new Error(`Process exited while waiting for ${url}\n${logs.get(child)?.join('')}`)
    try { if ((await fetch(url)).ok) return } catch { /* retry until deadline */ }
    await new Promise(resolve => setTimeout(resolve, 250))
  }
  throw new Error(`Timed out waiting for ${url}\n${logs.get(child)?.join('')}`)
}

function stopAll() {
  for (const child of children.reverse()) {
    if (child.pid && child.exitCode === null) {
      try { process.kill(-child.pid, 'SIGTERM') } catch { /* already stopped */ }
    }
  }
}

try {
  const mock = start('node', ['tests/mock-provider.mjs'])
  await waitFor('http://127.0.0.1:41800/v1/models', mock, 10_000)

  const backend = start('cargo', ['run', '--manifest-path', 'backend/Cargo.toml'], {
    APP_BIND: '127.0.0.1:41788',
    AI_DEFAULT_CODER: 'hermes',
    AI_DEFAULT_PROVIDER: 'openai',
    OPENAI_API_KEY: 'test-openai-key',
    OPENAI_BASE_URL: 'http://127.0.0.1:41800/v1',
    OPENAI_MODEL: 'openai-test',
    ANTHROPIC_API_KEY: 'test-anthropic-key',
    ANTHROPIC_BASE_URL: 'http://127.0.0.1:41800',
    ANTHROPIC_MODEL: 'anthropic-test',
    GEMINI_API_KEY: 'test-gemini-key',
    GEMINI_BASE_URL: 'http://127.0.0.1:41800/v1beta',
    GEMINI_MODEL: 'gemini-test',
    OLLAMA_BASE_URL: 'http://127.0.0.1:41800/ollama/v1',
    OLLAMA_MODEL: 'ollama-test',
    AI_ENDPOINTS_JSON: JSON.stringify([
      { id: 'z-ai', name: 'Z.AI', ai_coder: 'hermes', ai_coder_name: 'Hermes', provider: 'z.ai', base_url: 'http://127.0.0.1:41800/zai/v1', model: 'glm-5.2' },
      { id: 'codex', name: 'Codex', base_url: 'http://127.0.0.1:41800/codex/v1', model: 'gpt-5.6' },
    ]),
  })
  await waitFor('http://127.0.0.1:41788/api/health', backend)

  const providerTest = start('node', ['tests/provider-contract.mjs'])
  await new Promise((resolve, reject) => {
    providerTest.once('exit', code => code === 0 ? resolve() : reject(new Error(logs.get(providerTest)?.join('') || `Provider tests exited ${code}`)))
    providerTest.once('error', reject)
  })
  process.stdout.write(logs.get(providerTest)?.join('') ?? '')

  const web = start('npm', ['run', 'dev:web'])
  await waitFor('http://127.0.0.1:41789', web, 30_000)

  const test = start('node', ['tests/e2e.mjs'])
  await new Promise((resolve, reject) => {
    test.once('exit', code => code === 0 ? resolve() : reject(new Error(logs.get(test)?.join('') || `E2E exited ${code}`)))
    test.once('error', reject)
  })
  process.stdout.write(logs.get(test)?.join('') ?? '')
} finally {
  stopAll()
}
