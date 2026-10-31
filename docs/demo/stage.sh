#!/bin/sh
# Builds the demo's pool on this Mac, all of it synthetic, and prints the
# folder it lives in. Source <folder>/env.sh to use it; `stage.sh --down
# <folder>` stops it and deletes it.
#
#   w=$(sh docs/demo/stage.sh) && . "$w/env.sh"
#   herdr-pool status
#   sh docs/demo/stage.sh --down "$w"
#
# Three machines, laptop (here), studio and mini, are hc cores of their own on
# this Mac, like the integration suite's: each has its own home and name, and
# reports the cores, memory, chip and load in machines/<name> (bin/sysctl).
# The repository is shop/, made up for the demo, and claude is bin/claude, a
# script. hc, herdr-pool, git, npm and Claude Code's config all live in the
# folder: nothing of yours is read or touched, and your own hc sessions don't
# show.
set -eu
kit=$(cd "$(dirname "$0")" && pwd)
repo_root=$(cd "$kit/../.." && pwd)
hc_bin=$(command -v herdr-core 2>/dev/null || echo "$HOME/.local/bin/herdr-core")

if [ "${1:-}" = --down ]; then
  w=${2:?usage: stage.sh --down <folder>}
  [ -f "$w/env.sh" ] || { echo "not a demo folder: $w" >&2; exit 2; }
  # shellcheck disable=SC1091
  . "$w/env.sh"
  herdr-pool rm --force $(herdr-pool ls --all --json 2>/dev/null | sed -n 's/.*"job": *"\([^"]*\)".*/\1/p') >/dev/null 2>&1 || :
  # Here first: while it runs, it dials the other machines and would start their cores again.
  for s in hpd hpd-studio hpd-mini; do HC_SESSION=$s "$hc_bin" server stop >/dev/null 2>&1 || :; done
  # A program still running from the folder (a core that didn't stop) ends by its PID.
  pids=$(ps -axo pid=,command= | awk -v w="$w/" 'index($2, w) == 1 {print $1}')
  [ -z "$pids" ] || kill $pids 2>/dev/null || :
  rm -rf "$w"
  exit 0
fi

bin="$repo_root/target/release/herdr-pool"
[ -x "$hc_bin" ] || { echo "needs herdr-core: curl -fsSL https://releases.herdr.dev/core/install.sh | sh" >&2; exit 2; }
[ -x "$bin" ] || { echo "build first: cargo build --release" >&2; exit 2; }
node_dir=$(dirname "$(command -v node)") || { echo "needs node" >&2; exit 2; }

# Short on purpose: Unix socket paths have a ~104 byte limit on macOS.
w=$(mktemp -d /tmp/hpd.XXXXXX)
mkdir -p "$w/bin" "$w/s" "$w/c/herdr-pool" "$w/c/herdr-core/templates" "$w/npm"
ln -s "$hc_bin" "$w/bin/herdr-core"
ln -s "$hc_bin" "$w/bin/hc"
ln -s "$bin" "$w/bin/herdr-pool"
for m in laptop studio mini; do
  mkdir -p "$w/m/$m"
  # Claude Code is set up there, as far as the pool can tell: it records its trust here.
  printf '{"projects": {}}\n' > "$w/m/$m/.claude.json"
done
printf '[user]\n\tname = Sam Rivera\n\temail = sam@example.com\n[init]\n\tdefaultBranch = main\n[advice]\n\tdetachedHead = false\n' > "$w/gitconfig"

cat > "$w/env.sh" <<EOF
# The demo pool's environment (stage.sh). This shell is now "laptop".
export DEMO_WORK='$w' DEMO_KIT='$kit' DEMO_MACHINE=laptop HOME='$w/m/laptop'
export PATH='$w/bin:$kit/bin:$node_dir:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin'
export XDG_STATE_HOME='$w/s' XDG_CONFIG_HOME='$w/c' HERDR_POOL_STATE='$w/pool-state.json'
export HC_SESSION=hpd HERDR_POOL_HC='$w/bin/herdr-core' SHELL='$kit/bin/login-shell'
export GIT_CONFIG_GLOBAL='$w/gitconfig' npm_config_cache='$w/npm' npm_config_update_notifier=false
export LANG=en_US.UTF-8 LC_ALL=en_US.UTF-8 DEMO_PACE='${DEMO_PACE:-1}'
unset HC_TERMINAL_ID HC_TERMINAL_SOCKET CLAUDE_CONFIG_DIR TERM_PROGRAM TERM_PROGRAM_VERSION
EOF
# shellcheck disable=SC1091
. "$w/env.sh"
cd "$HOME"

cat > "$XDG_CONFIG_HOME/herdr-pool/config.toml" <<'EOF'
[defaults]
max_agents = 4
max_jobs = 2

[machine.studio]
tags = ["big"]
max_jobs = 4

[machine.mini]
max_agents = 2
EOF

# Each core is named for its machine: hc names a core after the host the first
# time it starts and keeps the name in core.json.
name_core() { # name_core <session> <machine>
  HOME="$w/m/$2" DEMO_MACHINE=$2 HC_SESSION=$1 herdr-core server start >/dev/null
  HC_SESSION=$1 herdr-core server stop >/dev/null
  f="$XDG_STATE_HOME/herdr-core/$1/core.json"
  sed -i '' "s/\"name\": *\"[^\"]*\"/\"name\": \"$2\"/" "$f"
}
name_core hpd laptop
name_core hpd-studio studio
name_core hpd-mini mini

# The shop, with its lockfile, so trees install from it.
mkdir -p "$HOME/code"
cp -R "$kit/shop" "$HOME/code/shop"
cd "$HOME/code/shop"
git init -q
npm install --no-audit --no-fund --loglevel=error >/dev/null
git add -A
git commit -qm "Start the shop"

for m in studio mini; do
  herdr-pool add --via "env HOME=$w/m/$m DEMO_MACHINE=$m sh -c" --name "$m" --session "hpd-$m" >&2
done

# Warm trees on every machine, as a pool that has been in use has: their
# dependencies are installed, so the recording shows the work, not npm. A job
# holds one tree on studio and one here while the run takes every other slot,
# so each has one more than its share of shards: the agents' trees.
herdr-pool run --on studio --name hold-studio -- sleep 600 >&2
herdr-pool run --on local --name hold-laptop -- sleep 600 >&2
herdr-pool test --shards 8 -- npx vitest run >&2
herdr-pool rm --force hold-studio hold-laptop >&2
herdr-pool gc >&2

# Work in progress: run and test take it along.
printf '\nPrices are in cents everywhere; `formatPrice` makes them text.\n' >> README.md
echo "$w"
