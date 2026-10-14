# Wonder Vision pixel art

Puts pixel art, GIFs and short videos on the Thermalright Wonder Vision 360's curved screen, as a looping
playlist, without TRCC:

- **`player/`** (Rust, `wvplay.exe`) drives the screen directly over USB and plays the playlist.
- **The studio** (`python wv.py studio`, in your browser) is where you fit art to the screen and build the
  playlist.

## First time

```
cd player
cargo build --release          # builds player\target\release\wvplay.exe
```

Only one program can drive the screen at a time, so close TRCC and InfoPanel before playing, and stop them
starting with Windows. TRCC does that through a Task Scheduler task called `TRCCAppStartup`. The studio
warns you if it finds one.

## Making a playlist

```
python wv.py studio                   # opens http://127.0.0.1:8137 in your browser
python wv.py studio "D:\gifs\cat.gif"
```

1. **Open some art:** drop a file on the page, use Open image, or paste with Ctrl+V. PNG, GIF, WebP, JPG,
   BMP, Aseprite, and MP4 or other video work.
2. **Fit it to the screen:**
   - **Size:** whole-number scales keep pixel art crisp. The Fit/Fill buttons size it to the flat front or
     the whole screen. If a file was saved enlarged (a 160px piece saved at 480px), the studio notices and
     counts in real art pixels. "Allow in-between sizes" and "Smooth scaling" are for photos and video.
   - **Position:** drag it, use the arrow keys, type X/Y, or use the align buttons. Drag a corner to resize:
     the shape always stays the same. Parts hanging off the screen show faded, so you can see what gets cut.
   - **Empty space:** a solid colour (it starts as the art's own background colour), stretched edge pixels,
     mirrored copies, or tiles.
   - **Curve guides:** the amber bands are the curved edges. The right one wraps around the side of the
     pump block.
3. **Add to playlist**, with how many seconds it stays on screen. Adding again under the same name replaces
   that piece.
4. **Play on cooler.** It loops the playlist, each piece for its time, and keeps playing after you close the
   studio. Reorder, retime or remove pieces in the list at any time: the player picks changes up within a
   second.
5. **Start playing when Windows starts** (the checkbox under Play) adds the player to your startup apps, so
   the playlist comes back after a reboot without opening the studio. It's per-user, needs no admin, and
   shows in Task Manager's Startup apps.

The playlist is `playlist/playlist.json` plus one file per piece: a 1600x720 PNG (animated PNG for
animations) or, for videos, an MP4. The studio remembers each image's layout in your browser.

**Videos** are kept at up to 30 frames a second and the first 30 seconds. Adding one to the playlist takes
about as long as the clip.

**Export files** writes the same layout to `out/` as PNG (animated if the art is), GIF or MP4, for use
elsewhere.

## Using the studio from other devices

Make a `studio.json` next to `wv.py`:

```json
{ "listen": ["localhost", "tailscale"], "port": 8137 }
```

`"tailscale"` means this PC's Tailscale address, looked up each time the studio starts. The studio then
prints a link like `http://mypc.tail1234.ts.net:8137/`, which works from your other devices on the tailnet.
Tailscale encrypts the connection and keeps everyone else out. Any device on your tailnet can use the studio,
so don't share the tailnet with people you wouldn't hand the cooler to. You can also list plain IP addresses.
Without the file, the studio only answers on this PC. Windows may ask once whether Python can accept
connections: allow it.

## The player

```
wvplay play playlist\playlist.json    # or one file, or a folder (each file --seconds S, default 30)
wvplay prepare <what>                 # do all the preparation without the screen, and time it
wvplay info                           # what the screen reports about itself
```

- It turns the picture 180° because the panel is mounted upside down (`--no-rotate` to turn that off).
- Every frame is converted to JPEG once, when a piece first plays, and kept in
  `%LOCALAPPDATA%\wvplay\cache` (up to 2 GB). After that, playing is just sending stored bytes on a timer:
  about 0.1% of one CPU core and 11 MB of memory.
- It reconnects by itself after sleep or a replug, and re-sends stills every 2 s so the screen recovers if
  the panel resets.
- `wvplayw.exe` is the same program without a console window. The studio and the startup entry use it, logging
  to `.cache\wvplay.log`. Video pieces need ffmpeg, which they pass it (`--ffmpeg`).

How the screen works (from the InfoPanel fork by emaspa and the TRCC decompile in
Lexonight1/thermalright-trcc-linux):
- It's a WinUSB device, `87AD:70DB`.
- After a 64-byte handshake, each frame is a 64-byte header plus a JPEG, sent in one USB transfer.
- It takes **1600x720** and shows it 1.5x enlarged on its 2400x1080 glass. It also accepts 2400x1080, but
  then the outer columns flicker, which is why TRCC sends 1600x720 too.

## Sizes

Everything works in the panel's 1600x720. Art laid out at a whole-number fraction of that upscales cleanly
(`python wv.py sizes`):

| scale | canvas  | on the glass |
|-------|---------|--------------|
| @2    | 800x360 | each art pixel 3x3 glass pixels |
| **@4**| **400x180** | **6x6: the default, about the look of 160px Corsair art** |
| @5    | 320x144 | slightly uneven: avoid odd scales |
| @8    | 200x90  | 12x12, chunky |
| @10   | 160x72  | 15x15 |

Measured from a photo of a test chart on the cooler:
- Top and bottom are flat to the edge.
- The left edge curls away within about 68 px.
- The right ~360 px wrap around the side of the pump block.

So the flat front is x 68..1240, about 1172x720. Art in the right-hand wrap is seen from the side, so it
suits scenery that carries on round the corner, not anything you need to read from the front.

## Drawing your own (Aseprite)

```
python wv.py new forest               # art/forest.aseprite, 400x180, with curve guides; opens Aseprite
python wv.py export forest            # out/forest.png (animated PNG if animated)
python wv.py port "D:\old\cat.gif"    # put square Corsair art on a 20:9 canvas to extend sideways
```

The canvas has a locked `guides` layer that export skips. Animate with normal Aseprite frames: per-frame
durations are kept.

## Notes

- **TRCC kills any program named `ffmpeg`.** That's in its code (the decompile shows it), and it's why
  `wv.py` runs ffmpeg through a renamed copy at `.cache/wv-ffmpeg.exe`.
- Pieces are lossless up to the player, which sends JPEG at quality 95 with full-resolution colour (4:4:4)
  for art, and 4:2:0 for video.
- GIFs and PNGs get an exact palette built from the art when it has 256 colours or fewer, so nothing is
  dithered.
- On an OLED, black pixels are switched off, so black backgrounds are free and help against burn-in. A
  static, bright image shown 24/7 is the main burn-in risk. A playlist that moves between pieces helps.
- If Windows Photos has a file open, it can't be overwritten. The studio says so if that happens.
