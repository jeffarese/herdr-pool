#!/bin/sh
# Installs herdr-pool as ~/.local/bin/herdr-pool (or $HERDR_POOL_INSTALL_DIR),
# installs the agent skill for Claude Code (every ~/.claude*/skills), and
# offers the guided setup. Run it again to update.
#
# From a checkout it builds with Cargo:
#   sh scripts/install.sh
# Anywhere else it downloads the release binary for this machine (macOS,
# Linux or WSL; x86_64 or arm64) and checks its SHA-256:
#   curl -fsSL https://raw.githubusercontent.com/jeffarese/herdr-pool/main/scripts/install.sh | sh
#
# --prebuilt downloads even from a checkout. HERDR_POOL_VERSION=0.2.0 picks a
# release (default: the latest); HERDR_POOL_DOWNLOAD_URL is where the release
# files are (tests, mirrors).
set -eu
repo=jeffarese/herdr-pool
bin="${HERDR_POOL_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf 'herdr-pool: %s\n' "$*"; }
die() { printf 'herdr-pool: %s\n' "$*" >&2; exit 1; }

prebuilt=${HERDR_POOL_PREBUILT:-}
for arg in "$@"; do
  case $arg in
    --prebuilt) prebuilt=1 ;;
    -h | --help) sed -n '2,15p' "$0" 2>/dev/null | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown option $arg" ;;
  esac
done

# A checkout: this file sits in <repo>/scripts next to Cargo.toml.
root=
if [ -z "$prebuilt" ] && [ -f "$0" ]; then
  root=$(cd "$(dirname "$0")/.." && pwd)
  grep -q '^name = "herdr-pool"$' "$root/Cargo.toml" 2>/dev/null || root=
fi

if [ -n "$root" ]; then
  cd "$root"
  command -v cargo >/dev/null 2>&1 ||
    die "needs Rust 1.85 or later (https://rustup.rs), or run with --prebuilt"
  cargo build --release --locked
  mkdir -p "$bin"
  install -m 0755 target/release/herdr-pool "$bin/herdr-pool"
else
  command -v curl >/dev/null 2>&1 || die "needs curl"
  case "$(uname -s)" in
    Darwin) os=apple-darwin ;;
    Linux) os=unknown-linux-musl ;;
    *) die "herdr-pool runs on macOS and Linux (WSL included), not $(uname -s)" ;;
  esac
  case "$(uname -m)" in
    x86_64 | amd64) arch=x86_64 ;;
    arm64 | aarch64) arch=aarch64 ;;
    *) die "no release build for $(uname -m); build from source instead" ;;
  esac
  asset="herdr-pool-$arch-$os"
  if [ -n "${HERDR_POOL_DOWNLOAD_URL:-}" ]; then
    base=${HERDR_POOL_DOWNLOAD_URL%/}
  elif [ -n "${HERDR_POOL_VERSION:-}" ]; then
    base="https://github.com/$repo/releases/download/v${HERDR_POOL_VERSION#v}"
  else
    base="https://github.com/$repo/releases/latest/download"
  fi
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  say "downloading $asset from $base"
  curl -fsSL "$base/$asset.tar.gz" -o "$tmp/$asset.tar.gz" ||
    die "no $asset.tar.gz at $base (v0.1.0 has no binaries: build from source)"
  curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS" || die "no SHA256SUMS at $base"
  want=$(awk -v f="$asset.tar.gz" '$2 == f { print $1 }' "$tmp/SHA256SUMS")
  if command -v sha256sum >/dev/null 2>&1; then
    got=$(sha256sum "$tmp/$asset.tar.gz" | cut -d' ' -f1)
  else
    got=$(shasum -a 256 "$tmp/$asset.tar.gz" | cut -d' ' -f1)
  fi
  [ -n "$want" ] && [ "$want" = "$got" ] || die "checksum mismatch for $asset.tar.gz; nothing installed"
  tar -xzf "$tmp/$asset.tar.gz" -C "$tmp"
  "$tmp/$asset/herdr-pool" --version >/dev/null 2>&1 || die "the downloaded herdr-pool does not run here"
  mkdir -p "$bin"
  # Replace, don't overwrite in place: a running herdr-pool keeps its file.
  cp "$tmp/$asset/herdr-pool" "$bin/herdr-pool.new"
  chmod 0755 "$bin/herdr-pool.new"
  mv -f "$bin/herdr-pool.new" "$bin/herdr-pool"
fi

say "installed $bin/herdr-pool ($("$bin/herdr-pool" --version))"
"$bin/herdr-pool" skill install
case ":$PATH:" in
  *":$bin:"*) ;;
  *) say "add $bin to your PATH" ;;
esac
# At a terminal, offer the guided setup (it asks before changing anything).
# Piped from curl, stdin is the script: ask on the terminal instead.
if [ -t 1 ] && (: < /dev/tty) 2>/dev/null; then
  printf 'herdr-pool: set up your machines now (herdr-pool init)? [Y/n] '
  read -r answer < /dev/tty || answer=n
  case $answer in
    "" | [Yy]*) exec "$bin/herdr-pool" init < /dev/tty ;;
  esac
fi
say "next, herdr-pool init"
