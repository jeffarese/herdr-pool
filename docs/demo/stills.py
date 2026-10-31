"""Crops the recording's shots for the README: each keeps its window bar and the
lines with something on them, plus the padding below, with the window's round
corners at the bottom again.

    python3 stills.py <shots folder> <media folder>
"""
import os
import sys

from PIL import Image, ImageChops, ImageDraw

PAD = 22      # the tape's Padding
BAR = 36      # its WindowBarSize
RADIUS = 10   # its BorderRadius


def crop(path, dest):
    img = Image.open(path).convert("RGBA")
    w, h = img.size
    # The terminal's own background, from a corner of the padding under the bar.
    bg = img.getpixel((PAD // 2, BAR + PAD // 2))
    body = img.crop((0, BAR, w, h)).convert("RGB")
    plain = Image.new("RGB", body.size, bg[:3])
    box = ImageChops.difference(body, plain).point(lambda v: 255 if v > 24 else 0).getbbox()
    bottom = BAR + (box[3] if box else PAD) + PAD
    out = img.crop((0, 0, w, min(h, bottom)))
    mask = Image.new("L", out.size, 0)
    ImageDraw.Draw(mask).rounded_rectangle((0, 0, out.size[0] - 1, out.size[1] - 1), RADIUS, fill=255)
    out.putalpha(ImageChops.multiply(out.getchannel("A"), mask))
    out.save(dest, optimize=True)
    print(f"{os.path.basename(dest)}: {out.size[0]}x{out.size[1]}")


def main():
    shots, media = sys.argv[1], sys.argv[2]
    for name in sorted(os.listdir(shots)):
        if name.endswith(".png"):
            crop(os.path.join(shots, name), os.path.join(media, name))


if __name__ == "__main__":
    main()
