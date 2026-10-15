"""Studio: a scene editor for the Wonder Vision screen (docs/scenes.md), the playlist, and the player.

  python wv.py studio [image]

Serves studio/index.html (and studio.js) to a browser on this PC only. The page draws the scenes and edits the
playlist document, which saves here as it changes; the player draws the same document on the cooler.
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
from urllib.parse import parse_qs, urlparse

import numpy as np
from PIL import Image, ImageFont

import wv

PAGE = wv.ROOT / "studio" / "index.html"
SCRIPT = PAGE.with_name("studio.js")  # the page's code, served at /studio.js
UPLOADS = wv.ROOT / ".cache" / "studio"
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
        self.src = None  # its `src` in the playlist, once kept in sources/
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
        return info

    def png(self, i):
        if i not in self.pngs:
            buf = io.BytesIO()
            self.frames[i][0].save(buf, "PNG", compress_level=1)
            self.pngs[i] = buf.getvalue()
        return self.pngs[i]




# --- The playlist: scenes of layers and objects (docs/scenes.md) ---------------------------------
#
# playlist/playlist.json, version 2: scenes, each a stack of layers holding image, video, text and shape objects,
# plus a global layer stack over every scene. The page edits the whole document and sends it back as it changes;
# the player re-reads it whenever it changes, so editing here is live. Media lives in playlist/sources/, named by
# content hash. A version 1 file (one piece file plus text per item) is converted the first time it's read.

PLAYLIST_DIR = wv.ROOT / "playlist"
PLAYLIST = PLAYLIST_DIR / "playlist.json"
PLAYER = wv.ROOT / "player" / "target" / "release" / "wvplayw.exe"  # the one without a console window
PLAYER_LOG = wv.ROOT / ".cache" / "wvplay.log"
RUN_KEY = r"Software\Microsoft\Windows\CurrentVersion\Run"  # per-user startup apps
RUN_NAME = "Wonder Vision player"
RIVALS = re.compile(r"TRCC|InfoPanel|USBLCD", re.I)  # other programs that drive the cooler
rivals_seen = (0.0, [])
DEFAULT_SECONDS = 30
TIDY_AGE = 60  # seconds: a source newer than this may be an upload whose object hasn't been saved yet
# Requests run on their own threads, and edits save as they happen: one change to playlist.json at a time.
playlist_lock = threading.RLock()


def read_json(path):
    """A JSON file as it is, or {} before there is one."""
    with playlist_lock:
        try:
            return json.loads(path.read_text(encoding="utf-8"))
        except FileNotFoundError:
            return {}


def write_atomic(path, data):
    """Writes JSON all at once, so the player never reads half a file."""
    with playlist_lock:
        PLAYLIST_DIR.mkdir(exist_ok=True)
        tmp = path.with_suffix(".tmp")
        tmp.write_text(json.dumps(data, indent=2, ensure_ascii=False), encoding="utf-8")
        os.replace(tmp, path)


def sources_dir():
    return PLAYLIST_DIR / "sources"


def source_name(path):
    """The relative `src` of a kept file."""
    return f"sources/{Path(path).name}"


def keep_file(path, data=None):
    """Keeps a media file in sources/ under its content hash (a hard link where possible: no extra space).
    Returns (kept path, src)."""
    sources_dir().mkdir(parents=True, exist_ok=True)
    data = path.read_bytes() if data is None else data
    kept = sources_dir() / f"{hashlib.sha1(data).hexdigest()[:12]}{Path(path).suffix.lower()}"
    if not kept.exists():
        if path.exists():
            try:
                os.link(path, kept)
            except OSError:  # another drive
                shutil.copy2(path, kept)
        else:
            kept.write_bytes(data)
    return kept, source_name(kept)


def media_size(path):
    """(width, height) of a media file as the studio and player see it: for a video, after the decode's
    fit-to-screen scaling (decode_video), so a version 1 layout's scale applies to the same size."""
    if path.suffix.lower() in VIDEO_TYPES:
        ff = wv.find_ffmpeg()
        probe = subprocess.run([ff, "-hide_banner", "-i", str(path)], capture_output=True, text=True, errors="replace",
                               creationflags=NO_WINDOW).stderr if ff else ""
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
            _, art["src"] = keep_file(piece)
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
    """A version 1 text widget as a text object: the widget's keys plus the common ones."""
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
        return doc


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
    for s in doc["scenes"]:
        try:
            s["seconds"] = max(1, min(86400, int(s.get("seconds") or DEFAULT_SECONDS)))
        except (TypeError, ValueError):
            raise ValueError("a scene's seconds must be a number") from None
    return doc


