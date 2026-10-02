#!/usr/bin/env bash
# remove-unshipped.sh — remove the upstream surfaces that the Digdir fork does
# not ship, so that dependency alerts and reviews only cover what we build.
#
# Usage:
#   scripts/digdir/remove-unshipped.sh
#
# Run it from anywhere inside the repository. It is idempotent: it creates the
# cleanup commit's changes on a fresh upstream base, and during a
# synchronization rebase it resolves the modify/delete conflicts that upstream
# edits to removed paths cause. It stages its changes; it does not commit.
#
# Removes:
#   - the Go, Node.js, Python and Ruby SDKs and their examples;
#   - the TypeScript halves of the shared packages (the Rust halves and the
#     protocol fixtures stay, because the Rust crates and tests use them);
#   - the mcp and skills submodules;
#   - upstream community and Dependabot files that the Digdir organization
#     defaults or the fork's own settings replace.
#
# Also drops the removed crates from the Cargo workspace, their submodules
# from .gitmodules, and their packages from Cargo.lock.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

removed_paths=(
  sdk/go
  sdk/node-ts
  sdk/python
  sdk/ruby
  examples/python
  examples/typescript
  packages/agent-client/typescript
  packages/control-client/typescript
  packages/microsandbox-types/typescript
  packages/protocol-client/typescript
  packages/package.json
  packages/package-lock.json
  mcp
  skills
  CODE_OF_CONDUCT.md
  SECURITY.md
  .github/dependabot.yml
)

is_removed() {
  local path=$1 removed
  for removed in "${removed_paths[@]}"; do
    if [[ $path == "$removed" || $path == "$removed"/* ]]; then
      return 0
    fi
  done
  return 1
}

# Submodule entries first: `git rm` needs .gitmodules to stay consistent.
while read -r key path; do
  if is_removed "$path"; then
    git config --file .gitmodules --remove-section "${key%.path}"
  fi
done < <(git config --file .gitmodules --get-regexp '^submodule\..*\.path$' || true)
git add .gitmodules

git rm -r --quiet --cached --ignore-unmatch -- "${removed_paths[@]}"
rm -rf -- "${removed_paths[@]}"

# Drop workspace members under removed paths, keeping every other line as is.
in_members=false
while IFS= read -r line; do
  if [[ $line == "members = ["* ]]; then
    in_members=true
  elif $in_members && [[ $line == "]"* ]]; then
    in_members=false
  elif $in_members && [[ $line =~ ^[[:space:]]*\"([^\"]+)\",?[[:space:]]*$ ]] && is_removed "${BASH_REMATCH[1]}"; then
    continue
  fi
  printf '%s\n' "$line"
done < Cargo.toml > Cargo.toml.tmp
mv Cargo.toml.tmp Cargo.toml
git add Cargo.toml

# Resolving the workspace prunes the removed packages from the lockfile without
# upgrading anything else.
cargo metadata --format-version 1 > /dev/null
git add Cargo.lock

echo "Removed the upstream surfaces the Digdir fork does not ship. Review the staged changes."
