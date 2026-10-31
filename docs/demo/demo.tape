# The README's video, recorded by record.sh against the pool stage.sh builds.
# Run from target/demo: it writes raw.mp4 and shots/*.png there.

Output raw.mp4

Set Shell "bash"
Set FontFamily "JetBrainsMono Nerd Font"
Set FontSize 16
Set LineHeight 1.15
Set Width 1280
Set Height 780
Set Padding 22
Set Margin 0
Set WindowBar Colorful
Set WindowBarSize 36
Set BorderRadius 10
Set Theme "Catppuccin Mocha"
Set TypingSpeed 42ms
Set CursorBlink false
Set Framerate 30
Set WaitTimeout 60s

Require herdr-pool

# One step per line, each ending the shell if it fails (and with it the take):
# outside the demo's pool, these commands would reach your own machines. ($HOME,
# not ~: bash expands a ~ on the line it reads, before source changes HOME.)
Hide
Type `source "$DEMO_ENV" && [ "$HC_SESSION" = hpd ] || exit 1`
Enter
Type `cd "$HOME/code/shop" || exit 1`
Enter
Type `PS1='\[\e[38;5;245m\]\w\[\e[0m\] \[\e[38;2;203;166;247m\]❯\[\e[0m\] '`
Enter
Type "clear"
Enter
Sleep 1s
Show

# Three machines with very different room.
Sleep 600ms
Type "herdr-pool status"
Sleep 400ms
Enter
Wait+Screen /jobs running/
Sleep 4s
Screenshot shots/status.png
Sleep 700ms

# Two agents, each placed where there is room, each on a snapshot of this tree.
Ctrl+L
Sleep 300ms
Type `herdr-pool run --name retries claude "Add retries to the webhook client"`
Sleep 400ms
Enter
Wait+Screen /pull retries/
Sleep 1.8s
Screenshot shots/run.png
Sleep 700ms
Type `herdr-pool run --name prices claude "Show prices in the shop's currency"`
Sleep 400ms
Enter
Wait+Screen /pull prices/
Sleep 1.8s

# Where they are.
Ctrl+L
Sleep 300ms
Type "herdr-pool ls"
Sleep 300ms
Enter
Wait+Screen /live/
Sleep 2.6s
Screenshot shots/ls.png
Sleep 700ms

# Look over one's shoulder, on the other machine.
Ctrl+L
Sleep 300ms
Type "hc attach studio/retries"
Sleep 400ms
Enter
Sleep 5s
Screenshot shots/attach.png
Sleep 700ms
Ctrl+\
Sleep 800ms

# Meanwhile, the whole suite, split over the machines.
Ctrl+L
Sleep 300ms
Type "herdr-pool test --shards 6 -- npx vitest run"
Sleep 400ms
Enter
Wait+Screen /[2-5] of 6 done/
Sleep 300ms
Screenshot shots/test-live.png
Sleep 700ms
Wait+Screen /of 6 passed/
Sleep 3s
Screenshot shots/test.png
Sleep 700ms

# Until the agents are done.
Ctrl+L
Sleep 300ms
Type "herdr-pool wait"
Sleep 300ms
Enter
Wait+Screen@90s /prices +done/
Wait+Screen@90s /retries +done/
Sleep 1.5s

# Their work, back here.
Type "herdr-pool diff retries --stat"
Sleep 300ms
Enter
Sleep 2.2s
Type "herdr-pool pull retries --apply"
Sleep 300ms
Enter
Wait+Screen /applied/
Sleep 1.6s
Type "git status --short"
Sleep 300ms
Enter
Sleep 2.5s
Screenshot shots/pull.png
Sleep 700ms
Sleep 1s
