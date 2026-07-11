# Releasing Backseater

Releases are Windows installers built with [Velopack](https://velopack.io) and published to
GitHub Releases by CI. The app updates itself from the same feed (banner appears when a new
version has been downloaded; it also applies pending updates on the next launch).

## Cutting a release

1. Bump `version` in the workspace `Cargo.toml` and add a `## vX.Y.Z` section to
   `CHANGELOG.md` (it becomes the GitHub release notes), then commit and push.
2. **Wait for CI on that commit to go green** — it's the quality gate, and its
   run is what warms the build cache the release run restores (tag-ref runs can
   only restore caches created on `main`).
3. Tag the CI-green commit and push the tag:

   ```sh
   git tag v0.2.0
   git push origin v0.2.0
   ```

3. `.github/workflows/release.yml` builds `backseater.exe`, packs it with `vpk`, and publishes
   a GitHub Release containing:
   - `Backseater-win-Setup.exe` — the installer (this is what users download)
   - `Backseater-win-Portable.zip` — portable build (does not auto-update)
   - `Backseater-<version>-full.nupkg` / `-delta.nupkg` — the update feed the app consumes
   - `releases.win.json` — the feed index

The pack version comes from the tag, so the tag is the source of truth; keeping `Cargo.toml`
in sync is for `--version`-style correctness, not the updater.

The "Download previous release" step is `continue-on-error` because the very first release has
nothing to build a delta from — that failure is expected once.

## Beta releases

Tag with a pre-release suffix — e.g. `git tag v0.3.0-beta.1` — and the workflow publishes it as
a GitHub **pre-release**. Only users who enabled About → "Get beta updates" receive it;
everyone else skips it, and beta users move to the next stable automatically once it's published
(semver: `0.3.0-beta.1 < 0.3.0`).

**The beta → stable cycle:** develop on `main`, tag `v0.3.0-beta.1`, let beta users test. Fixes
go on `main` and get tagged `v0.3.0-beta.2`, etc. — beta users auto-update to each. When solid,
tag `v0.3.0` (usually on the same commit as the last good beta); everyone converges on it.
Not every release needs a beta — small fixes can tag stable directly.

**Changelog during betas:** keep ONE `## v0.3.0` section and keep appending beta-cycle fixes to
it — no per-beta sections. A beta tag inherits its stable section as release notes (the
workflow strips the `-beta.N` suffix as a fallback when no exact section exists), and the final
stable release publishes the completed section.

Releases ship unsigned (SmartScreen shows "unknown publisher"; users click
More info → Run anyway). If code signing is added later, it slots into the workflow at two
points: `backseater.exe` before `vpk pack`, and `Backseater-win-Setup.exe` after it.
