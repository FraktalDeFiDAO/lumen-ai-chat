#!/usr/bin/env node

const args = process.argv.slice(2)
let coder
let output

if (args[0] === 'chat') {
  coder = 'hermes'
  if (!args.includes('safe') || args.includes('--yolo')) process.exit(2)
  output = 'Mock coder response: hermes read-only'
} else if (args[0] === 'run') {
  coder = 'opencode-or-kilocode'
  if (args.includes('--auto') || args.includes('--dangerously-skip-permissions')) process.exit(2)
  output = 'Mock coder response: opencode/kilocode read-only'
} else if (args[0] === '--print') {
  coder = 'claude'
  const permissionIndex = args.indexOf('--permission-mode')
  if (permissionIndex < 0 || args[permissionIndex + 1] !== 'plan') process.exit(2)
  output = 'Mock coder response: claude read-only'
} else if (args[0] === 'exec') {
  coder = 'codex'
  const sandboxIndex = args.indexOf('--sandbox')
  if (sandboxIndex < 0 || args[sandboxIndex + 1] !== 'read-only' || args.includes('--full-auto')) process.exit(2)
  output = 'Mock coder response: codex read-only'
} else {
  process.exit(2)
}

if (coder === 'hermes') {
  process.stdout.write(`${output}\n`)
} else if (coder === 'claude') {
  process.stdout.write(`${JSON.stringify({ result: output })}\n`)
} else if (coder === 'codex') {
  process.stdout.write(`${JSON.stringify({ type: 'item.completed', item: { type: 'agent_message', text: output } })}\n`)
} else {
  process.stdout.write(`${JSON.stringify({ type: 'text', part: { text: output } })}\n`)
}
