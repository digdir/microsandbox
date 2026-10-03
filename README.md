# Microsandbox (Digdir fork)

This is the Norwegian Digitalisation Agency's (Digdir) fork of
[Microsandbox](https://github.com/superradcompany/microsandbox), which runs untrusted workloads inside local microVMs.
The fork builds and publishes the host runtime for the Rust SDK. See the upstream repository for the original
documentation.

## Branches

- `main-digdir` (default): the Digdir patch queue on a stable upstream release tag. It is rebuilt on each
  synchronization with upstream, so its history is rewritten.
- `main`: a mirror of upstream `main`, updated by fast-forward only.

## Using the Digdir build

Use the Rust SDK as a Git dependency pinned to a release tag, `v<upstream-version>-digdir.<n>`. The SDK installs the
host runtime from the matching release of this repository.

## Digdir modifications

The fork stays as close to upstream as possible and adds a change only where it is clearly needed, because every
change has to be carried forward on each synchronization. The changes are the commits on `main-digdir` after the
upstream release tag, such as host-side authorization of sandbox network traffic, portable prepared root filesystems,
fixes for Windows hosts and publishing the runtime from this repository.

The fork also removes upstream surfaces it does not ship, such as the Go, Node.js, Python and Ruby SDKs. Upstream's
`DEVELOPMENT.md` and `CONTRIBUTING.md` are kept unchanged, so they still refer to some of them.

## Contributing and maintenance

[CONTRIBUTING-digdir.md](CONTRIBUTING-digdir.md) describes how to change the fork, and
[MAINTAINING-digdir.md](MAINTAINING-digdir.md) how it is synchronized with upstream and how runtimes are released.
Building is unchanged from upstream; see [DEVELOPMENT.md](DEVELOPMENT.md).

Report problems with the Digdir changes or builds as issues in this repository. Problems that also exist upstream, and
changes that are useful beyond Digdir, belong upstream. Report security vulnerabilities as described in [Digdir's
security policy](https://github.com/digdir/.github/blob/main/SECURITY.md), not in public issues or pull requests.

## License

Microsandbox and the Digdir modifications are licensed under the Apache License 2.0 ([LICENSE](LICENSE)). Each runtime
release also publishes the license texts and third-party notices for what it contains, and the corresponding source of
the bundled libkrunfw firmware.
