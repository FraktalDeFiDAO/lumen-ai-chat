## What this changes

<!-- The behaviour that is different afterwards, not a list of files. -->

## Why

<!-- What was wrong before. If this is a fix, what the failure looked like. -->

## What it gives up

<!-- Every change costs something: a dependency, a slower build, a case that is
     now unsupported. If genuinely nothing, say so. -->

## Verified

<!-- What you ran, not what you believe. Paste the counts. -->

- [ ] `npm run check:server`
- [ ] `npm run lint` and `npm run typecheck`
- [ ] `npm test`
- [ ] `npm run test:e2e`
- [ ] Both container images build (`Containerfile.backend`, `Containerfile.frontend`)
- [ ] Compose still validates, if it was touched

## Anything a reviewer should look at twice

<!-- Security boundaries, data that could cross between users, anything you were
     unsure about. Say what you did *not* verify. -->
