#!/usr/bin/env bash
set -euo pipefail

# Build for this machine's Rust host target. No upload or publication occurs.
project_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$project_dir"
release_target=$(rustc -vV | sed -n 's/^host: //p')
case "$release_target" in
    *-unknown-linux-gnu|*-apple-darwin) ;;
    *) echo "Unsupported release host: $release_target" >&2; exit 1 ;;
esac
release_target_dir=${CARGO_TARGET_DIR:-target}
cargo build --release --locked --target "$release_target" --target-dir "$release_target_dir"
release_binary="$release_target_dir/$release_target/release/forkpicker"
release_version=$("$release_binary" --version)
release_version=${release_version#forkpicker }
case "$release_version" in
    ''|*[!0-9A-Za-z.+-]*) echo "Invalid binary version: $release_version" >&2; exit 1 ;;
esac
release_name="forkpicker-$release_version-$release_target"
release_stage=$(mktemp -d)
trap 'rm -rf -- "$release_stage"' EXIT
mkdir -p "$release_stage/$release_name" dist
cp "$release_binary" README.md LICENSE CHANGELOG.md "$release_stage/$release_name/"
cp -R docs "$release_stage/$release_name/"
# Include source referenced by the documentation and its build manifest.
cp -R src tests scripts "$release_stage/$release_name/"
cp Cargo.toml Cargo.lock "$release_stage/$release_name/"
cp PKGBUILD build.sh "$release_stage/$release_name/"
tar -czf "$release_stage/$release_name.tar.gz" -C "$release_stage" "$release_name"
mv "$release_stage/$release_name.tar.gz" "dist/$release_name.tar.gz"
(
    cd dist
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$release_name.tar.gz" > "$release_name.tar.gz.sha256"
    else
        shasum -a 256 "$release_name.tar.gz" > "$release_name.tar.gz.sha256"
    fi
)
echo "Archive: $project_dir/dist/$release_name.tar.gz"
echo "Checksum: $project_dir/dist/$release_name.tar.gz.sha256"
