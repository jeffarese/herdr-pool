#!/bin/sh
# End to end against a real hc. A scratch hc session is "here"; another
# session on this host stands in for machine `a` (added with --via 'sh -c').
# Machine `b` is a third session here too, or, with HPIT_REMOTE=user@host,
# that real machine over ssh, running a scratch core whose files all live in
# one folder under /tmp there. Your own hc sessions, config and templates are
# never touched (here or there), and every core this starts is stopped at the end.
#
#   cargo build --release && sh tests/integration.sh
#   HPIT_REMOTE=you@other-mac sh tests/integration.sh
set -u
here=$(cd "$(dirname "$0")/.." && pwd)
bin=${HERDR_POOL_BIN:-$here/target/release/herdr-pool}
hc=$(command -v herdr-core 2>/dev/null || echo "$HOME/.local/bin/herdr-core")
remote=${HPIT_REMOTE:-}
[ -x "$hc" ] || { echo "needs herdr-core: curl -fsSL https://releases.herdr.dev/core/install.sh | sh"; exit 2; }
[ -x "$bin" ] || { echo "build first: cargo build --release"; exit 2; }

# Short on purpose: Unix socket paths have a ~104 byte limit on macOS.
work=$(mktemp -d /tmp/hpit.XXXXXX)
export XDG_STATE_HOME="$work/s" XDG_CONFIG_HOME="$work/c" HERDR_POOL_STATE="$work/pool-state.json"
export HC_SESSION=hpit HERDR_POOL_HC="$hc"
unset HC_TERMINAL_ID HC_TERMINAL_SOCKET
export GIT_CONFIG_GLOBAL="$work/gitconfig"
printf '[user]\n\tname = t\n\temail = t@t\n[init]\n\tdefaultBranch = main\n' > "$GIT_CONFIG_GLOBAL"
# Claude Code's config for every core here: the pool trusts work trees in
# this one, never in yours.
export CLAUDE_CONFIG_DIR="$work/claude"
mkdir "$CLAUDE_CONFIG_DIR" && printf '{"projects": {}}\n' > "$CLAUDE_CONFIG_DIR/.claude.json"

there() { # there <script>: runs on the remote machine, with b's scratch core
  ssh -o BatchMode=yes -o ConnectTimeout=10 "$remote" \
    "export XDG_STATE_HOME='$work/rs' XDG_CONFIG_HOME='$work/rc' CLAUDE_CONFIG_DIR='$work/claude' HC_SESSION=hpit-b; $1"
}
remote_ready=
cleanup() {
  # "Here" first: while it runs, it dials its machines again and would
  # start their cores anew.
  HC_SESSION=hpit "$hc" server stop >/dev/null 2>&1 || :
  if [ -n "$remote_ready" ]; then
    there "p=\$(command -v herdr-core) || p=\$HOME/.local/bin/herdr-core; \"\$p\" server stop >/dev/null 2>&1; rm -rf '$work'" || :
  fi
  for s in hpit-a hpit-b; do HC_SESSION=$s "$hc" server stop >/dev/null 2>&1 || :; done
  rm -rf "$work"
}
trap cleanup EXIT INT TERM

pass=0 failed=0
ok() { pass=$((pass + 1)); echo "ok    $1"; }
bad() { failed=$((failed + 1)); echo "FAIL  $1"; [ -n "${2:-}" ] && printf '%s\n' "$2" | sed 's/^/      /'; }
check() { # check <name> <command...>: passes when the command does
  name=$1; shift
  if out=$("$@" 2>&1); then ok "$name"; else bad "$name" "$out"; fi
}
has() { printf '%s' "$1" | grep -q -- "$2"; }
until_true() { # until_true <seconds> <command...>
  n=$1; shift
  while [ "$n" -gt 0 ]; do "$@" >/dev/null 2>&1 && return 0; sleep 1; n=$((n - 1)); done
  return 1
}
shards_listed() { "$bin" ls | grep -q ' shard '; }

