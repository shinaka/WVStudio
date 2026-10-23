"""Studio: a scene editor for the Wonder Vision screen (docs/scenes.md), the playlist, and the player.

  python wv.py studio [image]

Serves studio/index.html (and studio.js) to a browser on this PC only. The page draws the scenes and edits the
playlist document, which saves here as it changes; the player draws the same document on the cooler. Widgets
(docs/widgets.md) are files of their own in widgets/, beside the playlist's folder, saved one by one.
"""
import base64
import contextlib
import copy
import csv
import errno
import hashlib
import io
import json
import math
import os
import re
import secrets
import shutil
import stat
import struct
import subprocess
import threading
import time
import unicodedata
import urllib.error
import urllib.request
import warnings
import webbrowser
import winreg
import zipfile
import zlib
from collections import OrderedDict
from http.client import HTTPException
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlparse

import numpy as np
from PIL import Image, ImageFont

import wv

PAGE = wv.ROOT / "studio" / "index.html"
SCRIPT = PAGE.with_name("studio.js")  # the page's code, served at /studio.js
# the page's code for widgets (docs/widgets.md), each served at /<name>: these three names only
WIDGET_SCRIPTS = {name: PAGE.with_name(name) for name in ("widgets.js", "library.js", "copies.js")}
UPLOADS = wv.ROOT / ".cache" / "studio"
KEY = np.array([1 << 16, 1 << 8, 1], np.uint32)
VIDEO_TYPES = {".mp4", ".m4v", ".mov", ".webm", ".mkv", ".avi"}
# The container ffmpeg reads each video as, by its extension, as the player does: never anything else, so a file
# named .mp4 that's really a playlist of other files or addresses can't make ffmpeg open them.
VIDEO_FORMATS = {".mp4": "mov", ".m4v": "mov", ".mov": "mov", ".mkv": "matroska", ".webm": "matroska", ".avi": "avi"}
VIDEO_FPS = 30  # the most frames a second a video is kept at: about what the panel shows comfortably
VIDEO_SECONDS = 30  # the longest stretch of a video that's kept
NO_WINDOW = subprocess.CREATE_NO_WINDOW

# Opened media, one Source per file (normcased absolute path), least recently used first. The oldest go while
# their decoded frames pass MEDIA_BYTES in all, or there are more than MEDIA_COUNT (docs/widgets.md: thumbnails of
# a library load a lot of media, and mustn't push out what a scene is still loading). One used in the last
# MEDIA_GRACE seconds is still having its frames fetched by the page, so it stays until they pass MEDIA_HARD; and
# one that went is opened again when its frames are asked for (`opened`), so the page never gets a 404 for a file
# it was just told about.
sources = OrderedDict()
sources_lock = threading.Lock()
MEDIA_BYTES = 512 << 20
MEDIA_HARD = 1 << 30
MEDIA_GRACE = 30  # seconds
MEDIA_COUNT = 64
opened = OrderedDict()  # content id -> Seen: the files opened lately, to open again or serve a video's preview
OPENED_COUNT = 4096
opening = {}  # normcased path -> Lock, held while that file is opened: one decode, however many ask at once
# At most DECODE_BYTES of frames are decoded at once, across every request: the page opens a whole library's
# pictures together, and a shared widget's picture may take up to 1 GB as frames. A file waits for room; one
# bigger than that waits until it's alone.
DECODE_BYTES = 1 << 30
VIDEO_COST = 256 << 20  # about the most a video's kept JPEGs take (VIDEO_FPS x VIDEO_SECONDS frames)
decoding = threading.Condition()
decoding_bytes = 0
initial = None  # info for the image named on the command line


def video_args(path):
    """What goes before `-i` on every ffmpeg run on a media file: file access only, read as the container its
    extension names."""
    return ["-protocol_whitelist", "file", "-f", VIDEO_FORMATS.get(Path(path).suffix.lower(), "mov")]


def probe_video(path):
    """ffmpeg's description of a video file (its stderr), or "" without ffmpeg."""
    ff = wv.find_ffmpeg()
    if not ff:
        return ""
    return subprocess.run([ff, "-hide_banner", *video_args(path), "-i", str(path)], capture_output=True, text=True,
                          errors="replace", stdin=subprocess.DEVNULL, timeout=60, creationflags=NO_WINDOW).stderr


def native_size(probe):
    """A video's own [width, height] as the player reads it: the first WxH on its Video line, swapped when the file
    says it's turned a quarter (a phone video). None when the probe has no video."""
    m = re.search(r"Stream #.*Video:.*?\b(\d{2,5})x(\d{2,5})\b", probe)
    if not m:
        return None
    w, h = int(m[1]), int(m[2])
    # older ffmpeg says "displaymatrix: rotation of -90.00 degrees", ffmpeg 8 "Display Matrix: rotation of 90.00 degrees"
    turn = (re.search(r"display ?matrix: rotation of (-?\d+(?:\.\d+)?) degrees", probe, re.I)
            or re.search(r"\brotate\s*:\s*(-?\d+(?:\.\d+)?)", probe))
    if turn and round(abs(float(turn[1]))) % 180 == 90:
        w, h = h, w
    return [w, h]


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
    probe = probe_video(path)
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
        ff, "-v", "error", "-y", "-t", str(VIDEO_SECONDS), *video_args(path), "-i", str(path), "-filter_complex", graph,
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
    details = {"fps": fps, "trimmed": seconds > VIDEO_SECONDS + 0.5}
    return JpegFrames(jpegs, 1000 / fps), details, native_size(probe)


class Source:
    def __init__(self, path, name, ref=None):
        self.name, self.path = name, path
        self.stamp = file_stamp(path)  # what the file was when it was opened: a change opens it again
        self.id = hashlib.sha1(path.read_bytes()).hexdigest()[:12]
        self.video = self.native = self.preview = None
        self.used = time.monotonic()  # when the page last asked for it (trim_sources spares it for a moment)
        if path.suffix.lower() in VIDEO_TYPES:
            UPLOADS.mkdir(parents=True, exist_ok=True)
            self.preview = UPLOADS / f"{self.id}.preview.mp4"
            self.frames, self.video, self.native = decode_video(path, self.preview)
            # the JPEGs, plus the one frame decoded at a time
            self.bytes = sum(map(len, self.frames.jpegs)) + self.frames[0][0].width * self.frames[0][0].height * 4
        else:
            self.frames = wv.load_frames(path)
            self.bytes = sum(im.width * im.height * 4 for im, _ in self.frames)
        self.pngs = {}
        self.src = ref  # how the page names it: sources/<file>, inputs/<file> or widgets/<id>/<path>
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
        info = {"id": self.id, "name": self.name, "width": first.shape[1], "height": first.shape[0],
                "durations": [self.frames[0][1]] * len(self.frames) if self.video else [d for _, d in self.frames],
                # a compressed video never has clean enlarged pixels to find
                "factor": 1 if self.video else max(1, wv.native_factor(self.frames)),
                "edge": f"#{edge:06x}"}
        if self.video:
            info["video"] = {"url": f"/api/video/{self.id}.mp4", **self.video}
            # its own size, turned as it plays: `width` and `height` are the decoded frames' (keep_aspect needs this)
            info["native"] = self.native or [info["width"], info["height"]]
        return info

    def png(self, i):
        if i not in self.pngs:
            buf = io.BytesIO()
            self.frames[i][0].save(buf, "PNG", compress_level=1)
            self.pngs[i] = buf.getvalue()
        return self.pngs[i]




# --- The playlist: scenes of layers and objects (docs/scenes.md) ---------------------------------
#
# playlist/playlist.json, version 2: scenes, each a stack of layers holding image, video, text, shape and widget
# objects (copies), plus a global layer stack over every scene. The page edits the whole document and sends it back
# as it changes; the player re-reads it whenever it changes, so editing here is live. Media lives in
# playlist/sources/, and the pictures chosen for copies' inputs in playlist/inputs/, each named by content hash. A
# version 1 file (one piece file plus text per item) is converted the first time it's read.

PLAYLIST_DIR = wv.ROOT / "playlist"
PLAYLIST = PLAYLIST_DIR / "playlist.json"
PLAYER = wv.ROOT / "player" / "target" / "release" / "wvplayw.exe"  # the one without a console window
PLAYER_LOG = wv.ROOT / ".cache" / "wvplay.log"
RUN_KEY = r"Software\Microsoft\Windows\CurrentVersion\Run"  # per-user startup apps
RUN_NAME = "Wonder Vision player"
RIVALS = re.compile(r"TRCC|InfoPanel|USBLCD", re.I)  # other programs that drive the cooler
rivals_seen = (0.0, [])
DEFAULT_SECONDS = 30
# Requests run on their own threads, and edits save as they happen: one change to playlist.json (or a widget's
# folder) at a time.
playlist_lock = threading.RLock()


def read_json(path):
    """A JSON file as it is, or {} before there is one."""
    with playlist_lock:
        try:
            return json.loads(path.read_text(encoding="utf-8"))
        except FileNotFoundError:
            return {}


def in_use(e):
    """Whether Windows refused a file operation because a program has the file open."""
    return isinstance(e, PermissionError) or getattr(e, "winerror", None) in (5, 32)


def replace_file(tmp, path):
    """os.replace, tried again for a moment while Windows refuses it: the player reading the file at that instant
    is enough."""
    for wait in (0.01, 0.02, 0.05, 0.1, 0.15, 0.25, 0.4, 0.6, None):
        try:
            return os.replace(tmp, path)
        except OSError as e:
            if wait is None or not in_use(e):
                raise
            time.sleep(wait)


def write_file(path, data):
    """Writes bytes all at once, through <name>.tmp beside it, so nothing ever reads half a file."""
    with playlist_lock:
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp = path.with_name(path.name + ".tmp")
        if os.path.lexists(tmp):
            os.unlink(tmp)  # left by a failed write, or a link someone put there: never written through
        try:
            with open(tmp, "xb") as f:
                f.write(data)
            replace_file(tmp, path)
        except BaseException:
            try:
                os.unlink(tmp)
            except OSError:
                pass
            raise


JSON_DEPTH = 127  # arrays and objects inside each other: the player's JSON reader (serde_json) stops at 128
TOO_DEEP = f"nests arrays and objects more than {JSON_DEPTH} deep, which the player doesn't read"


def strict_json(text):
    """JSON as the player reads it (serde_json), which refuses the whole file for any of these, so here they're
    refused too: NaN and Infinity, which aren't JSON numbers; a number past a double's range (1e400, or an integer
    of 400 digits), which Python would read as inf or keep exact, and which the page's JSON.parse then can't take
    or reads as Infinity; a string or key holding half of a surrogate pair (a lone \\ud83d), which isn't text; and
    nesting deeper than the player reads."""
    def refuse(name):
        raise ValueError(f"{name} isn't a JSON number")

    def real(s):
        v = float(s)
        if not math.isfinite(v):
            raise ValueError(f"the number {short_number(s)} is out of range")
        return v

    def whole(s):
        # past 309 digits it's past a double's range: refused before int() is asked to read thousands of digits
        if len(s.lstrip("-")) <= 309:
            v = int(s)
            try:
                float(v)
                return v
            except OverflowError:
                pass
        raise ValueError(f"the number {short_number(s)} is out of range")
    try:
        value = json.loads(text, parse_constant=refuse, parse_float=real, parse_int=whole)
    except RecursionError:
        raise ValueError(f"it {TOO_DEEP}") from None
    if too_deep(value):
        raise ValueError(f"it {TOO_DEEP}")
    if (half := lone_surrogate(value)) is not None:
        raise ValueError(f"a string in it holds \\u{ord(half):04x}, half of a surrogate pair, which isn't text")
    return value


def short_number(s):
    """A number's text for a message: at most 24 characters."""
    return s if len(s) <= 24 else s[:20] + "…"


SURROGATE = re.compile("[\ud800-\udfff]")


def lone_surrogate(value):
    """The first lone surrogate (U+D800 to U+DFFF: half of a pair, which JSON's \\u escapes can spell but no text
    holds) in any string or key of a JSON value, or None. A loop, not recursion, as in too_deep."""
    for s in strings_in(value, keys=True):
        if m := SURROGATE.search(s):
            return m[0]
    return None


def too_deep(value):
    """Whether arrays and objects nest more than JSON_DEPTH deep in a JSON value. A loop, not recursion: a file
    made by hand can nest deeper than Python recurses."""
    stack = [(value, 1)]
    while stack:
        v, depth = stack.pop()
        if isinstance(v, (dict, list)):
            if depth > JSON_DEPTH:
                return True
            stack.extend((x, depth + 1) for x in (v.values() if isinstance(v, dict) else v))
    return False


def json_bytes(data):
    """JSON as the studio writes it: indent 2, UTF-8 without a byte-order mark, Windows line ends as ever."""
    text = json.dumps(data, indent=2, ensure_ascii=False, allow_nan=False)
    return text.replace("\n", os.linesep).encode("utf-8")  # a newline inside a string is written \n


def write_atomic(path, data):
    """Writes JSON (json_bytes) all at once, so the player never reads half a file."""
    write_file(path, json_bytes(data))


def file_stamp(path):
    """A file's stamp, "<modified ns>:<size>": a string, because a modified time in nanoseconds doesn't survive
    the page's JavaScript numbers. The page sends it back as it got it, and a save compares the two."""
    st = os.stat(path)
    return f"{st.st_mtime_ns}:{st.st_size}"


def sources_dir():
    return PLAYLIST_DIR / "sources"


def source_name(path):
    """The relative `src` of a kept file."""
    return f"sources/{Path(path).name}"


# --- Tidying by last use (docs/widgets.md, Sharing) --------------------------------------------------------------
#
# One rule for playlist/sources/, playlist/inputs/ and each widget's media/: a file goes when the document it
# belongs to doesn't name it and nothing has named it for TIDY_AGE. "Nothing" includes the page's undo history,
# which tells us what it holds (POST /api/media/hold), so an undo never brings back an object whose file has gone.
# Only names the studio makes are tidied: <hash12>.<ext> in sources/ and inputs/, <stem>-<hash8>.<ext> in media/.

TIDY_AGE = 600  # seconds
last_used = {}  # os.path.normcase(absolute path) -> time.time() it was last named; after a restart, mtimes stand in
KEPT_NAME = re.compile(r"[0-9a-f]{12}\.[a-z0-9]+")
MEDIA_NAME = re.compile(r"[a-z0-9](?:[a-z0-9-]{0,38}[a-z0-9])?-[0-9a-f]{8}\.[a-z0-9]+")


def use_key(path):
    return os.path.normcase(os.path.abspath(path))


def note_used(paths):
    """Remembers that these files are named now."""
    now = time.time()
    for p in paths:
        last_used[use_key(p)] = now


def strings_in(value, keys=False):
    """Every string anywhere in a JSON value, in order (with `keys`, objects' keys too). A loop, not recursion, as
    in too_deep."""
    stack = [value]
    while stack:
        v = stack.pop()
        if isinstance(v, str):
            yield v
        elif isinstance(v, dict):
            stack.extend(reversed([x for kv in v.items() for x in kv] if keys else list(v.values())))
        elif isinstance(v, list):
            stack.extend(reversed(v))


def named_files(doc, folders):
    """{folder: {normcased file names}}: the files directly in each of `folders` (relative names like "sources" or
    "media") that a document names anywhere, as `<folder>/<file>`, compared as Windows compares names: the folder
    too, so Media/<file> names the file that Windows opens for it."""
    named = {f: set() for f in folders}
    by_case = {os.path.normcase(f): f for f in folders}
    for s in strings_in(doc):
        parts = s.strip(" ").split("/")
        if len(parts) == 2 and parts[1] and (folder := by_case.get(os.path.normcase(parts[0]))):
            named[folder].add(os.path.normcase(parts[1]))
    return named


def tidy_folder(folder, named, pattern):
    """Deletes the files directly in `folder` with a name the studio makes (`pattern`) that aren't `named` and
    haven't been for TIDY_AGE. Nothing else is ever touched: not a file named by hand, not a folder, not a link."""
    if not real_dir(folder):
        return  # missing, or a link to somewhere else: nothing there is ours to delete
    try:
        entries = list(os.scandir(folder))
    except OSError:
        return
    now = time.time()
    for e in entries:
        if not pattern.fullmatch(e.name) or os.path.normcase(e.name) in named:
            continue
        try:
            st = e.stat(follow_symlinks=False)
            if not stat.S_ISREG(st.st_mode) or e.is_symlink():
                continue
            seen = last_used.get(use_key(e.path), st.st_mtime)
            if now - seen <= TIDY_AGE:
                continue
            # and only a file the studio named: the hash in its name is its content's
            digest = re.search(r"([0-9a-f]{8,12})\.[a-z0-9]+$", e.name)[1]
            with open(e.path, "rb") as f:
                if not hashlib.file_digest(f, "sha1").hexdigest().startswith(digest):
                    continue
            os.unlink(e.path)
            last_used.pop(use_key(e.path), None)
        except OSError:
            pass  # open in another program: next time


