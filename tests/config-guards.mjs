import { spawn } from 'node:child_process'
import { reservePort } from './ports.mjs'

const cwd = new URL('..', import.meta.url).pathname
const binaryOverrides = {
  AI_CODER_HERMES_BIN: '/definitely/missing/hermes',
  AI_CODER_OPENCODE_BIN: '/definitely/missing/opencode',
  AI_CODER_KILOCODE_BIN: '/definitely/missing/kilocode',
  AI_CODER_CLAUDE_BIN: '/definitely/missing/claude',
  AI_CODER_CODEX_BIN: '/definitely/missing/codex',
}

function cleanEnvironment(extra = {}) {
  const environment = { ...process.env }
  const providerPrefixes = ['OPENAI_', 'ANTHROPIC_', 'GEMINI_', 'GOOGLE_', 'OLLAMA_']
  const exact = new Set(['AI_PROVIDER', 'AI_BASE_URL', 'AI_MODEL', 'AI_API_KEY', 'AI_ENDPOINTS_JSON', 'AI_CODER_ROUTES_JSON', 'AI_DEFAULT_CODER', 'AI_DEFAULT_PROVIDER'])
  for (const key of Object.keys(environment)) {
    if (exact.has(key) || providerPrefixes.some(prefix => key.startsWith(prefix))) delete environment[key]
  }
  return { ...environment, ...binaryOverrides, ...extra }
}

function start(extra) {
  const child = spawn('cargo', ['run', '--quiet', '--manifest-path', 'backend/Cargo.toml'], {
    cwd,
    detached: true,
    env: cleanEnvironment(extra),
    stdio: ['ignore', 'pipe', 'pipe'],
  })
  let output = ''
  child.stdout.on('data', data => { output += data })
  child.stderr.on('data', data => { output += data })
  return { child, output: () => output }
}

async function exited(process, timeoutMs = 30_000) {
  return await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('Configuration guard process did not exit')), timeoutMs)
    process.once('error', reject)
    process.once('exit', code => { clearTimeout(timer); resolve(code) })
  })
}

async function waitFor(url, process, output, timeoutMs = 30_000) {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    if (process.exitCode !== null) throw new Error(`Explicit Ollama guard exited early: ${output()}`)
    try { if ((await fetch(url)).ok) return } catch { /* retry */ }
    await new Promise(resolve => setTimeout(resolve, 200))
  }
  throw new Error(`Explicit Ollama guard timed out: ${output()}`)
}

const port = await reservePort()
const baseURL = `http://127.0.0.1:${port}`
const implicit = start({ APP_BIND: `127.0.0.1:${port}`, OLLAMA_MODEL: 'must-not-enable' })
const implicitCode = await exited(implicit.child)
if (implicitCode === 0 || !implicit.output().includes('no AI backends are configured')) {
  throw new Error(`OLLAMA_MODEL implicitly enabled local inference: ${implicit.output()}`)
}

const explicit = start({
  APP_BIND: `127.0.0.1:${port}`,
  AI_DEFAULT_CODER: 'api',
  AI_DEFAULT_PROVIDER: 'ollama',
  OLLAMA_ENABLED: 'true',
  OLLAMA_MODEL: 'explicit-test',
  OLLAMA_BASE_URL: 'http://127.0.0.1:9/v1',
})
try {
  await waitFor(`${baseURL}/api/ready`, explicit.child, explicit.output)
  const payload = await (await fetch(`${baseURL}/api/models`)).json()
  if (!payload.models?.some(model => model.key === 'api/ollama/explicit-test')) {
    throw new Error(`Explicit Ollama route was not configured: ${JSON.stringify(payload)}`)
  }
} finally {
  if (explicit.child.pid && explicit.child.exitCode === null) {
    try { process.kill(-explicit.child.pid, 'SIGTERM') } catch { /* already stopped */ }
  }
}

console.log('Configuration guards passed: OLLAMA_MODEL alone is inert; OLLAMA_ENABLED=true works')
