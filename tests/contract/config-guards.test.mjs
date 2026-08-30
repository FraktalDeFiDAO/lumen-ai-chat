/// Guards on what configuration alone is allowed to switch on.
///
/// Naming a local model is not the same as asking for local inference, and the
/// difference matters: a stray `OLLAMA_MODEL` in an operator's environment must
/// not silently give the service a backend it was never configured with. These
/// start the real binary against a cleaned environment and assert what it does.

import assert from 'node:assert/strict'
import { spawn } from 'node:child_process'
import { after, describe, it } from 'node:test'
import { reservePort } from '../ports.mjs'

const cwd = new URL('../..', import.meta.url).pathname

/// Every coder resolves to nothing, so a route is configured only by a provider.
const binaryOverrides = {
  AI_CODER_HERMES_BIN: '/definitely/missing/hermes',
  AI_CODER_OPENCODE_BIN: '/definitely/missing/opencode',
  AI_CODER_KILOCODE_BIN: '/definitely/missing/kilocode',
  AI_CODER_CLAUDE_BIN: '/definitely/missing/claude',
  AI_CODER_CODEX_BIN: '/definitely/missing/codex',
}

/// The developer running this has provider keys exported; the guard is only
/// meaningful against an environment that carries none of them.
function cleanEnvironment(extra) {
  const environment = { ...process.env }
  const prefixes = ['OPENAI_', 'ANTHROPIC_', 'GEMINI_', 'GOOGLE_', 'OLLAMA_']
  const exact = new Set([
    'AI_PROVIDER', 'AI_BASE_URL', 'AI_MODEL', 'AI_API_KEY', 'AI_ENDPOINTS_JSON',
    'AI_CODER_ROUTES_JSON', 'AI_DEFAULT_CODER', 'AI_DEFAULT_PROVIDER',
  ])
  for (const key of Object.keys(environment)) {
    if (exact.has(key) || prefixes.some(prefix => key.startsWith(prefix))) delete environment[key]
  }
  return { ...environment, ...binaryOverrides, ...extra }
}

const running = []

function start(extra) {
  const child = spawn('cargo', ['run', '--quiet', '--manifest-path', 'backend/Cargo.toml'], {
    cwd,
    detached: true,
    env: cleanEnvironment(extra),
    stdio: ['ignore', 'pipe', 'pipe'],
  })
  running.push(child)
  let output = ''
  for (const stream of [child.stdout, child.stderr]) stream.on('data', data => { output += data })
  return { child, output: () => output }
}

function stop(child) {
  if (child.pid && child.exitCode === null) {
    try { process.kill(-child.pid, 'SIGTERM') } catch { /* already stopped */ }
  }
}

function exited(child, timeoutMs = 60_000) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('the process never exited')), timeoutMs)
    child.once('error', reject)
    child.once('exit', code => { clearTimeout(timer); resolve(code) })
  })
}

async function waitFor(url, child, output, timeoutMs = 60_000) {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    if (child.exitCode !== null) throw new Error(`the service exited early: ${output()}`)
    try { if ((await fetch(url)).ok) return } catch { /* not up yet */ }
    await new Promise(resolve => setTimeout(resolve, 200))
  }
  throw new Error(`the service never became ready: ${output()}`)
}

after(() => running.forEach(stop))

describe('local inference is opt-in', () => {
  it('refuses to start when OLLAMA_MODEL is the only thing naming a backend', async () => {
    const port = await reservePort()
    const service = start({ APP_BIND: `127.0.0.1:${port}`, OLLAMA_MODEL: 'must-not-enable' })

    const code = await exited(service.child)
    assert.notEqual(code, 0, `the service started with no configured backend: ${service.output()}`)
    assert.match(
      service.output(),
      /no AI backends are configured/,
      `the refusal did not explain itself: ${service.output()}`,
    )
  })

  it('configures the route once OLLAMA_ENABLED asks for it', async () => {
    const port = await reservePort()
    const baseURL = `http://127.0.0.1:${port}`
    const service = start({
      APP_BIND: `127.0.0.1:${port}`,
      AI_DEFAULT_CODER: 'api',
      AI_DEFAULT_PROVIDER: 'ollama',
      OLLAMA_ENABLED: 'true',
      OLLAMA_MODEL: 'explicit-test',
      OLLAMA_BASE_URL: 'http://127.0.0.1:9/v1',
    })

    try {
      await waitFor(`${baseURL}/api/ready`, service.child, service.output)
      const payload = await (await fetch(`${baseURL}/api/models`)).json()
      assert.ok(
        payload.models?.some(model => model.key === 'api/ollama/explicit-test'),
        `the explicit route was not configured: ${JSON.stringify(payload)}`,
      )
    } finally {
      stop(service.child)
    }
  })
})