# Reusable widgets (docs/widgets.md) live next to the playlist's folder, as plugins/ does. Functions, not constants,
# so a harness that moves PLAYLIST_DIR moves them too.
def widgets_dir():
    return PLAYLIST_DIR.parent / "widgets"


def trash_dir():
    """Deleted widgets, kept so they can be restored."""
    return widgets_dir() / ".deleted"


def inputs_dir():
    """The pictures chosen for copies' image inputs, in a scene or the global layer."""
    return PLAYLIST_DIR / "inputs"


def keep_file(data, suffix, folder="sources"):
    """Keeps a media file's bytes in playlist/sources/ (or inputs/) under its content hash, written as a copy (a
    hard link would keep its original's old modified time, and a file of the same name elsewhere must never stand
    in for the bytes). Returns (kept path, src)."""
    kept = PLAYLIST_DIR / folder / f"{hashlib.sha1(data).hexdigest()[:12]}{suffix.lower()}"
    with playlist_lock:
        if not kept.is_file():
            write_file(kept, data)
        note_used([kept])
    return kept, f"{folder}/{kept.name}"


def media_size(path):
    """(width, height) of a media file as the studio and player see it: for a video, after the decode's
    fit-to-screen scaling (decode_video), so a version 1 layout's scale applies to the same size."""
    if path.suffix.lower() in VIDEO_TYPES:
        probe = probe_video(path)
        m = re.search(r"Stream #.*Video:.*?\b(\d{2,5})x(\d{2,5})\b", probe)
        if not m:
            return wv.SCREEN_W, wv.SCREEN_H
        iw, ih = int(m[1]), int(m[2])
        k = min(1, max(wv.SCREEN_W / iw, wv.SCREEN_H / ih))
        return int(iw * k / 2) * 2, int(ih * k / 2) * 2
    with Image.open(path) as im:
        return im.size


def convert_v1(data):
    """A version 1 playlist as a version 2 document (docs/scenes.md, Moving from version 1)."""
    scenes = []
    for n, item in enumerate(data.get("items", [])):
        if not isinstance(item, dict) or not isinstance(item.get("file"), str):
            continue
        piece = PLAYLIST_DIR / Path(item["file"]).name
        lay = item.get("layout") if isinstance(item.get("layout"), dict) else {}
        kept = item.get("source") and (PLAYLIST_DIR / item["source"])
        art = {"id": "art", "type": "image", "name": Path(item["file"]).stem, "visible": True, "locked": False,
               "opacity": 1.0, "x": 0, "y": 0, "w": wv.SCREEN_W, "h": wv.SCREEN_H, "rotate": 0, "flip": False,
               "smooth": False, "fill": "none", "pixel": 1, "src": None}
        background = "#000000"
        if kept and kept.is_file():
            art["src"] = item["source"]
            if lay:
                try:
                    sw, sh = media_size(kept)
                    rotate = int(lay.get("rotate", 0)) % 360
                    factor = max(1, int(lay.get("factor", 1)))
                    scale = float(lay.get("scale", 1))
                    if rotate % 180:
                        sw, sh = sh, sw
                    art.update(w=max(1, round(sw / factor * scale)), h=max(1, round(sh / factor * scale)),
                               x=int(lay.get("x", 0)), y=int(lay.get("y", 0)), rotate=rotate, flip=bool(lay.get("flip")),
                               smooth=bool(lay.get("smooth")), pixel=factor)
                    fill = lay.get("fill", "solid")
                    art["fill"] = fill if fill in ("extend", "mirror", "tile") else "none"
                    if isinstance(lay.get("bg"), str) and re.fullmatch(r"#[0-9a-fA-F]{6}", lay["bg"]):
                        background = lay["bg"].lower()
                except (OSError, ValueError, TypeError):
                    pass  # the file can't be read for its size: shown at full screen, still editable
        elif piece.is_file():  # no kept original: the finished piece itself, as it is
            _, art["src"] = keep_file(piece.read_bytes(), piece.suffix)
        else:
            continue  # nothing left of it
        if piece.suffix.lower() in VIDEO_TYPES or (art["src"] and Path(art["src"]).suffix.lower() in VIDEO_TYPES):
            art["type"] = "video"
        texts = [text_object(w) for w in item.get("widgets", []) if isinstance(w, dict)]
        scenes.append({"id": f"s{n}", "name": Path(item["file"]).stem, "seconds": max(1, int(item.get("seconds") or DEFAULT_SECONDS)),
                       "background": background,
                       "layers": [{"id": "art", "name": "Art", "visible": True, "locked": False, "opacity": 1.0, "objects": [art]},
                                  {"id": "text", "name": "Text", "visible": True, "locked": False, "opacity": 1.0, "objects": texts}]})
    overlay = [text_object(w) for w in data.get("overlay", []) if isinstance(w, dict)]
    rest = {k: v for k, v in data.items() if k not in ("items", "overlay", "version", "seconds", "global", "scenes")}
    return {"version": 2, "seconds": data.get("seconds", DEFAULT_SECONDS),
            "global": {"layers": [{"id": "text", "name": "Text", "visible": True, "locked": False, "opacity": 1.0, "objects": overlay}]},
            "scenes": scenes, **rest}


def text_object(w):
    """A version 1 text (an entry of `widgets` or `overlay`) as a text object: its keys plus the common ones."""
    return {"name": str(w.get("text", "")).split("\n")[0][:40] or "Text", "visible": True, "locked": False, "opacity": 1.0,
            "w": 0, "h": 0, "rotate": 0, "flip": False, **w, "type": "text", "id": str(w.get("id", "w"))}


def read_document():
    """playlist.json as a version 2 document, converting a version 1 file (kept as playlist.v1.json) the first time."""
    with playlist_lock:
        data = read_json(PLAYLIST)
        if data.get("version") == 2:
            doc = data
        elif not data:
            doc = {"version": 2, "seconds": DEFAULT_SECONDS, "global": {"layers": []}, "scenes": []}
        else:
            doc = convert_v1(data)
            backup = PLAYLIST.with_name("playlist.v1.json")
            if not backup.exists():
                shutil.copy2(PLAYLIST, backup)
            write_atomic(PLAYLIST, doc)
        doc.setdefault("seconds", DEFAULT_SECONDS)
        doc.setdefault("global", {"layers": []})
        doc.setdefault("scenes", [])
        note_playlist(doc)
        return doc


def note_playlist(doc):
    """Notes the use of every file in playlist/sources/ and inputs/ that the document names."""
    for folder, names in named_files(doc, ("sources", "inputs")).items():
        note_used(PLAYLIST_DIR / folder / name for name in names)


def check_scale(o):
    """A copy's `scale` clamped to what both renderers draw (docs/widgets.md, A copy). One that's missing or not a
    positive number is left: the renderers then take it from the stored width."""
    s = o.get("scale")
    if isinstance(s, (int, float)) and not isinstance(s, bool) and math.isfinite(s) and s > 0:
        o["scale"] = min(16, max(0.01, s))


def check_copy(o):
    """The parts of a copy (an object of type "widget") checked in any document: its values must be strings."""
    if "inputs" in o:
        values = o["inputs"]
        o["inputs"] = {k: v for k, v in values.items() if isinstance(v, str)} if isinstance(values, dict) else {}
    check_scale(o)


def check_document(doc):
    """The page's document, checked for shape only: the renderers skip what they don't understand."""
    if not isinstance(doc, dict) or doc.get("version") != 2:
        raise ValueError("expected a version 2 playlist document")
    if not isinstance(doc.get("scenes"), list) or not all(isinstance(s, dict) for s in doc["scenes"]):
        raise ValueError("scenes must be a list of objects")
    if not isinstance(doc.get("global"), dict):
        raise ValueError("global must be an object")
    for holder in [doc["global"], *doc["scenes"]]:
        layers = holder.setdefault("layers", [])
        if not isinstance(layers, list) or not all(isinstance(l, dict) and isinstance(l.get("objects", []), list) for l in layers):
            raise ValueError("layers must be a list of objects, each with its objects")
        for layer in layers:
            layer.setdefault("objects", [])
            if not all(isinstance(o, dict) for o in layer["objects"]):
                raise ValueError("a layer's objects must be objects")
            for o in layer["objects"]:
                if o.get("type") == "widget":
                    check_copy(o)  # a copy of a widget that isn't here is kept: it shows as missing
    for s in doc["scenes"]:
        try:
            s["seconds"] = max(1, min(86400, int(s.get("seconds") or DEFAULT_SECONDS)))
        except (TypeError, ValueError):
            raise ValueError("a scene's seconds must be a number") from None
    return doc


def save_document(doc):
    """The page's whole document, saved as it is (unknown keys and all). Then the files in sources/ and inputs/
    that nothing has named for a while go (tidy_folder)."""
    doc = check_document(doc)
    with playlist_lock:
        write_atomic(PLAYLIST, doc)
        named = named_files(doc, ("sources", "inputs"))
        note_playlist(doc)
        tidy_folder(sources_dir(), named["sources"], KEPT_NAME)
        tidy_folder(inputs_dir(), named["inputs"], KEPT_NAME)
    return {"ok": True}


def media_info(src):
    """What the page needs to draw a kept media file: its frames' urls, size, timing and `pixel` factor."""
    info = {**src.info, "src": src.src, "animated": bool(src.video) or len(src.info["durations"]) > 1,
            "pixel": src.info["factor"]}
    info["type"] = "video" if src.video else "image"
    return info


IMAGE_FORMATS = {".png": "PNG", ".apng": "PNG", ".gif": "GIF", ".webp": "WEBP", ".jpg": "JPEG", ".jpeg": "JPEG",
                 ".bmp": "BMP"}
UPLOAD_TYPES = VIDEO_TYPES | set(IMAGE_FORMATS) | {".aseprite", ".ase"}  # what a scene takes
# What a file coming into a widget (an upload, a copy, an import) or an input picture may be. A scene's own uploads
# keep the studio's old rules: the file must open, nothing more.
MAX_SIDE = 8192  # pixels, across or down: more than a 1600 x 720 panel ever needs
MAX_DECODED = 1 << 30  # bytes of RGBA, all frames together
# What the player itself opens (media.rs): a file already in a widget's folder, put there by hand, is drawn on the
# cooler up to these, so the studio opens it too.
PLAYER_SIDE = 16384
PLAYER_DECODED = 2 << 30


def upload(body, name, to=None):
    """A dropped or picked file: kept in sources/ by content hash (or, `to` "inputs", in inputs/: a picture for a
    copy's image input), opened, and described for the page. A scene takes what it always took; an input picture
    is drawn inside a widget, so it follows a widget's rules (input_picture)."""
    if not body:
        raise ValueError("the file is empty")
    if to not in (None, "sources", "inputs"):
        raise ValueError("`to` must be inputs")
    suffix = Path(name).suffix.lower()
    if suffix not in UPLOAD_TYPES:
        raise ValueError(f"{suffix or 'that'} isn't a kind of file the studio can open")
    if to == "inputs":
        body, suffix = input_picture(body, suffix)
    folder = to or "sources"
    path = PLAYLIST_DIR / folder / f"{hashlib.sha1(body).hexdigest()[:12]}{suffix}"
    fresh = not path.is_file()
    kept, src_name = keep_file(body, suffix, folder)
    try:
        return open_media(kept, src_name, name)
    except BaseException:  # SystemExit too: wv.aseprite exits when Aseprite is missing
        if fresh:  # just written and unreadable: not worth keeping
            kept.unlink(missing_ok=True)
        raise


def open_kept(src_name, name=None):
    """Opens sources/<file> (cached) and describes it."""
    file = Path(src_name).name
    if not PART.fullmatch(file):
        raise ValueError("bad source name")
    path = sources_dir() / file
    if not inside(path, sources_dir()):
        raise FileNotFoundError(f"{src_name} isn't in the playlist folder")
    return open_media(path, source_name(path), name)


def open_media(path, ref, name=None):
    """Opens a media file (cached, one Source per file) and describes it for the page, with `src` its ref."""
    return {**media_info(open_source(path, ref, name)), "src": ref}


class Seen:
    """A file opened lately, by its content id: enough to open it again (`opened`)."""

    def __init__(self, src, ref):
        self.path, self.ref, self.name, self.preview = src.path, ref, src.name, src.preview


def open_source(path, ref, name=None):
    """The Source of a media file: the cached one while the file is unchanged, else opened now. Requests for one
    file wait for a single decode, and every decode waits for room (decode_room)."""
    key = use_key(path)
    while True:
        with sources_lock:
            src = sources.get(key)
            if src and src.stamp == file_stamp(path):
                src.used = time.monotonic()
                sources.move_to_end(key)
                return src
            lock = opening.get(key)
            if lock is None:
                lock = opening[key] = threading.Lock()
                lock.acquire()
                break
        with lock:  # another request is opening it: then look again
            pass
    try:
        if not ref.startswith("sources/"):  # widget media and input pictures may come from anywhere: headers first
            check_media_file(path, PLAYER_SIDE, PLAYER_DECODED)
        with decode_room(decode_cost(path)):
            src = Source(path, name or path.name, ref)
        with sources_lock:
            sources[key] = src
            sources.move_to_end(key)
            opened[src.id] = Seen(src, ref)
            opened.move_to_end(src.id)
            while len(opened) > OPENED_COUNT:
                opened.popitem(last=False)
            trim_sources()
    finally:
        with sources_lock:
            del opening[key]
        lock.release()
    return src


def trim_sources():
    """Lets the least recently used sources go (under sources_lock) while there are more than MEDIA_COUNT or their
    frames pass MEDIA_BYTES, except that one used in the last MEDIA_GRACE seconds, whose frames the page may still be
    fetching, goes only while they pass MEDIA_HARD. The newest always stays."""
    total, now = sum(s.bytes for s in sources.values()), time.monotonic()
    while len(sources) > 1:
        oldest = next(iter(sources.values()))  # least recently used: if it's recent, they all are
        if not (len(sources) > MEDIA_COUNT or total > MEDIA_HARD
                or (total > MEDIA_BYTES and now - oldest.used > MEDIA_GRACE)):
            break
        sources.popitem(last=False)
        total -= oldest.bytes


def decode_cost(path):
    """About how many bytes opening a media file takes while it's decoded: an image's frames as RGBA, from its
    headers; a video's kept JPEGs. 0 when its headers can't be read (opening it fails anyway)."""
    if path.suffix.lower() in VIDEO_TYPES:
        return VIDEO_COST
    try:
        with warnings.catch_warnings():
            warnings.simplefilter("ignore")
            with Image.open(path) as im:
                return getattr(im, "n_frames", 1) * im.width * im.height * 4
    except Exception:
        return 0


@contextlib.contextmanager
def decode_room(cost):
    """Waits until `cost` bytes of decoding fit in DECODE_BYTES with what's being decoded now, and holds them."""
    global decoding_bytes
    cost = min(cost, DECODE_BYTES)
    with decoding:
        decoding.wait_for(lambda: decoding_bytes + cost <= DECODE_BYTES)
        decoding_bytes += cost
    try:
        yield
    finally:
        with decoding:
            decoding_bytes -= cost
            decoding.notify_all()


def source_by_id(sid):
    """The Source with this content id, for /api/frame, or None. One that was let go (trim_sources) is opened again
    while its file still holds the same content: the page may still be fetching the frames of a file it was told
    about a moment ago."""
    with sources_lock:
        for key, src in sources.items():
            if src.id == sid:
                src.used = time.monotonic()
                sources.move_to_end(key)
                return src
        seen = opened.get(sid)
    if seen is None or not inside(seen.path, seen.path.parent):  # gone, or not a plain file any more
        return None
    try:
        src = open_source(seen.path, seen.ref, seen.name)
    except KeyboardInterrupt:
        raise
    except BaseException:  # unreadable now (SystemExit too: wv.aseprite exits when Aseprite is missing)
        return None
    return src if src.id == sid else None  # changed since: the page's frames are of what it was


