# Live data in text (spec)

Text widgets (see widgets.md) can contain placeholders that the player fills with live values:
Claude usage, PC sensors, Home Assistant entities and the time. Sources are set up in the studio; the
player polls them and draws the text.

## Placeholders

| written | shows |
|---|---|
| `{key}` | the value in its default form |
| `{key:fmt}` | formatted (see below) |
| `{key\|fallback}` | `fallback` when the value is missing or stale |
| `{key:fmt\|fallback}` | both |
| `{{` and `}}` | literal braces |

- Keys are lowercase and dotted, e.g. `claude.week` or `pc.gpu.temp`. Unknown, missing or stale values with
  no fallback show `--`.
- Default forms:
  - Numbers: whole numbers have no decimals; others are rounded to 1 decimal place with a trailing `.0`
    dropped.
  - Times: `%H:%M`, local time.
  - Text: as is.
- Formats:
  - `d`: a whole number, rounded.
  - `.Nf`: N decimals.
  - Strftime-style patterns (`%H:%M`, `%a %d %b`, ...) for times.
  - A numeric format on text, or a time format on a number, is ignored.
- A widget whose text has at least one placeholder is **live**. Anything else is static, as before.

## Sources: `playlist/data.json`

```json
{
  "api_port": 8139,
  "sources": [
    { "type": "claude", "enabled": true, "interval": 300 },
    { "type": "pc", "enabled": true, "interval": 60, "hwinfo": true },
    { "type": "homeassistant", "enabled": true, "interval": 60,
      "url": "http://homeassistant.local:8123", "token": "<long-lived token>",
      "entities": ["sensor.living_room_temperature", "weather.home"] }
  ]
}
```

- Every source has `enabled` and `interval`, in seconds, defaulting to 60 with a minimum of 5. Claude's
  default is 300 and its minimum 60: its endpoint rate-limits a login that polls too often (HTTP 429).
- Values go stale after 3 x interval without a successful update, except Claude's, which last 60 minutes.
  The last good values are kept in `%LOCALAPPDATA%\wvplay\last-values.json` (values and their time
  only, never a token), so they are back right after a restart, with their real age, and a source whose
  last success is younger than its interval isn't polled again at start. A source that is told to wait
  (429 or 503, honouring `Retry-After`), or Claude on any error, backs off: 2, 4, 8 minutes, up to 30.
- `time` is built in and always there.
- The file lives in `playlist/`, which is gitignored, because it holds the HA token. The token is never
  logged, and never sent back to the browser.

### Keys each source provides

- **claude:** Claude plan usage, from the endpoint Claude Code's `/usage` uses.
  - **Reading the login:** on each poll, read `%USERPROFILE%\.claude\.credentials.json` and take
    `claudeAiOauth.accessToken` and `expiresAt` (in ms).
    - If the token has expired, skip the request and let the values go stale.
    - **Never** refresh the token, write that file, or log or store the token. Claude Code refreshes it
      whenever it runs.
  - **Request:** `GET https://api.anthropic.com/api/oauth/usage`, with headers
    `Authorization: Bearer <token>`, `anthropic-beta: oauth-2025-04-20` and `User-Agent: wvplay`.
    The endpoint is undocumented, so parse leniently and skip fields that are missing.
  - **Keys:**
    - From `five_hour`: `claude.session`, the utilization in percent; `claude.session_resets`, a time;
      `claude.session_resets_in`, text like `2h 34m` or `45m`.
    - From `seven_day`: `claude.week`, `claude.week_resets` and `claude.week_resets_in`.
    - From the `limits` array: `claude.session_severity` and `claude.week_severity`, taken from the entries
      whose `kind` is `session` and `weekly_all` (e.g. `normal`, `critical`).
    - For each `weekly_scoped` entry whose `scope.model.display_name` is set:
      `claude.week.<name, lowercased>` holding its `percent`.
