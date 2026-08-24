import { spawn } from 'node:child_process'
import { reservePorts } from './ports.mjs'

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

async function stopAll() {
  const running = children.reverse().filter(child => child.pid && child.exitCode === null)
  const exits = running.map(child => new Promise(resolve => child.once('exit', resolve)))
  for (const child of running) {
    if (child.pid && child.exitCode === null) {
      try { process.kill(-child.pid, 'SIGTERM') } catch { /* already stopped */ }
    }
  }
  await Promise.race([
    Promise.all(exits),
    new Promise(resolve => setTimeout(resolve, 5000)),
  ])
  for (const child of running) {
    if (child.exitCode === null && child.pid) {
      try { process.kill(-child.pid, 'SIGKILL') } catch { /* already stopped */ }
    }
  }
}

try {
  const [mockPort, apiPort, webPort] = await reservePorts(3)
  const mockURL = `http://127.0.0.1:${mockPort}`
  const apiURL = `http://127.0.0.1:${apiPort}`
  const webURL = `http://127.0.0.1:${webPort}`
  const configGuards = start('node', ['tests/config-guards.mjs'])
  await new Promise((resolve, reject) => {
    configGuards.once('exit', code => code === 0 ? resolve() : reject(new Error(logs.get(configGuards)?.join('') || `Configuration guards exited ${code}`)))
    configGuards.once('error', reject)
  })
  process.stdout.write(logs.get(configGuards)?.join('') ?? '')

  const mock = start('node', ['tests/mock-provider.mjs'], { MOCK_AI_PORT: String(mockPort) })
  await waitFor(`${mockURL}/v1/models`, mock, 10_000)

  const backend = start('cargo', ['run', '--manifest-path', 'backend/Cargo.toml'], {
    APP_BIND: `127.0.0.1:${apiPort}`,
    AI_DEFAULT_CODER: 'hermes',
    AI_DEFAULT_PROVIDER: 'z.ai',
    AI_CODER_WORKSPACE: cwd,
    AI_CODER_HERMES_BIN: `${cwd}tests/mock-coder.mjs`,
    AI_CODER_OPENCODE_BIN: `${cwd}tests/mock-coder.mjs`,
    AI_CODER_KILOCODE_BIN: `${cwd}tests/mock-coder.mjs`,
    AI_CODER_CLAUDE_BIN: `${cwd}tests/mock-coder.mjs`,
    AI_CODER_CODEX_BIN: `${cwd}tests/mock-coder.mjs`,
    AI_CODER_ROUTES_JSON: JSON.stringify([
      { coder: 'hermes', provider: 'z.ai', provider_name: 'Z.AI', model: 'glm-5.2', model_name: 'GLM 5.2' },
      { coder: 'opencode', provider: 'openrouter', provider_name: 'OpenRouter', model: 'qwen/test' },
      { coder: 'kilocode', provider: 'kilocode', provider_name: 'KiloCode', model: 'kilo-test' },
      { coder: 'claude', provider: 'claude', provider_name: 'Claude', model: 'sonnet-test' },
      { coder: 'codex', provider: 'codex', provider_name: 'Codex', model: 'gpt-test' },
    ]),
    OPENAI_API_KEY: 'test-openai-key',
    OPENAI_BASE_URL: `${mockURL}/v1`,
    OPENAI_MODEL: 'openai-test',
    ANTHROPIC_API_KEY: 'test-anthropic-key',
    ANTHROPIC_BASE_URL: mockURL,
    ANTHROPIC_MODEL: 'anthropic-test',
    GEMINI_API_KEY: 'test-gemini-key',
    GEMINI_BASE_URL: `${mockURL}/v1beta`,
    GEMINI_MODEL: 'gemini-test',
    OLLAMA_BASE_URL: `${mockURL}/ollama/v1`,
    OLLAMA_MODEL: 'ollama-test',
    OLLAMA_ENABLED: 'true',
  })
  await waitFor(`${apiURL}/api/ready`, backend)

  const providerTest = start('node', ['tests/provider-contract.mjs'], { API_URL: apiURL })
  await new Promise((resolve, reject) => {
    providerTest.once('exit', code => code === 0 ? resolve() : reject(new Error(logs.get(providerTest)?.join('') || `Provider tests exited ${code}`)))
    providerTest.once('error', reject)
  })
  process.stdout.write(logs.get(providerTest)?.join('') ?? '')

  const web = start('npm', ['run', 'dev:web', '--', '--port', String(webPort), '--strictPort'], {
    VITE_API_TARGET: apiURL,
  })
  await waitFor(webURL, web, 30_000)

  const test = start('node', ['tests/e2e.mjs'], { BASE_URL: webURL })
  await new Promise((resolve, reject) => {
    test.once('exit', code => code === 0 ? resolve() : reject(new Error(logs.get(test)?.join('') || `E2E exited ${code}`)))
    test.once('error', reject)
  })
  process.stdout.write(logs.get(test)?.join('') ?? '')
} finally {
  await stopAll()
}