def video_preview(sid):
    """The browser's copy of the video with this content id, for /api/video, or None. It's a file of its own, so
    it's served from there even after its Source was let go."""
    with sources_lock:
        for key, src in sources.items():
            if src.id == sid:
                src.used = time.monotonic()
                sources.move_to_end(key)
                return src.preview
        seen = opened.get(sid)
    return seen.preview if seen and seen.preview and seen.preview.is_file() else None


def export_scene(req):
    """A PNG of a scene, rendered by the player (`wvplay render`), into out/."""
    exe = PLAYER.with_name("wvplay.exe")
    if not exe.exists():
        raise ValueError("the player isn't built yet: run `cargo build --release` in the player folder")
    index, name = int(req["scene"]), safe_name(req.get("name") or f"scene{req['scene']}")
    wv.OUT.mkdir(exist_ok=True)
    out = wv.OUT / f"{name}.png"
    cmd = [str(exe), "render", str(PLAYLIST), str(index), str(out)]
    if "time" in req:
        cmd += ["--time", str(int(req["time"]))]
    done = subprocess.run(cmd, capture_output=True, text=True, errors="replace", timeout=120, creationflags=NO_WINDOW)
    if done.returncode or not out.exists():
        raise ValueError(f"the player couldn't render it: {(done.stderr or done.stdout).strip()[-300:]}")
    return {"files": [{"name": out.name, "bytes": out.stat().st_size}], "folder": str(wv.OUT)}


def safe_name(name):
    return re.sub(r"[^\w .()-]", "_", str(name)).strip(" .") or "untitled"

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
    # whether the player running draws widgets: it says so at /info; an older one doesn't know /info (None: none
    # running, or it didn't answer, so there's nothing to tell yet)
    widgets = None
    if running:
        info = player_api("/info")
        if isinstance(info, dict) and info.get("widgets") == 1:
            widgets = True
        elif not (isinstance(info, dict) and info.get("player") is False):
            widgets = False
    return {"running": running, "log": last, "built": PLAYER.exists(), "autostart": autostart(), "rivals": rivals(),
            "widgets": widgets}


player_widgets = None  # what player_features() found: /api/config's player_widgets


def player_features():
    """Whether the built player (the wvplay.exe beside PLAYER, which Play and Export PNG use) draws widgets: True
    when `wvplay.exe features` prints `widgets 1`, False when it runs and doesn't (built before widgets), None when
    it isn't built or gives no answer. Run when the studio starts; /api/config tells the page."""
    global player_widgets
    exe, found = PLAYER.with_name("wvplay.exe"), None
    if exe.is_file():
        try:
            out = subprocess.run([str(exe), "features"], capture_output=True, text=True, errors="replace", timeout=5,
                                 stdin=subprocess.DEVNULL, creationflags=NO_WINDOW).stdout
            found = "widgets 1" in (line.strip() for line in out.splitlines())
        except (OSError, subprocess.SubprocessError):
            found = None
    player_widgets = found
    return found


def start_player():
    if not PLAYER.exists():
        raise ValueError("the player isn't built yet: run `cargo build --release` in the player folder")
    if not read_document()["scenes"]:
        raise ValueError("the playlist is empty: add a scene first")
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


# --- Fonts, for text --------------------------------------------------------------------

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
    """The font at `path` if font_list lists it, or if it's one a widget carries in its fonts\\ (a text pasted out
    of a widget names it there): no other file is served."""
    font_list()
    found = fonts_seen[2].get(os.path.normcase(path))
    if found is None and carried_font(path):
        found = Path(path)
    return found


def carried_font(path):
    """Whether `path` is a font a widget carries: widgets/<id>/fonts/<name>, a file of its own there."""
    if not isinstance(path, str) or not os.path.isabs(path):
        return False
    p = Path(path)
    wid = p.parent.parent.name
    return (p.parent.name.lower() == "fonts" and font_name_ok(p.name) and WIDGET_ID.fullmatch(wid) is not None
            and wid not in RESERVED and same_file(p.parent.parent.parent, widgets_dir())
            and real_dir(widgets_dir() / wid) and inside(p, widgets_dir() / wid))


# --- Reusable widgets (docs/widgets.md) ---------------------------------------------------------------------------
#
# widgets/<id>/widget.json, beside the playlist's folder: small canvases of their own that scenes, the global layer
# and other widgets place copies of. The page edits a widget as it edits a scene, and saves each one on its own.
# The studio can be reached over Tailscale and widgets are shared as zips from anywhere, so every id, path and file
# name that arrives is checked before it's used in a path, every file is checked to be what its name says, and
# nothing is written anywhere but inside playlist/ and widgets/ (and .cache/studio/ for a moment).

WIDGET_ID = re.compile(r"[a-z0-9](?:[a-z0-9-]{0,38}[a-z0-9])?")
INPUT_ID = re.compile(r"[a-z][a-z0-9_]{0,31}")
PART = re.compile(r"[A-Za-z0-9_](?:[A-Za-z0-9_.-]{0,98}[A-Za-z0-9_-])?")  # one part of a relative path
FONT_NAME = re.compile(r"[^\\/:*?\"<>|\x00-\x1f]{1,100}\.(?:ttf|otf)", re.I)
RESERVED = {"con", "prn", "aux", "nul", *(f"com{i}" for i in range(1, 10)), *(f"lpt{i}" for i in range(1, 10))}
MEDIA_TYPES = {".png", ".apng", ".gif", ".webp", ".jpg", ".jpeg", ".bmp", ".mp4", ".m4v", ".mov", ".webm", ".mkv"}
ASEPRITE_TYPES = {".aseprite", ".ase"}  # turned into PNG on the way into a widget, never kept
IMPORT_TYPES = MEDIA_TYPES  # the media a shared widget may hold; an Aseprite file or an .avi refuses the whole import
COLOR = re.compile(r"#(?:[0-9a-fA-F]{3}|[0-9a-fA-F]{6})")
REF = re.compile(r"\{\$[a-z][a-z0-9_]{0,31}\}")  # a whole value that names one of the widget's inputs: {$id}
INPUT_TYPES = ("text", "color", "image")
MAX_INPUTS = 32
MAX_OBJECTS = 2000
MAX_WIDGET_JSON = 1 << 20  # bytes of widget.json as it's written: the listing and the player skip a bigger one
TRASH_NAME = re.compile(r"(?P<id>.+)-(?P<when>\d{8}-\d{6})(?:-(?P<n>\d+))?")  # in .deleted: <id>-<yyyymmdd-hhmmss>[-n]
DEFAULT_LAYER = {"id": "l1", "name": "Layer 1", "visible": True, "locked": False, "opacity": 1, "objects": []}
EXAMPLES_READ_ONLY = "The examples are read-only: duplicate it to edit."
AVI_REFUSED = "Widgets take MP4, MOV, WebM or MKV video: convert this one first."
NEEDS_ASEPRITE = "Turning an Aseprite file into a PNG needs Aseprite (set ASEPRITE)."
FONT_MAGIC = (b"\x00\x01\x00\x00", b"true", b"OTTO")


class Refused(Exception):
    """A widget request that can't be done, with the status that says why (404, 409, 413) and anything more the
    answer carries (a 409's current stamp). Every other ValueError is a 400."""

    def __init__(self, status, message, **extra):
        super().__init__(message)
        self.status, self.extra = status, extra


# --- Names and paths

def reserved(name):
    """Whether Windows reserves a name: its stem, before the first dot, in any letter case (con, NUL.png)."""
    return name.split(".")[0].lower() in RESERVED


def widget_folder(wid):
    """widgets/<id> for an id that is one (docs/widgets.md, Ids); anything else is a ValueError, before any path is
    made from it."""
    if not isinstance(wid, str) or not WIDGET_ID.fullmatch(wid) or wid in RESERVED:
        raise ValueError(f"{str(wid)[:60]!r} isn't a widget id.")
    return widgets_dir() / wid


def installed_folder(wid):
    """widget_folder(id) for a widget that's here: a plain folder, not a link or junction to somewhere else (the
    listing calls that a problem, and nothing is read through it). Else 404."""
    folder = widget_folder(wid)
    if not real_dir(folder):
        raise Refused(404, f"There's no widget {wid}.")
    return folder


def is_example(wid):
    """The shipped examples are tracked in git: the studio never writes into them."""
    return wid.startswith("example-")


def rel_path_ok(p):
    """A path relative to a widget's folder (docs/widgets.md, Paths): 1 to 4 parts joined by /, each a PART, none
    reserved. The same rule as the player's path_ok."""
    if not isinstance(p, str):
        return False
    parts = p.split("/")
    return 1 <= len(parts) <= 4 and all(PART.fullmatch(x) and x not in (".", "..") and not reserved(x) for x in parts)


def widget_path_ok(p):
    """A path a widget may name, compared as both renderers compare whole values (spaces trimmed). Its first part
    can't be `widgets`: that's the page's own name for a file (widgets/<id>/media/...), saved by mistake."""
    p = p.strip(" ")
    return rel_path_ok(p) and p.split("/")[0].lower() != "widgets"


def font_name_ok(n):
    """A font's file name (docs/widgets.md, A font name): the same rule in the player, here, and in imports."""
    return (isinstance(n, str) and FONT_NAME.fullmatch(n) is not None and n[0] not in " ." and n[-1] not in " ."
            and not reserved(n))


def check_color(s):
    """A colour (docs/widgets.md, widget.json) as lowercase #rrggbb, or None: the same rule as the player's."""
    if not isinstance(s, str):
        return None
    s = s.strip(" ")
    if not COLOR.fullmatch(s):
        return None
    s = s.lower()
    return s if len(s) == 7 else "#" + "".join(c * 2 for c in s[1:])


def is_ref(v):
    """Whether a whole value is {$id}, naming an input."""
    return isinstance(v, str) and REF.fullmatch(v.strip(" ")) is not None


def same_file(a, b):
    try:
        return os.path.samefile(a, b)
    except (OSError, ValueError):  # ValueError: a NUL in a path from a query string
        return False


def real_dir(path):
    """Whether `path` is a folder itself: not a link or junction that leads somewhere else."""
    p = Path(path)
    try:
        return p.is_dir() and not p.is_symlink() and not p.is_junction()
    except OSError:
        return False


def inside(path, folder):
    """Whether `path` is a regular file inside `folder`, and not a link: after every link and junction on the way
    is followed, it's still in there. Every file served or read from a widget's folder passes this."""
    try:
        p = Path(path)
        if p.is_symlink():
            return False
        real, base = p.resolve(), Path(folder).resolve()
        return real != base and real.is_relative_to(base) and stat.S_ISREG(os.stat(real).st_mode)
    except (OSError, RuntimeError, ValueError):
        return False


def dest_in(folder, rel):
    """Where `rel` (a checked path) goes inside a widget's `folder`. Refused when a link or junction on the way
    would lead the write somewhere else."""
    if not real_dir(folder):
        raise Refused(404, f"widgets\\{folder.name} isn't there.")
    parts, here = rel.split("/"), folder
    for part in parts[:-1]:
        here = here / part
        if os.path.lexists(here) and not real_dir(here):
            raise ValueError(f"{rel} can't be written: {part} in it isn't a plain folder.")
    return here / parts[-1]


def slug(text):
    """Lowercase, accents removed, each run of anything but a-z and 0-9 one -, and no - at the ends."""
    s = unicodedata.normalize("NFKD", str(text).lower())
    s = "".join(c for c in s if not unicodedata.combining(c))
    return re.sub(r"[^a-z0-9]+", "-", s).strip("-")


def make_id(name):
    """An id made from a name (docs/widgets.md, Ids): its slug cut to 40; `widget` when that's empty, and with
    `-widget` added after a name Windows reserves."""
    s = slug(name)[:40].strip("-")
    if not s:
        return "widget"
    return f"{s}-widget" if s in RESERVED else s


def numbered(stem, n):
    """`stem` with -n added, the stem cut so the id stays within 40 characters."""
    suffix = f"-{n}"
    return stem[:40 - len(suffix)].rstrip("-") + suffix


def taken_ids():
    """What a new widget's id can't be (casefolded, as Windows compares folder names): every folder in widgets/,
    and the id of every widget in .deleted, so a restore can't clash with a widget made since."""
    taken = set()
    for folder, trash in ((widgets_dir(), False), (trash_dir(), True)):
        try:
            names = [e.name for e in os.scandir(folder)]
        except OSError:
            continue
        for name in names:
            m = TRASH_NAME.fullmatch(name) if trash else None
            taken.add((m["id"] if m else name).casefold())
    return taken


def fresh_id(name, taken=None):
    """A new widget's id: made from its name, then -2, -3 and so on while that's taken. Never an example's: the
    studio never writes into a folder named example-..., so a widget named "Example clock" gets `clock`."""
    stem = make_id(name)
    while is_example(stem):
        stem = make_id(stem[len("example-"):])
    taken = taken_ids() if taken is None else taken
    wid, n = stem, 1
    while wid.casefold() in taken:
        n += 1
        wid = numbered(stem, n)
    return wid


# --- Reading widgets

def newer(w):
    """Whether a widget.json was made by a newer studio: its `version` is above 1."""
    v = w.get("version")
    return isinstance(v, (int, float)) and not isinstance(v, bool) and v > 1


def read_widget(folder):
    """A widget folder's widget.json, as it is: (content, stamp, problem). The stamp is taken just before reading,
    so a change that lands during the read shows up as one next time. One over 1 MB is a problem: the player skips
    it too, so the page never draws it."""
    path = folder / "widget.json"
    try:
        stamp = file_stamp(path)
        if not inside(path, folder):
            return None, None, "Its widget.json is a link, or not a file."
        with open(path, "rb") as f:
            data = f.read(MAX_WIDGET_JSON + 1)  # never more than that
        if len(data) > MAX_WIDGET_JSON:
            return None, None, "Its widget.json is too big: more than 1 MB."
        text = data.decode("utf-8")
    except FileNotFoundError:
        return None, None, "There's no widget.json in it."
    except UnicodeDecodeError:
        return None, None, "Its widget.json isn't UTF-8 text."
    except OSError as e:
        return None, None, f"Its widget.json can't be read ({e.strerror or e})."
    try:
        w = strict_json(text)
    except ValueError as e:
        return None, None, f"Its widget.json isn't valid JSON ({e})."
    if not isinstance(w, dict):
        return None, None, "Its widget.json doesn't hold an object."
    return w, stamp, None


def widget_name(folder, wid):
    """A widget's name for a message, or its id."""
    w, _, _ = read_widget(folder)
    name = w.get("name") if w else None
    return name.strip()[:80] if isinstance(name, str) and name.strip() else wid


def objects_of(holder):
    """Every object in a widget's (or any holder's) layers, as found: objects only."""
    layers = holder.get("layers") if isinstance(holder, dict) else None
    for layer in layers if isinstance(layers, list) else []:
        objects = layer.get("objects") if isinstance(layer, dict) else None
        for o in objects if isinstance(objects, list) else []:
            if isinstance(o, dict):
                yield o


def input_types(inputs):
    """{input id: type} for a widget's inputs, read as the player reads them: the first 32 with a good id and type."""
    types = {}
    for i in inputs if isinstance(inputs, list) else []:
        if len(types) == MAX_INPUTS:
            break
        if (isinstance(i, dict) and isinstance(i.get("id"), str) and INPUT_ID.fullmatch(i["id"])
                and i["id"] not in types and i.get("type") in INPUT_TYPES):
            types[i["id"]] = i["type"]
    return types


def installed_types(wid):
    """The input types of the widget with this id here, or None when there's no such widget to read."""
    try:
        folder = widget_folder(wid)
    except ValueError:
        return None
    w, _, _ = read_widget(folder) if real_dir(folder) else (None, None, None)
    return input_types(w.get("inputs")) if w else None


def copies_in(w):
    """The ids a widget places copies of, in order, each once."""
    return list(dict.fromkeys(o["widget"] for o in objects_of(w) if o.get("type") == "widget" and isinstance(o.get("widget"), str)))


def media_files(folder, w):
    """The files directly in a widget's media/ that it names."""
    return [folder / "media" / name for name in named_files(w, ("media",))["media"]]


def text_fonts(w):
    """The font names a widget's texts use (empty means Segoe UI, and isn't one)."""
    return {o["font"] for o in objects_of(w) if o.get("type") == "text" and isinstance(o.get("font"), str) and o["font"]}


# --- Fonts: looked up by file name, read in pure Python

