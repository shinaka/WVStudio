"""Pixel-art pipeline for the Thermalright Wonder Vision 360 (6.67" curved AMOLED, 20:9).

The panel takes 1600x720 frames and shows them 1.5x enlarged on its 2400x1080 glass. Art lays out on a
canvas that is an exact integer fraction of 1600x720, then upscales nearest-neighbour:

  python wv.py studio [image]            fit existing art by eye in the browser, and build the playlist
  python wv.py sizes                     canvas sizes that scale perfectly to the panel
  python wv.py new forest --scale 4      400x180 canvas with safe-area guides, opens in Aseprite
  python wv.py export forest             art/forest.aseprite -> out/forest.png (animated PNG if animated)
  python wv.py port old.gif              re-frame square art (e.g. from a Corsair LCD) onto 20:9

player/ (Rust, `wvplay`) shows the results on the panel, replacing TRCC.
"""
import argparse
import json
import math
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

import numpy as np
from PIL import Image, ImageColor, ImageSequence

# What the panel takes. It also accepts 2400x1080, its glass's own size, but then its outer columns flicker,
# so this is what TRCC and InfoPanel send too. The panel enlarges it 1.5x onto the glass.
SCREEN_W, SCREEN_H = 1600, 720
GLASS_MM = 154.5  # approx. width of the 6.67" 20:9 glass, for physical pixel sizes
CORSAIR_MM_PER_PX = 53.3 / 480  # 2.1" 480x480 iCUE LCD, for comparing with old art

# Panel pixels bent by the curved edges or hidden by rounded corners. Measured from a photo of a test chart
# on the cooler (2026-10-13). Top and bottom are flat. The left edge curls away within its first ~68 px, and
# the right ~360 px wrap around the side of the pump block, so the flat front is x 68..1240. All four corners
# sit on those bends with nothing visibly cut off, so no corner mask.
SAFE = {"left": 68, "right": 360, "top": 0, "bottom": 0, "corner_radius": 0}

ROOT = Path(__file__).resolve().parent
ART, OUT = ROOT / "art", ROOT / "out"
CANVAS_LUA = ROOT / "aseprite" / "wv_canvas.lua"
ASEPRITE = Path(os.environ.get("ASEPRITE", r"C:\Program Files\Aseprite\Aseprite.exe"))


def exact_scales():
    return [s for s in range(2, 41) if SCREEN_W % s == 0 and SCREEN_H % s == 0]


def glass_note(s):
    """How an art pixel at scale s lands on the glass, after the panel's own 1.5x enlargement."""
    if s % 2:
        return "art pixels come out slightly uneven on the glass"
    return f"each art pixel is {s * 3 // 2}x{s * 3 // 2} glass pixels"


def cmd_sizes(args):
    print("scale  canvas     art pixel")
    for s in exact_scales():
        print(f"  @{s:<3} {SCREEN_W // s:>4}x{SCREEN_H // s:<4}  {GLASS_MM / SCREEN_W * s:.2f} mm   {glass_note(s)}")
    corsair = ", ".join(f"{480 // k}px art = {CORSAIR_MM_PER_PX * k:.2f} mm" for k in (2, 3, 4))
    print(f"\nFor comparison, Corsair 480x480 LCD: {corsair}")


# --- Aseprite / ffmpeg -------------------------------------------------------

def aseprite(*args):
    if not ASEPRITE.exists():
        sys.exit(f"Aseprite not found at {ASEPRITE} (set the ASEPRITE env var)")
    subprocess.run([str(ASEPRITE), "-b", *map(str, args)], check=True)


