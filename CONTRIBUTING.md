# Contributing

## The branches

```
feature/*  fix/*  chore/*        short-lived, one change each
      │
      ▼
    dev            integration; everything lands here first
      │
      ▼
  staging          release candidate; what is about to ship
      │
      ▼
    main           what is deployed
```

Work is opened against `dev`. When `dev` is worth shipping it is promoted to
`staging` by pull request, and `staging` to `main` the same way. A promotion is a
merge of a commit that has already gone green where it was, not a fresh attempt.

`hotfix/*` is the exception, and the only one: a branch named that way may be
opened directly against `staging` or `main`. Merge it back down to `dev`
afterwards, or the next promotion will quietly revert it.

A release commit is the same shape: it is cut on `main` and has to come back
down. See [RELEASING.md](RELEASING.md).

## What is enforced, and by what

This repository is private on a Free account, where **GitHub cannot enforce
branch protection at all** — both classic protection and rulesets return
`403 Upgrade to GitHub Pro or make this repository public`. That is a real gap
and worth naming rather than papering over: nothing on the server will stop a
direct push to `main`.

What holds the line instead:

| Rule | Enforced by | Bypassable |
| --- | --- | --- |
| Every push and pull request is built and tested | `.github/workflows/ci.yml` | No |
| A promotion is not rebuilt, because the commit was already built where it came from | `.github/workflows/ci.yml` skips it | n/a |
| Promotions only go `dev → staging → main` | `.github/workflows/promotion.yml` | Only by merging without the check, which requires deliberate action |
| A promoted commit already passed CI where it came from | `.github/workflows/promotion.yml` | Same |

That second check waits for CI on the head commit rather than looking once. A
promotion opens at the same moment the source branch starts building, so a single
look always lands before CI could have finished — and a gate that always fails the
first time is one people learn to merge past. It gives up after 35 minutes.
| No direct commits to `dev`, `staging` or `main` | `githooks/pre-push`, opt-in per clone | Yes, `--no-verify` |

### Merging a pull request that touches `.github/workflows/`

GitHub refuses this over the API unless the token carries the `workflow` scope:

```
refusing to allow an OAuth App to create or update workflow
`.github/workflows/ci.yml` without `workflow` scope
```

`gh` is not granted it by default, so `gh pr merge` fails on exactly the pull
requests that change how anything is built. Either grant it once —

```bash
gh auth refresh -s workflow
```

— or merge that one locally and push, which is the same commit by a different
route:

```bash
git fetch origin refs/pull/<n>/head:pr-<n>
git switch dev && git merge pr-<n>
git push --no-verify origin dev     # --no-verify: the hook below, deliberately
```

The `--no-verify` is the case the hook was written to allow: a decision, not an
accident.

Install the local guard once per clone:

```bash
git config core.hooksPath githooks
```

If the repository ever becomes public, or the account upgrades, replace the hook
with real branch protection: require the `quality` check, require a review, and
disallow force pushes. The workflow above is designed to survive that change
without alteration.

## Commits

Conventional prefixes — `feat:`, `fix:`, `test:`, `chore:`, `docs:`, `refactor:`.
One concern per commit.

The subject says what changed. **The body says why**, and that is the part that
matters here: what was wrong before, what was considered, and what the change
gives up. A reader a year from now has the diff already; what they do not have is
the reasoning. Several commits in this history exist mainly to record a decision.

## Before opening a pull request

```bash
npm run check:server   # rustfmt, clippy -D warnings, every Rust test
npm run lint           # eslint, type-aware
npm run typecheck
npm test               # vitest
npm run build
npm run test:e2e       # contract suites + both browser suites
npm run audit
```

CI runs all of it, plus a Compose validation, both container image builds, and a
hardened-container smoke test. Two of those only fail outside a plain checkout —
the frontend image copies no `tests/`, and Compose merges environments — so run
the image builds locally before claiming a change is done:

```bash
podman build -f Containerfile.backend -t lumen-ai-chat-api:dev .
podman build -f Containerfile.frontend -t lumen-ai-chat-web:dev .
```

`npm run test:podman` needs a running Compose deployment and makes real calls
through a real coder CLI. It is not in CI, and it costs whatever your provider
charges.

## Releases

`main` is tagged, never branched from for a release. See [RELEASING.md](RELEASING.md).
