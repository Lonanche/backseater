# Releasing Backseater

Releases are built with [Velopack](https://velopack.io) and published to GitHub Releases
**automatically by CI** (`.github/workflows/ci.yml`): every push to `main` runs formatting,
Clippy, and tests using the same `scripts/check.ps1` and pinned Rust toolchain as local
development. After validation, a separate release job checks whether the workspace version
in `Cargo.toml` already has a GitHub release. If it doesn't, the job builds the optimized
executable, packs the Windows installer, and publishes it. The `vX.Y.Z` tag is created
by the publish step; tags are never pushed by hand. The app updates itself from the published
releases (banner appears when a new version has been downloaded; it also applies pending updates
on the next launch).

## Cutting a release

1. Bump `version` in the workspace `Cargo.toml` — e.g. `0.2.3`, or `0.2.3-beta.1` for a beta.
   Run `cargo check --workspace` to refresh `Cargo.lock` and include it in the commit.
2. Add (or extend) the `## v0.2.3` section in `CHANGELOG.md` — it becomes the release notes.
3. Commit and push.

CI gates the release on formatting, Clippy, tests, **and the changelog**: a version with no matching
`CHANGELOG.md` section fails the run instead of releasing (forgetting step 2 can't ship a
release with empty notes). A red build publishes nothing — fix and push again.
On green, the release appears at `https://github.com/Lonanche/backseater/releases` containing:

- `Backseater-win-Setup.exe` — the installer (this is what users download)
- `Backseater-win-Portable.zip` — portable build (does not auto-update)
- `Backseater-<version>-full.nupkg` / `-delta.nupkg` — the update feed the app consumes
- `releases.win.json` — the feed index

Once the version has been published, later pushes skip the optimized build and packaging.
PRs only run validation, and new commits cancel obsolete runs for the same PR.
Release jobs are serialized and never cancel an active publication. The version check runs
inside that serialization slot, so a waiting job sees releases published by an earlier job.
GitHub retains only the newest pending release job when several pushes queue up; let a
release finish before bumping to another version that must be published separately.
Only a 404 from the release lookup means the version is unpublished; other API failures
stop the job.

Validation and release builds have separate dependency caches. Validation restores a
fallback cache on a miss and saves partial work there on failure; only a successful check
run populates the main cache. This prevents a failed Clippy run from leaving an immutable
cache that never acquires compiled test dependencies. A first successful run on a new key
may save both caches; subsequent exact hits only use the main cache. Release dependencies
are saved even if packaging or publication fails. All Clippy, test, and release commands
use `--locked`; an outdated lockfile must be fixed locally. Release notes are checked before
the optimized build starts.

The "Download previous release" step is `continue-on-error` because a first release has nothing
to build a delta from — that failure is expected once.

## Beta releases

A version with a pre-release suffix (`0.3.0-beta.1`) publishes as a GitHub **pre-release**.
Only users who enabled About → "Get beta updates" receive it; everyone else skips it, and beta
users move to the next stable automatically once it's published (semver:
`0.3.0-beta.1 < 0.3.0`).

**The beta → stable cycle:** develop on `main`, set the version to `0.3.0-beta.1`, push → beta
users test. Fixes land on `main` with the version bumped to `0.3.0-beta.2`, etc. When solid,
set the plain `0.3.0` and push — everyone converges on the stable release. Not every release
needs a beta; small fixes can go straight to a stable version.

**Changelog during betas:** keep ONE `## v0.3.0` section and keep appending beta-cycle fixes to
it — no per-beta sections. A beta version inherits its stable section as release notes (the
workflow strips the `-beta.N` suffix as a fallback when no exact section exists), and the final
stable release publishes the completed section.

Releases ship unsigned (SmartScreen shows "unknown publisher"; users click
More info → Run anyway). If code signing is added later, it slots into the workflow at two
points: `backseater.exe` before `vpk pack`, and `Backseater-win-Setup.exe` after it.
