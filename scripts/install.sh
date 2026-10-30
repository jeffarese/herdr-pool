#!/bin/sh
# Builds herdr-pool, installs it as ~/.local/bin/herdr-pool, installs the
# agent skill for Claude Code (every ~/.claude*/skills), and offers the guided
# setup. Run again to update.
set -eu
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
cargo build --release --locked
bin="${HERDR_POOL_INSTALL_DIR:-$HOME/.local/bin}"
mkdir -p "$bin"
install -m 0755 target/release/herdr-pool "$bin/herdr-pool"
echo "herdr-pool: installed $bin/herdr-pool"
"$bin/herdr-pool" skill install
case ":$PATH:" in
  *":$bin:"*) ;;
  *) echo "herdr-pool: add $bin to your PATH" ;;
esac
# At a terminal, offer the guided setup (it asks before changing anything).
if [ -t 0 ] && [ -t 1 ]; then
  printf 'herdr-pool: set up your machines now (herdr-pool init)? [Y/n] '
  read -r answer || answer=n
  case $answer in
    ""|[Yy]*) exec "$bin/herdr-pool" init ;;
  esac
fi
echo "herdr-pool: next, herdr-pool init"

