/// Cut a release: rewrite both manifests, commit, tag.
///
/// Deliberately does not push. A tag is the one thing here that is painful to
/// take back once it has been seen, so the last step stays a person's.

import { readFileSync, writeFileSync } from 'node:fs'
import { execFileSync } from 'node:child_process'

const root = new URL('..', import.meta.url).pathname
const version = process.argv[2]

if (!version || !/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version)) {
  console.error('Usage: npm run release -- <major.minor.patch>')
  process.exit(1)
}

const git = (...args) =>
  execFileSync('git', args, { cwd: root, stdio: ['ignore', 'pipe', 'inherit'] }).toString().trim()

if (git('status', '--porcelain')) {
  console.error('The working tree has uncommitted changes; commit or stash them first.')
  process.exit(1)
}
const branch = git('rev-parse', '--abbrev-ref', 'HEAD')
if (branch !== 'main') {
  console.error(`Releases are cut from main, not ${branch}.`)
  process.exit(1)
}

const packagePath = `${root}package.json`
const manifest = JSON.parse(readFileSync(packagePath, 'utf8'))
manifest.version = version
writeFileSync(packagePath, `${JSON.stringify(manifest, null, 2)}\n`)

const cargoPath = `${root}backend/Cargo.toml`
const cargo = readFileSync(cargoPath, 'utf8')
let replaced = false
writeFileSync(cargoPath, cargo.split('\n').map(line => {
  // Only the first `version =`, which is the package's own; dependencies have
  // their own and must not be touched.
  if (!replaced && /^version\s*=/.test(line)) { replaced = true; return `version = "${version}"` }
  return line
}).join('\n'))
if (!replaced) {
  console.error('backend/Cargo.toml has no version line to rewrite.')
  process.exit(1)
}

// Keep Cargo.lock honest about the version it just changed.
execFileSync('cargo', ['update', '--workspace', '--offline'], {
  cwd: `${root}backend`, stdio: 'inherit',
})

git('add', 'package.json', 'backend/Cargo.toml', 'backend/Cargo.lock')
git('commit', '-m', `chore: release ${version}`)
git('tag', '-a', `v${version}`, '-m', `Release ${version}`)

console.log(`\nTagged v${version}. Nothing has been pushed.`)
console.log('When you are ready:  git push origin main --follow-tags')
