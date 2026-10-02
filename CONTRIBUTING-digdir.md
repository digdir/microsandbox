# Contributing to the Digdir fork

How to change this fork. How it is synchronized with upstream and how runtimes are released is in
[MAINTAINING-digdir.md](MAINTAINING-digdir.md).

## Patch queue

`main-digdir` is an ordered patch queue on top of an upstream release tag.

- A change is a pull request against `main-digdir`, checked by CI and merged with a rebase merge. Commits are only
  folded and reordered when the queue is rebuilt on a synchronization.
- Each commit is one coherent change that could be offered upstream on its own, with its documentation in the same
  commit, so that dropping a commit drops everything that came with it.
- The queue is ordered: documentation and governance, build, cleanup and CI, the libkrunfw pin, features and fixes,
  and the version bump last.
- Change upstream files only as much as a patch needs; every changed line can conflict on the next synchronization.
- Follow upstream's coding standards in [AGENTS.md](AGENTS.md) and [DEVELOPMENT.md](DEVELOPMENT.md). Commits in this
  fork are not signed.

## Rust toolchain

`rust-toolchain.toml` pins the toolchain that upstream's Rust Quality job passed with for the release we build on.
Cargo, pre-commit hooks, Digdir CI and runtime releases all use it; avoid `cargo +stable`, which bypasses the pin.

## Referencing upstream

When a commit, pull request, issue or comment here mentions an upstream issue or pull request, GitHub adds a
cross-reference to it that upstream's maintainers see. Avoid that noise:

- Cite upstream changes by short commit SHA. Never mention upstream issues, pull requests or security advisories,
  whether as `#123`, `owner/repo#123`, a link or a GHSA identifier, and refer to triage rows as "row 3".
- Refer to this fork's issues and pull requests as `digdir/microsandbox#123`, which always resolves to this
  repository.

Before pushing, this prints every reference in the commit messages that is not to this fork:

```sh
git log --format=%B <base>..HEAD \
  | grep -o -E '([[:alnum:]_.-]+/[[:alnum:]_.-]+)?#[0-9]+|GHSA-[[:alnum:]-]+|github\.com/[^/]+/[^/]+/(issues|pull)/[0-9]+' \
  | grep -v -E '^(digdir/microsandbox#|github\.com/digdir/microsandbox/)'
```

## Contributing upstream

Start upstream contribution branches from upstream's default branch and cherry-pick one Digdir change at a time. Do
not merge them back into `main-digdir`; a later upstream release brings accepted work into the base, where the patch
can be dropped.
