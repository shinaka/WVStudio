"""Studio: lay out existing art on the Wonder Vision screen by eye, build the playlist, and run the player.

  python wv.py studio [image]

Serves studio/index.html to a browser on this PC only. The page does the previewing and layout. Exports
and playlist pieces rebuild the same layout here with numpy and go through wv.py's writers, so the files
match the preview.
"""
import csv
import errno
import hashlib
import io
import json
import os
import re
import shutil
import subprocess
import threading
import time
import urllib.error
import urllib.request
import webbrowser
import winreg
from http.client import HTTPException
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, unquote, urlparse

import numpy as np
from PIL import Image, ImageColor, ImageFont

import wv

PAGE = wv.ROOT / "studio" / "index.html"
UPLOADS = wv.ROOT / ".cache" / "studio"
FORMATS = ("png", "gif", "mp4")
FILL_MODES = ("solid", "extend", "mirror", "tile")
# Clockwise quarter turns, as the page applies them (PIL's ROTATE_* turn anticlockwise).
TURNS = {0: None, 90: Image.Transpose.ROTATE_270, 180: Image.Transpose.ROTATE_180, 270: Image.Transpose.ROTATE_90}
KEY = np.array([1 << 16, 1 << 8, 1], np.uint32)
VIDEO_TYPES = {".mp4", ".m4v", ".mov", ".webm", ".mkv", ".avi"}
VIDEO_FPS = 30  # the most frames a second a video is kept at: about what the panel shows comfortably
VIDEO_SECONDS = 30  # the longest stretch of a video that's kept
NO_WINDOW = subprocess.CREATE_NO_WINDOW

sources = {}  # id -> Source, the few most recently opened
initial = None  # info for the image named on the command line


class JpegFrames:
    """A video's frames, kept as JPEG (a few hundred KB each rather than MBs) and decoded when used."""

    def __init__(self, jpegs, duration):
        self.jpegs, self.duration, self.last = jpegs, duration, (None, None)

    def __len__(self):
        return len(self.jpegs)

    def __getitem__(self, i):
        if self.last[0] != i:
            with Image.open(io.BytesIO(self.jpegs[i])) as im:  # IndexError past the end ends iteration
                self.last = (i, im.convert("RGBA"))
        return self.last[1], self.duration


def decode_video(path, preview):
    """Frames of a video, at most VIDEO_FPS and VIDEO_SECONDS, plus a small copy for the browser to preview.

    Frames come out just big enough to fill the screen when cropped, never enlarged: the layout scales from
    there. Returns (JpegFrames, details).
    """
    ff = wv.find_ffmpeg()
    if not ff:
        raise ValueError("opening videos needs ffmpeg (winget install BtbN.FFmpeg.GPL.8.1)")
    probe = subprocess.run([ff, "-hide_banner", "-i", str(path)], capture_output=True, text=True, errors="replace",
                           creationflags=NO_WINDOW).stderr
    if not re.search(r"Stream #.*Video:", probe):
        raise ValueError("there's no video in it")
    rate = re.search(r"Stream #.*Video:.*?(\d+(?:\.\d+)?) fps", probe)
    length = re.search(r"Duration: (\d+):(\d+):(\d+(?:\.\d+)?)", probe)
    fps = min(VIDEO_FPS, float(rate[1])) if rate else VIDEO_FPS
    seconds = int(length[1]) * 3600 + int(length[2]) * 60 + float(length[3]) if length else 0
    k = f"min(1\\,max({wv.SCREEN_W}/iw\\,{wv.SCREEN_H}/ih))"
    graph = (f"[0:v]fps={fps:g},scale=w=trunc(iw*{k}/2)*2:h=trunc(ih*{k}/2)*2:flags=lanczos,format=yuv420p,"
             "split[frames][preview]")
    proc = subprocess.Popen([
        ff, "-v", "error", "-y", "-t", str(VIDEO_SECONDS), "-i", str(path), "-filter_complex", graph,
        "-map", "[preview]", "-c:v", "libx264", "-preset", "veryfast", "-crf", "20", "-movflags", "+faststart",
        "-an", str(preview),
        "-map", "[frames]", "-c:v", "mjpeg", "-q:v", "3", "-f", "image2pipe", "-",
    ], stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, creationflags=NO_WINDOW)
    # split the stream of JPEGs as it arrives (each ends FF D9), rather than holding it all twice
    jpegs, buf = [], bytearray()
    while chunk := proc.stdout.read(1 << 20):
        buf += chunk
        start = 0
        while (end := buf.find(b"\xff\xd9", start)) != -1:
            jpegs.append(bytes(buf[start:end + 2]))
            start = end + 2
        del buf[:start]
    if proc.wait() or not jpegs:
        raise ValueError(f"ffmpeg couldn't read it: {proc.stderr.read().decode(errors='replace').strip()[-300:]}")
    return JpegFrames(jpegs, 1000 / fps), {"fps": fps, "trimmed": seconds > VIDEO_SECONDS + 0.5}


class Source:
    def __init__(self, path, name):
        self.name, self.path = name, path
        self.id = hashlib.sha1(path.read_bytes()).hexdigest()[:12]
        self.video = None
        if path.suffix.lower() in VIDEO_TYPES:
            UPLOADS.mkdir(parents=True, exist_ok=True)
            self.preview = UPLOADS / f"{self.id}.preview.mp4"
            self.frames, self.video = decode_video(path, self.preview)
        else:
            self.frames = wv.load_frames(path)
        self.pngs = {}
        self.info = self.describe()

    def describe(self):
        first = np.asarray(self.frames[0][0])
        # The commonest opaque colour around the edge is usually the art's background: a good default fill.
        border = np.concatenate([first[0], first[-1], first[:, 0], first[:, -1]])
        border = border[border[:, 3] == 255, :3]
        edge = 0
        if len(border):
            keys, counts = np.unique(border.astype(np.uint32) @ KEY, return_counts=True)
            edge = int(keys[counts.argmax()])
        colours = np.zeros(0, np.uint32)
        for im, _ in self.frames:
            a = np.asarray(im).reshape(-1, 4)
            colours = np.union1d(colours, a[a[:, 3] > 0, :3].astype(np.uint32) @ KEY)
            if len(colours) > 256:
                break  # all the page needs to know is whether a GIF can keep every colour
        info = {"id": self.id, "name": self.name, "width": first.shape[1], "height": first.shape[0],
                "durations": [self.frames[0][1]] * len(self.frames) if self.video else [d for _, d in self.frames],
                # a compressed video never has clean enlarged pixels to find
                "factor": 1 if self.video else wv.native_factor(self.frames),
                "colors": len(colours), "edge": f"#{edge:06x}"}
        if self.video:
            info["video"] = {"url": f"/api/video/{self.id}.mp4", **self.video}
        return info

    def png(self, i):
        if i not in self.pngs:
            buf = io.BytesIO()
            self.frames[i][0].save(buf, "PNG", compress_level=1)
            self.pngs[i] = buf.getvalue()
        return self.pngs[i]


