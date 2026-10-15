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
    { "type": "pc", "enabled": true, "interval": 60, "hwinfo": false,
      "lhm": true, "lhm_url": "http://127.0.0.1:8085" },
    { "type": "homeassistant", "enabled": true, "interval": 60,
      "url": "http://homeassistant.local:8123", "token": "<long-lived token>",
      "entities": ["sensor.living_room_temperature", "weather.home"] }
  ]
}
```

- Every source has `enabled` and `interval`, in seconds, defaulting to 60 with a minimum of 5 and a maximum
  of 86400 (a day). Claude's default is 300 and its minimum 60: its endpoint rate-limits a login that polls
  too often (HTTP 429).
- Values go stale after 3 x interval without a successful update, except Claude's, which last 60 minutes.
  The last good values are kept in `%LOCALAPPDATA%\wvplay\last-values.json` (values and their time
  only, never a token), so they are back right after a restart, with their real age, and a source whose
  last success is younger than its interval isn't polled again at start. A source that is told to wait
  (429 or 503, honouring `Retry-After`), or Claude on any error, backs off: 2, 4, 8 minutes, up to 30. A
  streaming plugin isn't polled, so it never backs off.
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
  - **Settings:**
    - `hwinfo` (default `false`): read HWiNFO too.
    - `lhm` (default `false`): read Libre Hardware Monitor (LHM) too.
    - `lhm_url` (default `http://127.0.0.1:8085`): where LHM's web server answers. Blank means the default,
      and `http://` is assumed when no scheme is given. A trailing `/data.json` (the address a browser
      shows for the data) is dropped.
  - **From Windows:**
    - `pc.cpu.load` in percent, from GetSystemTimes deltas.
    - From GlobalMemoryStatusEx: `pc.ram.used_gb`, `pc.ram.total_gb` and `pc.ram.used_pct`.
  - **From NVIDIA** (NVML, loaded from nvml.dll at runtime; skipped if missing):
    - `pc.gpu.temp` (°C), `pc.gpu.load` (%) and `pc.gpu.power` (W);
    - `pc.gpu.vram_used_gb` and `pc.gpu.vram_total_gb`.
  - **From HWiNFO**, with `"hwinfo": true`, if HWiNFO is running with Shared Memory Support on:
    - Every reading becomes `hwinfo.<sensor>.<label>`, both parts slugified to lowercase a-z0-9 and `_`.
  - **From Libre Hardware Monitor**, with `"lhm": true`, if LHM is running with its Remote Web Server on
    (setup below):
    - **Request:** `GET <lhm_url>/data.json`, given up after 3 s in all, so a missing LHM never holds up
      the poll.
      - On this PC (127.0.0.1, ::1, localhost) it never goes through a proxy that `HTTP_PROXY` or
        `ALL_PROXY` names. `localhost` is asked at 127.0.0.1 only: LHM answers there whenever it answers at
        ::1, and Windows takes about 2 s to give up on each address.
      - A reply over 4 MB isn't read (a real one is well under 1 MB). At most 4096 sensors are read, with a
        note when there are more.
    - **The reply** is LHM's sensor tree. It goes: a root ("Sensor"), the computer, its hardware (which can
      nest, like a motherboard's SuperIO chip), a group per sensor type ("Temperatures"), then the sensors.
      - Every node has `id`, `Text`, `Min`, `Value`, `Max`, `ImageURL` and `Children`.
      - Sensors also have `SensorId` (like `/amdcpu/0/temperature/2`) and `Type` (like `Temperature`).
        Since 0.9.5 they also have `RawMin`, `RawValue` and `RawMax`, and hardware has `HardwareId`.
      - Without these fields (older versions), a group is a node whose children are all leaves.
    - **Keys:** every sensor with a reading becomes `lhm.<hardware>.<type>.<sensor>`, each part slugified,
      with accents dropped ("Température" is `temperature`):
      - `<hardware>` is the nearest hardware node, e.g. `lhm.nuvoton_nct6799d.temperature.cpu` for the
        motherboard's chip.
      - A name with nothing left in a-z or 0-9 (a network adapter Windows calls "イーサネット", say) is
        its id instead: the hardware's whole `HardwareId` (`nic_<guid>`), or the type and number that end
        the sensor's `SensorId` (`temperature_3`). With no id either, it's `hardware` or `sensor`.
      - `<type>` is the sensor type in lowercase, from `Type`: `temperature`, `load`, `clock`, `power`,
        `fan`, `voltage`, `current`, `data`, `smalldata`, `throughput`, `control`, `level`, `factor`,
        `frequency`, `energy`, `noise`, `timespan`, `timing`, `flow`, `conductivity`, `humidity`.
        Without `Type`, it comes from the group's name: "Temperatures" is `temperature`, "Capacities"
        `energy`, "Times" `timespan`.
      - Two hardware nodes with the same name (two identical GPUs, say) are told apart in tree order: the
        second is `<name>_2`, the third `<name>_3`. Any key still taken gets `_2`, `_3` on the end the
        same way. Sensors with no reading count too, so a key stays with its sensor while another one's
        reading comes and goes.
    - **Values** are numbers, in LHM's units:
      - The units: °C, %, MHz, W, RPM and V. Throughput is in B/s, except "Connection Speed", in bit/s.
      - Data is in GB and small data in MB, as in 0.9.6. LHM's newest code gives data in bytes, which the
        player turns into GB. It has no small data: what was small data (GPU memory, say) comes as data,
        so its key moves from `smalldata` to `data`, in GB.
      - The number comes from `RawValue` when there is one: a number in LHM's newest code, text like
        "45.3 °C" in 0.9.5 and 0.9.6. Otherwise it's the number `Value` starts with. Before 0.9.5, that's
        the only one, and it shows throughput scaled ("512.0 KB/s", "1.2 MB/s", "1Gbps"), which is turned
        back into B/s (bit/s).
      - Both texts follow Windows' Regional format (Settings > Time & language > Language & region), not
        its display language. So "45,3 °C" reads as 45.3, and so do "45٫3 °C" (Arabic, Persian) and
        "45/3 °C" (Persian, on LHM's .NET Framework build). Invisible direction marks around a minus sign
        are skipped.
      - A time span ("1:02:03") becomes seconds. A big one in `RawValue` that the .NET Framework build
        writes with an exponent ("1.234568e+07") reads as 12345680. A temperature that LHM is set to show in
        °F is turned back into °C.
      - Sensors with no value ("-") are left out.
    - **If LHM doesn't answer** (not running, web server off, not its data), the poll gets a note:
      "Libre Hardware Monitor isn't answering at `<url>` (why): check that it's running, with Options >
      Remote Web Server on". The rest of the pc values come through as usual: the pc source doesn't fail
      or back off.
  - **`pc.cpu.temp`** is the CPU die temperature:
    - **HWiNFO's**, when HWiNFO is on and gave one: the reading labelled like "CPU (Tctl/Tdie)", else the
      first CPU temperature.
    - **Else LHM's**, from the CPU hardware (a `HardwareId` or `SensorId` like `/amdcpu/0` or
      `/intelcpu/0`):
      - "Core (Tctl/Tdie)" on AMD. Where LHM shows them apart: "Core (Tdie)", then "Core (Tctl)".
      - "CPU Package" on Intel.
      - Otherwise "Core Max", then "Core Average", then the CPU's first temperature.
