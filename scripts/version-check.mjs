/// The version lives in three places, and they drift silently.
///
/// `package.json` builds the frontend, `Cargo.toml` builds the gateway and goes
/// out as its User-Agent to every provider, and the tag is what was actually
/// shipped. A release where those disagree is a release nobody can trace.

import { readFileSync } from 'node:fs'
import { execFileSync } from 'node:child_process'

const root = new URL('..', import.meta.url).pathname

const npmVersion = JSON.parse(readFileSync(`${root}package.json`, 'utf8')).version
const cargoVersion = readFileSync(`${root}backend/Cargo.toml`, 'utf8')
  .split('\n')
  .find(line => /^version\s*=/.test(line))
  ?.split('"')[1]

const problems = []
if (!cargoVersion) problems.push('backend/Cargo.toml has no version')
else if (cargoVersion !== npmVersion) {
  problems.push(`package.json is ${npmVersion} but backend/Cargo.toml is ${cargoVersion}`)
}

// The tag only has to agree when the checkout is exactly on one.
let tag = ''
try {
  tag = execFileSync('git', ['describe', '--tags', '--exact-match'], {
    cwd: root, stdio: ['ignore', 'pipe', 'ignore'],
  }).toString().trim()
} catch { /* not on a tag, which is the normal case */ }

if (tag && tag.replace(/^v/, '') !== npmVersion) {
  problems.push(`the tag is ${tag} but the manifests say ${npmVersion}`)
}

if (problems.length) {
  console.error('Version mismatch:')
  for (const problem of problems) console.error(`  ${problem}`)
  process.exit(1)
}

console.log(`Version ${npmVersion} agrees across the manifests${tag ? ` and the tag ${tag}` : ''}.`)