def open_source(path, name):
    src = Source(path, name)
    while len(sources) >= 3:  # each holds every frame in memory (videos as JPEG)
        sources.pop(next(iter(sources)))
    sources[src.id] = src
    return src.info


# --- Layout, matching the page's drawing ----------------------------------------

def axis_map(screen, at, size, src, mode):
    """For each screen pixel along one axis: the source pixel shown there, and whether the art covers it.

    The art is `size` px long on screen, starting at `at`, drawn from `src` source pixels.
    """
    u = np.arange(screen) - at
    if mode == "extend":
        u = u.clip(0, size - 1)
    elif mode == "tile":
        u = u % size
    elif mode == "mirror":
        u = u % (2 * size)
        u = np.where(u < size, u, 2 * size - 1 - u)
    covered = (u >= 0) & (u < size)
    # nearest neighbour, sampling at pixel centres like the browser does
    return (2 * u.clip(0, size - 1) + 1) * src // (2 * size), covered


def layout_placer(x, y, width, height, src_size, mode):
    """A `place` for wv.write_gif that lays a frame out the way the page draws it."""
    ys, y_in = axis_map(wv.SCREEN_H, y, height, src_size[1], mode)
    xs, x_in = axis_map(wv.SCREEN_W, x, width, src_size[0], mode)
    uncovered = ~(y_in[:, None] & x_in[None, :])

    def place(im, fill):
        out = np.asarray(im)[ys[:, None], xs[None, :]]
        out[uncovered] = fill
        return Image.fromarray(out)
    return place


class Placed:
    """Full-screen frames made on demand: at ~8 MB each, a whole animation is too much to hold."""

    def __init__(self, frames, place, fill):
        self.frames, self.place, self.fill = frames, place, fill

    def __len__(self):
        return len(self.frames)

    def __getitem__(self, i):
        return self.place(self.frames[i], self.fill)


class Prepared:
    """The art's frames turned, flipped, flattened and (when smooth) resized, one at a time as they're used:
    a video's worth at once wouldn't fit in memory."""

    def __init__(self, frames, turn, flip, bg, size):
        self.frames, self.turn, self.flip, self.bg, self.size = frames, turn, flip, bg, size
        self.last = (None, None)

    def __len__(self):
        return len(self.frames)

    def __getitem__(self, i):
        if self.last[0] != i:
            im = self.frames[i][0]  # IndexError past the end ends iteration
            if self.turn:
                im = im.transpose(self.turn)
            if self.flip:
                im = im.transpose(Image.Transpose.FLIP_LEFT_RIGHT)
            im = wv.flatten([(im, 0)], self.bg)[0]
            self.last = (i, im.resize(self.size, Image.LANCZOS) if self.size else im)
        return self.last[1]


def prepare(src, p):
    """The page's layout `p` applied to the art: (frames to place, durations, background, place)."""
    width, height, x, y = (int(p[k]) for k in ("width", "height", "x", "y"))
    if not (0 < width <= 4 * wv.SCREEN_W and 0 < height <= 4 * wv.SCREEN_H):
        raise ValueError(f"art size {width}x{height} out of range")
    mode, turn, bg = p["fill"], TURNS[int(p["rotate"])], ImageColor.getrgb(p["bg"])[:3]
    if mode not in FILL_MODES:
        raise ValueError(f"unknown fill {mode}")
    flat = Prepared(src.frames, turn, p["flip"], bg, (width, height) if p["smooth"] else None)
    durations = [src.frames[0][1]] * len(src.frames) if src.video else [d for _, d in src.frames]
    return flat, durations, bg, layout_placer(x, y, width, height, flat[0].size, mode)


def write_video_mp4(src, flat, durations, bg, path, place, yuv444=False):
    """An MP4 of a video source at its own frame rate, played once through (the player loops it)."""
    return wv.write_mp4(Placed(flat, place, bg), durations, 1, (0, 0), bg, path, fps=round(src.video["fps"]),
                        min_seconds=0, yuv444=yuv444, x264=wv.VIDEO_X264)


def export(req):
    src = sources.get(req["id"])
    if not src:
        raise ValueError("that image is no longer loaded; open it again")
    flat, durations, bg, place = prepare(src, req["place"])
    name = safe_name(req["name"])
    formats = [f for f in req["formats"] if f in FORMATS]
    if src.video and set(formats) - {"mp4"}:
        raise ValueError("a video only exports as MP4: as PNG or GIF it would be enormous")

    wv.OUT.mkdir(exist_ok=True)
    written, notes = [], []
    for fmt in formats:
        path = wv.OUT / f"{name}.{fmt}"
        if fmt == "png":
            wv.write_png(flat, durations, bg, path, place)
        elif fmt == "gif":
            wv.write_gif(flat, durations, bg, path, place)
        elif src.video:
            write_video_mp4(src, flat, durations, bg, path, place, bool(req.get("yuv444")))
        elif not wv.write_mp4(Placed(flat, place, bg), durations, 1, (0, 0), bg, path, yuv444=bool(req.get("yuv444"))):
            notes.append("MP4 skipped: ffmpeg not found (winget install BtbN.FFmpeg.GPL.8.1)")
            continue
        written.append({"name": path.name, "bytes": path.stat().st_size})
    return {"files": written, "notes": notes, "folder": str(wv.OUT)}


def safe_name(name):
    return re.sub(r"[^\w .()-]", "_", name).strip(" .") or "untitled"


# --- Playlist and player -------------------------------------------------------------
#
# The playlist is playlist/playlist.json plus one 1600x720 PNG (animated PNG for animations) per piece, which
# is what player/ (wvplay) plays, drawing text over it (docs/widgets.md). wvplay re-reads the playlist whenever
# it changes, so editing here is live.

