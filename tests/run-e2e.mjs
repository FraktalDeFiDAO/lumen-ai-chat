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
    AI_PROVIDER: 'openai-compatible',
    AI_BASE_URL: 'http://127.0.0.1:41800/v1',
    AI_MODEL: 'lumen-test',
  })
  await waitFor('http://127.0.0.1:41788/api/health', backend)

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