def font_tables(head, size):
    """{tag: (offset, length)} from a font's first bytes (12 + 16 × 64 is enough) and its size. ValueError unless
    it's a well-formed TrueType or OpenType font: every table inside the file. Nothing else parses a widget's font
    before this has passed."""
    if len(head) < 12 or head[:4] not in FONT_MAGIC:
        raise ValueError("it isn't a TrueType or OpenType font")
    count = struct.unpack(">H", head[4:6])[0]
    if not 1 <= count <= 64 or 12 + 16 * count > min(len(head), size):
        raise ValueError("it isn't a well-formed font (its table directory is wrong)")
    tables = {}
    for i in range(count):
        tag, _, offset, length = struct.unpack(">4sIII", head[12 + 16 * i:28 + 16 * i])
        if offset + length > size:
            raise ValueError("it isn't a well-formed font (a table runs past its end)")
        tables[tag] = (offset, length)
    return tables


def check_font_bytes(data):
    """Refuses (ValueError) bytes that aren't a well-formed font."""
    font_tables(data[:12 + 16 * 64], len(data))


def font_names(font):
    """(family, style) of a font, the bytes or a file: name IDs 16 and 17 (else 1 and 2) from its `name` table,
    Windows Unicode records first. Pure Python, so a font from a widget or a zip is never opened by FreeType."""
    if isinstance(font, (bytes, bytearray)):
        return names_in(lambda at, n: bytes(font[at:at + n]), len(font))
    with open(font, "rb") as f:
        def read(at, n):
            f.seek(at)
            return f.read(n)
        return names_in(read, os.fstat(f.fileno()).st_size)


def names_in(read, size):
    """font_names, from `read(offset, n)` and the font's size: every offset is checked against what was read."""
    at, length = font_tables(read(0, 12 + 16 * 64), size).get(b"name", (0, 0))
    table = read(at, min(length, 1 << 20))
    best = {}  # name id -> (rank, text): the lowest rank wins
    if len(table) >= 6:
        count, store = struct.unpack(">HH", table[2:6])
        for i in range(count):
            record = table[6 + 12 * i:18 + 12 * i]
            if len(record) < 12:
                break
            platform, encoding, language, nid, n, offset = struct.unpack(">6H", record)
            if nid not in (1, 2, 16, 17):
                continue
            if platform == 3 and encoding in (0, 1, 10):
                rank, codec = (0 if language == 0x409 else 1), "utf-16-be"
            elif platform == 0:
                rank, codec = 2, "utf-16-be"
            elif platform == 1 and encoding == 0:
                rank, codec = 3, "mac_roman"
            else:
                continue
            text = table[store + offset:store + offset + n].decode(codec, "replace").replace("\x00", "").strip()
            if text and rank < best.get(nid, (9, ""))[0]:
                best[nid] = (rank, text)
    pick = lambda *ids: next((best[i][1] for i in ids if i in best), "")
    return pick(16, 1), pick(17, 2)


font_names_seen = {}  # (normcased path, stamp) -> (family, style)


def font_names_of(path):
    """font_names of a font file, kept until the file changes; ("", "") when it can't be read."""
    try:
        key = (use_key(path), file_stamp(path))
    except OSError:
        return "", ""
    if key not in font_names_seen:
        try:
            font_names_seen[key] = font_names(path)
        except (OSError, ValueError, struct.error):
            font_names_seen[key] = ("", "")
    return font_names_seen[key]


def font_folders(folder=None):
    """Where a widget's font name is looked up, in order (docs/widgets.md, Fonts): the widget's own fonts/,
    Windows' fonts, the fonts installed for this user, and the project's (the folder holding widgets/, then fonts)."""
    return ([folder / "fonts"] if folder else []) + [FONT_DIRS[0], FONT_DIRS[1], widgets_dir().parent / "fonts"]


def find_font(folder, name):
    """The file a widget in `folder` (None: no widget's own) means by a font name, or None when this PC has none."""
    if not font_name_ok(name):
        return None
    for d in font_folders(folder):
        p = d / name
        if inside(p, folder) if folder and d == folder / "fonts" else p.is_file():
            return p
    return None


def widget_font(wid, name):
    """GET /api/font?widget=<id>&name=<file>: the file that widget's font name means, found as the player finds it."""
    folder = installed_folder(wid)
    if not font_name_ok(name):
        raise ValueError(f"{name[:100]!r} isn't a font file's name.")
    path = find_font(folder, name)
    if path is None:
        raise Refused(404, f"{name} isn't on this PC.")
    return path


def is_windows_font(path):
    return os.path.normcase(str(Path(path).parent)) == os.path.normcase(str(FONT_DIRS[0]))


installed_seen = (None, [])  # (the folders and their modified times, [(path, family, style)])


def installed_fonts():
    """[(path, family, style)] for every font the lookup finds outside a widget's own folder, read by font_names.
    Kept until one of the folders changes, as it does when a font is installed."""
    global installed_seen
    folders = font_folders()
    key = []
    for d in folders:
        try:
            key.append((str(d), d.stat().st_mtime_ns))
        except OSError:
            key.append((str(d), None))
    if key != installed_seen[0]:
        found = []
        for d in folders:
            try:
                entries = sorted(os.scandir(d), key=lambda e: e.name.lower())
            except OSError:
                continue
            for e in entries:
                if font_name_ok(e.name) and e.is_file():
                    family, style = font_names_of(Path(e.path))
                    if family:
                        found.append((Path(e.path), family, style))
        installed_seen = (key, found)
    return installed_seen[1]


def installed_named(family, style):
    """The file names of the fonts on this PC with this family and style: one may be the missing font under
    another name (Orbitron[wght].ttf for Orbitron-Bold.ttf)."""
    want = (family.casefold(), style.casefold())
    return sorted({p.name for p, f, s in installed_fonts() if (f.casefold(), s.casefold()) == want}, key=str.lower)


def resolved_fonts(folder, w):
    """Each font name the widget's texts use, as the lookup finds it: {path, stamp, family, style}, or, when it
    isn't on this PC, {path: null, family, style, installed} with the family and style the widget remembers."""
    remembered = w.get("fonts") if isinstance(w.get("fonts"), dict) else {}
    out = {}
    for name in sorted(text_fonts(w)):
        found = find_font(folder, name)
        if found:
            family, style = font_names_of(found)
            out[name] = {"path": str(found), "stamp": file_stamp(found), "family": family, "style": style}
            continue
        entry = remembered.get(name) if isinstance(remembered.get(name), dict) else {}
        family = entry.get("family") if isinstance(entry.get("family"), str) else ""
        style = entry.get("style") if isinstance(entry.get("style"), str) else ""
        out[name] = {"path": None, "family": family, "style": style,
                     "installed": installed_named(family, style) if family else []}
    return out


def own_fonts(folder):
    """The fonts a widget carries in its fonts/: [{name, family, style}]."""
    out = []
    try:
        entries = sorted(os.scandir(folder / "fonts"), key=lambda e: e.name.lower())
    except OSError:
        return out
    for e in entries:
        if font_name_ok(e.name) and inside(e.path, folder):
            family, style = font_names_of(Path(e.path))
            if family:
                out.append({"name": e.name, "family": family, "style": style})
    return out


def fonts_entry(w, folder):
    """A widget's `fonts`, kept up to date: the family and style of each font its texts use, from the file the
    lookup finds; a name not found keeps the entry it had; names no text uses go."""
    old = w.get("fonts") if isinstance(w.get("fonts"), dict) else {}
    fonts = {}
    for name in sorted(n for n in text_fonts(w) if font_name_ok(n)):
        found = find_font(folder, name)
        family, style = font_names_of(found) if found else ("", "")
        if family:
            fonts[name] = {"family": family, "style": style}
        elif isinstance(old.get(name), dict):
            fonts[name] = old[name]
    return fonts


# --- Listing

def list_widgets():
    """GET /api/widgets: every widget in widgets/, as it is on disk, with what the page needs to draw it; the
    folders that can't be widgets, with why; and the widgets in .deleted that can be restored."""
    folder = widgets_dir()
    out = {"folder": str(folder.absolute()), "widgets": [], "problems": [], "deleted": []}
    with playlist_lock:  # each widget.json as one save left it
        try:
            entries = sorted(os.scandir(folder), key=lambda e: e.name)
        except OSError:
            entries = []
        for e in entries:
            if e.name.startswith(".") or not e.is_dir():
                continue  # the studio's own (.deleted, .import-...), and files
            path = Path(e.path)
            if not WIDGET_ID.fullmatch(e.name) or e.name in RESERVED:
                out["problems"].append({"folder": e.name, "problem": f"The folder name `{e.name}` isn't a widget id: "
                                        f"rename it to `{make_id(e.name)}`."})
                continue
            if not real_dir(path):
                out["problems"].append({"folder": e.name, "problem": "It's a link to another folder: a widget's "
                                        "folder must be a plain folder in widgets\\."})
                continue
            w, stamp, problem = read_widget(path)
            if problem:
                out["problems"].append({"folder": e.name, "problem": problem})
                continue
            out["widgets"].append({"id": e.name, "widget": w, "stamp": stamp,
                                   "read_only": "example" if is_example(e.name) else "newer" if newer(w) else None})
            note_used(media_files(path, w))
        out["deleted"] = deleted_widgets()
    for entry in out["widgets"]:  # fonts are looked up outside the lock: the first lookup reads every font's name
        path = folder / entry["id"]
        entry["resolved_fonts"], entry["own_fonts"] = resolved_fonts(path, entry["widget"]), own_fonts(path)
    return out


def deleted_widgets():
    """The widgets in .deleted, newest first: [{trash, id, name, when}], `when` in seconds since 1970."""
    out = []
    try:
        entries = list(os.scandir(trash_dir()))
    except OSError:
        return out
    for e in entries:
        m = TRASH_NAME.fullmatch(e.name)
        if not m or not WIDGET_ID.fullmatch(m["id"]) or m["id"] in RESERVED or not real_dir(e.path):
            continue
        try:
            when = time.mktime(time.strptime(m["when"], "%Y%m%d-%H%M%S"))
        except (ValueError, OverflowError):
            continue
        out.append({"trash": e.name, "id": m["id"], "name": widget_name(Path(e.path), m["id"]), "when": when})
    out.sort(key=lambda d: (d["when"], d["trash"]), reverse=True)
    return out


def reveal_folder(req):
    """The folder POST /api/reveal opens: out/ as ever; with {"widget": id}, that widget's folder; with
    {"widgets": true}, widgets/ itself, where a folder the listing gives as a problem is (its name may not be one the
    studio can ask for: `CPU Badge`, or a link)."""
    req = req if isinstance(req, dict) else {}
    if req.get("widgets") is True:
        if not real_dir(widgets_dir()):
            raise ValueError("There's no widgets folder.")
        return widgets_dir()
    wid = req.get("widget")
    if wid is None:
        return wv.OUT
    folder = widget_folder(wid)
    if not real_dir(folder):
        raise ValueError(f"There's no widget {wid}.")
    return folder


# --- Checking a widget before it's written

def whole_number(v, default, top):
    """A container size: rounded half up and clamped to 1..top; `default` when it isn't a number."""
    if isinstance(v, bool) or not isinstance(v, (int, float)) or not math.isfinite(v):
        return default
    return min(top, max(1, math.floor(v + 0.5)))


def color_field(v, fallback):
    """A colour field as it's saved: {$id} kept, a colour as lowercase #rrggbb, anything else `fallback` (the
    renderer's own fallback, written out), so both renderers read it alike."""
    if is_ref(v):
        return v.strip(" ")
    return check_color(v) or fallback


def check_inputs(inputs):
    """A widget's inputs as saved: the first 32 with a good, unused id and a known type, each with a default its
    type takes (else that type's blank) and a name of at most 60 characters."""
    out, seen = [], set()
    for i in inputs if isinstance(inputs, list) else []:
        if len(out) == MAX_INPUTS:
            break
        if not (isinstance(i, dict) and isinstance(i.get("id"), str) and INPUT_ID.fullmatch(i["id"])
                and i["id"] not in seen and i.get("type") in INPUT_TYPES):
            continue
        seen.add(i["id"])
        i = dict(i)
        i["name"] = (i["name"][:60] if isinstance(i.get("name"), str) else "") or i["id"]
        default = i.get("default")
        if i["type"] == "text":
            i["default"] = default[:2000] if isinstance(default, str) else ""
        elif i["type"] == "color":
            i["default"] = color_field(default, "#ffffff")
        else:
            default = default if isinstance(default, str) else ""
            if default.strip(" ") and not (is_ref(default) or widget_path_ok(default)):
                raise ValueError(f"The picture input {i['name']}: {default!r} isn't a path inside the widget's folder.")
            i["default"] = default
        out.append(i)
    return out


def check_part(o, wid, types_of):
    """One object of a widget, checked and normalised in place (check_widget)."""
    kind, what = o.get("type"), f"{o.get('name') or o.get('id') or 'An object'}"
    if kind in ("image", "video"):
        src = o.get("src")
        if src not in (None, "") and not (isinstance(src, str) and (is_ref(src) or widget_path_ok(src))):
            raise ValueError(f"{what}: its file {src!r} isn't a path inside the widget's folder.")
        if "tint" in o:
            o["tint"] = color_field(o["tint"], None)
    elif kind == "text":
        font = o.get("font")
        if font not in (None, "") and not font_name_ok(font):
            raise ValueError(f"{what}: its font must be a font file's name, like bahnschrift.ttf, not {font!r}.")
        if "color" in o:
            o["color"] = color_field(o["color"], "#ffffff")
        if isinstance(o.get("outline"), dict) and "color" in o["outline"]:
            o["outline"]["color"] = color_field(o["outline"]["color"], "#000000")
    elif kind == "shape":
        fill = o.get("fill")
        if isinstance(fill, dict):
            if "color" in fill:
                fill["color"] = color_field(fill["color"], "#ffffff")
            for stop in fill.get("stops") if isinstance(fill.get("stops"), list) else []:
                if isinstance(stop, dict) and "color" in stop:
                    stop["color"] = color_field(stop["color"], "#ffffff")
        if isinstance(o.get("stroke"), dict) and "color" in o["stroke"]:
            o["stroke"]["color"] = color_field(o["stroke"]["color"], "#ffffff")
    elif kind == "widget":
        if wid is not None and o.get("widget") == wid:
            raise ValueError("A widget can't contain itself.")
        check_copy(o)
        types = types_of(o["widget"]) if isinstance(o.get("widget"), str) else None
        values = o.get("inputs")
        for k, v in list(values.items()) if types and isinstance(values, dict) else []:
            if types.get(k) == "color":
                c = color_field(v, None)
                if c is None:
                    del values[k]  # not a colour: the default applies, as both renderers would have it
                else:
                    values[k] = c
            elif types.get(k) == "image" and v.strip(" ") and not (is_ref(v) or widget_path_ok(v)):
                raise ValueError(f"{what}: its picture {v!r} isn't a path inside the widget's folder.")


def check_widget(w, wid=None, folder=None, types_of=None):
    """A widget as the page (or a zip) gives it, checked and normalised before it's written (docs/widgets.md,
    widget.json). A refused save loses an edit, so it refuses (ValueError) only what's unsafe or would be lost: a
    newer version, more than 2000 objects or 1 MB as it's written, a path outside the folder, a font that isn't a
    file name, a copy of itself. Everything else is normalised, so both renderers read the same thing: sizes,
    inputs, names, colours. `wid` is its id; `folder` is where its fonts are looked up to keep `fonts` up to date
    (None: left as it is); `types_of(id)` gives the input types of a widget it places, or None (default: the widgets
    here). Returns the widget to write; unknown keys are kept."""
    if not isinstance(w, dict):
        raise ValueError("A widget must be an object.")
    if newer(w):
        raise ValueError(f"This widget was made by a newer studio (version {w['version']}).")
    if too_deep(w):
        raise ValueError(f"This widget {TOO_DEEP}.")
    w = copy.deepcopy(w)
    w["version"] = 1
    name = w.get("name").strip()[:80] if isinstance(w.get("name"), str) else ""
    w["name"] = name or wid or ""
    w["description"] = w["description"][:500] if isinstance(w.get("description"), str) else ""
    w["w"], w["h"] = whole_number(w.get("w"), 360, 1600), whole_number(w.get("h"), 120, 720)
    w["background"] = color_field(w.get("background"), None)
    w["inputs"] = check_inputs(w.get("inputs"))
    layers = w.setdefault("layers", [])
    if not isinstance(layers, list) or not all(isinstance(l, dict) and isinstance(l.get("objects", []), list) for l in layers):
        raise ValueError("layers must be a list of objects, each with its objects")
    if sum(len(l.get("objects", [])) for l in layers) > MAX_OBJECTS:
        raise ValueError(f"A widget can hold at most {MAX_OBJECTS} objects.")
    seen = {}

    def types(inner):
        if inner not in seen:
            seen[inner] = (types_of or installed_types)(inner)
        return seen[inner]
    for layer in layers:
        objects = layer.setdefault("objects", [])
        if not all(isinstance(o, dict) for o in objects):
            raise ValueError("a layer's objects must be objects")
        for o in objects:
            check_part(o, wid, types)
    if folder is not None:
        w["fonts"] = fonts_entry(w, folder)
    elif not isinstance(w.get("fonts"), dict):
        w["fonts"] = {}
    check_size(w)
    return w


