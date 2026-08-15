# Contributing

`breadarr` — single-daemon Sonarr+Radarr+Prowlarr replacement for a
homelab host (`breadarrd` + `breadarr-tui`).

It is bakery-distributed (`bakery install breadarr`), **not** baked into
the BOS ISO, **not** a GTK/desktop-shell app, and does **not** emit bread
events. bread's `KNOWN_APPS` list reserves `"arr"`; this repo does not
emit on that id.

Part of the bread ecosystem; this repo follows the same single-trunk
branch/release workflow as every other bakery-channel product.

## Branches

There is one long-lived branch: **`main`**. All day-to-day work lands here.
Every push to `main` automatically builds and publishes a **dev-track**
build (see Tracks below) — a real install you can test before cutting
anything more formal.

New work — features and bug fixes alike — goes on a short-lived branch:

```
feature/<short-name>
fix/<issue-number-or-short-name>
```

Branch off `main`, open a PR/push back into `main` when ready. Short-lived
branches get deleted on merge — they never accumulate the kind of drift a
second long-lived branch does.

## The release cycle

There's no separate `beta` or release branch — "stable" and "beta" are both
just **tags** on `main`, not branches that need to be kept in sync:

1. Work accumulates on `main` via `feature/x` / `fix/x` branches. Each push
   auto-publishes a dev build — install it with `bakery track set dev` and
   `bakery update --all`, then fix anything broken with another push.
2. When you want to stabilize before a real release, tag a release
   candidate: `git tag vX.Y.Z-rc.1 && git push origin vX.Y.Z-rc.1`. That
   tag alone triggers a beta-track build — "freezing" is just pausing
   pushes to `main` while you test it, not a branch operation. Cut
   `-rc.2`, `-rc.3`, etc. for further fixes.
3. Once an RC has gone without issues, tag the real release:
   `git tag vX.Y.Z && git push origin vX.Y.Z` — that's what triggers the
   signed stable release build.

This repo has no GitHub mirror. Push tags and branches to `origin`
(Forgejo) only.

## Tracks, from a user's perspective

```
bakery track show              # what you're currently on (defaults to stable)
bakery track set dev           # or beta, or stable
bakery update --all            # pull the latest build on your current track
```

| Track  | What it is | Published from |
|--------|-----------|-----------------|
| `stable` | The last tagged release | a `vX.Y.Z` tag |
| `beta` | Latest release candidate | a `vX.Y.Z-rc.N` tag |
| `dev` | Bleeding edge | `main`, on every push |

Dev versions are auto-computed (`X.Y.Z-dev.<timestamp>+<sha>`) from the
latest published stable tag, so they always sort as newer than what you
have installed — no manual version bumping needed. Beta versions are just
the RC tag itself (already valid semver, already sorts below the real
release it's a candidate for).

## Local development

```sh
cargo build --release --workspace --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

Host tools the daemon shells out to: `mkvtoolnix-cli` (`mkvmerge`) and
`ffmpeg`/`ffprobe`. See `bakery.toml` and the README setup section.

## CI

- `check.yml` — clippy + test on push to `feature/**`, `fix/**`, and
  `main`, and on pull requests.
- `dev-release.yml` — triggered on push to `main`.
- `rc-release.yml` — triggered on any `vX.Y.Z-rc.N` tag push.
- `release.yml` — triggered on any other `v*` tag push, cuts the actual
  stable release.

All CI runs on a self-hosted runner. See
[bread-ecosystem's docs/release-channels.md](https://git.breadway.dev/Breadway/bread-ecosystem/src/branch/main/docs/release-channels.md)
for the full policy, including how a new product gets wired onto these tracks.

## Questions

Open an issue on this repo's Forgejo tracker.