mkdir -p "$XDG_CONFIG_HOME/herdr-pool" "$XDG_CONFIG_HOME/herdr-core/templates"
cat > "$XDG_CONFIG_HOME/herdr-pool/config.toml" <<EOF
[defaults]
max_jobs = 2
[machine.local]
root = "$work/root-local"
[machine.a]
root = "$work/root-a"
tags = ["big"]
[machine.b]
root = "$work/root-b"
EOF

# A stand-in agent, called claude: says its status the way Claude Code does
# (OSC 7501), takes its task from the terminal, does some work and reports
# back the way the footer tells it to. It sits at the same path on every
# machine, so one template serves them all.
mkdir "$work/bin"
cat > "$work/bin/claude" <<'EOF'
#!/bin/sh
HC=$(command -v herdr-core 2>/dev/null || echo "$HOME/.local/bin/herdr-core")
st() { printf '\033]7501;state=%s\033\\' "$1"; }
st idle
task=""
while IFS= read -r line; do
  task="$task$line
"
  case $line in *"hc send "*|*"short summary"*) break ;; esac
done
st working
printf '%s' "$task" > task.txt
echo "agent work" > agent.txt
to=$(printf '%s' "$task" | sed -n 's/.*report with: hc send \([^ ]*\) .*/\1/p' | head -n 1)
[ -n "$to" ] && "$HC" send "$to" "fake: done on $HC_SESSION"
st done
sleep 600
EOF
chmod +x "$work/bin/claude"
cat > "$XDG_CONFIG_HOME/herdr-core/templates/fakeagent.md" <<EOF
---
description: a fake agent for tests
command: $work/bin/claude
---
You are a fake agent.
EOF
if [ -n "$remote" ]; then
  there "mkdir -p '$work/bin' '$work/claude' && printf '{\"projects\": {}}\\n' > '$work/claude/.claude.json' && cat > '$work/bin/claude' && chmod +x '$work/bin/claude'" < "$work/bin/claude" || { echo "can't set up $remote"; exit 2; }
  remote_ready=1
fi

# A repository with uncommitted and untracked work, setup that leaves an
# ignored marker, and a test that fails for shards listed in fail.txt.
repo="$work/repo"
mkdir -p "$repo" && cd "$repo" || exit 2
git init -q
printf 'deps/\n' > .gitignore
printf 'setup = "mkdir -p deps && date +%%s%%N > deps/ready"\nsetup_when = ["setup.lock"]\ntest = "sh t.sh {shard} {total}"\n' > .herdr-pool.toml
echo 1 > setup.lock
cat > t.sh <<'EOF'
echo "shard $1 of $2 in $(pwd)"
[ -f deps/ready ] || { echo "setup never ran"; exit 3; }
if grep -qx "$1" fail.txt 2>/dev/null; then echo "shard $1 failed on purpose"; exit 1; fi
EOF
echo committed > a.txt
git add -A && git commit -qm one
echo uncommitted >> a.txt
echo untracked > u.txt

echo "hc $("$hc" --version) · herdr-pool $("$bin" --version) · work $work${remote:+ · b is $remote}"

# --- machines ------------------------------------------------------------
check "add machine a" "$bin" add --via 'sh -c' --name a --session hpit-a
if [ -n "$remote" ]; then
  check "add machine b ($remote)" "$bin" add --via "ssh -T -o BatchMode=yes $remote env XDG_STATE_HOME=$work/rs XDG_CONFIG_HOME=$work/rc CLAUDE_CONFIG_DIR=$work/claude" --name b --session hpit-b
else
  check "add machine b" "$bin" add --via 'sh -c' --name b --session hpit-b
fi
out=$("$bin" status 2>&1)
if has "$out" "^a  *ok" && has "$out" "^b  *ok" && has "$out" "^local"; then ok "status lists three machines"; else bad "status lists three machines" "$out"; fi
out=$("$bin" each -- 'echo "session=$HC_SESSION"' 2>&1)
if has "$out" "session=hpit-a" && has "$out" "session=hpit-b" && has "$out" "session=hpit$"; then ok "each runs on every machine"; else bad "each runs on every machine" "$out"; fi

