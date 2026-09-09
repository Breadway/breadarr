# AGENTS.md: Repo hygiene

Scope: this file covers *repo hygiene*: branching, remotes, CI, cleanup. It is not project documentation.

This repo follows the branch/release workflow documented in `CONTRIBUTING.md`,
read and follow it for any git, branch, or release work here (the
single-trunk model, `feature/x`/`fix/x` branch naming, how RC tags work,
etc). Don't improvise a different workflow. The short version: there is one
long-lived branch, `main`; no `dev` or `beta` branch exists. `main`
auto-publishes a bakery dev-track build on every push. "Beta" and "stable"
are both just tags, not branches: push a `vX.Y.Z-rc.N` tag to publish a
beta-track build, push a plain `vX.Y.Z` tag to cut the signed stable
release. "Freezing" for stabilization means pausing pushes to `main`, not
moving a branch.

## Product identity

breadarr is a **homelab** product (single-daemon Sonarr/Radarr/Prowlarr
replacement: `breadarrd` + `breadarr-tui`). It is bakery-distributed
(`bakery install breadarr`), **not** baked into the BOS ISO, **not** a
GTK/desktop-shell app, and does **not** emit or subscribe to bread events.
bread's `KNOWN_APPS` list already reserves `"arr"`; do not start emitting
on that id unless an explicit integration pass asks for it.

Do not add a bos-settings panel, a breadway.dev website entry, or an ISO
bake. Those are out of scope for this product.

## Remotes
- `origin`: Forgejo (`git.breadway.dev`) only. Unlike most sibling
  bread-ecosystem repos, this one has no GitHub mirror; don't assume a
  `github` remote exists. Push `origin` only.

## CI

Workflows live under `.forgejo/workflows/` (not `.github/`):

- `check.yml`: clippy + test on push to `feature/**` and `fix/**`,
  before a change reaches `main` and triggers a dev-track release.
- `dev-release.yml`: bakery dev-track build on every push to `main`.
- `rc-release.yml`: bakery beta-track build on any `vX.Y.Z-rc.N` tag.
- `release.yml`: signed bakery stable release on any other `v*` tag
  (skips tags containing `-rc.`).

All four run on the self-hosted `hestia` runner. There is no GitHub
Release upload (no GitHub mirror). Do not claim this repo has no CI.

## Cleanup
- Delete feature/fix branches once merged. Check with `git branch --merged main`.

## Don't
- Don't embed credentials in remote URLs; SSH or a credential helper only.
- Don't emit bread events, add a settings panel, website entry, or ISO bake.
- Don't commit leftover source tarballs (`**/src.tar.xz`).