- **homeassistant:**
  - Each interval, one `GET <url>/api/states` with `Authorization: Bearer <token>`, filtered to the listed
    entities.
  - Each entity gives:
    - `ha.<entity_id>`: its state, as a number when it's numeric;
    - `ha.<entity_id>.unit`: from `unit_of_measurement`;
    - `ha.<entity_id>.<attribute>`: one per attribute that is a plain value.
- **time:** `time` is the current local time.

### Setting up Libre Hardware Monitor

LHM is open source and free, with no time limit. HWiNFO's free Shared Memory Support stops after 12
hours.

1. **Install it:** `winget install LibreHardwareMonitor.LibreHardwareMonitor`, or unzip a release from
   <https://github.com/LibreHardwareMonitor/LibreHardwareMonitor/releases>.
2. **Run it as administrator.**
   - CPU temperatures, and the motherboard's sensor chip, need its driver, which only loads when LHM runs
     elevated. Newer versions use PawnIO and offer to install it.
   - Without the driver, LHM still runs, but `pc.cpu.temp` and most temperatures are missing.
3. **Turn on the web server:**
   - Under Options > Remote Web Server > Interface / Port, set the port to 8085, the player's default.
   - Then click Options > Remote Web Server > Run. LHM remembers this.
4. **Keep the web server to this PC.** This is the step to get right.
   - **The aim:** listen on 127.0.0.1 only, never on 0.0.0.0. 0.0.0.0 publishes the server to every network
     the PC is on, and this PC's firewall is off for Private networks.
   - **LHM can't do it by itself (0.9.6, and its current code).** Its Interface list offers only the PC's
     own network addresses and 0.0.0.0. When the address it's set to isn't one of those, including
     127.0.0.1 typed into its `.config`, it listens on every interface instead.
   - **Why it matters:** with no password, anyone who reaches the server can read every sensor. They can
     also change fan and pump control values (`GET /Sensor?action=Set&id=...&value=...`) and reset
     min/max. Any web page open in a browser on this PC can send those requests too.
   - **What helps:**
     - **Windows Firewall on for the Private profile**, with no allow rule for port 8085, keeps other
       machines out. This PC's own requests always get through.
     - **Choosing the PC's LAN address** limits the server to that one network. `lhm_url` must then be
       that address, and it breaks if the address changes.
     - **Options > Remote Web Server > Authentication** (a user name and password) keeps everyone out,
       including web pages. But the player doesn't send a password yet.
5. **Tick Options > Run On Windows Startup while LHM is running as administrator.**
   - Ticked like that, LHM makes a logon task that runs with highest privileges. The driver then loads
     after a restart with no prompt.
   - Ticked without elevation, LHM uses the registry Run key instead, and CPU temperatures go missing after
     a restart.
   - Start Minimized and Minimize To Tray keep it out of the way.
6. **Turn it on in the studio:**
   - In the Data sources window, on This PC, tick **Read Libre Hardware Monitor sensors**.
   - Change **Libre Hardware Monitor address** only if the port or interface changed.
   - **Test** lists the `lhm.*` keys, or, under the other values, why LHM didn't answer.

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
  - A card per source type, with enable, interval and the type's fields (pc: HWiNFO, Libre Hardware
    Monitor and its address; HA: URL, token, entities).
  - The HA token box is write-only: it shows "set" and lets you replace it.
  - A **Test** button shows the values that source returns now, or its error, and its notes on parts it
    skipped (LHM not answering, say).
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