# --- two runs at once are the first to use a machine ---------------------
"$bin" run --on b --name first1 -- true > "$work/first1.txt" 2>&1 & p1=$!
"$bin" run --on b --name first2 -- true > "$work/first2.txt" 2>&1 & p2=$!
wait $p1; c1=$?; wait $p2; c2=$?
if [ $c1 = 0 ] && [ $c2 = 0 ]; then ok "two first runs on a machine at once"; else bad "two first runs on a machine at once" "$(cat "$work/first1.txt" "$work/first2.txt")"; fi
"$bin" wait first1 first2 --timeout 60 >/dev/null 2>&1
check "rm both" "$bin" rm first1 first2

# --- a command in a work tree, and its work back -------------------------
out=$("$bin" run --on a --name job1 -- sh -c 'cat a.txt u.txt > seen.txt; echo work > w.txt; git add w.txt; git commit -qm "job commit"; echo late > late.txt' 2>&1)
if has "$out" "^job1  a/t" && has "$out" "+uncommitted"; then ok "run places on a with a dirty snapshot"; else bad "run places on a with a dirty snapshot" "$out"; fi
out=$("$bin" wait job1 --timeout 60 2>&1)
if has "$out" "exited (0)"; then ok "wait sees it exit"; else bad "wait sees it exit" "$out"; fi
out=$("$bin" cat job1 seen.txt 2>&1)
if has "$out" "uncommitted" && has "$out" "untracked"; then ok "the job saw uncommitted and untracked files"; else bad "the job saw uncommitted and untracked files" "$out"; fi
out=$("$bin" diff job1 --stat 2>&1)
if has "$out" "late.txt" && has "$out" "w.txt"; then ok "diff shows committed and uncommitted work"; else bad "diff shows committed and uncommitted work" "$out"; fi
out=$("$bin" rm job1 2>&1); code=$?
if [ $code != 0 ] && has "$out" "work you haven't pulled"; then ok "rm refuses unpulled work"; else bad "rm refuses unpulled work" "$out"; fi
out=$("$bin" pull job1 2>&1)
if has "$out" "^pool/job1  2 commit" && [ "$(git show pool/job1:late.txt 2>/dev/null)" = late ]; then ok "pull brings commits and uncommitted work"; else bad "pull brings commits and uncommitted work" "$out"; fi
if [ "$(git status --porcelain | tr '\n' ' ')" = " M a.txt ?? u.txt " ]; then ok "your work tree and index are untouched"; else bad "your work tree and index are untouched" "$(git status --porcelain)"; fi
check "rm after pull" "$bin" rm job1
out=$("$bin" ls --all 2>&1)
if ! has "$out" "job1"; then ok "ls no longer lists job1"; else bad "ls no longer lists job1" "$out"; fi

# --- where things run ----------------------------------------------------
out=$("$bin" run --here --on a -- true 2>&1); code=$?
if [ $code = 2 ] && has "$out" "this machine"; then ok "--here never runs elsewhere"; else bad "--here never runs elsewhere" "$out"; fi
out=$("$bin" run --needs big --name onbig -- sleep 30 2>&1)
if has "$out" "on a"; then ok "--needs big lands on a"; else bad "--needs big lands on a" "$out"; fi
"$bin" run --needs big --name onbig2 -- sleep 30 >/dev/null 2>&1
out=$("$bin" run --needs big --name onbig3 -- sleep 30 2>&1)
if has "$out" "no machine has room"; then ok "a full machine refuses more"; else bad "a full machine refuses more" "$out"; fi
check "rm --force the sleepers" "$bin" rm onbig onbig2 --force

# --- a file bigger than hc passes back in one go -------------------------
"$bin" run --on b --name big -- sh -c 'head -c 10000000 /dev/zero | tr "\0" x > big.bin' >/dev/null 2>&1
"$bin" wait big --timeout 60 >/dev/null 2>&1
size=$("$bin" cat big big.bin 2>/dev/null | wc -c | tr -d ' ')
if [ "$size" = 10000000 ]; then ok "cat brings a 10 MB file whole"; else bad "cat brings a 10 MB file whole" "got $size bytes"; fi
check "rm big" "$bin" rm big --force