def find_ffmpeg():
    """ffmpeg under another name: something on this PC (TRCC, it seems) kills any ffmpeg.exe it sees."""
    # winget installs aren't on PATH until a new shell is opened
    winget = Path(os.environ.get("LOCALAPPDATA", "")) / "Microsoft" / "WinGet"
    for exe in [shutil.which("ffmpeg"), winget / "Links" / "ffmpeg.exe",
                *winget.glob("Packages/*FFmpeg*/*/bin/ffmpeg.exe")]:
        if exe and Path(exe).exists():
            real, alias = Path(exe).resolve(), ROOT / ".cache" / "wv-ffmpeg.exe"
            if not (alias.exists() and alias.samefile(real)):
                alias.parent.mkdir(exist_ok=True)
                alias.unlink(missing_ok=True)
                try:
                    os.link(real, alias)
                except OSError:  # different drive
                    shutil.copy2(real, alias)
            return str(alias)
    return None


# --- Canvases ----------------------------------------------------------------

def guide_overlay(scale):
    """Translucent overlay at canvas resolution: red = off the glass, amber = on the curved edge."""
    w, h = SCREEN_W // scale, SCREEN_H // scale
    x, y = np.meshgrid((np.arange(w) + 0.5) * scale, (np.arange(h) + 0.5) * scale)  # art-pixel centres
    r = SAFE["corner_radius"]
    off_glass = (x - x.clip(r, SCREEN_W - r)) ** 2 + (y - y.clip(r, SCREEN_H - r)) ** 2 > r * r
    curved = ((x < SAFE["left"]) | (x > SCREEN_W - SAFE["right"])
              | (y < SAFE["top"]) | (y > SCREEN_H - SAFE["bottom"]))
    rgba = np.zeros((h, w, 4), np.uint8)
    rgba[curved] = (255, 160, 0, 70)
    rgba[off_glass] = (255, 0, 64, 170)
    return Image.fromarray(rgba)


def make_canvas(out, scale, start_from=None, open_after=True):
    out.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory() as tmp:
        guides = Path(tmp, "guides.png")
        guide_overlay(scale).save(guides)
        params = ["--script-param", f"out={out}", "--script-param", f"guides={guides}"]
        if start_from:
            params += ["--script-param", f"from={start_from}"]
        aseprite(*params, "--script", CANVAS_LUA)
    if not out.exists():
        sys.exit(f"Aseprite didn't write {out}")
    print(f"created {out.relative_to(ROOT)}  ({SCREEN_W // scale}x{SCREEN_H // scale}, exports @{scale})")
    if open_after:
        subprocess.Popen([str(ASEPRITE), str(out)])


def cmd_new(args):
    if SCREEN_W % args.scale or SCREEN_H % args.scale:
        sys.exit(f"--scale must divide {SCREEN_W}x{SCREEN_H}: one of {exact_scales()}")
    out = ART / f"{args.name}.aseprite"
    if out.exists():
        sys.exit(f"{out.relative_to(ROOT)} already exists")
    make_canvas(out, args.scale, open_after=not args.no_open)


# --- Loading and fitting -----------------------------------------------------

def resolve(name):
    path = Path(name)
    if path.exists():
        return path
    for ext in (".aseprite", ".ase", ".png", ".gif"):
        if (candidate := ART / f"{name}{ext}").exists():
            return candidate
    sys.exit(f"can't find {name} (or art/{name}.aseprite)")


def load_frames(path):
    """[(RGBA image, duration ms)] at art resolution."""
    if path.suffix.lower() in (".aseprite", ".ase"):
        with tempfile.TemporaryDirectory() as tmp:
            sheet, data = Path(tmp, "sheet.png"), Path(tmp, "sheet.json")
            # --ignore-layer only applies to files named after it
            aseprite("--ignore-layer", "guides", path, "--sheet", sheet, "--sheet-type", "rows",
                     "--data", data, "--format", "json-array")
            meta = json.loads(data.read_text(encoding="utf-8"))
            with Image.open(sheet) as im:
                rgba = im.convert("RGBA")
        return [(rgba.crop((r["x"], r["y"], r["x"] + r["w"], r["y"] + r["h"])), f["duration"])
                for f in meta["frames"] for r in [f["frame"]]]
    with Image.open(path) as im:
        return [(f.convert("RGBA"), f.info.get("duration") or 100) for f in ImageSequence.Iterator(im)]