def check_size(w):
    """Refuses (ValueError) a widget whose widget.json, written as the studio writes it, would be more than 1 MB:
    the listing and the player skip a file that big."""
    try:
        size = len(json_bytes(w))
    except UnicodeEncodeError as e:  # a lone surrogate, which UTF-8 (and the player) can't hold
        half = e.object[e.start:e.end][:1]
        raise ValueError(f"This widget holds text that isn't text: \\u{ord(half):04x} is half of a surrogate pair."
                         if half else "This widget holds text that isn't text.") from None
    except ValueError:  # NaN or Infinity, which JSON hasn't got
        raise ValueError("This widget holds a number that isn't one (NaN or Infinity).") from None
    if size > MAX_WIDGET_JSON:
        raise ValueError("This widget would be more than 1 MB as saved.")


# --- Saving, and the library's own moves

def save_widget(req):
    """POST /api/widgets/save: the page's widget, written if the file is still the one the page had (its stamp),
    or `force`. Then the files in its media/ that nothing has named for a while go."""
    if not isinstance(req, dict):
        raise ValueError("expected {id, widget, stamp}")
    wid = req.get("id")
    folder = widget_folder(wid)
    if is_example(wid):
        raise Refused(409, EXAMPLES_READ_ONLY)
    sent = req.get("widget")
    shown = sent.get("name").strip() if isinstance(sent, dict) and isinstance(sent.get("name"), str) else ""
    shown = shown or wid
    with playlist_lock:
        if not real_dir(folder):
            raise Refused(404, f"{shown} was deleted outside this page.")
        on_disk, stamp, _ = read_widget(folder)
        if on_disk is not None and newer(on_disk):
            raise Refused(409, f"{shown} was made by a newer studio: this one doesn't save over it.")
        if req.get("force") is not True and req.get("stamp") != stamp:
            raise Refused(409, f"{shown} was changed outside this page.", stamp=stamp)
        widget = check_widget(sent, wid, folder)
        write_atomic(folder / "widget.json", widget)
        named = named_files(widget, ("media",))["media"]
        note_used(folder / "media" / name for name in named)
        tidy_folder(folder / "media", named, MEDIA_NAME)
        return {"ok": True, "widget": widget, "stamp": file_stamp(folder / "widget.json")}


def move_folder(src, dst, name):
    """Moves a widget's folder, whole or not at all. Windows won't while a program has a file in it open (the
    player's ffmpeg, an Explorer preview): that's tried for about 2 s, then refused with 409 and nothing changed.
    A folder already at `dst` (Windows would refuse that the same way) is a FileExistsError."""
    deadline = time.monotonic() + 2
    while True:
        if os.path.lexists(dst):
            raise FileExistsError(errno.EEXIST, "there's a folder there already", str(dst))
        try:
            return os.replace(src, dst)
        except OSError as e:
            if not in_use(e):
                raise
            if time.monotonic() >= deadline:
                raise Refused(409, f"{name} is in use (a program has its files open): try again.") from None
            time.sleep(0.1)


def trash_name(wid):
    """A folder name in .deleted for a widget deleted now: <id>-<yyyymmdd-hhmmss>, then -2, -3 when that's taken."""
    base = f"{wid}-{time.strftime('%Y%m%d-%H%M%S', time.localtime(time.time()))}"
    name, n = base, 1
    while os.path.lexists(trash_dir() / name):
        n += 1
        name = f"{base}-{n}"
    return name


def to_trash(folder, wid):
    """Moves a widget's folder into .deleted (nothing there is ever deleted by the studio). Returns its name there."""
    trash = trash_name(wid)
    made = not os.path.lexists(trash_dir())
    trash_dir().mkdir(parents=True, exist_ok=True)
    try:
        move_folder(folder, trash_dir() / trash, widget_name(folder, wid))
    except BaseException:
        if made:
            try:
                trash_dir().rmdir()  # only if it's still empty
            except OSError:
                pass
        raise
    return trash


def delete_widget(req):
    """POST /api/widgets/delete: moves the widget's folder into .deleted, from where it can be restored."""
    wid = req.get("id") if isinstance(req, dict) else None
    folder = widget_folder(wid)
    with playlist_lock:
        if not real_dir(folder):
            raise Refused(404, f"There's no widget {wid}.")
        return {"trash": to_trash(folder, wid)}


def restore_widget(req):
    """POST /api/widgets/restore: moves a deleted widget back to widgets/<id>, unless something has that id now."""
    trash = req.get("trash") if isinstance(req, dict) else None
    m = TRASH_NAME.fullmatch(trash) if isinstance(trash, str) else None
    if not m:
        raise ValueError(f"{str(trash)[:80]!r} isn't a deleted widget's name.")
    wid = m["id"]
    folder = widget_folder(wid)
    src = trash_dir() / trash
    with playlist_lock:
        if not real_dir(src):
            raise Refused(404, "That widget isn't in Recently deleted any more.")
        name = widget_name(src, wid)
        if os.path.lexists(folder):
            raise Refused(409, f"{name} can't come back: there's a widget in widgets\\{wid} now.")
        move_folder(src, folder, name)
        w, stamp, _ = read_widget(folder)
        note_used(all_media(folder))
        return {"id": wid, "widget": w, "stamp": stamp}


def all_media(folder):
    """Every file directly in a widget's media/."""
    try:
        return [Path(e.path) for e in os.scandir(folder / "media") if e.is_file()]
    except OSError:
        return []


def skip_on_copy(folder, names):
    """What a duplicate leaves out: half-written *.tmp files, and links and junctions (which lead elsewhere)."""
    out = []
    for n in names:
        p = Path(folder) / n
        if n.lower().endswith(".tmp") or p.is_symlink() or p.is_junction():
            out.append(n)
    return out


def staging_folder(kind):
    """A new folder of the studio's own in widgets/ (its name starts with a dot, so it's never a widget), where a
    widget is put together before it's moved into place whole."""
    widgets_dir().mkdir(parents=True, exist_ok=True)
    folder = widgets_dir() / f".{kind}-{secrets.token_hex(4)}"
    folder.mkdir()
    return folder


def duplicate_widget(req):
    """POST /api/widgets/duplicate: a copy of a widget's folder under a new id, made from `name`, or under exactly
    `folder` (Change folder name)."""
    if not isinstance(req, dict):
        raise ValueError("expected {id, name}")
    wid, target = req.get("id"), req.get("folder")
    src = widget_folder(wid)
    if target is not None:
        widget_folder(target)
        if is_example(target):
            raise ValueError("Folder names starting with example- are kept for the examples that come with the "
                             "studio: choose another name.")
    name = req.get("name").strip()[:80] if isinstance(req.get("name"), str) else ""
    with playlist_lock:
        if not real_dir(src):
            raise Refused(404, f"There's no widget {wid}.")
        w, _, problem = read_widget(src)
        if w is None:
            raise ValueError(f"{wid} can't be duplicated: {problem}")
        if target is not None and target.casefold() in taken_ids():
            raise Refused(409, f"widgets\\{target} is taken, by a widget or one in Recently deleted: choose another name.")
        w["name"] = name or shown_name(w, wid)
        check_size(w)  # it's written again, as the studio writes it
        new = target if target is not None else fresh_id(w["name"])
        staging = staging_folder("new")
        try:
            shutil.copytree(src, staging / "w", ignore=skip_on_copy)
            write_atomic(staging / "w" / "widget.json", w)
            move_folder(staging / "w", widgets_dir() / new, w["name"])
        except FileExistsError:
            raise Refused(409, f"widgets\\{new} was made meanwhile: try again.") from None
        finally:
            shutil.rmtree(staging, ignore_errors=True)
        note_used(all_media(widgets_dir() / new))
        return {"id": new, "widget": w, "stamp": file_stamp(widgets_dir() / new / "widget.json")}


# --- Media into a widget

def media_name(stem, data, suffix):
    """A widget media file's name (docs/widgets.md, Media): <stem>-<hash>.<ext>, the stem from the file's own name
    (cut to 40, `media` when empty), the hash the first 8 hex digits of the bytes' SHA-1. The same file always gets
    the same name, so nothing is stored twice."""
    digest = hashlib.sha1(data).hexdigest()[:8]
    s = slug(stem)
    if s.endswith("-" + digest):
        s = s[:-9]  # named so already: kept as it is
    return f"{s[:40].strip('-') or 'media'}-{digest}{suffix.lower()}"


def check_image(f, suffix, side=MAX_SIDE, decoded=MAX_DECODED):
    """Refuses (ValueError) an image that isn't the format its suffix names, or is more than `side` pixels across
    or down, or would take more than `decoded` bytes to show. Only that format's reader runs, and only on the
    headers."""
    fmt = IMAGE_FORMATS[suffix]
    try:
        with warnings.catch_warnings():
            warnings.simplefilter("ignore")
            with Image.open(f, formats=[fmt]) as im:
                w, h = im.size
                if w > side or h > side:
                    raise ValueError(f"it's {w} × {h}: a picture can be at most {side} pixels across and down")
                frames = getattr(im, "n_frames", 1)
    except ValueError:
        raise
    except Image.DecompressionBombError:  # Pillow opens at most about 179 million pixels
        raise ValueError("it has more pixels than the studio can open") from None
    except Exception:
        raise ValueError(f"it isn't a {suffix[1:].upper()} file, whatever its name says") from None
    if frames * w * h * 4 > decoded:
        raise ValueError(f"its {frames} frames of {w} × {h} would take more than {decoded >> 30} GB to show")


def check_video(path):
    """Refuses (ValueError) a video ffmpeg can't read as the container its extension names, with a video in it."""
    if not wv.find_ffmpeg():
        raise ValueError("checking a video needs ffmpeg (winget install BtbN.FFmpeg.GPL.8.1)")
    try:
        size = native_size(probe_video(path))
    except (OSError, subprocess.SubprocessError):
        size = None
    if not size:
        raise ValueError(f"it isn't a {path.suffix[1:].upper()} video, whatever its name says")
    if max(size) > MAX_SIDE:
        raise ValueError(f"it's {size[0]} × {size[1]}: a video can be at most {MAX_SIDE} pixels across and down")


def scratch_file(data, suffix):
    """A temporary copy of some bytes in .cache/studio/, for a tool that reads files. Delete it after."""
    UPLOADS.mkdir(parents=True, exist_ok=True)
    path = UPLOADS / f"check-{secrets.token_hex(6)}{suffix}"
    with open(path, "xb") as f:
        f.write(data)
    return path


def check_media_bytes(data, suffix):
    """Refuses (ValueError, with a sentence) media bytes that aren't what their suffix says (docs/widgets.md,
    Sharing): an image must open as that format, at most 8192 pixels across and down and 1 GB decoded; an Aseprite
    file must start like one; a video must be read by ffmpeg as its own container, with a video in it."""
    suffix = suffix.lower()
    if suffix in IMAGE_FORMATS:
        check_image(io.BytesIO(data), suffix)
    elif suffix in ASEPRITE_TYPES:
        if len(data) < 6 or data[4:6] != b"\xe0\xa5":
            raise ValueError("it isn't an Aseprite file, whatever its name says")
    elif suffix in VIDEO_FORMATS:
        path = scratch_file(data, suffix)
        try:
            check_video(path)
        finally:
            path.unlink(missing_ok=True)
    else:
        raise ValueError(f"{suffix or 'a file without an extension'} isn't a kind of media the studio takes")


def check_media_file(path, side, decoded):
    """check_image for an image already on disk, before it's decoded, with the limits it's opened up to (a video
    is only ever read as its own container anyway)."""
    suffix = path.suffix.lower()
    if suffix in IMAGE_FORMATS:
        with open(path, "rb") as f:
            check_image(f, suffix, side, decoded)


def to_png(data):
    """An Aseprite file as a PNG (one frame) or an animated PNG with its frames' durations, through wv.load_frames
    (which runs Aseprite). The player can't read Aseprite files, and the PC a widget is shared with may not have
    Aseprite, so a widget only ever keeps the PNG."""
    if not wv.ASEPRITE.exists():
        raise ValueError(NEEDS_ASEPRITE)
    check_media_bytes(data, ".aseprite")
    path = scratch_file(data, ".aseprite")
    try:
        frames = wv.load_frames(path)
    except (SystemExit, subprocess.CalledProcessError, OSError, ValueError, KeyError) as e:  # wv's helpers exit on failure
        raise ValueError(f"Aseprite couldn't turn it into a PNG ({e})") from None
    finally:
        path.unlink(missing_ok=True)
    if not frames:
        raise ValueError("Aseprite found no frames in it")
    buf = io.BytesIO()
    images = [im for im, _ in frames]
    if len(images) == 1:
        images[0].save(buf, "PNG")
    else:
        images[0].save(buf, "PNG", save_all=True, append_images=images[1:], duration=[d for _, d in frames], loop=0)
    return buf.getvalue()


def into_widget(path, label=None):
    """A media file as a widget keeps it: (its name in media/, its bytes). An Aseprite file becomes a PNG, an .avi
    is refused, and anything else must be what its name says. A file the studio named by its hash (in sources/ or
    inputs/) takes its stem from `label` (the object's name) instead."""
    suffix = path.suffix.lower()
    if suffix == ".avi":
        raise ValueError(AVI_REFUSED)
    if suffix not in MEDIA_TYPES | ASEPRITE_TYPES:
        raise ValueError(f"{suffix or 'a file without an extension'} isn't a kind of file a widget can hold")
    data, stem = path.read_bytes(), path.stem
    if KEPT_NAME.fullmatch(path.name):
        stem = label if isinstance(label, str) and slug(label) else "media"
    if suffix in ASEPRITE_TYPES:
        data, suffix = to_png(data), ".png"
    else:
        check_media_bytes(data, suffix)
    return media_name(stem, data, suffix), data


def input_picture(data, suffix):
    """A file going into playlist/inputs/ as a picture for a copy's image input: it's drawn inside a widget, so a
    widget's rules apply (an .avi is refused, an Aseprite file becomes a PNG, and it must be what its name says).
    Returns (bytes, suffix) to keep."""
    if suffix == ".avi":
        raise ValueError(AVI_REFUSED)
    if suffix in ASEPRITE_TYPES:
        data, suffix = to_png(data), ".png"
    check_media_bytes(data, suffix)
    return data, suffix


def put_media(folder, file, data):
    """Writes a file into a widget's media/ (a copy, never a link), unless it's there already: the name says the
    bytes are the same. Returns its path, and whether it was written now."""
    dest = dest_in(folder, f"media/{file}")
    with playlist_lock:
        fresh = not dest.is_file()
        if fresh:
            write_file(dest, data)
        note_used([dest])
    return dest, fresh


def widget_upload(wid, name, body):
    """POST /api/widgets/upload: a file picked or dropped while a widget is open, kept in its media/."""
    folder = widget_folder(wid)
    if is_example(wid):
        raise Refused(409, EXAMPLES_READ_ONLY)
    if not body:
        raise ValueError("the file is empty")
    name = Path(name or "upload.png").name
    suffix = Path(name).suffix.lower()
    if suffix == ".avi":
        raise ValueError(AVI_REFUSED)
    if suffix not in MEDIA_TYPES | ASEPRITE_TYPES:
        raise ValueError(f"{suffix or 'that'} isn't a kind of file a widget can hold")
    check_media_bytes(body, suffix)
    if suffix in ASEPRITE_TYPES:
        body, suffix = to_png(body), ".png"
    file = media_name(Path(name).stem, body, suffix)
    dest, fresh = put_media(folder, file, body)
    try:
        info = open_media(dest, f"widgets/{wid}/media/{file}", name)
    except Exception:
        if fresh:  # just written and unreadable: not worth keeping
            dest.unlink(missing_ok=True)
        raise
    return {**info, "src": f"media/{file}", "ref": f"widgets/{wid}/media/{file}"}


