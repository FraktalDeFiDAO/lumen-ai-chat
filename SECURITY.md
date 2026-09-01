# Security

## Reporting

Report anything you believe is a vulnerability privately, through
[GitHub's advisory form](https://github.com/FraktalDeFiDAO/lumen-ai-chat/security/advisories/new).
Please do not open a public issue first.

Include what you did, what happened, and what you expected. A working exploit is
not required and not expected; a description precise enough to reproduce is.

## What this software is, and what that means for its threat model

Lumen executes AI coding agents against a filesystem. That is the product, not a
side effect, so the interesting questions are all about containment.

**Single-user deployment (the default).** The gateway binds to loopback, has no
identity, and serves whoever can reach the port. That is deliberate: the boundary
is the machine, and anyone with a shell on it can already do everything Lumen
can. A single-user deployment reachable from a network is a misconfiguration, and
`Config::validate` refuses to start one.

**Hosted deployment.** Identity comes from a header set by a reverse proxy, which
is worth exactly what the network makes it worth. `AI_IDENTITY_SHARED_SECRET`
turns that from an assertion into something checkable, and off loopback the
service refuses to start without it.

The properties we intend to hold, and would treat a break in as a vulnerability:

| Property | Where it is enforced |
| --- | --- |
| One user cannot read, modify or delete another's conversations | `backend/src/store.rs` — no method reaches a row without naming its owner |
| One user's agent cannot read or write another's files | `backend/src/workspaces.rs` — a subtree per user, resolved per request |
| One user's index never returns another's code | an index per user, under `AI_HARNESS_MEMORY_DIR/<user>` |
| A path from a model cannot escape the workspace | `backend/src/harness/workspace.rs` — normalised, then re-checked after resolving symlinks |
| Secrets in the workspace never reach a coder CLI | `backend/src/coders.rs` — the read-only copy excludes them by name and extension |
| A forged identity header is refused when a proxy secret is configured | `backend/src/identity.rs` — compared in constant time, before the identity is read |
| An unauthenticated request cannot reach conversations or the harness | every handler extracts `CurrentUser` |

Each has tests, at the module and again over HTTP.

## Known limitations, deliberately accepted

These are design decisions rather than oversights. Reporting them is welcome but
will not be treated as a vulnerability without a concrete escalation.

- **A shared writable workspace is refused, not contained.** Several users, one
  root, writes enabled cannot be made safe, so the service will not start.
- **`AI_TRUSTED_PROXY` is an operator's assertion about the network.** The shared
  secret is the only part a process can verify.
- **Branch protection is unavailable** on this repository's plan, so the promotion
  rules are enforced by workflow and a local hook rather than by GitHub.
- **A native coder CLI runs with the permissions of the process.** It is given a
  secret-filtered read-only copy of the workspace, not the workspace itself, but
  it is still someone else's binary.

## Supported versions

The most recent tag. This project does not backport.