# --- an agent on another machine reports to its parent here --------------
cat > "$work/parent.sh" <<EOF
"$bin" run --on b --name ag1 fakeagent "Say hello" > "$work/parent-run.txt" 2>&1
"$hc" wait --children any --timeout 60 > "$work/parent-wait.txt" 2>&1
IFS= read -r msg
printf '%s\n' "\$msg" > "$work/parent-msg.txt"
sleep 600
EOF
"$hc" run --bg --name parent -- sh "$work/parent.sh" >/dev/null 2>&1
until_true 90 test -s "$work/parent-msg.txt"
out=$(cat "$work/parent-run.txt" 2>/dev/null)
if has "$out" "^ag1  b/t" && has "$out" "reports to "; then ok "agent started on b with a reply address"; else bad "agent started on b with a reply address" "$out"; fi
out=$(cat "$work/parent-msg.txt" 2>/dev/null)
if has "$out" "fake: done on hpit-b"; then ok "the agent's report reached its parent here"; else bad "the agent's report reached its parent here" "$out$(cat "$work/parent-wait.txt" 2>/dev/null)"; fi
out=$("$bin" cat ag1 task.txt 2>&1)
if has "$out" "You are a fake agent" && has "$out" "Say hello" && has "$out" "job ag1 on b"; then ok "the agent got template, task and footer"; else bad "the agent got template, task and footer" "$out"; fi
if [ -n "$remote" ]; then trusted=$(there "cat '$work/claude/.claude.json'"); else trusted=$(cat "$CLAUDE_CONFIG_DIR/.claude.json"); fi
if has "$trusted" "root-b/repos/repo-[0-9a-f]*\.git\": {" && has "$trusted" '"hasTrustDialogAccepted": true'; then ok "Claude Code on b trusts the repository's trees"; else bad "Claude Code on b trusts the repository's trees" "$trusted"; fi
out=$("$bin" pull ag1 --apply 2>&1)
if has "$out" "applied" && [ -f agent.txt ]; then ok "pull --apply puts the agent's work in your tree"; else bad "pull --apply puts the agent's work in your tree" "$out"; fi
rm -f agent.txt task.txt
"$hc" kill parent >/dev/null 2>&1
check "rm the agent" "$bin" rm ag1 --force

# --- tests split across machines ----------------------------------------
"$bin" test --shards 2 -- 'sleep 4; echo {shard}' > "$work/slow.txt" 2>&1 & tp=$!
until_true 60 shards_listed
out=$("$bin" ls 2>&1)
if has "$out" " shard " && ! has "$out" "gone"; then ok "ls during a test run: its shards, nothing gone"; else bad "ls during a test run: its shards, nothing gone" "$out"; fi
wait $tp
"$bin" test --shards 2 -- 'sleep 60; echo {shard}' > "$work/stopped.txt" 2>&1 & tp=$!
until_true 60 shards_listed
kill -INT $tp
wait $tp; code=$?
out=$("$bin" ls --all 2>&1)
if [ $code = 130 ] && has "$(cat "$work/stopped.txt")" "stopped test-" && has "$out" "no pool jobs"; then ok "Ctrl-C ends a test run and leaves nothing behind"; else bad "Ctrl-C ends a test run and leaves nothing behind" "exit $code: $(cat "$work/stopped.txt")
$out"; fi
echo 2 > fail.txt
out=$("$bin" test --shards 3 2>&1); code=$?
if [ $code = 1 ] && has "$out" "FAIL  2/3" && has "$out" "shard 2 failed on purpose" && has "$out" "FAILED: 1 of 3"; then ok "a failing shard fails the run, with its tail"; else bad "a failing shard fails the run, with its tail" "$out"; fi
run=$(printf '%s\n' "$out" | sed -n 's/^test \(test-[a-z0-9]*\):.*/\1/p')
out=$("$bin" rm "$run" 2>&1)
if has "$out" "removed $run" && ! "$bin" ls --all | grep -q "$run"; then ok "rm takes a test run's failed shards"; else bad "rm takes a test run's failed shards" "$out"; fi
rm fail.txt
out=$("$bin" test --shards 3 2>&1); code=$?
if [ $code = 0 ] && has "$out" "passed: 3 of 3"; then ok "a passing run"; else bad "a passing run" "$out"; fi
out=$("$bin" test --each -- 'test -f deps/ready && echo warm' 2>&1); code=$?
machines=$(printf '%s\n' "$out" | sed -nE 's/^ok +[0-9]+\/3 +([a-z]+).*/\1/p' | sort | tr '\n' ' ')
if [ $code = 0 ] && [ "$machines" = "a b local " ]; then ok "--each runs once on every machine, trees warm"; else bad "--each runs once on every machine, trees warm" "$out"; fi