PLAYLIST_DIR = wv.ROOT / "playlist"
PLAYLIST = PLAYLIST_DIR / "playlist.json"
PLAYER = wv.ROOT / "player" / "target" / "release" / "wvplayw.exe"  # the one without a console window
PLAYER_LOG = wv.ROOT / ".cache" / "wvplay.log"
RUN_KEY = r"Software\Microsoft\Windows\CurrentVersion\Run"  # per-user startup apps
RUN_NAME = "Wonder Vision player"
RIVALS = re.compile(r"TRCC|InfoPanel|USBLCD", re.I)  # other programs that drive the cooler
rivals_seen = (0.0, [])
DEFAULT_SECONDS = 30
thumbs = {}  # (file, mtime) -> PNG bytes
# Requests run on their own threads, and text edits save as they happen: one change to playlist.json at a time.
playlist_lock = threading.RLock()


def read_json():
    """playlist.json as it is, or {} before there is one."""
    with playlist_lock:
        try:
            return json.loads(PLAYLIST.read_text(encoding="utf-8"))
        except FileNotFoundError:
            return {}


def read_playlist():
    """The playlist's items: file and seconds (what the player reads), each piece's own text (`widgets`), and
    for re-editing the original's name, where it's kept (`source`) and the page's `layout`. Plus the global
    layer of text over every piece (`overlay`). See docs/widgets.md."""
    data = read_json()
    items = [{**i, "seconds": i.get("seconds", DEFAULT_SECONDS)}
             for i in data.get("items", []) if (PLAYLIST_DIR / i["file"]).is_file()]
    return {"seconds": data.get("seconds", DEFAULT_SECONDS), "overlay": data.get("overlay", []), "items": items}


def keep_source(src):
    """Keeps a piece's original next to the playlist (a hard link where possible: no extra space)."""
    sources_dir = PLAYLIST_DIR / "sources"  # each piece's original, so it can be laid out again
    sources_dir.mkdir(parents=True, exist_ok=True)
    kept = sources_dir / f"{src.id}{src.path.suffix.lower()}"
    if not kept.exists():
        try:
            os.link(src.path, kept)
        except OSError:  # another drive
            shutil.copy2(src.path, kept)
    return kept.relative_to(PLAYLIST_DIR).as_posix()


def tidy_sources(items):
    """Deletes kept originals that no piece uses any more."""
    used = {i.get("source") for i in items}
    if (sources_dir := PLAYLIST_DIR / "sources").exists():
        for path in sources_dir.iterdir():
            if f"sources/{path.name}" not in used:
                path.unlink(missing_ok=True)


def write_playlist(items=None, **keys):
    """Writes playlist.json with these items (else the ones there) and top-level keys (`overlay`), keeping every
    other key as it is: ones this version doesn't know about too."""
    with playlist_lock:
        data = {"seconds": DEFAULT_SECONDS, **read_json(), **keys}
        data["items"] = data.get("items", []) if items is None else items
        PLAYLIST_DIR.mkdir(exist_ok=True)
        tmp = PLAYLIST.with_suffix(".tmp")
        tmp.write_text(json.dumps(data, indent=2), encoding="utf-8")
        os.replace(tmp, PLAYLIST)  # all at once, so the player never reads half a file


def playlist_state():
    items = []
    playlist = read_playlist()
    for item in playlist["items"]:
        path = PLAYLIST_DIR / item["file"]
        if path.suffix.lower() in VIDEO_TYPES:
            frames = "video"
        else:
            with Image.open(path) as im:
                frames = getattr(im, "n_frames", 1)
        items.append({**item, "frames": frames, "version": int(path.stat().st_mtime)})
    return {"items": items, "overlay": playlist["overlay"], **player_state()}


def widget_list(value):
    """Text widgets from the page, checked only for shape: the player skips any it doesn't understand."""
    if not isinstance(value, list) or not all(isinstance(w, dict) for w in value):
        raise ValueError("widgets must be a list of objects")
    return value


def save_overlay(req):
    """The global layer, the text over every piece: saved as it's edited."""
    overlay = widget_list(req.get("overlay"))
    write_playlist(overlay=overlay)
    return {"overlay": overlay}


def save_widgets(req):
    """One piece's own text, saved as it's edited. Only playlist.json changes: the piece's art isn't made again."""
    widgets = widget_list(req.get("widgets"))
    with playlist_lock:
        items = read_playlist()["items"]
        item = next((i for i in items if i["file"] == Path(req["file"]).name), None)
        if not item:
            raise ValueError("that piece isn't in the playlist any more")
        item["widgets"] = widgets
        write_playlist(items)
    return {"file": item["file"]}


def add_to_playlist(req):
    src = sources.get(req["id"])
    if not src:
        raise ValueError("that image is no longer loaded; open it again")
    flat, durations, bg, place = prepare(src, req["place"])
    stem, ext = safe_name(req["name"]), ".mp4" if src.video else ".png"
    file = stem + ext
    PLAYLIST_DIR.mkdir(exist_ok=True)
    source = keep_source(src)  # before writing the piece: editing an old piece, the piece is the original
    tmp = PLAYLIST_DIR / f"{stem}.tmp{ext}"
    if src.video:
        write_video_mp4(src, flat, durations, bg, tmp, place)
    else:
        wv.write_png(flat, durations, bg, tmp, place)
    os.replace(tmp, PLAYLIST_DIR / file)
    entry = {"file": file, "seconds": max(1, int(req.get("seconds") or DEFAULT_SECONDS)), "name": src.name,
             "source": source, "layout": req.get("layout")}
    if "widgets" in req:
        entry["widgets"] = widget_list(req["widgets"])  # the piece's own text: the player draws it
    with playlist_lock:
        items = read_playlist()["items"]
        for other in [i for i in items if Path(i["file"]).stem == stem and i["file"] != file]:
            items.remove(other)  # same name, other kind (a still replaced by a video, say)
            (PLAYLIST_DIR / other["file"]).unlink(missing_ok=True)
        existing = next((i for i in items if i["file"] == file), None)
        if existing:
            existing.update(entry)  # in place: it keeps its spot in the playlist
        else:
            items.append(entry)
        write_playlist(items)
        tidy_sources(items)
    return {**playlist_state(), "added": file, "replaced": existing is not None}


def save_playlist(req):
    """The page's edited list: new order, times, and anything removed (whose files are deleted)."""
    with playlist_lock:
        old = {i["file"]: i for i in read_playlist()["items"]}
        items = [{**old[name], "seconds": max(1, min(86400, int(i["seconds"])))}
                 for i in req["items"] if (name := Path(i["file"]).name) in old]
        write_playlist(items)
        for gone in old.keys() - {i["file"] for i in items}:
            (PLAYLIST_DIR / gone).unlink(missing_ok=True)
        tidy_sources(items)
    return playlist_state()


