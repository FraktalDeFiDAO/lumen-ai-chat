# Releasing

A release is a tag on `main`. There is no release branch: `staging` already is
one, and cutting a second would mean two places to backport to.

## Versioning

Semantic versioning, carried in three places that must agree:

| Where | Why it exists |
| --- | --- |
| `package.json` | the frontend build |
| `backend/Cargo.toml` | the gateway, and its `User-Agent` to every provider |
| the git tag | what was actually shipped |

`npm run version:check` fails when they disagree.

## Cutting one

```bash
npm run release -- 0.2.0       # rewrites both manifests, commits, tags
git push origin main --follow-tags
```

The workflow in `.github/workflows/release.yml` builds both images from the
tagged commit, runs the full gate against it, and publishes a GitHub release
whose notes are the commit subjects since the previous tag.

## Then merge it back down

The version bump is a commit on `main`, and `dev` and `staging` do not have it.
Leave it there and the next promotion quietly reverts the release: `staging`
still says the old version, `staging → main` carries that backwards, and the
tag stops matching the manifests it was cut from.

```bash
git switch dev && git merge main && git push origin dev
```

`staging` picks it up on the next `dev → staging`, so only one merge is needed.
This is the same rule as a `hotfix/*`: anything that lands above `dev` has to
come back down, or the branch it skipped will undo it.

## What is deliberately not automated

**Publishing images to a registry.** There is no registry configured, and
choosing one is a decision with consequences — where they live, who can pull
them, what the retention is. Until that is decided, `compose.yaml` builds
locally from source, which is honest about where the image came from.

**Deployment.** Nothing here deploys anything. `main` being tagged means the code
is ready; moving it onto a machine is still a person's decision.
