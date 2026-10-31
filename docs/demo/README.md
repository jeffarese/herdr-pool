# The README's demo

The [video](../media/demo.mp4) and stills on the [README](../../README.md) are one real run of herdr-pool
against a pool that is synthetic from end to end. Nothing in them comes from a real machine, repository or
person.

**Real:** herdr-pool (this repository's release build) and hc, and everything they do: placing jobs,
snapshots of the work tree, bundles, mirrors and work trees, dependency installs, the test shards and their
vitest runs, the agents' edits and test runs in their trees, `diff` and `pull`.

**Synthetic:**

- **The machines.** laptop, studio and mini are three hc cores on one Mac, like the integration suite's, each
  with its own home folder and core name. herdr-pool asks a machine for its cores, memory, chip and load with
  `sysctl`; for these, [bin/sysctl](bin/sysctl) answers from [machines/](machines) instead. `hostname` and
  `ioreg` give each its own name and hardware id ([bin](bin)), and [bin/login-shell](bin/login-shell) keeps
  each machine's PATH as [stage.sh](stage.sh) set it, so the real `sysctl` stays out of the way.
- **The repository.** [shop/](shop) is a small TypeScript library made up for the demo. Its vitest suite has
  42 tests and takes about 12 seconds in one go, since its [config](shop/vitest.config.ts) runs files one at
  a time.
- **The agents.** [bin/claude](bin/claude) is a script, started by herdr-pool as it would start Claude Code. It
  reports its status with OSC 7501 as Claude Code does, so hc and `herdr-pool wait` follow it; draws a screen
  modelled on Claude Code's; puts its change from [changes/](changes) in its work tree; runs the real tests
  on it; and ends with a summary. `DEMO_PACE` stretches its pauses (`record.sh` uses 3) so `wait` has
  something to wait for. It never calls a model.
- **The person.** Git commits as Sam Rivera, sam@example.com.

hc, herdr-pool, git, npm and Claude Code's config all live in one folder under `/tmp`: nothing of yours is
read or written, and your own hc sessions don't show.

## Record it again

```sh
brew install vhs          # it also needs ffmpeg; and node, and python3 with Pillow
cargo build --release
sh docs/demo/record.sh
```

[record.sh](record.sh) stages the pool with [stage.sh](stage.sh), with warm work trees on every machine as a
pool in use has. It records [demo.tape](demo.tape) with [VHS](https://github.com/charmbracelet/vhs) into
`target/demo`, encodes `docs/media/demo.mp4` (H.264) and `demo.gif` (12 fps, 960 px wide), crops the tape's
screenshots into `docs/media` with [stills.py](stills.py), and stops the pool. The tape's first lines end
its shell if the demo's environment isn't in place, so a take can never reach your own machines.

## Look around

In a shell you can close afterwards (`env.sh` changes its HOME and PATH):

```sh
w=$(sh docs/demo/stage.sh) && . "$w/env.sh" && cd "$HOME/code/shop"
herdr-pool status
herdr-pool run claude "Add retries to the webhook client"   # or "...prices...": the stories in bin/claude
herdr-pool ls                                               # its ADDRESS is what hc attach takes
sh "$DEMO_KIT/stage.sh" --down "$w"                          # stops it all and deletes the folder
```