def edit_piece(req):
    """Opens a playlist piece for laying out again: its original and layout when kept, else the piece itself
    (pieces added before originals were kept)."""
    item = next((i for i in read_playlist()["items"] if i["file"] == Path(req["file"]).name), None)
    if not item:
        raise ValueError("that piece isn't in the playlist any more")
    kept = item.get("source") and PLAYLIST_DIR / item["source"]
    widgets = item.get("widgets", [])  # its text: saved as it's edited, so newer than the layout
    if kept and kept.is_file():
        return {"source": open_source(kept, item.get("name") or kept.name), "layout": item.get("layout"),
                "widgets": widgets, "original": True}
    # the finished piece fills the screen as it is; in-between sizes, since whole-number ones jump past the screen
    as_is = {"factor": 1, "scale": 1, "fractional": True, "smooth": False, "x": 0, "y": 0, "fill": "solid",
             "bg": "#000000"}
    return {"source": open_source(PLAYLIST_DIR / item["file"], item["file"]), "layout": as_is, "widgets": widgets,
            "original": False}


def thumbnail(file):
    path = PLAYLIST_DIR / Path(file).name
    key = (path.name, path.stat().st_mtime)
    if key not in thumbs:
        if path.suffix.lower() in VIDEO_TYPES:
            first = subprocess.run([wv.find_ffmpeg(), "-v", "error", "-i", str(path), "-frames:v", "1",
                                    "-f", "image2pipe", "-c:v", "png", "-"], capture_output=True,
                                   creationflags=NO_WINDOW).stdout
            source = Image.open(io.BytesIO(first))
        else:
            source = Image.open(path)
        with source as im:
            small = im.convert("RGB").reduce(8)  # first frame, 200x90
        buf = io.BytesIO()
        small.save(buf, "PNG")
        thumbs[key] = buf.getvalue()
    return thumbs[key]


def player_command():
    ffmpeg = ["--ffmpeg", wv.find_ffmpeg()] if wv.find_ffmpeg() else []  # for video pieces
    return [str(PLAYER), "play", str(PLAYLIST), *ffmpeg, "--log", str(PLAYER_LOG)]


def player_running():
    # wvplayw.exe as the studio and Windows start it, or wvplay.exe run by hand
    out = subprocess.run(["tasklist", "/FI", "IMAGENAME eq wvplay*", "/NH"], capture_output=True, text=True,
                         creationflags=NO_WINDOW).stdout
    return "wvplay" in out.lower()


def autostart():
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, RUN_KEY) as key:
            winreg.QueryValueEx(key, RUN_NAME)
            return True
    except FileNotFoundError:
        return False


def set_autostart(on):
    with winreg.OpenKey(winreg.HKEY_CURRENT_USER, RUN_KEY, 0, winreg.KEY_SET_VALUE) as key:
        if on:
            winreg.SetValueEx(key, RUN_NAME, 0, winreg.REG_SZ, subprocess.list2cmdline(player_command()))
        else:
            try:
                winreg.DeleteValue(key, RUN_NAME)
            except FileNotFoundError:
                pass
    return player_state()


def rivals():
    """Other cooler software set to start with Windows: it would take the cooler before the player can."""
    global rivals_seen
    if time.time() - rivals_seen[0] > 60:
        found = []
        tasks = subprocess.run(["schtasks", "/Query", "/FO", "CSV", "/NH"], capture_output=True, text=True,
                               errors="replace", creationflags=NO_WINDOW).stdout
        for row in csv.reader(tasks.splitlines()):
            if len(row) >= 3 and RIVALS.search(row[0]) and row[2].strip().lower() != "disabled":
                found.append(f"{row[0].lstrip(chr(92))} in Task Scheduler")
        for hive, path in ((winreg.HKEY_CURRENT_USER, RUN_KEY), (winreg.HKEY_LOCAL_MACHINE, RUN_KEY),
                           (winreg.HKEY_LOCAL_MACHINE, RUN_KEY.replace("Software", "Software\\WOW6432Node"))):
            try:
                with winreg.OpenKey(hive, path) as key:
                    for i in range(winreg.QueryInfoKey(key)[1]):
                        name, value, _ = winreg.EnumValue(key, i)
                        if RIVALS.search(f"{name} {value}"):
                            found.append(f"{name} in startup apps")
            except OSError:
                pass
        rivals_seen = (time.time(), sorted(set(found)))
    return rivals_seen[1]


def player_state():
    running = player_running()
    last = ""
    if running and PLAYER_LOG.exists():
        lines = PLAYER_LOG.read_text(encoding="utf-8", errors="replace").strip().splitlines()
        last = lines[-1] if lines else ""
    return {"running": running, "log": last, "built": PLAYER.exists(), "autostart": autostart(), "rivals": rivals()}


def start_player():
    if not PLAYER.exists():
        raise ValueError("the player isn't built yet: run `cargo build --release` in the player folder")
    if not read_playlist()["items"]:
        raise ValueError("the playlist is empty: add something first")
    if not player_running():
        PLAYER_LOG.parent.mkdir(exist_ok=True)
        # its own process group, so it keeps playing after the studio closes; it writes its own log
        subprocess.Popen(player_command(), stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                         stderr=subprocess.DEVNULL, creationflags=subprocess.CREATE_NEW_PROCESS_GROUP)
        time.sleep(1.5)
        if not player_running():
            raise ValueError(f"the player stopped straight away: {PLAYER_LOG.read_text(encoding='utf-8').strip()}")
    return player_state()


def stop_player():
    subprocess.run(["taskkill", "/IM", "wvplay*", "/F"], capture_output=True, creationflags=NO_WINDOW)
    return player_state()


# --- Fonts, for text widgets ------------------------------------------------------------

FONT_DIRS = [Path(r"C:\Windows\Fonts"),
             Path(os.path.expandvars(r"%LOCALAPPDATA%\Microsoft\Windows\Fonts")),  # fonts installed per user
             wv.ROOT / "fonts"]  # the project's own
DEFAULT_FONT = FONT_DIRS[0] / "segoeui.ttf"  # what text uses when its font is missing, in the player too
fonts_seen = (None, [], {})  # (the folders' modified times, the list, normcase(path) -> path)


