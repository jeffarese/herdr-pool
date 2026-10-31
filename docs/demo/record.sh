#!/bin/sh
# Records the README's video and stills against a synthetic pool (stage.sh),
# then stops the pool. Needs vhs (brew install vhs), ffmpeg, node, python3
# with Pillow, and a release build:
#
#   cargo build --release && sh docs/demo/record.sh
#
# The raw take and shots land in target/demo; the encoded video, the GIF and
# the cropped stills in docs/media.
set -eu
kit=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$kit/../.." && pwd)
media="$root/docs/media"
out="$root/target/demo"
for t in vhs ffmpeg node python3; do
  command -v "$t" >/dev/null || { echo "needs $t" >&2; exit 2; }
done

rm -rf "$out" && mkdir -p "$out/shots" "$media"
# The agents take about half a minute, so `wait` has something to wait for.
w=$(DEMO_PACE=${DEMO_PACE:-3} sh "$kit/stage.sh")
trap 'sh "$kit/stage.sh" --down "$w"' EXIT INT TERM
echo "pool staged in $w; recording" >&2

(cd "$out" && DEMO_ENV="$w/env.sh" vhs "$kit/demo.tape")

ffmpeg -y -loglevel error -i "$out/raw.mp4" -c:v libx264 -preset slow -crf 24 -pix_fmt yuv420p \
  -movflags +faststart -an "$media/demo.mp4"
ffmpeg -y -loglevel error -i "$out/raw.mp4" -vf "fps=12,scale=960:-1:flags=lanczos,split[a][b];\
[a]palettegen=max_colors=96:stats_mode=diff[p];[b][p]paletteuse=dither=none:diff_mode=rectangle" \
  "$media/demo.gif"
python3 -I "$kit/stills.py" "$out/shots" "$media"
ls -la "$media" >&2