# --- cleanup --------------------------------------------------------------
check "gc" "$bin" gc
out=$("$bin" ls --all 2>&1)
if has "$out" "no pool jobs"; then ok "nothing left after gc"; else bad "nothing left after gc" "$out"; fi
out=$("$bin" doctor 2>&1)
if has "$out" "^a  ok" && has "$out" "^b  ok"; then ok "doctor checks every machine"; else bad "doctor checks every machine" "$out"; fi

# --- what each machine keeps of the repository -----------------------------
on_b() { if [ -n "$remote" ]; then there "$1"; else sh -c "$1"; fi; }
key=$(cd "$work/root-local/repos" 2>/dev/null && ls -d repo-*.git | head -n 1)
key=${key%.git}
out=$("$bin" repos 2>&1)
if [ -n "$key" ] && has "$out" "^local  $key " && has "$out" "^a  $key " && has "$out" "^b  $key "; then ok "repos lists the copy on every machine"; else bad "repos lists the copy on every machine" "$out"; fi
"$bin" run --on b --name holder -- sleep 60 >/dev/null 2>&1
out=$("$bin" repos rm --on b 2>&1); code=$?
if [ $code = 1 ] && has "$out" "holder is still at work" && on_b "test -d '$work/root-b/repos/$key.git'"; then ok "a job at work keeps its copy"; else bad "a job at work keeps its copy" "$out"; fi
"$bin" rm holder --force >/dev/null 2>&1
# A Claude Code session log of an agent in one of b's trees, named the way
# Claude Code names them.
real=$(on_b "cd '$work/root-b' && pwd -P")
log=$(printf '%s' "$real/trees/$key/1" | sed 's/[^A-Za-z0-9]/-/g')
on_b "mkdir -p '$work/claude/projects/$log'"
out=$("$bin" repos rm --on b 2>&1); code=$?
if [ $code = 0 ] && has "$out" "^b: deleted $key (" && has "$out" "session log stays: .*$log (herdr-pool repos rm $key --agent-logs" \
  && ! on_b "test -e '$work/root-b/repos/$key.git' || test -e '$work/root-b/trees/$key'" && on_b "test -d '$work/claude/projects/$log'"; then
  ok "repos rm deletes b's copy and says where agents' logs stay"
else bad "repos rm deletes b's copy and says where agents' logs stay" "$out"; fi
out=$("$bin" repos rm --on b --agent-logs 2>&1); code=$?
if [ $code = 0 ] && has "$out" "deleted 1 agent session log of $key" && ! on_b "test -e '$work/claude/projects/$log'"; then ok "--agent-logs deletes those logs"; else bad "--agent-logs deletes those logs" "$out"; fi
out=$("$bin" run --on b --name again -- true 2>&1)
"$bin" wait again --timeout 60 >/dev/null 2>&1
if has "$out" "^again  b/t"; then ok "a run afterwards makes the copy again"; else bad "a run afterwards makes the copy again" "$out"; fi
"$bin" rm again >/dev/null 2>&1
out=$("$bin" repos rm --all 2>&1); code=$?
left=$(find "$work/root-local" "$work/root-a" -mindepth 1 2>/dev/null; on_b "find '$work/root-b' -mindepth 1 2>/dev/null")
if [ $code = 0 ] && [ -z "$left" ]; then ok "repos rm --all leaves nothing on any machine"; else bad "repos rm --all leaves nothing on any machine" "$out
$left"; fi

echo "$pass passed, $failed failed"
[ "$failed" = 0 ]
