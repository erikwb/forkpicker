# Releasing

The changelog stays marked **Unreleased** until a version is published. Uploading the source repository does not require publishing a release or a crates.io package.

## Validate the source package

From a clean checkout with a current stable Rust toolchain and Git:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
cargo package --locked
```

`cargo package` builds the extracted source package as well. During local preparation, `--allow-dirty` allows verification before committing. Review `cargo package --list` to confirm the contents: Cargo's explicit include list excludes saved reports, caches, and distribution archives. Prompts, dashboard assets, and tests are included.

Before publishing, confirm the manifest version, CLI `--version`, and changelog agree, and replace **Unreleased** with the release date. Check installation from the extracted crate with `cargo install --path PATH --locked --root TEMP_DIR`, then run its `forkpicker --help` and `forkpicker run --help`.

## Build a native archive

```sh
./scripts/package-release.sh
```

This builds for the local Rust host target on Linux or macOS and writes a `.tar.gz` and SHA-256 checksum under `dist/`. Each archive includes the executable, license, documentation, and source. It performs no upload, tagging, or publication.

Build each architecture on its corresponding release host. Linux GNU binaries depend on the build host's glibc baseline; use the oldest intended supported environment and test the archive on that baseline. macOS archives must likewise be tested on the intended architecture and OS version. A build from a development workstation alone does not establish compatibility with other systems.

Extract each archive into a temporary directory, verify its checksum, and run the bundled executable's `--version`, `--help`, and `run --help`. CI covers the source build on Linux and macOS; published binary compatibility needs its own verification.

## Build an Arch package

```sh
./build.sh
./build.sh -si
```

The root PKGBUILD builds a fresh local source snapshot, runs the test suite, and packages `/usr/bin/forkpicker` with its license and introductory documentation. `build.sh` passes options through to makepkg; `-si` installs missing dependencies and installs the completed package. Run it without sudo. Keep `pkgver` in PKGBUILD aligned with Cargo.toml; bump `pkgrel` for packaging-only revisions.

Source extraction happens under `.makepkg/build/`, Cargo artifacts stay under `.makepkg/target/`, and packages default to `dist/` (`PKGDEST` can override this). Every invocation replaces the extracted source and rebuilds the package, including local uncommitted edits, while retaining Cargo's compilation cache. Reports, caches, and previous packages are excluded from the snapshot. This PKGBUILD uses a local archive supplied by `build.sh`; an AUR submission would need a published source URL and checksum.

## Publish

Commit the release source, tag the matching version, and publish the reviewed archives with their checksums and changelog. If distributing through crates.io, also verify the crate name is available and run `cargo publish --dry-run` before publishing. Do not advertise `cargo install forkpicker` until that crate has actually been published.