def used_sources(doc):
    return {o.get("src") for holder in [doc.get("global", {}), *doc.get("scenes", [])]
            for layer in holder.get("layers", []) for o in layer.get("objects", []) if isinstance(o, dict)}


def tidy_sources(doc):
    """Deletes kept media that no object uses any more, leaving fresh uploads whose objects may not be saved yet."""
    used = used_sources(doc)
    if sources_dir().exists():
        for path in sources_dir().iterdir():
            if source_name(path) not in used and time.time() - path.stat().st_mtime > TIDY_AGE:
                path.unlink(missing_ok=True)


def save_document(doc):
    """The page's whole document, saved as it is (unknown keys and all)."""
    doc = check_document(doc)
    with playlist_lock:
        write_atomic(PLAYLIST, doc)
        tidy_sources(doc)
    return {"ok": True}


def media_info(src):
    """What the page needs to draw a kept media file: its frames' urls, size, timing and `pixel` factor."""
    info = {**src.info, "src": src.src, "animated": bool(src.video) or len(src.info["durations"]) > 1,
            "pixel": src.info["factor"]}
    info["type"] = "video" if src.video else "image"
    return info


def upload(body, name):
    """A dropped or picked file: kept in sources/ by content hash, opened, and described for the page."""
    if not body:
        raise ValueError("the file is empty")
    suffix = Path(name).suffix.lower()
    if suffix not in VIDEO_TYPES and suffix not in (".png", ".gif", ".webp", ".jpg", ".jpeg", ".bmp", ".apng", ".aseprite", ".ase"):
        raise ValueError(f"{suffix or 'that'} isn't a kind of file the studio can open")
    kept, src_name = keep_file(Path(name), body)
    try:
        info = open_kept(src_name, name)
    except Exception:
        if time.time() - kept.stat().st_mtime < 5:  # just written and unreadable: not worth keeping
            kept.unlink(missing_ok=True)
        raise
    return info


def open_kept(src_name, name=None):
    """Opens sources/<file> (cached) and describes it."""
    file = Path(src_name).name
    if not re.fullmatch(r"[\w.-]+", file):
        raise ValueError("bad source name")
    path = sources_dir() / file
    if not path.is_file():
        raise FileNotFoundError(f"{src_name} isn't in the playlist folder")
    for s in sources.values():
        if s.src == source_name(path):
            return media_info(s)
    src = Source(path, name or file)
    src.src = source_name(path)
    while len(sources) >= 12:  # each holds every frame in memory (videos as JPEG)
        sources.pop(next(iter(sources)))
    sources[src.id] = src
    return media_info(src)


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
    return {"running": running, "log": last, "built": PLAYER.exists(), "autostart": autostart(), "rivals": rivals()}


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
            try:
                self.json(read_document())
            except ValueError as e:
                self.json({"error": str(e)}, 500)
        elif url.path == "/api/player":
            self.json(player_state())
        elif url.path == "/studio.js":
            self.send(200, SCRIPT.read_bytes(), "text/javascript; charset=utf-8")
        elif (m := re.fullmatch(r"/api/media/([\w.-]+)", url.path)):
            try:
                self.json(open_kept(m[1]))
            except (OSError, ValueError) as e:
                self.json({"error": str(e)}, 404)
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
            if url.path == "/api/upload":
                name = Path(parse_qs(url.query).get("name", ["upload.png"])[0]).name
                self.json(upload(body, name))
            elif url.path == "/api/export":
                self.json(export_scene(json.loads(body)))
            elif url.path == "/api/reveal":
                os.startfile(wv.OUT)
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
        _, src_name = keep_file(path)
        initial = open_kept(src_name, path.name)
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