def media_get(rest):
    """GET /api/media/inputs/<file> and /api/media/widgets/<id>/<path>: media info, `src` the page's ref."""
    parts = rest.split("/")
    if parts[0] == "inputs" and len(parts) == 2 and PART.fullmatch(parts[1]):
        folder, rel = inputs_dir(), parts[1]
    elif parts[0] == "widgets" and len(parts) >= 3:
        folder, rel = installed_folder(parts[1]), "/".join(parts[2:])
        if not rel_path_ok(rel):
            raise ValueError(f"{rel[:100]!r} isn't a path inside a widget's folder.")
    else:
        raise ValueError("That isn't a media file's name.")
    if os.path.splitext(rel)[1].lower() not in MEDIA_TYPES:  # nothing else is opened: an Aseprite file would run Aseprite
        raise ValueError(f"{rel[:100]!r} isn't a kind of media a widget holds.")
    path = folder.joinpath(*rel.split("/"))
    if not inside(path, folder):
        raise Refused(404, f"{rest} isn't there.")
    return open_media(path, rest)


def holder_of(spec):
    """(widget id, folder) for a holder: "playlist" is (None, the playlist's folder); "widget:<id>" a widget's."""
    if spec == "playlist":
        return None, PLAYLIST_DIR
    if isinstance(spec, str) and spec.startswith("widget:"):
        wid = spec[len("widget:"):]
        return wid, installed_folder(wid)
    raise ValueError('a holder is "playlist" or "widget:<id>"')


def holder_file(wid, folder, ref):
    """The file a path in a holder names, if it's there: in the playlist, sources/<file> or inputs/<file>; in a
    widget, a path inside its folder. Else None."""
    if not isinstance(ref, str):
        return None
    ref = ref.strip(" ")
    if wid is None:
        parts = ref.split("/")
        kept = os.path.normcase(parts[0])  # in any letter case, as Windows opens it
        if len(parts) != 2 or kept not in ("sources", "inputs") or not PART.fullmatch(parts[1]):
            return None
        path, folder = PLAYLIST_DIR / kept / parts[1], PLAYLIST_DIR / kept
    elif widget_path_ok(ref) and os.path.splitext(ref)[1].lower() in MEDIA_TYPES:
        path = folder.joinpath(*ref.split("/"))  # only media: an Aseprite file put there by hand isn't run
    else:
        return None
    return path if inside(path, folder) else None


def font_file_for(font, wid, folder):
    """The file a text's `font` means in its holder, or None when it isn't on this PC. In a scene it's an absolute
    path, taken only in one of the font folders the studio lists or a widget's fonts/; in a widget, a file name
    found by the lookup."""
    if wid is not None:
        return find_font(folder, font)
    if not isinstance(font, str) or not os.path.isabs(font):
        return None
    p = Path(font)
    listed = os.path.normcase(str(p.parent)) in {os.path.normcase(str(d)) for d in FONT_DIRS}
    return p if (listed and font_name_ok(p.name) and p.is_file()) or carried_font(font) else None


def font_for_widget(font, wid, folder):
    """A text's font as a widget names it: (file name, the bytes to copy into its fonts/ or None, found). Windows'
    own fonts stay names (every PC has them); any other is copied, so the widget carries it to another PC."""
    path = font_file_for(font, wid, folder)
    name = font if wid is not None else Path(font).name
    if path is None:
        return (name if font_name_ok(name) else ""), None, False
    if is_windows_font(path):
        return path.name, None, True
    data = path.read_bytes()
    check_font_bytes(data)
    return path.name, data, True


def copy_media(req):
    """POST /api/media/copy: the files and fonts objects need when they're pasted from one holder into another,
    copied (never linked) into the new one: a widget's media/ and fonts/, or the playlist's sources/ (inputs/ for
    copies' values, `"as": "input"`). Answers the new names, which the page writes into the pasted objects."""
    if not isinstance(req, dict):
        raise ValueError("expected {from, to, files, fonts}")
    from_id, from_folder = holder_of(req.get("from"))
    to_id, to_folder = holder_of(req.get("to"))
    if to_id is not None and is_example(to_id):
        raise Refused(409, EXAMPLES_READ_ONLY)
    files, fonts = req.get("files") or [], req.get("fonts") or []
    if not (isinstance(files, list) and isinstance(fonts, list)):
        raise ValueError("files and fonts must be lists")
    out = {"files": {}, "fonts": {}, "missing": []}
    for ref in files:
        path = holder_file(from_id, from_folder, ref)
        if path is None:
            out["missing"].append({"file": str(ref), "why": "It isn't there."})
            continue
        try:
            if to_id is not None:
                file, data = into_widget(path)
                put_media(to_folder, file, data)
                out["files"][ref] = f"media/{file}"
            elif req.get("as") == "input":
                data, suffix = input_picture(path.read_bytes(), path.suffix.lower())
                out["files"][ref] = keep_file(data, suffix, "inputs")[1]
            else:  # into a scene, which takes what it always took
                out["files"][ref] = keep_file(path.read_bytes(), path.suffix, "sources")[1]
        except ValueError as e:
            out["missing"].append({"file": ref, "why": str(e)})
    for font in fonts:
        if not isinstance(font, str) or not font:
            continue
        try:
            if to_id is not None:
                name, data, found = font_for_widget(font, from_id, from_folder)
                dest = dest_in(to_folder, f"fonts/{name}") if data is not None else None
                with playlist_lock:
                    if dest is not None and not dest.exists():  # one it has already of that name wins, as it would
                        write_file(dest, data)
                if name:
                    out["fonts"][font] = name
            else:  # into a scene, a text names its font by its absolute path
                path = font_file_for(font, from_id, from_folder)
                found = path is not None
                if found:
                    out["fonts"][font] = str(path)
            if not found:
                out["missing"].append({"file": font, "why": "That font isn't on this PC."})
        except ValueError as e:
            out["missing"].append({"file": font, "why": str(e)})
    return out


def hold_media(req):
    """POST /api/media/hold: the files the page's undo history names, noted as used, so tidying keeps what an undo
    could bring back. Refs outside sources/, inputs/ and widgets' media/ are ignored."""
    refs = req.get("refs") if isinstance(req, dict) else None
    if not isinstance(refs, list):
        raise ValueError("expected {refs: [...]}")
    note_used(p for p in map(ref_file, refs) if p)
    return {"ok": True}


def ref_file(ref):
    """The file a page ref names (sources/<file>, inputs/<file> or widgets/<id>/media/<file>) if it's there, else
    None. The folders' names in any letter case, as Windows opens them (the id is exact, as everywhere)."""
    if not isinstance(ref, str):
        return None
    parts = ref.strip(" ").split("/")
    low = [os.path.normcase(p) for p in parts]
    if len(parts) == 2 and low[0] in ("sources", "inputs") and PART.fullmatch(parts[1]):
        folder = PLAYLIST_DIR / low[0]
    elif len(parts) == 4 and low[0] == "widgets" and low[2] == "media" and PART.fullmatch(parts[3]):
        try:
            folder = installed_folder(parts[1]) / "media"
        except (ValueError, Refused):
            return None
    else:
        return None
    path = folder / parts[-1]
    return path if inside(path, folder) else None


def new_widget(req):
    """POST /api/widgets/new: a widget made from a name, a size and a background, under a fresh id. From selection
    sends the objects (`layers`, `inputs`) and the holder they came from (`from`): every file they name there is
    copied into the new folder's media/, and every font that isn't Windows' own into its fonts/."""
    if not isinstance(req, dict):
        raise ValueError("expected {name, w, h}")
    name = req.get("name") if isinstance(req.get("name"), str) else ""
    widget = copy.deepcopy({"version": 1, "name": name, "description": req.get("description", ""), "w": req.get("w"),
                            "h": req.get("h"), "background": req.get("background"), "fonts": {},
                            "inputs": req.get("inputs", []),
                            "layers": req["layers"] if "layers" in req else [DEFAULT_LAYER]})
    files, fonts, missing = {}, {}, []
    if req.get("from") is not None:
        from_id, from_folder = holder_of(req["from"])
        bring_selection(widget, from_id, from_folder, files, fonts, missing)
    with playlist_lock:
        staging = staging_folder("new")
        try:
            for file, data in files.items():
                write_file(staging / "w" / "media" / file, data)
            for font, data in fonts.items():
                write_file(staging / "w" / "fonts" / font, data)
            (staging / "w").mkdir(exist_ok=True)
            taken = taken_ids()
            while True:
                wid = fresh_id(name, taken)
                widget = check_widget(widget, wid, staging / "w")
                write_atomic(staging / "w" / "widget.json", widget)
                try:
                    move_folder(staging / "w", widgets_dir() / wid, widget["name"])
                    break
                except FileExistsError:  # made by hand meanwhile: the next one
                    taken.add(wid.casefold())
        finally:
            shutil.rmtree(staging, ignore_errors=True)
        note_used(widgets_dir() / wid / "media" / file for file in files)
        return {"id": wid, "widget": widget, "stamp": file_stamp(widgets_dir() / wid / "widget.json"),
                "missing": missing}


def bring_selection(widget, from_id, from_folder, files, fonts, missing):
    """From selection's files (new_widget): rewrites, in place, every path the objects name in the holder they came
    from to its copy in the new widget, collecting the bytes in `files` (media/) and `fonts` (fonts/)."""
    def bring(ref, label=None):
        path = holder_file(from_id, from_folder, ref)
        if path is None:
            missing.append({"file": ref, "why": "It isn't there."})
            return None
        try:
            file, data = into_widget(path, label)
        except ValueError as e:
            missing.append({"file": ref, "why": str(e)})
            return None
        files[file] = data
        return f"media/{file}"

    def is_path(v):
        return isinstance(v, str) and v.strip(" ") != "" and not is_ref(v)
    for i in widget["inputs"] if isinstance(widget["inputs"], list) else []:
        if isinstance(i, dict) and i.get("type") == "image" and is_path(i.get("default")):
            i["default"] = bring(i["default"]) or i["default"]
    types = {}
    for o in objects_of(widget):
        kind = o.get("type")
        if kind in ("image", "video") and is_path(o.get("src")):
            o["src"] = bring(o["src"], o.get("name")) or o["src"]
        elif kind == "text" and isinstance(o.get("font"), str) and o["font"]:
            name, data, found = font_for_widget(o["font"], from_id, from_folder)
            if not found:
                missing.append({"file": o["font"], "why": "That font isn't on this PC."})
            o["font"] = name
            if data is not None:
                fonts[name] = data
        elif kind == "widget" and isinstance(o.get("inputs"), dict):
            inner = o.get("widget")
            if inner not in types:
                types[inner] = installed_types(inner) if isinstance(inner, str) else None
            for k, v in o["inputs"].items():
                known = types[inner]
                # a picture value: by the widget's own input types, or, for a widget that isn't here, a file it names
                if is_path(v) and (known.get(k) == "image" if known else holder_file(from_id, from_folder, v)):
                    o["inputs"][k] = bring(v) or v


# --- Sharing: export and import (docs/widgets.md, Sharing)

def shown_name(w, wid):
    """A widget's name for a note, or its id."""
    name = w.get("name") if isinstance(w, dict) else None
    return name.strip()[:80] if isinstance(name, str) and name.strip() else wid


def preview_png(url):
    """A widget's picture from the page: a PNG data URL of at most 2 MB that really is a PNG. Returns the bytes."""
    head = "data:image/png;base64,"
    if not isinstance(url, str) or not url.startswith(head):
        raise ValueError("The picture must be a PNG data URL.")
    try:
        data = base64.b64decode(url[len(head):], validate=True)
    except ValueError:
        raise ValueError("The picture isn't a PNG data URL.") from None
    if len(data) > 2 << 20:
        raise ValueError("The picture is more than 2 MB.")
    try:
        check_media_bytes(data, ".png")
    except ValueError as e:
        raise ValueError(f"The picture: {e}.") from None
    return data


def used_widgets(wid, folder, w, notes):
    """[(id, folder, widget)]: a widget and every widget it places, at any depth, each once. Missing and broken
    widgets, and copies that close a cycle, are left out, each with a note."""
    order, seen = [], set()

    def visit(wid, folder, w, path):
        order.append((wid, folder, w))
        seen.add(wid)
        name = shown_name(w, wid)
        for inner in copies_in(w):
            if inner in path:
                notes.append(f"{name} contains {inner}, which contains {name}: that inner copy doesn't draw.")
                continue
            if inner in seen:
                continue
            seen.add(inner)
            try:
                f = widget_folder(inner)
            except ValueError:
                notes.append(f"{name} has a copy of {inner[:60]!r}, which isn't a widget id: it's left out.")
                continue
            iw, _, problem = read_widget(f) if real_dir(f) else (None, None, None)
            if iw is None:
                notes.append(f"{name} uses {inner}, which isn't in your widgets: it's left out of the zip." if not problem
                             else f"{name} uses {inner}, which can't be read ({problem}): it's left out of the zip.")
            elif len(path) >= 64:
                notes.append(f"{name} uses {inner} more than 64 copies deep: it's left out.")
            else:
                visit(inner, f, iw, path + [inner])
    visit(wid, folder, w, [wid])
    return order


def zip_files(folder, w, extra):
    """What a widget's folder in the zip holds: widget.json, every file it names, its fonts/, and preview.png,
    README.md and LICENSE files when it has them; plus `extra` (paths that go in the zip only). {normcase: path}"""
    found = {"widget.json": "widget.json"}

    def add(rel):
        if os.path.normcase(rel) not in found and inside(folder.joinpath(*rel.split("/")), folder):
            found[os.path.normcase(rel)] = rel
    for s in strings_in(w):
        if widget_path_ok(s) and os.path.splitext(s.strip(" "))[1].lower() in IMPORT_TYPES:
            add(s.strip(" "))  # media only: an import takes nothing else from a widget's paths
    try:
        entries = sorted(e.name for e in os.scandir(folder))
        fonts = sorted(e.name for e in os.scandir(folder / "fonts")) if real_dir(folder / "fonts") else []
    except OSError:
        entries, fonts = [], []
    for name in fonts:
        if font_name_ok(name):
            add(f"fonts/{name}")
    for name in entries:
        if name.lower() in ("preview.png", "readme.md") or (name.upper().startswith("LICENSE") and PART.fullmatch(name)):
            add(name)
    for rel in extra:
        found[os.path.normcase(rel)] = rel
    return found


def export_widget(req):
    """POST /api/widgets/export: a zip of the widget's folder and every widget it uses, and notes on what's left out.
    `preview` (a PNG data URL) is written as its preview.png first, and each font named in `fonts` that a widget in
    the zip uses from this user's or the project's fonts (never Windows' own) is copied into that widget's fonts/,
    so the folder carries it from then on. An example's folder is never written: those go into the zip only.
    Returns (the zip's bytes, notes)."""
    if not isinstance(req, dict):
        raise ValueError("expected {id}")
    wid = req.get("id")
    folder = widget_folder(wid)
    fonts = req.get("fonts") or []
    if not isinstance(fonts, list) or not all(isinstance(f, str) for f in fonts):
        raise ValueError("fonts must be a list of font file names")
    preview = preview_png(req["preview"]) if req.get("preview") is not None else None
    with playlist_lock:
        if not real_dir(folder):
            raise Refused(404, f"There's no widget {wid}.")
        w, _, problem = read_widget(folder)
        if w is None:
            raise ValueError(f"{wid} can't be exported: {problem}")
        notes = []
        order = used_widgets(wid, folder, w, notes)
        extra = {}  # (id, path in its folder) -> bytes that go into the zip only
        if preview is not None:
            if is_example(wid):
                extra[(wid, "preview.png")] = preview
            else:
                write_file(dest_in(folder, "preview.png"), preview)
        shareable = {os.path.normcase(str(d)) for d in font_folders()[1:]}  # this user's and the project's
        for iid, ifolder, iw in order:
            for name in sorted(set(fonts) & text_fonts(iw)):
                found = find_font(ifolder, name)
                if found is None or os.path.normcase(str(found.parent)) not in shareable:
                    continue  # its own already, Windows' own, or not on this PC
                data = found.read_bytes()
                if is_example(iid):
                    extra[(iid, f"fonts/{name}")] = data
                else:
                    write_file(dest_in(ifolder, f"fonts/{name}"), data)
        buf = io.BytesIO()
        with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as zf:
            for iid, ifolder, iw in order:
                files = zip_files(ifolder, iw, [rel for i, rel in extra if i == iid])
                for rel in sorted(files.values(), key=str.lower):
                    data = extra.get((iid, rel))
                    zf.writestr(f"{iid}/{rel}", data if data is not None else ifolder.joinpath(*rel.split("/")).read_bytes())
    return buf.getvalue(), notes