def font_list():
    """[{path, family, style}] for the .ttf and .otf fonts in FONT_DIRS, by name. Cached until a folder changes,
    as it does when a font is installed or removed."""
    global fonts_seen
    stamp = tuple(d.stat().st_mtime if d.is_dir() else None for d in FONT_DIRS)
    if stamp != fonts_seen[0]:
        found = []
        for d in FONT_DIRS:
            for path in sorted(d.iterdir()) if d.is_dir() else []:
                if path.suffix.lower() not in (".ttf", ".otf"):
                    continue
                try:
                    family, style = ImageFont.truetype(str(path)).getname()
                except (OSError, ValueError):
                    continue  # not a font that can be read
                found.append({"path": str(path), "family": family or path.stem, "style": style or ""})
        found.sort(key=lambda f: (f["family"].lower(), f["style"].lower(), f["path"].lower()))
        fonts_seen = (stamp, found, {os.path.normcase(f["path"]): Path(f["path"]) for f in found})
    return fonts_seen[1]


def font_file(path):
    """The font at `path` if font_list lists it: no other file is served."""
    font_list()
    return fonts_seen[2].get(os.path.normcase(path))


# --- Live data in text (docs/data.md) -----------------------------------------------------
#
# The sources the player polls are set up in playlist/data.json. Their values come from the player's own API,
# which only answers on this PC: the page reaches it through the proxies here. data.json can hold a Home
# Assistant token or a plugin's password, which never goes back to the page.

DATA_API_PORT = 8139
DATA_TIMEOUT = 2   # seconds: the player answers straight away, or isn't running
TEST_TIMEOUT = 20  # a test polls the source once (a web request), so it can take a while
RESCAN_TIMEOUT = 10
no_proxy = urllib.request.build_opener(urllib.request.ProxyHandler({}))  # straight to 127.0.0.1, never a proxy

# The built-in sources, as the player describes them (docs/sources.md), for when it isn't running: the window
# still shows them then, as the sidebar cards used to. With the player up, its catalog is used instead.
BUILTIN_CATALOG = [
    {"id": "claude", "name": "Claude usage", "description": "Your plan's session and weekly use, read with Claude "
     "Code's login on this PC.", "interval": {"default": 60, "min": 30}, "settings": [],
     "provides": [{"key": "claude.session", "label": "Session used", "unit": "%", "kind": "number"},
                  {"key": "claude.session_resets", "label": "Session resets", "kind": "time"},
                  {"key": "claude.session_resets_in", "label": "Session resets in", "kind": "text"},
                  {"key": "claude.week", "label": "Week used", "unit": "%", "kind": "number"},
                  {"key": "claude.week_resets", "label": "Week resets", "kind": "time"},
                  {"key": "claude.week_resets_in", "label": "Week resets in", "kind": "text"},
                  {"key": "claude.session_severity", "label": "Session severity", "kind": "text"},
                  {"key": "claude.week_severity", "label": "Week severity", "kind": "text"},
                  {"key": "claude.week.*", "label": "Per-model weekly use", "unit": "%", "kind": "number", "pattern": True}]},
    {"id": "pc", "name": "PC sensors", "description": "CPU, memory and NVIDIA GPU. CPU temperature needs HWiNFO "
     "running with Shared Memory Support on.", "interval": {"default": 60, "min": 5},
     "settings": [{"key": "hwinfo", "label": "Read HWiNFO too", "type": "bool", "default": False}],
     "provides": [{"key": "pc.cpu.load", "label": "CPU load", "unit": "%", "kind": "number"},
                  {"key": "pc.cpu.temp", "label": "CPU temperature", "unit": "°C", "kind": "number"},
                  {"key": "pc.ram.used_gb", "label": "RAM used", "unit": "GB", "kind": "number"},
                  {"key": "pc.ram.total_gb", "label": "RAM total", "unit": "GB", "kind": "number"},
                  {"key": "pc.ram.used_pct", "label": "RAM used", "unit": "%", "kind": "number"},
                  {"key": "pc.gpu.temp", "label": "GPU temperature", "unit": "°C", "kind": "number"},
                  {"key": "pc.gpu.load", "label": "GPU load", "unit": "%", "kind": "number"},
                  {"key": "pc.gpu.power", "label": "GPU power", "unit": "W", "kind": "number"},
                  {"key": "pc.gpu.vram_used_gb", "label": "VRAM used", "unit": "GB", "kind": "number"},
                  {"key": "pc.gpu.vram_total_gb", "label": "VRAM total", "unit": "GB", "kind": "number"},
                  {"key": "hwinfo.*", "label": "HWiNFO readings", "pattern": True}]},
    {"id": "homeassistant", "name": "Home Assistant", "description": "The state and attributes of the entities listed.",
     "interval": {"default": 60, "min": 5},
     "settings": [{"key": "url", "label": "Address", "type": "text", "default": "", "placeholder": "http://homeassistant.local:8123"},
                  {"key": "token", "label": "Token", "type": "password",
                   "help": "A long-lived access token (your profile in Home Assistant, Security)."},
                  {"key": "entities", "label": "Entities", "type": "list", "default": [],
                   "placeholder": "sensor.living_room_temperature\nweather.home"}],
     "provides": [{"key": "ha.*", "label": "Each entity's state, unit and attributes", "pattern": True}]},
    {"id": "time", "name": "Time", "description": "The current local time. Always on.", "interval": {"default": 60, "min": 5},
     "settings": [], "provides": [{"key": "time", "label": "Now", "kind": "time"}]},
]
SECRET_KEY = re.compile(r"(^|_)(token|password|secret|api_?key)$", re.I)  # a setting that must be a password
catalog_cache = []  # the player's catalog as last seen, so password settings stay known when it's down
plugins_folder_seen = ""  # where the player said its plugins are, when it has


def data_path():
    return PLAYLIST_DIR / "data.json"


def plugins_dir():
    return PLAYLIST_DIR.parent / "plugins"


def read_data():
    """data.json as it is (passwords included: for this file only), or {} before there is one."""
    with playlist_lock:
        try:
            data = json.loads(data_path().read_text(encoding="utf-8"))
        except FileNotFoundError:
            return {}
        except ValueError as e:
            raise ValueError(f"{data_path().name} isn't valid JSON ({e}): fix it or delete it") from None
    if not isinstance(data, dict):
        raise ValueError(f"{data_path().name} should hold an object: fix it or delete it")
    return data


