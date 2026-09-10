# Local source package. Run ./build.sh to snapshot the checkout and invoke makepkg.
pkgname=forkpicker
pkgver=0.1.0
pkgrel=1
pkgdesc='Discover useful changes in GitHub forks and review them with coding-agent CLIs'
url='https://github.com/erikwb/forkpicker'
arch=('x86_64' 'aarch64')
license=('MIT')
depends=('git' 'gcc-libs' 'glibc')
makedepends=('cargo')
optdepends=(
  'github-cli: use an existing GitHub login for API authentication'
  'xdg-utils: open reports in the default browser with --open'
)
# Cargo's release profile already enables thin LTO and strips the executable.
options=('!lto' '!debug')
source=('forkpicker-source.tar.gz')
# This is a fresh local checkout snapshot, not a downloaded release archive.
sha256sums=('SKIP')

prepare() {
  cd "$srcdir"
  cargo fetch --locked --target "$CARCH-unknown-linux-gnu"
}

build() {
  cd "$srcdir"
  cargo build --release --frozen --target "$CARCH-unknown-linux-gnu" \
    --target-dir "$startdir/target"
}

check() {
  cd "$srcdir"
  cargo test --frozen --target "$CARCH-unknown-linux-gnu" \
    --target-dir "$startdir/target"
}

package() {
  cd "$srcdir"
  install -Dm755 "$startdir/target/$CARCH-unknown-linux-gnu/release/forkpicker" \
    "$pkgdir/usr/bin/forkpicker"
  install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
  install -Dm644 README.md CHANGELOG.md -t "$pkgdir/usr/share/doc/$pkgname/"
}