def notes_header(notes, limit=4096):
    """X-Widget-Notes: the notes as JSON in ASCII (http.server writes headers as Latin-1), at most `limit` bytes:
    notes past that are dropped, and the last says how many."""
    text = json.dumps(notes, ensure_ascii=True)
    if len(text) <= limit:
        return text
    k, size = 0, 2
    while k < len(notes) and size + len(json.dumps(notes[k], ensure_ascii=True)) + 2 <= limit - 40:
        size += len(json.dumps(notes[k], ensure_ascii=True)) + 2
        k += 1
    while True:
        text = json.dumps(notes[:k] + [f"… and {len(notes) - k} more"], ensure_ascii=True)
        if len(text) <= limit or k == 0:
            return text
        k -= 1


ZIP_ENTRIES = 256
ZIP_TOTAL = 128 << 20  # bytes unpacked, counted while reading: a zip's own headers can lie
ZIP_RATIO = 100  # an entry over 1 MB may not unpack to more than this many times its packed size
LIMITS = {"widget.json": MAX_WIDGET_JSON, "preview": 2 << 20, "text": 1 << 20, "font": 32 << 20, "media": 64 << 20}


def zip_entries(zf):
    """The files in a zip to look at: [(path parts, ZipInfo)], with \\ read as /. Refuses (ValueError) the whole
    import for an entry that could land outside its folder, an encrypted entry, a link, an Aseprite file or an
    .avi, or two names Windows would take for one file. Skips folders, __MACOSX, names starting with a dot,
    Thumbs.db and desktop.ini."""
    infos = zf.infolist()
    if len(infos) > ZIP_ENTRIES:
        raise ValueError(f"The zip holds {len(infos)} entries: an import takes at most {ZIP_ENTRIES}.")
    entries, keys = [], {}
    for zi in infos:
        raw = zi.orig_filename
        name = raw.replace("\\", "/")
        is_folder = name.endswith("/")
        parts = (name[:-1] if is_folder else name).split("/")
        if "\x00" in raw or name.startswith("/") or ":" in name or any(p in ("", ".", "..") for p in parts):
            raise ValueError(f"The zip holds {raw!r}, which could be written outside its folder: it isn't imported.")
        if zi.flag_bits & 0x1:
            raise ValueError(f"The zip is encrypted ({raw}): it isn't imported.")
        if (zi.external_attr >> 16) & 0o170000 == 0o120000:
            raise ValueError(f"The zip holds a link, {raw}: it isn't imported.")
        if is_folder:
            continue
        suffix = os.path.splitext(parts[-1].rstrip(" ."))[1].lower()
        if suffix in ASEPRITE_TYPES:
            raise ValueError(f"The zip holds {raw}, an Aseprite file: a shared widget can't (turning it into a PNG "
                             "would run Aseprite on a file from the internet). It isn't imported.")
        if suffix == ".avi":
            raise ValueError(f"The zip holds {raw}, an .avi video: widgets take MP4, MOV, WebM or MKV. It isn't imported.")
        if parts[0] == "__MACOSX" or any(p.startswith(".") for p in parts) or parts[-1].lower() in ("thumbs.db", "desktop.ini"):
            continue
        key = "/".join(p.casefold().rstrip(" .") for p in parts)
        if key in keys:
            raise ValueError(f"The zip holds {keys[key]} and {raw}, which Windows would take for the same file: "
                             "it isn't imported.")
        keys[key] = raw
        entries.append((parts, zi))
    return entries


def read_entry(zf, zi, limit, total):
    """An entry's bytes, read in chunks and counted as they come, never trusting the sizes the zip claims."""
    out = bytearray()
    try:
        with zf.open(zi) as f:
            while chunk := f.read(1 << 16):
                out += chunk
                total[0] += len(chunk)
                if len(out) > limit:
                    raise ValueError(f"{zi.orig_filename} is more than {limit >> 20} MB: the import is refused.")
                if total[0] > ZIP_TOTAL:
                    raise ValueError(f"The zip unpacks to more than {ZIP_TOTAL >> 20} MB: the import is refused.")
                if len(out) > 1 << 20 and len(out) > ZIP_RATIO * max(zi.compress_size, 1):
                    raise ValueError(f"{zi.orig_filename} unpacks to more than {ZIP_RATIO} times its size: "
                                     "the import is refused.")
    except (zipfile.BadZipFile, zlib.error, EOFError, OSError, NotImplementedError, RuntimeError, struct.error) as e:
        raise ValueError(f"{zi.orig_filename} can't be read from the zip ({e}): the import is refused.") from None
    return bytes(out)


def zip_kind(rel):
    """What a file in a widget's folder in a zip is, with where it's written and its size limit, or None for a file
    a widget doesn't hold: (kind, path)."""
    parts = rel.split("/")
    low = rel.casefold()
    if low == "widget.json":
        return "widget.json", "widget.json"
    if low == "preview.png":
        return "preview", "preview.png"
    if low == "readme.md":
        return "text", "README.md"
    if len(parts) == 1 and rel.upper().startswith("LICENSE") and PART.fullmatch(rel):
        return "text", rel
    if len(parts) == 2 and parts[0].casefold() == "fonts" and font_name_ok(parts[1]):
        return "font", f"fonts/{parts[1]}"
    if rel_path_ok(rel) and os.path.splitext(rel)[1].lower() in IMPORT_TYPES:
        return "media", rel
    return None


def named_paths(w, types_of):
    """The paths a widget's widget.json names, which must all be in its zip: its media's `src`, its picture
    inputs' defaults, and the picture values of its copies (for a widget whose inputs are known)."""
    def path(v):
        return isinstance(v, str) and v.strip(" ") != "" and not is_ref(v)
    names = {i["default"].strip(" ") for i in w["inputs"] if i["type"] == "image" and path(i["default"])}
    for o in objects_of(w):
        if o.get("type") in ("image", "video") and path(o.get("src")):
            names.add(o["src"].strip(" "))
        elif o.get("type") == "widget" and isinstance(o.get("inputs"), dict):
            types = types_of(o["widget"]) if isinstance(o.get("widget"), str) else None
            names |= {v.strip(" ") for k, v in o["inputs"].items() if types and types.get(k) == "image" and path(v)}
    return names


def read_zip(body, zip_name):
    """A shared zip's widgets, every file read and checked before anything is written. The zip is untrusted (it
    came from anywhere, and the studio can be reached over Tailscale), so anything unsafe refuses the whole import
    (ValueError) and anything a widget doesn't hold is skipped with a note.

    Layout: each top-level folder holding a widget.json is a widget, its id the folder's name (or one made from
    it); a widget.json at the zip's root makes the whole zip one widget, its id made from the zip's file name.
    Returns ([{folder, id, widget, checked, files, keys, named, notes}], notes): `widget` as the zip has it,
    `checked` as check_widget reads it, `files` {path: bytes}, `keys` {casefolded path: path}."""
    try:
        zf = zipfile.ZipFile(io.BytesIO(body))
        entries = zip_entries(zf)
    except (zipfile.BadZipFile, zipfile.LargeZipFile, OSError, EOFError, struct.error, IndexError, NotImplementedError):
        raise ValueError("That isn't a zip file, or it's damaged.") from None
    notes, groups = [], {}
    at_root = any(len(p) == 1 and p[0].casefold() == "widget.json" for p, _ in entries)
    for parts, zi in entries:
        if at_root:
            groups.setdefault("", []).append((parts, zi))
        elif len(parts) >= 2:
            groups.setdefault(parts[0], []).append((parts[1:], zi))
        else:
            notes.append(f"Skipped {zi.orig_filename}: it isn't in a widget's folder.")
    widgets = {f: g for f, g in groups.items() if any(len(p) == 1 and p[0].casefold() == "widget.json" for p, _ in g)}
    notes += [f"Skipped the folder {f}: there's no widget.json in it." for f in groups if f not in widgets]
    if not widgets:
        raise ValueError("There's no widget in this zip: no widget.json.")
    # ids: a folder named like an id keeps it; any other gets one made from it, and the root widget one made from
    # the zip's name
    found, ids = [], set()
    for f in sorted(widgets, key=lambda f: (f == "" or not WIDGET_ID.fullmatch(f) or f in RESERVED, f)):
        if f and WIDGET_ID.fullmatch(f) and f not in RESERVED:
            base = f
        else:
            base = make_id(Path(str(zip_name or "")).stem if f == "" else f)
        wid, n = base, 1
        while wid in ids:
            n += 1
            wid = numbered(base, n)
        ids.add(wid)
        found.append({"folder": f, "id": wid, "files": {}, "keys": {}, "notes": []})
    total = [0]
    for x in found:
        for parts, zi in widgets[x["folder"]]:
            rel = "/".join(parts)
            kind = zip_kind(rel)
            if kind is None:
                why = ("a widget keeps its fonts directly in fonts\\, each named like Orbitron Bold.ttf"
                       if os.path.splitext(rel)[1].lower() in (".ttf", ".otf") else "a widget doesn't hold that kind of file")
                x["notes"].append(f"Skipped {rel}: {why}.")
                continue
            x["files"][kind[1]] = read_entry(zf, zi, LIMITS[kind[0]], total)
            x["keys"][kind[1].casefold().rstrip(" .")] = kind[1]
    # every widget.json first, so a copy's inputs can be read from a widget in the zip
    for x in found:
        label = x["folder"] or x["id"]
        try:
            x["widget"] = strict_json(x["files"]["widget.json"].decode("utf-8"))
        except (UnicodeDecodeError, ValueError) as e:
            raise ValueError(f"{label}: its widget.json isn't valid JSON ({e}): the import is refused.") from None
        if not isinstance(x["widget"], dict):
            raise ValueError(f"{label}: its widget.json doesn't hold an object: the import is refused.")
    in_zip = {x["id"]: input_types(x["widget"].get("inputs")) for x in found}

    def types_of(wid):
        return in_zip[wid] if wid in in_zip else installed_types(wid)
    for x in found:
        label = x["folder"] or x["id"]
        try:
            x["checked"] = check_widget(x["widget"], x["id"], None, types_of)
        except ValueError as e:
            raise ValueError(f"{label}: {e} The import is refused.") from None
        x["named"] = named_paths(x["checked"], types_of)
        for rel in sorted(x["named"]):
            if rel.casefold().rstrip(" .") not in x["keys"]:
                raise ValueError(f"{label}: its widget.json names {rel}, which isn't in the zip: the import is refused.")
        for rel, data in x["files"].items():
            kind = zip_kind(rel)[0]
            try:
                if kind == "media":
                    check_media_bytes(data, os.path.splitext(rel)[1])
                elif kind == "font":
                    check_font_bytes(data)
                elif kind == "preview":
                    check_media_bytes(data, ".png")
            except ValueError as e:
                raise ValueError(f"{label}/{rel}: {e}. The import is refused.") from None
    return found, notes


def installed_ids():
    """The ids of the widgets here: folders in widgets/ named exactly like an id."""
    try:
        return {e.name for e in os.scandir(widgets_dir())
                if WIDGET_ID.fullmatch(e.name) and e.name not in RESERVED and real_dir(e.path)}
    except OSError:
        return set()


def compare_installed(x):
    """Whether this PC has the zip's widget already: None (no), "same" (the same widget.json and the same bytes
    in every file it names) or "different"."""
    folder = widgets_dir() / x["id"]
    if not os.path.lexists(folder):
        return None
    w, _, _ = read_widget(folder) if real_dir(folder) else (None, None, None)
    if w != x["widget"]:
        return "different"
    for rel in x["named"]:
        path = folder.joinpath(*rel.split("/"))
        if not inside(path, folder) or path.read_bytes() != x["files"][x["keys"][rel.casefold().rstrip(" .")]]:
            return "different"
    return "same"


def describe_import(x, found, installed, graph):
    """What the import window shows for one widget of a zip (mode=check)."""
    w, zip_ids = x["checked"], {y["id"] for y in found}
    preview = x["files"].get("preview.png")
    needs = sorted({i for i in copies_in(w) if WIDGET_ID.fullmatch(i) and i not in zip_ids and i not in installed})
    fonts_missing = []
    remembered = w.get("fonts") if isinstance(w.get("fonts"), dict) else {}
    for name in sorted(n for n in text_fonts(w) if font_name_ok(n)):
        if f"fonts/{name}".casefold() in x["keys"] or find_font(None, name):
            continue
        entry = remembered.get(name) if isinstance(remembered.get(name), dict) else {}
        family = entry.get("family") if isinstance(entry.get("family"), str) else ""
        style = entry.get("style") if isinstance(entry.get("style"), str) else ""
        fonts_missing.append({"name": name, "family": family, "style": style,
                              "installed": installed_named(family, style) if family else []})
    exists, notes = compare_installed(x), list(x["notes"])
    if exists == "different" and is_example(x["id"]):
        notes.append(f"{x['id']} is one of the examples, which the studio doesn't change: Replace skips it, and Keep "
                     "both imports it under another id.")
    return {"folder": x["folder"], "id": x["id"], "name": w["name"], "description": w["description"], "w": w["w"],
            "h": w["h"], "inputs": w["inputs"],
            "preview": "data:image/png;base64," + base64.b64encode(preview).decode() if preview else None,
            "exists": exists, "needs": needs, "cycles": cycles_through(x["id"], graph),
            "fonts_missing": fonts_missing, "notes": notes}


def cycles_through(wid, graph):
    """The other widgets on a cycle through `wid` (graph: id -> the ids it places): each inner copy that closes one
    won't draw. A cycle is only reported: the renderers simply skip that copy."""
    def reach(start):
        seen, stack = set(), list(graph.get(start, ()))
        while stack:
            n = stack.pop()
            if n not in seen:
                seen.add(n)
                stack.extend(graph.get(n, ()))
        return seen
    return sorted(other for other in reach(wid) if other != wid and wid in reach(other))


def import_widgets(body, mode, choices, zip_name):
    """POST /api/widgets/import. mode=check reads the zip and describes each widget in it, writing nothing.
    mode=import puts them in the library: one this PC has the same of is skipped; a different one follows
    `choices` ({folder: "keep-both", "replace" or "skip"}): keep-both imports it under a fresh id made from its name
    (and points the zip's other widgets at that), replace moves this PC's one to .deleted first (never an example:
    those are skipped). Each widget is written from the bytes read into a folder of the studio's own, then they're
    all moved into place, or none are."""
    if mode not in ("check", "import"):
        raise ValueError('mode must be "check" or "import"')
    found, notes = read_zip(body, zip_name)
    installed = installed_ids()
    if mode == "check":
        graph = {}
        for wid in installed:
            w, _, _ = read_widget(widgets_dir() / wid)
            graph[wid] = copies_in(w) if w else []
        graph.update({x["id"]: copies_in(x["checked"]) for x in found})  # the zip's take their ids' places
        return {"widgets": [describe_import(x, found, installed, graph) for x in found], "errors": [], "notes": notes}
    try:
        choices = json.loads(choices) if choices else {}
    except ValueError:
        choices = None
    if not isinstance(choices, dict):
        raise ValueError('choices must be {"<folder>": "keep-both", "replace" or "skip"}')
    imported, skipped, plan, renames = [], [], [], {}
    with playlist_lock:
        taken = taken_ids() | {x["id"] for x in found}
        for x in found:
            exists, choice = compare_installed(x), choices.get(x["folder"])
            if choice == "skip":
                skipped.append({"folder": x["folder"], "why": "You chose to skip it."})
            elif exists == "same":
                skipped.append({"folder": x["folder"], "why": "The same widget is here already."})
            elif exists == "different" and choice == "keep-both":
                # a fresh id made from its name, as for any new widget (docs/widgets.md, Ids)
                renames[x["id"]] = new = fresh_id(x["checked"]["name"], taken)
                taken.add(new.casefold())
                plan.append((x, new, False))
            elif exists == "different" and choice == "replace" and is_example(x["id"]):
                skipped.append({"folder": x["folder"], "why": f"{x['id']} is one of the examples, which the studio "
                                "doesn't change: choose Keep both to import it under another id."})
            elif exists == "different" and choice == "replace" and not real_dir(widgets_dir() / x["id"]):
                skipped.append({"folder": x["folder"], "why": f"widgets\\{x['id']} isn't a plain folder (it's a link, "
                                "or a file): it isn't replaced."})
            elif exists == "different" and choice == "replace":
                plan.append((x, x["id"], True))
            elif exists == "different":
                skipped.append({"folder": x["folder"], "why": f"You have a different widget {x['id']}: choose Keep "
                                + ("both." if is_example(x["id"]) else "both or Replace.")})
            else:
                plan.append((x, x["id"], False))
        writes = []
        for x, wid, _ in plan:
            files = dict(x["files"])
            w = x["widget"]
            if any(o.get("type") == "widget" and o.get("widget") in renames for o in objects_of(w)):
                w = copy.deepcopy(w)
                for o in objects_of(w):
                    if o.get("type") == "widget" and o.get("widget") in renames:
                        o["widget"] = renames[o["widget"]]
                try:
                    check_size(w)  # it's written again, as the studio writes it
                except ValueError as e:
                    raise ValueError(f"{x['folder'] or x['id']}: {e} The import is refused.") from None
                files["widget.json"] = json_bytes(w)
            x["written"] = w
            writes.append((wid, files))
        staging = staging_folder("import") if plan else None
        try:
            for wid, files in writes:
                for rel, data in files.items():
                    write_file(staging.joinpath(wid, *rel.split("/")), data)
            placed = place_imports(plan, staging, skipped)
        finally:
            if staging is not None:
                shutil.rmtree(staging, ignore_errors=True)
        for x, wid, trash in placed:
            dest = widgets_dir() / wid
            note_used(all_media(dest))
            imported.append({"from": x["folder"], "id": wid, "widget": x["written"],
                             "stamp": file_stamp(dest / "widget.json"), "replaced": trash})
    return {"imported": imported, "skipped": skipped, "notes": notes}


