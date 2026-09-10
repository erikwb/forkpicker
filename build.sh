#!/usr/bin/env bash
set -euo pipefail

project_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
if ! command -v makepkg >/dev/null 2>&1; then
  echo 'makepkg is required. Install the Arch Linux base-devel package first.' >&2
  exit 1
fi

# Never let makepkg use the checkout's src/ as its disposable extraction tree.
build_dir="$project_dir/.makepkg"
mkdir -p "$build_dir" "$project_dir/dist"
cp "$project_dir/PKGBUILD" "$build_dir/PKGBUILD"
tar -czf "$build_dir/forkpicker-source.tar.gz" -C "$project_dir" \
  Cargo.toml Cargo.lock src tests docs scripts \
  README.md CHANGELOG.md LICENSE PKGBUILD build.sh

cd "$build_dir"
export BUILDDIR="$build_dir/build"
export PKGDEST="${PKGDEST:-$project_dir/dist}"
# Always rebuild the current snapshot, even when its version is unchanged.
# Arguments retain their makepkg meaning: -s installs dependencies, -i installs.
exec makepkg --force --cleanbuild "$@"
