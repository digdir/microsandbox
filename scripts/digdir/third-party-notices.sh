#!/usr/bin/env bash
# third-party-notices.sh — write the third-party notices for a Digdir runtime
# release.
#
# Usage:
#   scripts/digdir/third-party-notices.sh <output-file>
#
# Covers the components a runtime release ships: the Rust dependencies of the
# `msb` host runtime and of the guest agent embedded in it, the musl C library
# the guest agent links statically, and libkrunfw with its bundled Linux
# kernel, followed by the license files the Rust dependencies ship. Requires
# cargo-about, Python 3 and an initialized vendor/libkrunfw submodule.
#
# notices/musl-COPYRIGHT is musl's COPYRIGHT file from the musl release that
# Rust's musl targets bundle for the pinned toolchain. Update it when that
# release changes.

set -euo pipefail

if (( $# != 1 )); then
  echo "Usage: $0 <output-file>" >&2
  exit 2
fi

output=$(realpath -m -- "$1")
cd "$(git rev-parse --show-toplevel)"

config=scripts/digdir/about.toml
template=scripts/digdir/about.hbs
kernel_version=$(awk '$1 == "KERNEL_VERSION" { print $3; exit }' vendor/libkrunfw/Makefile)
if [[ -z $kernel_version ]]; then
  echo "Could not read KERNEL_VERSION from vendor/libkrunfw/Makefile; is the submodule initialized?" >&2
  exit 1
fi

msb_args=(
  --manifest-path crates/cli/Cargo.toml
  --no-default-features --features embed-binaries,net,ssh
  --target x86_64-unknown-linux-gnu --target aarch64-unknown-linux-gnu
  --target aarch64-apple-darwin
  --target x86_64-pc-windows-msvc --target aarch64-pc-windows-msvc
)
agentd_args=(
  --manifest-path crates/agentd/Cargo.toml
  --target x86_64-unknown-linux-musl --target aarch64-unknown-linux-musl
)
reports=$(mktemp -d)
trap 'rm -rf "$reports"' EXIT
cargo about generate --locked --config "$config" --format json "${msb_args[@]}" > "$reports/msb.json"
cargo about generate --locked --config "$config" --format json "${agentd_args[@]}" > "$reports/agentd.json"

{
  cat <<EOF
# Third-party notices

This release of the Digdir build of Microsandbox contains the \`msb\` host
runtime, the guest agent embedded in it, and the libkrunfw library.
Microsandbox is licensed under Apache-2.0; see \`LICENSE\`. This file lists the
third-party components the release contains and their licenses.

## libkrunfw and the Linux kernel

libkrunfw bundles the Linux kernel (${kernel_version}). The Linux kernel and
the libkrunfw kernel patches are licensed under GPL-2.0-only; see
\`LICENSE-libkrunfw-GPL-2.0-only\`. The libkrunfw library code is licensed under
LGPL-2.1-only; see \`LICENSE-libkrunfw-LGPL-2.1-only\`. The corresponding source
is attached to this release as \`libkrunfw-source.tar.gz\` and
\`${kernel_version}.tar.gz\`.

## musl

The guest agent is statically linked with musl, the C library that Rust's musl
targets bundle. Its copyright and license terms follow.

\`\`\`text
EOF
  cat scripts/digdir/notices/musl-COPYRIGHT
  cat <<EOF
\`\`\`

## Rust dependencies of msb

EOF
  cargo about generate --locked --config "$config" "${msb_args[@]}" "$template"
  printf '\n## Rust dependencies of the guest agent\n\n'
  cargo about generate --locked --config "$config" "${agentd_args[@]}" "$template"
  cat <<EOF

## License files shipped by the Rust dependencies

The license texts above are identified automatically and do not always reproduce
the copyright notices of each dependency. The license, copying and notice files
that the dependencies ship are reproduced here verbatim; identical files are
listed once.

EOF
  python3 scripts/digdir/license-files.py "$reports/msb.json" "$reports/agentd.json"
} > "$output"

echo "Wrote $output"