- **pc:**
  - **From Windows:**
    - `pc.cpu.load` in percent, from GetSystemTimes deltas.
    - From GlobalMemoryStatusEx: `pc.ram.used_gb`, `pc.ram.total_gb` and `pc.ram.used_pct`.
  - **From NVIDIA** (NVML, loaded from nvml.dll at runtime; skipped if missing):
    - `pc.gpu.temp` (°C), `pc.gpu.load` (%) and `pc.gpu.power` (W);
    - `pc.gpu.vram_used_gb` and `pc.gpu.vram_total_gb`.
  - **From HWiNFO**, with `"hwinfo": true`, if HWiNFO is running with Shared Memory Support on:
    - Every reading becomes `hwinfo.<sensor>.<label>`, both parts slugified to lowercase a-z0-9 and `_`.
    - `pc.cpu.temp` is the CPU die temperature: the reading labelled like "CPU (Tctl/Tdie)", else the first
      CPU temperature.
- **homeassistant:**
  - Each interval, one `GET <url>/api/states` with `Authorization: Bearer <token>`, filtered to the listed
    entities.
  - Each entity gives:
    - `ha.<entity_id>`: its state, as a number when it's numeric;
    - `ha.<entity_id>.unit`: from `unit_of_measurement`;
    - `ha.<entity_id>.<attribute>`: one per attribute that is a plain value.
- **time:** `time` is the current local time.

## The player

- **Polling:**
  - Each enabled source is polled on its own thread, at its own interval.
  - data.json is watched like the playlist: changes are picked up once they settle for about 1.5 s.
  - Failures are logged briefly (never with tokens), and the values just go stale.
- **Local API**, on `127.0.0.1:<api_port>`:
  - It only answers when the Host header is `127.0.0.1` or `localhost`. It's for the studio, which
    proxies it to the browser.
  - `GET /values` returns `{"<key>": {"value": ..., "at": "<iso time>", "stale": bool, "source": "claude"}}`.
    Times are ISO strings.
  - `POST /resolve {"texts": ["CPU {pc.cpu.load:d}%", ...]}` returns `{"texts": [...]}`, filled with the
    current values. This is the single source of truth for formatting.
  - `POST /test {"source": {...}}` runs one source config once, without saving it, and returns
    `{"values": {...}}` or `{"error": "..."}`.
- **Drawing live text:**
  - Static widgets are drawn into the prepared frames as now.
  - Live widgets are drawn into a separate live layer, while playing, over the prepared frames. Live text
    always draws above static text: the one difference from list order.
  - **When to redraw:** the live layer is redrawn only when some live widget's resolved text changes. For
    the time, that's checked when the minute changes or values update.
  - **Building frames:** each frame is the cached JPEG decoded, the live layer blended over it, then
    re-encoded, rotated as before.
  - **Caching:** results are kept per text version, so an animation is re-encoded once per change, not
    every loop, and a still is re-encoded once.
  - **No live widgets:** exactly today's path, with zero extra cost.

## The studio

- **A "Data" section:**
  - A card per source type, with enable, interval and the type's fields (HA: URL, token, entities).
  - The HA token box is write-only: it shows "set" and lets you replace it.
  - A **Test** button shows the values that source returns now, or its error.
  - Changes save to data.json.
- **A live values table:** key and current value, filterable, with stale ones greyed out. Clicking a key
  inserts `{key}` into the selected widget's text at the cursor.
- **Preview:** widgets show their resolved text, via the player's `/resolve`. It refreshes when the text
  changes and about every 10 s. If the player isn't running, the raw placeholders show, and a note says so.
- **Endpoints:**
  - `GET/POST /api/data/config`: the GET leaves the token out and gives `token_set` instead; the POST keeps
    the existing token unless a new one is given.
  - `GET /api/data/values`, `POST /api/data/resolve` and `POST /api/data/test`: proxies to the player API.
    They answer `{"player": false}` when it's down.