def catalog_list(out):
    """The manifests in a /catalog answer, whichever way the player wraps them."""
    if isinstance(out, dict):
        out = out.get("sources", out.get("catalog"))
    return [m for m in out if isinstance(m, dict) and isinstance(m.get("id"), str)] if isinstance(out, list) else []


def data_catalog():
    """The player's catalog, for the page: {player, plugins_dir, sources}. Without the player, the built-ins alone."""
    global catalog_cache
    out = player_api("/catalog")
    found = catalog_list(out)
    if found:
        for m in found:
            m.pop("run", None)  # the studio never sees or sets a plugin's command
        catalog_cache = found
    up = not (isinstance(out, dict) and out.get("player") is False)
    folder = out.get("folder") if isinstance(out, dict) and isinstance(out.get("folder"), str) else plugins_folder_seen or str(plugins_dir())
    answer = {"player": up, "plugins_dir": folder, "sources": found if up else BUILTIN_CATALOG}
    if isinstance(out, dict) and "error" in out:
        answer["error"] = out["error"]
    return answer


def data_rescan():
    """Has the player re-read the plugins folder: the catalog after, with the scan's `plugins` count, `problems`
    (one line per folder it couldn't use) and `folder`."""
    global plugins_folder_seen
    out = player_api("/catalog/rescan", {}, RESCAN_TIMEOUT)
    if not isinstance(out, dict) or out.get("player") is False or "error" in out:
        return out
    if isinstance(out.get("folder"), str):
        plugins_folder_seen = out["folder"]
    problems = out.get("problems")
    return {**data_catalog(), "plugins": out.get("plugins"),
            "problems": [str(p) for p in problems] if isinstance(problems, list) else []}


def manifest(kind):
    """What's known about one source type: its manifest, from the player if it has answered, else the built-in one."""
    for m in [*catalog_cache, *BUILTIN_CATALOG]:
        if m.get("id") == kind:
            return m
    return {}


def settings_of(kind):
    """The manifest's settings, keyed, with only the well-formed ones."""
    return {s["key"]: s for s in manifest(kind).get("settings", []) if isinstance(s, dict) and isinstance(s.get("key"), str)}


def password_keys(kind, source=()):
    """The settings of a source that are secrets: never returned, never logged. Besides the manifest's `password`
    settings, any key that looks like one (`token`, `api_key`), so a plugin's secret is kept back even before the
    player has told us its manifest."""
    keys = {k for k, s in settings_of(kind).items() if s.get("type") == "password"}
    keys |= {k for k in source if isinstance(k, str) and SECRET_KEY.search(k)}
    if kind == "homeassistant":
        keys.add("token")
    return keys


def shown_source(s):
    """A source for the page: each password left out, with `<key>_set` saying whether there is one."""
    secrets = password_keys(s.get("type"), s)
    shown = {k: v for k, v in s.items() if k not in secrets}
    for k in sorted(secrets):
        shown[f"{k}_set"] = isinstance(s.get(k), str) and bool(s[k])
    return shown


def data_config():
    """data.json for the page, with no secret in it."""
    data = read_data()
    shown = [shown_source(s) if isinstance(s, dict) else s for s in data.get("sources", [])]
    return {**data, "api_port": data_port(data), "sources": shown}


def same_server(kind, a, b):
    """Whether two settings of a source point at the same place: every `text` setting in its manifest (and `url`,
    whatever the manifest says) the same, compared trimmed, without a trailing /, and ignoring case. A plugin can
    call its server setting anything, so all of them count; lists, numbers, bools and choices may differ freely."""
    norm = lambda v: v.strip().rstrip("/").lower() if isinstance(v, str) else ""
    keys = {k for k, s in settings_of(kind).items() if s.get("type") == "text"} | {"url"}
    return all(norm(a.get(k)) == norm(b.get(k)) for k in keys)


def min_interval(kind):
    m = manifest(kind).get("interval")
    try:
        return max(1, int(m.get("min", 5))) if isinstance(m, dict) else 5
    except (TypeError, ValueError):
        return 5


def check_setting(kind, spec, v):
    """A setting's value as saved, checked against its manifest type; unknown types pass as they are."""
    t, k = spec.get("type"), spec.get("key")
    if v is None:
        return v
    if t == "number":
        if isinstance(v, bool) or not isinstance(v, (int, float)):
            try:
                v = float(v) if isinstance(v, str) and "." in v else int(v)
            except (TypeError, ValueError):
                raise ValueError(f"{kind}: {k} must be a number") from None
        return v
    if t == "bool":
        return bool(v)
    if t == "list":
        if isinstance(v, str):
            v = v.split("\n")
        if not isinstance(v, list) or not all(isinstance(e, str) for e in v):
            raise ValueError(f"{kind}: {k} must be a list")
        return [e.strip() for e in v if e.strip()]
    if t == "choice" and isinstance(spec.get("choices"), list) and spec["choices"] and v not in spec["choices"]:
        raise ValueError(f"{kind}: {k} must be one of {', '.join(map(str, spec['choices']))}")
    return v.strip() if isinstance(v, str) and t in ("text", "choice") else v


def save_data_config(req):
    """The page's data sources, saved. Keys it doesn't send are kept, at the top and in each source (matched
    with the saved ones by type, in order), and so is each saved password unless a new one is given."""
    if not isinstance(req, dict) or not isinstance(req.get("sources", []), list):
        raise ValueError("expected {sources: [...]}")
    with playlist_lock:
        old = read_data()
        saved = {}  # type -> that type's saved sources, in order
        for s in old.get("sources", []):
            if isinstance(s, dict):
                saved.setdefault(s.get("type"), []).append(s)
        data = {**old, **{k: v for k, v in req.items() if k != "sources"}}
        if "sources" in req:
            seen = {}
            data["sources"] = []
            for s in req["sources"]:
                if not isinstance(s, dict):
                    raise ValueError("each source must be an object")
                kind = s.get("type")
                n = seen[kind] = seen.get(kind, -1) + 1
                was = saved.get(kind, [])[n] if n < len(saved.get(kind, [])) else {}
                secrets = password_keys(kind, [*s, *was])
                flags = {f"{k}_set" for k in secrets}
                merged = {**was, **{k: v for k, v in s.items() if k not in secrets and k not in flags}}
                for k in secrets:
                    new = s.get(k)
                    if isinstance(new, str) and new.strip():
                        merged[k] = new.strip()
                    elif not same_server(kind, merged, was):
                        # a saved secret only ever goes back to the server it was given for: anyone who can reach
                        # the studio could otherwise point the address at their own and have it sent there
                        merged.pop(k, None)
                s = merged
                if "interval" in s:
                    try:
                        s["interval"] = max(min_interval(kind), int(s["interval"]))
                    except (TypeError, ValueError):
                        raise ValueError(f"{kind}: interval must be a number of seconds") from None
                if "enabled" in s:
                    s["enabled"] = bool(s["enabled"])
                for k, spec in settings_of(kind).items():
                    if k in s and spec.get("type") != "password":
                        s[k] = check_setting(kind, spec, s[k])
                if "entities" in s and kind == "homeassistant":
                    s["entities"] = check_setting(kind, {"key": "entities", "type": "list"}, s["entities"])
                data["sources"].append(s)
        if "api_port" in data and not (isinstance(data["api_port"], int) and 0 < data["api_port"] < 65536):
            raise ValueError("api_port must be a port number")
        PLAYLIST_DIR.mkdir(exist_ok=True)
        tmp = data_path().with_suffix(".tmp")
        tmp.write_text(json.dumps(data, indent=2, ensure_ascii=False), encoding="utf-8")
        os.replace(tmp, data_path())  # all at once, so the player never reads half a file
    return data_config()


