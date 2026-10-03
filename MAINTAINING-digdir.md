# Maintaining the Digdir fork

How the fork is synchronized with upstream and how runtimes are released. How to change it day to day is in
[CONTRIBUTING-digdir.md](CONTRIBUTING-digdir.md).

The release unit is the Digdir runtime: a source change is complete when the matching runtime is published and
consumers pin both the source revision and the runtime digests.

## Branches and pins

| Repository | Upstream mirror | Patch queue | Pinned by |
| --- | --- | --- | --- |
| `digdir/microsandbox` (fork of `superradcompany/microsandbox`) | `main` | `main-digdir` | Consumers: Cargo Git revision, lockfile and runtime SHA-256 digests |
| `digdir/libkrunfw` (fork of `superradcompany/libkrunfw`) | `krunfw` | `main-digdir` | This repository's `vendor/libkrunfw` submodule |

`main-digdir` is based on a stable upstream release tag. Moving to a new one rebuilds the queue; never merge an
upstream release into it.

## Digdir files

Files whose name contains `digdir` are Digdir's own, such as `CONTRIBUTING-digdir.md`, `MAINTAINING-digdir.md`, the
`*-digdir.yml` workflows and `scripts/digdir/`, so they never collide with upstream files. A few files only work at a
fixed path and replace or extend upstream's:

- `README.md` replaces upstream's. On a synchronization, keep ours and check whether upstream's change affects what
  ours says.
- `AGENTS.md` keeps upstream's text with a Digdir section appended. On a conflict, take upstream's text and append the
  section again.
- `.github/CODEOWNERS` replaces upstream's. Keep ours.

## Rust toolchain

When synchronizing, take the exact toolchain from a successful upstream Rust Quality run of the new release, update
`rust-toolchain.toml`, and keep the Digdir workflows' formatting, Clippy and documentation checks in line with
upstream's.

## Removed upstream surfaces

`scripts/digdir/remove-unshipped.sh` removes the upstream surfaces the fork does not ship, listed in its header, and
updates the Cargo workspace, `.gitmodules` and `Cargo.lock` to match. When synchronizing, rerun it to resolve
modify/delete conflicts; it is idempotent. A conflict in the workspace member list needs a manual resolution. Add new
upstream surfaces the fork does not ship to the script.

## Digdir CI

Upstream's workflow files stay unchanged and are disabled with `gh workflow disable <file>`. The Digdir workflows are:

- `check-digdir.yml`: the checks, on pull requests and on demand. Caches are saved only in runs on `main-digdir`.
- `test-platform-digdir.yml`: a copy of upstream's `test-platform.yml`, called by `check-digdir.yml`.
- `release-digdir.yml`: a copy of upstream's release lanes. It publishes the runtime for a `v<version>` tag; a manual
  run builds and validates everything without publishing.

When synchronizing, compare upstream's release workflows and `test-platform.yml` with the Digdir copies and adopt
build changes. Disable new upstream workflows unless they are useful for our CI; remove a file GitHub cannot keep
disabled, such as one it cannot parse, in the cleanup commit. The kernel tarball's checksum lives in upstream's
`cache-libkrunfw-kernel` action.

## Invariants

- The version is `<upstream-version>-digdir.<n>` for every internal crate and in `Cargo.lock`, and `v<version>` tags
  the commit the runtime was built from. The SDK only launches a runtime of exactly its own version.
- Every published `msb` is built with `embed-binaries`; without it no guest agent is embedded and every sandbox start
  fails.
- The libkrunfw commit in `vendor/libkrunfw` is tagged in `digdir/libkrunfw` before the runtime is released.
- Tags and release assets are immutable; a correction is a new Digdir revision. Before `main-digdir` is rewritten,
  every commit a consumer pins must be tagged, so that it stays reachable.