def place_imports(plan, staging, skipped):
    """Moves an import's widgets from `staging` into widgets/, all of them or none (docs/widgets.md, Undo, saving
    and deleting): first each widget a Replace takes out goes to .deleted, then each new folder goes in. When
    Windows won't move one (a program has a file in it open), every move already made is moved back, last first,
    and the refusal (409, "in use") is raised: nothing has changed. Returns [(x, id, the old one's name in .deleted
    or None)]."""
    done, placed, trashed = [], [], {}  # done: (where a folder was, where it went), for moving it back
    had_trash = os.path.lexists(trash_dir())
    try:
        for x, wid, replace in plan:
            if replace:
                trashed[wid] = to_trash(widgets_dir() / wid, wid)
                done.append((widgets_dir() / wid, trash_dir() / trashed[wid]))
        for x, wid, replace in plan:
            dest = widgets_dir() / wid
            if os.path.lexists(dest):  # made by hand since the plan, or a replaced one's folder made again
                skipped.append({"folder": x["folder"], "why": f"widgets\\{wid} appeared meanwhile."})
                continue
            move_folder(staging / wid, dest, shown_name(x["written"], wid))
            done.append((staging / wid, dest))
            placed.append((x, wid, trashed.get(wid)))
    except BaseException as e:
        stuck = []
        for back, there in reversed(done):
            try:
                move_folder(there, back, there.name)
            except Exception:
                stuck.append(there)
        if not had_trash:
            try:
                trash_dir().rmdir()  # only if it's empty again
            except OSError:
                pass
        if stuck and not isinstance(e, KeyboardInterrupt):
            shown = " and ".join(f"widgets\\{p.relative_to(widgets_dir())}" for p in stuck)
            raise Refused(500, f"{e} Part of the import couldn't be undone, so look at {shown}.") from e
        raise
    return placed


# --- Live data in text (docs/data.md) -----------------------------------------------------
#
# The sources the player polls are set up in playlist/data.json. Their values come from the player's own API,
# which only answers on this PC: the page reaches it through the proxies here. data.json can hold a Home
# Assistant token or a plugin's password, which never goes back to the page.

DATA_API_PORT = 8139
DATA_TIMEOUT = 2   # seconds: the player answers straight away, or isn't running
PLAYER_POLL_LIMIT = 30  # the player gives up on a poll after this (a plugin's run, a web request's 20 s)
TEST_TIMEOUT = PLAYER_POLL_LIMIT + 15  # a test polls once: wait out the player's own limit, so its error is shown
RESCAN_TIMEOUT = 10
LONGEST_INTERVAL = 86400  # seconds: the player never waits longer between polls (catalog::LONGEST_INTERVAL)
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
     "running with Shared Memory Support on, or Libre Hardware Monitor with its Remote Web Server on.",
     "interval": {"default": 60, "min": 5},
     "settings": [{"key": "hwinfo", "label": "Read HWiNFO too", "type": "bool", "default": False},
                  {"key": "lhm", "label": "Read Libre Hardware Monitor sensors", "type": "bool", "default": False},
                  {"key": "lhm_url", "label": "Libre Hardware Monitor address", "type": "text",
                   "default": "http://127.0.0.1:8085"},
                  {"key": "lhm_user", "label": "Libre Hardware Monitor user name", "type": "text", "default": "",
                   "help": "Only if its Options > Remote Web Server > Authentication is on: the user name set there."},
                  {"key": "lhm_password", "label": "Libre Hardware Monitor password", "type": "password",
                   "help": "The password set in its Options > Remote Web Server > Authentication. Changing the address "
                           "or user name clears it, so type it last."}],
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
                  {"key": "hwinfo.*", "label": "HWiNFO readings", "pattern": True},
                  {"key": "lhm.*", "label": "Libre Hardware Monitor sensors", "pattern": True}]},
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
# Settings saved as typed, spaces at the ends included: LHM compares its user name and password exactly. Every other
# text setting and password (an address, a pasted token) is trimmed.
AS_TYPED = {"pc": {"lhm_user", "lhm_password"}}
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


def keys_of_type(kind, t):
    """The keys of a source's settings of one type: in its manifest, and for a built-in, in BUILTIN_CATALOG too, so
    a player from before a setting was added (still running, say) can't make the studio forget what it is."""
    builtin = [s for m in BUILTIN_CATALOG if m["id"] == kind for s in m["settings"]]
    return {k for k, s in settings_of(kind).items() if s.get("type") == t} | {s["key"] for s in builtin if s.get("type") == t}


def password_keys(kind, source=()):
    """The settings of a source that are secrets: never returned, never logged. Besides the manifest's `password`
    settings (Home Assistant's token, Libre Hardware Monitor's password, a plugin's), any key that looks like one
    (`token`, `api_key`), so a plugin's secret is kept back even before the player has told us its manifest."""
    keys = keys_of_type(kind, "password")
    keys |= {k for k in source if isinstance(k, str) and SECRET_KEY.search(k)}
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
    call its server setting anything, so all of them count; lists, numbers, bools and choices may differ freely.
    A user name counts too (This PC's lhm_user, beside lhm_url): a password goes with the login it was typed for."""
    norm = lambda v: v.strip().rstrip("/").lower() if isinstance(v, str) else ""
    keys = keys_of_type(kind, "text") | {"url"}
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
    trim = t in ("text", "choice") and k not in AS_TYPED.get(kind, ())
    return v.strip() if isinstance(v, str) and trim else v


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
                    if isinstance(new, str) and new.strip():  # one that's all spaces is blank, as in LHM
                        merged[k] = new if k in AS_TYPED.get(kind, ()) else new.strip()
                    elif not same_server(kind, merged, was):
                        # a saved secret only ever goes back to the server it was given for: anyone who can reach
                        # the studio could otherwise point the address at their own and have it sent there
                        merged.pop(k, None)
                s = merged
                if "interval" in s:
                    try:
                        s["interval"] = min(LONGEST_INTERVAL, max(min_interval(kind), int(s["interval"])))
                    except (TypeError, ValueError, OverflowError):
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


def player_api(path, body=None, timeout=DATA_TIMEOUT, slow_ok=False):
    """Asks the player's local API: its answer, or {"player": False} when it isn't running. `slow_ok`: the
    request may take a while (a test), so no answer in time is an error, not a player that isn't there."""
    req = urllib.request.Request(f"http://127.0.0.1:{data_port()}{path}",
                                 data=None if body is None else json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json"})
    try:
        with no_proxy.open(req, timeout=timeout) as r:
            return json.loads(r.read())
    except TimeoutError:  # it took the request, but gave no answer in time
        if slow_ok:
            return {"error": f"The player gave no answer in {timeout} s"}
        return {"player": False}
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
    return player_api("/test", {"source": source}, TEST_TIMEOUT, slow_ok=True)


# --- Server ----------------------------------------------------------------------

# The most a POST's body may be, by route, checked from Content-Length before anything is read (every other: 1 MB).
BODY_LIMITS = {"/api/widgets/import": 64 << 20, "/api/upload": 1 << 30, "/api/widgets/upload": 1 << 30,
               "/api/playlist": 16 << 20, "/api/widgets/export": 4 << 20, "/api/widgets/new": 2 << 20,
               "/api/widgets/save": 2 << 20}


def failure(e):
    """(status, answer) for what went wrong in a widget route: {"error": "<a sentence for the page>"}."""
    if isinstance(e, Refused):
        return e.status, {"error": str(e), **e.extra}
    if isinstance(e, ValueError):  # a bad request (JSON that isn't, an id that isn't one, a file that isn't)
        return 400, {"error": str(e)}
    if isinstance(e, RecursionError):  # a body whose arrays and objects nest past what json.loads reads
        return 400, {"error": "That request nests arrays and objects too deeply."}
    if isinstance(e, FileNotFoundError):
        return 404, {"error": f"{Path(str(e.filename or 'That file')).name} isn't there."}
    if isinstance(e, SystemExit):  # wv's helpers exit with a message on failure (Aseprite missing)
        return 400, {"error": str(e)}
    if isinstance(e, OSError) and in_use(e):
        return 500, {"error": f"{Path(e.filename or 'a file').name} is open in another program: try again."}
    if isinstance(e, OSError):
        return 500, {"error": f"{e.strerror or e}{f' ({Path(e.filename).name})' if e.filename else ''}"}
    return 500, {"error": f"{type(e).__name__}: {e}"}


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
            self.close_connection = True  # its body, if any, is never read
            self.send(403, b"forbidden", "text/plain")
        return ok

    def handle(self):
        try:
            super().handle()
        except ConnectionError:
            pass  # the browser went away mid-reply (closed tab, cancelled image load)

    def send(self, code, body, ctype, headers=None):
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(body)

    def json(self, obj, code=200):
        self.send(code, json.dumps(obj).encode(), "application/json")

    def answer(self, fn, *args):
        """Answers with fn(*args) as JSON, or with what went wrong (failure). Every widget route goes through
        here, so nothing it raises, a SystemExit included, gets past it."""
        try:
            out, code = fn(*args), 200
        except KeyboardInterrupt:
            raise
        except BaseException as e:
            code, out = failure(e)
        try:
            body = json.dumps(out, allow_nan=False).encode()
        except ValueError:  # NaN or Infinity, which the page's JSON.parse refuses: an error it can read instead
            code = 500
            body = json.dumps({"error": "The studio's answer held a number that isn't one (NaN or Infinity)."}).encode()
        self.send(code, body, "application/json")

    def read_body(self, path):
        """The request's body, read only when Content-Length is a whole number within the route's limit. Otherwise
        it's answered here, 400 or 413, before a byte is read, and this gives None."""
        length = self.headers.get("Content-Length", "0").strip()
        limit = BODY_LIMITS.get(path, 1 << 20)
        if not re.fullmatch(r"[0-9]{1,19}", length):
            error, code = "Content-Length must be a whole number of bytes.", 400
        elif int(length) > limit:
            error, code = f"That's too big: at most {limit >> 20} MB here.", 413
        else:
            return self.rfile.read(int(length))
        self.close_connection = True  # the body stays unread
        self.json({"error": error}, code)
        return None

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
            # "widgets": 1 tells a newer page this server has the widget routes (an older one has no such key)
            self.json({"screen": [wv.SCREEN_W, wv.SCREEN_H], "safe": wv.SAFE, "camera": wv.CAMERA_HOLE,
                       "font": str(DEFAULT_FONT), "initial": initial, "widgets": 1,
                       "widgets_folder": str(widgets_dir().absolute()), "player_widgets": player_widgets})
        elif (m := re.fullmatch(r"/api/frame/(\w+)/(\d+)\.png", url.path)) and (src := source_by_id(m[1])):
            i = int(m[2])
            if i < len(src.frames):
                return self.send(200, src.png(i), "image/png")
            self.send(404, b"no such frame", "text/plain")
        elif (m := re.fullmatch(r"/api/video/(\w+)\.mp4", url.path)) and (preview := video_preview(m[1])):
            self.send_file(preview, "video/mp4")
        elif url.path == "/api/widgets":
            self.answer(list_widgets)
        elif url.path.startswith(("/api/media/inputs/", "/api/media/widgets/")):
            self.answer(media_get, url.path[len("/api/media/"):])
        elif url.path == "/api/playlist":
            try:
                self.json(read_document())
            except ValueError as e:
                self.json({"error": str(e)}, 500)
        elif url.path == "/api/player":
            self.json(player_state())
        elif url.path == "/studio.js":
            self.send(200, SCRIPT.read_bytes(), "text/javascript; charset=utf-8")
        elif url.path[1:] in WIDGET_SCRIPTS:
            try:
                script = WIDGET_SCRIPTS[url.path[1:]].read_bytes()
            except OSError:
                return self.send(404, b"not found", "text/plain")
            self.send(200, script, "text/javascript; charset=utf-8")
        elif (m := re.fullmatch(r"/api/media/([\w.-]+)", url.path)):
            try:
                self.json(open_kept(m[1]))
            except KeyboardInterrupt:
                raise
            except BaseException as e:  # SystemExit too: wv.aseprite exits when Aseprite is missing
                self.json({"error": str(e)}, 404)
        elif url.path == "/api/fonts":
            self.json(font_list())
        elif url.path == "/api/font" and "widget" in (query := parse_qs(url.query)):
            try:
                path = widget_font(query["widget"][0], query.get("name", [""])[0])
            except KeyboardInterrupt:
                raise
            except BaseException as e:
                code, out = failure(e)
                return self.json(out, code)
            self.send_file(path, "font/otf" if path.suffix.lower() == ".otf" else "font/ttf")
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

    def widget_post(self, path, query, body):
        """The widget routes (docs/widgets.md): answers and gives True, or gives False for any other path."""
        q = lambda k, default=None: query.get(k, [default])[0]
        req = lambda: json.loads(body)
        if path == "/api/widgets/new":
            self.answer(lambda: new_widget(req()))
        elif path == "/api/widgets/save":
            self.answer(lambda: save_widget(req()))
        elif path == "/api/widgets/duplicate":
            self.answer(lambda: duplicate_widget(req()))
        elif path == "/api/widgets/delete":
            self.answer(lambda: delete_widget(req()))
        elif path == "/api/widgets/restore":
            self.answer(lambda: restore_widget(req()))
        elif path == "/api/widgets/upload":
            self.answer(widget_upload, q("id"), q("name", "upload.png"), body)
        elif path == "/api/media/hold":
            self.answer(lambda: hold_media(req()))
        elif path == "/api/media/copy":
            self.answer(lambda: copy_media(req()))
        elif path == "/api/widgets/import":
            self.answer(import_widgets, body, q("mode"), q("choices"), q("name"))
        elif path == "/api/widgets/export":
            try:
                r = req()
                data, notes = export_widget(r)
            except KeyboardInterrupt:
                raise
            except BaseException as e:
                code, out = failure(e)
                self.json(out, code)
                return True
            # the id is a checked one: nothing in it needs quoting
            self.send(200, data, "application/zip", {"Content-Disposition": f'attachment; filename="{r["id"]}.zip"',
                                                     "X-Widget-Notes": notes_header(notes)})
        else:
            return False
        return True

    def do_POST(self):
        if not self.trusted(post=True):
            return
        url = urlparse(self.path)
        body = self.read_body(url.path)
        if body is None:
            return
        query = parse_qs(url.query)
        if self.widget_post(url.path, query, body):
            return
        try:
            if url.path == "/api/upload":
                name, to = Path(query.get("name", ["upload.png"])[0]).name, query.get("to", [None])[0]
                if to == "inputs":  # an input picture is new: refused as the widget routes refuse (400, a sentence)
                    self.answer(upload, body, name, to)
                else:  # a scene's upload answers as it always has
                    self.json(upload(body, name, to))
            elif url.path == "/api/export":
                self.json(export_scene(json.loads(body)))
            elif url.path == "/api/reveal":
                os.startfile(reveal_folder(json.loads(body) if body.strip() else {}))
                self.json({})
            elif url.path == "/api/playlist":
                self.json(save_document(json.loads(body)))
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
        kept, src_name = keep_file(path.read_bytes(), path.suffix)
        initial = open_media(kept, src_name, path.name)
    player_features()  # whether the built player draws widgets, for /api/config
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