def data_port(data=None):
    """The port the player's API listens on, from data.json."""
    try:
        port = (read_data() if data is None else data).get("api_port", DATA_API_PORT)
    except ValueError:
        return DATA_API_PORT
    return port if isinstance(port, int) and 0 < port < 65536 else DATA_API_PORT


def player_api(path, body=None, timeout=DATA_TIMEOUT):
    """Asks the player's local API: its answer, or {"player": False} when it isn't running."""
    req = urllib.request.Request(f"http://127.0.0.1:{data_port()}{path}",
                                 data=None if body is None else json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json"})
    try:
        with no_proxy.open(req, timeout=timeout) as r:
            return json.loads(r.read())
    except urllib.error.HTTPError as e:  # it's running, but turned the request down
        try:
            out = json.loads(e.read())
        except ValueError:
            out = None
        return out if isinstance(out, dict) and "error" in out else {"error": f"the player answered {e.code} {e.reason}"}
    except (OSError, ValueError, HTTPException):  # nothing listening, no answer in time, or not the player
        return {"player": False}


def data_resolve(req):
    """The texts with their placeholders filled in by the player, which does all the formatting."""
    texts = req.get("texts") if isinstance(req, dict) else None
    if not isinstance(texts, list) or not all(isinstance(t, str) for t in texts):
        raise ValueError("expected {texts: [string, ...]}")
    return player_api("/resolve", {"texts": texts})


def data_test(req):
    """Runs one source as set up on the page, once, in the player. Without a new password, the saved one is used."""
    source = req.get("source") if isinstance(req, dict) else None
    if not isinstance(source, dict):
        raise ValueError("expected {source: {...}}")
    kind = source.get("type")
    secrets = password_keys(kind, source)
    source = {k: v for k, v in source.items() if k not in {f"{s}_set" for s in secrets}}
    missing = {k for k in secrets if not (isinstance(source.get(k), str) and source[k].strip())}
    for k in missing:
        source.pop(k, None)
    if missing:
        saved = next((s for s in read_data().get("sources", []) if isinstance(s, dict) and s.get("type") == kind
                      and same_server(kind, s, source)), None)  # only to its own server
        for k in missing:
            if saved and isinstance(saved.get(k), str) and saved[k]:
                source[k] = saved[k]
    return player_api("/test", {"source": source}, TEST_TIMEOUT)


# --- Server ----------------------------------------------------------------------

class Handler(BaseHTTPRequestHandler):
    def trusted(self, post=False):
        # Only the addresses the studio listens on: the Host check stops DNS-rebinding pages, and the
        # custom header makes other sites' requests need a CORS preflight, which this server never grants.
        port = self.server.server_port
        hosts = getattr(self.server, "hosts", None) or {f"127.0.0.1:{port}", f"localhost:{port}"}
        ok = self.headers.get("Host") in hosts
        if ok and post:
            ok = self.headers.get("X-Studio") == "1"
        if not ok:
            self.send(403, b"forbidden", "text/plain")
        return ok

    def handle(self):
        try:
            super().handle()
        except ConnectionError:
            pass  # the browser went away mid-reply (closed tab, cancelled image load)

    def send(self, code, body, ctype):
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def json(self, obj, code=200):
        self.send(code, json.dumps(obj).encode(), "application/json")

    def send_file(self, path, ctype):
        size = path.stat().st_size
        start, end = 0, size - 1
        wanted = re.fullmatch(r"bytes=(\d*)-(\d*)", self.headers.get("Range", ""))
        if wanted and (wanted[1] or wanted[2]):
            start = int(wanted[1]) if wanted[1] else max(0, size - int(wanted[2]))
            end = min(int(wanted[2]), size - 1) if wanted[1] and wanted[2] else end
            self.send_response(206)
            self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
        else:
            self.send_response(200)
        self.send_header("Content-Type", ctype)
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("Content-Length", str(end - start + 1))
        self.end_headers()
        with open(path, "rb") as f:
            f.seek(start)
            left = end - start + 1
            try:
                while left > 0 and (chunk := f.read(min(left, 1 << 20))):
                    self.wfile.write(chunk)
                    left -= len(chunk)
            except ConnectionError:
                pass  # the browser had what it wanted

    def do_GET(self):
        if not self.trusted():
            return
        url = urlparse(self.path)
        if url.path == "/":
            self.send(200, PAGE.read_bytes(), "text/html; charset=utf-8")
        elif url.path == "/api/config":
            self.json({"screen": [wv.SCREEN_W, wv.SCREEN_H], "safe": wv.SAFE, "camera": wv.CAMERA_HOLE,
                       "font": str(DEFAULT_FONT), "initial": initial})
        elif (m := re.fullmatch(r"/api/frame/(\w+)/(\d+)\.png", url.path)) and m[1] in sources:
            src, i = sources[m[1]], int(m[2])
            if i < len(src.frames):
                return self.send(200, src.png(i), "image/png")
            self.send(404, b"no such frame", "text/plain")
        elif (m := re.fullmatch(r"/api/video/(\w+)\.mp4", url.path)) and getattr(sources.get(m[1]), "video", None):
            self.send_file(sources[m[1]].preview, "video/mp4")
        elif url.path == "/api/playlist":
            self.json(playlist_state())
        elif url.path == "/api/player":
            self.json(player_state())
        elif (m := re.fullmatch(r"/api/thumb/([\w .()-]+\.(?:png|mp4))", unquote(url.path))) and (PLAYLIST_DIR / m[1]).is_file():
            self.send(200, thumbnail(m[1]), "image/png")
        elif url.path == "/api/fonts":
            self.json(font_list())
        elif url.path == "/api/font" and (path := font_file(parse_qs(url.query).get("path", [""])[0])):
            self.send_file(path, "font/otf" if path.suffix.lower() == ".otf" else "font/ttf")
        elif url.path == "/api/data/config":
            try:
                self.json(data_config())
            except ValueError as e:
                self.json({"error": str(e)}, 500)
        elif url.path == "/api/data/values":
            self.json(player_api("/values"))
        elif url.path == "/api/data/catalog":
            self.json(data_catalog())
        else:
            self.send(404, b"not found", "text/plain")

    def do_POST(self):
        if not self.trusted(post=True):
            return
        url = urlparse(self.path)
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        try:
            if url.path == "/api/load":
                name = Path(parse_qs(url.query).get("name", ["upload.png"])[0]).name
                UPLOADS.mkdir(parents=True, exist_ok=True)
                path = UPLOADS / (hashlib.sha1(body).hexdigest()[:12] + Path(name).suffix.lower())
                path.write_bytes(body)
                self.json(open_source(path, name))
            elif url.path == "/api/export":
                self.json(export(json.loads(body)))
            elif url.path == "/api/reveal":
                os.startfile(wv.OUT)
                self.json({})
            elif url.path == "/api/playlist/add":
                self.json(add_to_playlist(json.loads(body)))
            elif url.path == "/api/playlist":
                self.json(save_playlist(json.loads(body)))
            elif url.path == "/api/playlist/edit":
                self.json(edit_piece(json.loads(body)))
            elif url.path == "/api/playlist/overlay":
                self.json(save_overlay(json.loads(body)))
            elif url.path == "/api/playlist/widgets":
                self.json(save_widgets(json.loads(body)))
            elif url.path == "/api/player/start":
                self.json(start_player())
            elif url.path == "/api/player/stop":
                self.json(stop_player())
            elif url.path == "/api/autostart":
                self.json(set_autostart(bool(json.loads(body)["on"])))
            elif url.path == "/api/data/config":
                self.json(save_data_config(json.loads(body)))
            elif url.path == "/api/data/resolve":
                self.json(data_resolve(json.loads(body)))
            elif url.path == "/api/data/test":
                self.json(data_test(json.loads(body)))
            elif url.path == "/api/data/catalog/rescan":
                self.json(data_rescan())
            else:
                self.send(404, b"not found", "text/plain")
        except SystemExit as e:  # wv's helpers exit with a message on failure
            self.json({"error": str(e)}, 500)
        except OSError as e:
            if e.filename and e.errno in (errno.EACCES, errno.EINVAL):
                # EINVAL is what Windows gives when another program (Photos, say) has the file mapped
                e = f"couldn't overwrite {Path(e.filename).name}. It's probably open in another program " \
                    "(Photos, TRCC): close it there, or export under a different name."
            self.json({"error": str(e)}, 500)
        except Exception as e:
            self.json({"error": f"{type(e).__name__}: {e}"}, 500)

    def log_message(self, fmt, *args):
        pass  # one line per animation frame fetched is just noise


SETTINGS = wv.ROOT / "studio.json"


def tailscale():
    """This PC's Tailscale IPv4 address and its MagicDNS names, from the Tailscale CLI."""
    exe = shutil.which("tailscale") or r"C:\Program Files\Tailscale\tailscale.exe"
    out = subprocess.run([exe, "status", "--json"], capture_output=True, text=True, timeout=10,
                         creationflags=NO_WINDOW).stdout
    me = json.loads(out).get("Self", {})
    ips = [ip for ip in me.get("TailscaleIPs", []) if "." in ip]
    if not ips:
        raise ValueError("Tailscale has no address for this PC: is it connected?")
    dns = me.get("DNSName", "").rstrip(".")
    return ips[0], sorted({dns, dns.split(".")[0]} - {""})  # mypc.tail1234.ts.net and mypc


def listen_addresses(settings):
    """[(ip, other names it can be reached by)] for studio.json's "listen" list."""
    addresses = []
    for entry in settings.get("listen", ["localhost"]):
        if entry == "localhost":
            addresses.append(("127.0.0.1", ["localhost"]))
        elif entry == "tailscale":
            try:
                addresses.append(tailscale())
            except (OSError, ValueError, subprocess.TimeoutExpired) as e:
                print(f"warning: not listening on Tailscale: {e}")
        else:
            addresses.append((entry, []))
    if not addresses:
        raise SystemExit(f"nothing to listen on: check \"listen\" in {SETTINGS.name}")
    return addresses


def serve(image=None, port=None, open_browser=True):
    """Runs the studio on the addresses in studio.json (default: this PC only) until Ctrl+C.

    studio.json, all optional:  {"listen": ["localhost", "tailscale", "192.168.1.20"], "port": 8137}
    "tailscale" means this PC's Tailscale address, looked up each start.
    """
    global initial
    settings = json.loads(SETTINGS.read_text(encoding="utf-8")) if SETTINGS.exists() else {}
    if image:
        path = Path(image)
        if not path.exists():
            raise SystemExit(f"can't find {image}")
        initial = open_source(path, path.name)
    addresses = listen_addresses(settings)
    port = port or settings.get("port", 8137)
    for p in range(port, port + 20):  # the first port free on every address
        servers = []
        try:
            for ip, _ in addresses:
                servers.append(ThreadingHTTPServer((ip, p), Handler))
            break
        except OSError:
            for s in servers:
                s.server_close()
    else:
        raise SystemExit(f"no free port in {port}..{port + 19}")
    hosts = {f"{name}:{p}" for ip, names in addresses for name in (ip, *names)}
    for s in servers:
        s.hosts = hosts
    urls = [f"http://{max(names, key=len) if names and ip != '127.0.0.1' else ip}:{p}/" for ip, names in addresses]
    print(f"studio running at {'  and  '.join(urls)}  (Ctrl+C to stop)")
    for s in servers[1:]:
        threading.Thread(target=s.serve_forever, daemon=True).start()
    if open_browser:
        webbrowser.open(urls[0])
    try:
        servers[0].serve_forever()
    except KeyboardInterrupt:
        pass