- A source-only change that needs no new runtime can be tagged `v<runtime-version>-source.<n>` and keeps the runtime's
  version (see [Updating consumers](#updating-consumers)).

## Review gates

1. **After triage:** review the triage record before rebasing.
2. **Before the tag:** review the `sync/digdir-X.Y.Z` pull request: the `range-diff` against the triage record, the
   tree diff from the upstream tag, and the test results.

A rewritten sync branch never gets `pull_request` checks, so run them with `workflow_dispatch` after every push. Land
the approved queue by moving the branch; GitHub then marks the pull request as merged:

```bash
git push --force-with-lease=main-digdir:<old-tip> origin sync/digdir-X.Y.Z:main-digdir
```

## Synchronizing with upstream

### 1. Refresh the mirror

With `origin` as `digdir/microsandbox` and `upstream` as `superradcompany/microsandbox`:

```bash
git fetch origin
git fetch upstream --tags
git switch main
git merge --ff-only upstream/main
git push origin main
```

Stop if the fast-forward fails.

### 2. Select the base

Use the latest stable upstream release, not upstream `main`. The libkrunfw revision it pins is the libkrunfw base;
newer libkrunfw commits are a separate decision.

```bash
target_msb_tag=vX.Y.Z
git ls-tree "$target_msb_tag" vendor/libkrunfw
```

### 3. Triage

List the upstream commits that can affect the fork. The release workflow paths are included because the Digdir copies
must follow upstream's build changes.

```bash
old_msb_base=$(git merge-base origin/main-digdir "$target_msb_tag")
git log --oneline --no-merges "$old_msb_base".."$target_msb_tag" -- \
  crates sdk/rust vendor/libkrunfw Cargo.toml Cargo.lock \
  '.github/workflows/release*.yml' .github/workflows/test-platform.yml scripts/ci \
  ':!**/*.md' ':!crates/*/examples' ':!crates/*/benches'
```

Narrow it to the paths the Digdir patches own to forecast conflicts and spot patches upstream made redundant:

```bash
git diff --name-only "$old_msb_base" origin/main-digdir -- crates sdk/rust |
  xargs git log --oneline --no-merges "$old_msb_base".."$target_msb_tag" --
```

Write a triage record before rebasing: per Digdir commit, `keep`, `adapt` or `drop` with the upstream commits behind
the decision; upstream changes the fork relies on, such as protocol, guest agent, firmware or image catalog changes;
and upstream moves of files a patch touches.

### 4. Synchronize libkrunfw

Follow [digdir/libkrunfw's
MAINTAINING-digdir.md](https://github.com/digdir/libkrunfw/blob/main-digdir/MAINTAINING-digdir.md). If its base is
unchanged, keep the current libkrunfw commit.

### 5. Rebase

```bash
git switch -c sync/digdir-X.Y.Z origin/main-digdir
git rebase --interactive --onto "$target_msb_tag" "$old_msb_base"
```

Apply the triage record and the synchronization notes in the sections above. Drop the old version bump, pin the
libkrunfw commit from step 4, and add a fresh bump to `X.Y.Z-digdir.1` at the tip with a regenerated `Cargo.lock`.
Compare the queues and check the full tree diff from the upstream tag for stray files:

```bash
git range-diff "$old_msb_base"..origin/main-digdir "$target_msb_tag"..sync/digdir-X.Y.Z
```

### 6. Release

Run the workspace checks; runtime, networking, filesystem, image and protocol changes also need the hardware-backed
integration tests. Then:

1. Run `gh workflow run check-digdir.yml --ref main-digdir` and wait for it to pass.
2. Run `gh workflow run release-digdir.yml --ref main-digdir`, a dry run that builds and validates every artifact
   without publishing.
3. Tag the libkrunfw commit, as libkrunfw's MAINTAINING-digdir.md describes.
4. Tag `v<version>`. The release workflow builds and publishes the runtime for Linux, macOS and Windows.

A failed or partial release is not repaired; publish a new Digdir revision. After publishing, verify the checksums and
the Linux glibc baseline, and record the bundle digests for consumers.

## Updating consumers

- Adopt a runtime only after its release is complete and verified, and pin the source and the runtime together: the
  Git revision of every Microsandbox crate, the lockfile and the runtime digests.
- Pin only tagged revisions. A `v<runtime-version>-source.<n>` tag may reuse a published runtime only if the complete
  diff from the runtime tag leaves the published `msb`, guest agent, firmware, host/guest protocol, release workflow
  and artifact composition unchanged.
- Verify both a fresh runtime installation and an upgrade from the previously pinned version.

## Rollback

Roll back by restoring the previous tagged revision, lockfile and runtime digests together in the consumer; never mix
source and runtime from different Digdir versions.