def flatten(frames, bg):
    """Composite RGBA frames onto the background colour -> RGB."""
    return [Image.alpha_composite(Image.new("RGBA", im.size, bg + (255,)), im).convert("RGB")
            for im, _ in frames]


def fit(size, scale=None):
    """Integer scale and screen offset for art of this size."""
    w, h = size
    s = scale or min(SCREEN_W // w, SCREEN_H // h)
    if s < 1 or w * s > SCREEN_W or h * s > SCREEN_H:
        sys.exit(f"{w}x{h} @{s} doesn't fit on {SCREEN_W}x{SCREEN_H}")
    if (w * s, h * s) != (SCREEN_W, SCREEN_H):
        print(f"note: {w}x{h} @{s} = {w * s}x{h * s}, letterboxed to {SCREEN_W}x{SCREEN_H}. "
              "Use a canvas from `wv.py sizes` to fill the screen.")
    # even offsets keep art-pixel edges on the 2x2 chroma grid of video codecs
    return s, ((SCREEN_W - w * s) // 4 * 2, (SCREEN_H - h * s) // 4 * 2)


def to_screen(im, s, offset, bg):
    screen = Image.new(im.mode, (SCREEN_W, SCREEN_H), bg)
    screen.paste(im.resize((im.width * s, im.height * s), Image.NEAREST), offset)
    return screen


# --- Writers -----------------------------------------------------------------

def placer(s, offset):
    """A `place` for write_gif: nearest-neighbour upscale by s, pasted at offset."""
    return lambda im, fill: to_screen(im, s, offset, fill)


def paletted(frames, bg, place):
    """Full-screen frames sharing one exact palette taken from the art, or None if it has over 256 colours.

    `place(image, fill)` puts one frame on a full-screen image, `fill` being the background. A palette is
    exact (nothing approximated or dithered) and a third the memory of RGB, which adds up over long animations.
    """
    to_key = lambda f: np.asarray(f, np.uint32) @ np.array([1 << 16, 1 << 8, 1], np.uint32)
    bg_key = bg[0] << 16 | bg[1] << 8 | bg[2]
    keys = np.array([bg_key], np.uint32)
    for f in frames:  # frame by frame, stopping early: a video has thousands of colours in its first frame
        keys = np.union1d(keys, np.unique(to_key(f)))
        if len(keys) > 256:
            return None
    flat_palette = [int(k) >> shift & 255 for k in keys for shift in (16, 8, 0)]
    screens = []
    for f in frames:
        screen = place(Image.fromarray(np.searchsorted(keys, to_key(f)).astype(np.uint8)),
                       int(np.searchsorted(keys, bg_key)))
        screen.putpalette(flat_palette)
        screens.append(screen)
    return screens


def write_gif(frames, durations, bg, path, place):
    screens = paletted(frames, bg, place)
    if screens is None:
        print("warning: over 256 colours, so the GIF will be approximated (PNG keeps them all)")
        screens = [place(f, bg) for f in frames]
    screens[0].save(path, save_all=True, append_images=screens[1:], duration=durations, loop=0)


def write_png(frames, durations, bg, path, place):
    """A PNG, or an animated PNG for more than one frame: lossless whatever the colour count."""
    screens = paletted(frames, bg, place) or [place(f, bg) for f in frames]
    if len(screens) == 1:
        screens[0].save(path, optimize=True)
    else:
        screens[0].save(path, save_all=True, append_images=screens[1:], duration=durations, loop=0)


# x264 settings for pixel art: near-lossless, and tuned for flat colour and hard edges
PIXEL_X264 = ("-preset", "slow", "-crf", "12", "-tune", "animation")
VIDEO_X264 = ("-preset", "medium", "-crf", "16")


def write_mp4(frames, durations, s, offset, bg, path, fps=60, min_seconds=10, yuv444=False, x264=PIXEL_X264):
    ff = find_ffmpeg()
    if not ff:
        print(f"skipped {path.name}: ffmpeg not found (winget install BtbN.FFmpeg.GPL.8.1)")
        return False
    # Short loops are repeated so the player's loop seam comes around less often.
    total = sum(durations)
    loops = max(1, math.ceil(min_seconds * 1000 / total))
    # Frames go in at the output rate, each one the art frame that's showing at that moment. ffmpeg
    # does the nearest-neighbour upscale.
    ends = np.cumsum(durations) * fps
    order = np.searchsorted(ends, np.arange(round(loops * total * fps / 1000)) * 1000 % (total * fps), side="right")
    (w, h), (ox, oy) = frames[0].size, offset
    vf = (f"scale=iw*{s}:ih*{s}:flags=neighbor,pad={SCREEN_W}:{SCREEN_H}:{ox}:{oy}:0x{bytes(bg).hex()},"
          # bt709 matrix + tags so palette colours come out exactly as drawn; 'area' averages chroma
          # within each 2x2 block instead of smearing it across art-pixel edges like the default bicubic
          f"scale=out_color_matrix=bt709:out_range=tv:flags=area,format={'yuv444p' if yuv444 else 'yuv420p'}")
    proc = subprocess.Popen([
        ff, "-y", "-v", "error", "-f", "rawvideo", "-pix_fmt", "rgb24", "-s", f"{w}x{h}",
        "-framerate", str(fps), "-i", "-", "-vf", vf, "-c:v", "libx264", *x264,
        "-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709",
        "-color_range", "tv", "-movflags", "+faststart", "-an", path,
    ], stdin=subprocess.PIPE)
    try:
        shown = None
        for i in order:
            if i != shown:  # frames may be made on demand, so only fetch each one when it changes
                raw, shown = frames[i].tobytes(), i
            proc.stdin.write(raw)
        proc.stdin.close()
    except BrokenPipeError:
        pass  # ffmpeg died; its exit code says so below
    if proc.wait():
        sys.exit(f"ffmpeg failed (exit {proc.returncode}) writing {path.name}")
    return True


def cmd_export(args):
    src = resolve(args.art)
    frames = load_frames(src)
    durations = [d for _, d in frames]
    bg = ImageColor.getrgb(args.bg)[:3]
    flat = flatten(frames, bg)
    s, offset = fit(flat[0].size, args.scale)
    formats = args.formats.split(",") if args.formats else ["png"]
    if s % 2 and "mp4" in formats and not args.yuv444:
        print(f"warning: @{s} is odd, so MP4 colours will bleed across art-pixel edges. Prefer an even scale.")
    OUT.mkdir(exist_ok=True)
    for fmt in formats:
        path = OUT / f"{src.stem}.{fmt}"
        if fmt == "png":
            write_png(flat, durations, bg, path, placer(s, offset))
        elif fmt == "jpg":
            to_screen(flat[0], s, offset, bg).save(path, quality=100, subsampling=0)
        elif fmt == "gif":
            write_gif(flat, durations, bg, path, placer(s, offset))
        elif fmt == "mp4":
            if not write_mp4(flat, durations, s, offset, bg, path, args.fps, args.min_seconds, args.yuv444):
                continue
        else:
            sys.exit(f"unknown format {fmt} (png, jpg, gif, mp4)")
        print(f"wrote {path.relative_to(ROOT)}  ({len(flat)} frame{'s' * (len(flat) > 1)} @{s})")


# --- Porting old art ---------------------------------------------------------

def native_factor(frames):
    """Largest k such that every frame is a clean k-times nearest-neighbour upscale."""
    g = 0
    for im, _ in frames:
        a = np.asarray(im)
        col_edges = np.flatnonzero((a[:, 1:] != a[:, :-1]).any(axis=(0, 2))) + 1
        row_edges = np.flatnonzero((a[1:] != a[:-1]).any(axis=(1, 2))) + 1
        g = math.gcd(g, im.width, im.height, *col_edges.tolist(), *row_edges.tolist())
    return g


def cmd_port(args):
    src = Path(args.image)
    frames = load_frames(src)
    k = args.factor or native_factor(frames)
    w, h = frames[0][0].width // k, frames[0][0].height // k
    print(f"{src.name}: {frames[0][0].width}x{frames[0][0].height} = {w}x{h} art drawn @{k}")
    if k == 1 and not args.factor:
        print("  (no clean upscale found - if it's blurry/JPEG'd, pass --factor N)")
    fits = [s for s in exact_scales() if w <= SCREEN_W // s and h <= SCREEN_H // s]
    scale = args.scale or max(fits, default=None)
    if not scale or scale not in fits:
        sys.exit(f"{w}x{h} doesn't fit a canvas at @{scale}; ones that fit: {fits}")
    cw, ch = SCREEN_W // scale, SCREEN_H // scale
    bg = ImageColor.getrgb(args.bg)[:3] + (255,)
    ported = []
    for im, d in frames:
        canvas = Image.new("RGBA", (cw, ch), bg)
        canvas.alpha_composite(im.resize((w, h), Image.NEAREST), ((cw - w) // 2, (ch - h) // 2))
        ported.append(canvas)
    out = ART / f"{args.name or src.stem}.aseprite"
    if out.exists():
        sys.exit(f"{out.relative_to(ROOT)} already exists")
    with tempfile.TemporaryDirectory() as tmp:
        start = Path(tmp, "ported.gif" if len(ported) > 1 else "ported.png")
        ported[0].save(start, save_all=True, append_images=ported[1:],
                       duration=[d for _, d in frames], loop=0)
        make_canvas(out, scale, start_from=start, open_after=not args.no_open)


def cmd_studio(args):
    import studio  # the browser app lives in its own module
    studio.serve(args.image, args.port)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(required=True)

    sub.add_parser("sizes", help="list canvas sizes that scale exactly").set_defaults(func=cmd_sizes)

    p = sub.add_parser("new", help="new Aseprite canvas with safe-area guides")
    p.add_argument("name")
    p.add_argument("--scale", type=int, default=4, help="upscale factor (default 4 = 400x180 canvas)")
    p.add_argument("--no-open", action="store_true")
    p.set_defaults(func=cmd_new)

    p = sub.add_parser("export", help="upscale art to the panel's 1600x720 as PNG/GIF/MP4 in out/")
    p.add_argument("art", help="name in art/ or a path (.aseprite, .png, .gif)")
    p.add_argument("--formats", help="comma list of png,jpg,gif,mp4 (default png, animated if the art is)")
    p.add_argument("--scale", type=int, help="force a scale (default: largest that fits)")
    p.add_argument("--bg", default="#000000", help="colour behind transparency / letterbox (default black)")
    p.add_argument("--fps", type=int, default=60, help="MP4 frame rate (try 30 if the cooler stutters)")
    p.add_argument("--min-seconds", type=float, default=10, help="repeat short loops to at least this long")
    p.add_argument("--yuv444", action="store_true", help="full-colour MP4, if your cooler can play it")
    p.set_defaults(func=cmd_export)

    p = sub.add_parser("port", help="put old square art on a 20:9 canvas")
    p.add_argument("image", help="old art: PNG or GIF, upscaled or native")
    p.add_argument("--name", help="output name in art/ (default: image file name)")
    p.add_argument("--scale", type=int, help="target scale (default: largest canvas it fits)")
    p.add_argument("--factor", type=int, help="the old art's upscale factor, if auto-detect fails")
    p.add_argument("--bg", default="#000000", help="fill for the new space (default black)")
    p.add_argument("--no-open", action="store_true")
    p.set_defaults(func=cmd_port)

    p = sub.add_parser("studio", help="fit existing art by eye in the browser, then export")
    p.add_argument("image", nargs="?", help="art to open straight away (PNG, GIF, WebP, .aseprite, ...)")
    p.add_argument("--port", type=int, help="default 8137, or the port in studio.json")
    p.set_defaults(func=cmd_studio)

    args = ap.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
