# 2. Identity comes from a proxy, not from accounts we hold

Date: 2026-08-31

## Status

Accepted.

## Context

The hosted deployment needs to know who is asking. Two ways to get that: verify
credentials here, or trust something in front that already did.

Lumen executes AI coding agents against a filesystem. The interesting security
work is containment — per-user workspaces, per-user indexes, a store where no
query can reach a row without naming its owner. Password handling is the part of
the problem with the worst failure mode and the least novelty, and every hour
spent on reset flows is an hour not spent on the part that is actually specific
to this software.

The deployment already required an identity-aware reverse proxy for remote
access before any of this existed.

## Decision

Identity is a header set by a trusted proxy. `AI_IDENTITY_SHARED_SECRET` makes
that checkable rather than merely asserted, and off loopback the service refuses
to start without it.

`IdentityProvider` is a trait with two implementations. A third that verifies
credentials here changes one file and nothing else.

## Consequences

Lumen cannot be run multi-user without a proxy in front. That is a real cost and
it will be the first thing someone asks for.

We are not adding it until someone does. Building authentication nobody has
requested is how the worst bugs get written, and the trait means the decision
stays cheap to reverse.
