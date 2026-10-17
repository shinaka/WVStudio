# Live data in text (spec)

Text objects (see text.md) can contain placeholders that the player fills with live values:
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
- Inside a widget, `{$id}` is one of its inputs, filled in before placeholders are read, so a copy's value
  can be a placeholder of its own (see [widgets.md](widgets.md)).
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
- A text object whose text has at least one placeholder is **live**. Anything else is static, as before.

## Sources: `playlist/data.json`

```json
{
  "api_port": 8139,
  "sources": [
    { "type": "claude", "enabled": true, "interval": 300 },
    { "type": "pc", "enabled": true, "interval": 60, "hwinfo": false,
      "lhm": true, "lhm_url": "http://127.0.0.1:8085",
      "lhm_user": "cooler", "lhm_password": "<LHM password>" },
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
- The file lives in `playlist/`, which is gitignored, because it holds the HA token and LHM's password.
  Neither is ever logged, shown in a note or an error, or sent back to the browser.

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
      shows for the data) is dropped, and so is a user name and password written into it
      (`http://user:password@...`): the address shows in notes and in the studio, so use the next two.
      LHM's address has no `@` of its own, so that's everything up to the last `@` after the scheme, even
      when the password holds a `/`, `?` or `#`.
    - `lhm_user` (default blank) and `lhm_password` (a secret, like the HA token): the user name and
      password set in LHM's Options > Remote Web Server > Authentication. When either is set, they're sent.
      Both are kept and sent as typed, spaces at either end included, since LHM compares them exactly.
      Changing `lhm_url` or `lhm_user` in the studio drops the saved password, so it has to be typed again
      (see [The studio](#the-studio)).
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
      - **With `lhm_user` or `lhm_password` set**, it carries `Authorization: Basic <base64 of
        user:password>`, HTTP Basic, the only kind LHM takes. That goes to `lhm_url` alone:
        - never through a proxy, wherever the address is;
        - and no redirect is followed, with or without a login (LHM never redirects, so one is a note).
      - **How LHM checks it** (its `HttpServer.cs`, on .NET's `HttpListener`; 0.9.6 and its current code
        check the same way, but 0.9.6 and earlier lose the password each time LHM starts: see step 4 of the
        setup below):
        - Every path, `data.json` and `/Sensor` included, is behind the check.
        - A request with no login gets 401 and `WWW-Authenticate: Basic realm="Libre Hardware Monitor"`.
        - The base64 is read as Latin-1, then split at the first `:`. The user name must match exactly,
          case included. The password's SHA-256 (of its UTF-8) must match the stored hash.
        - So the player sends the login as Latin-1 bytes. One with a character outside Latin-1 (like €),
          or a user name with a `:`, can never match there, and the note says so.
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
    - **If LHM wants a login** (HTTP 401), the note says which case it is. `<url>` never holds a user name
      or password, and no note, log line, error or `/test` answer ever holds the password.
      - **None was set:** "Libre Hardware Monitor at `<url>` asks for a user name and password (its
        Options > Remote Web Server > Authentication is on, or was only just turned off: then turn Remote
        Web Server > Run off and on): set them on This PC in the studio's Data sources window". A running
        server keeps asking for a login after Authentication is unticked, until Run is turned off and on.
      - **The one set was refused:** "Libre Hardware Monitor at `<url>` refused the user name or password:
        check them against its Options > Remote Web Server > Authentication (if Authentication was only
        just turned on, turn Remote Web Server > Run off and on). LHM 0.9.6 and earlier lose that password
        whenever LHM restarts: type it there again".
      - **One LHM could never accept** (see above) is refused with why: "... it can't check a user name
        with a ':' in it, or any character outside Latin-1 (like €), so choose others ...".
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
4. **Turn on Authentication.** This is the step to get right.
   - **Why:** LHM 0.9.6's web server can't be kept to this PC (see step 5). It listens on the PC's network
     addresses, and this PC's firewall is off for Private networks. With no password, anyone who reaches
     port 8085 can read every sensor. They can also change fan and pump control values
     (`GET /Sensor?action=Set&id=...&value=...`) and reset min/max, and so can any web page open in a
     browser on this PC.
   - **How:** click Options > Remote Web Server > Authentication. In the "Set HTTP Credentials" dialog:
     - tick **Enable HTTP Basic Authentication**;
     - under Credentials, fill in **HTTP UserName** and **HTTP Password**;
     - click OK.
   - **Then restart the web server:** click Options > Remote Web Server > Run twice (off, then on). As the
     dialog says, a running server only takes the change once restarted. Until then it refuses every
     request, even one with the right password. (A new user name or password alone takes at once.)
   - **Choosing them:**
     - Both are matched exactly, case and spaces at either end included. The user name can't contain `:`.
     - Use only Latin-1 characters (no €, say): LHM can't match anything else, whatever sends it.
     - LHM keeps only the password's SHA-256, unsalted, in `LibreHardwareMonitor.config` beside its exe.
       If you forget it, set a new one. So pick a long random one you use nowhere else.
   - **Clicking OK in that dialog again later** depends on the version. The password box always opens
     empty.
     - **0.9.6 and earlier:** OK always sets the password to what's in the box. So type the password each
       time you click OK there, or click Cancel. OK with the box blank sets an empty password: LHM then
       lets in the user name with no password, and refuses the one the player sends.
     - **Builds with LHM's fix 95e97cf** (May 2026, in no release yet): a blank box keeps the password
       already set. A blank one the first time leaves no password at all, and every login is refused.
   - **0.9.6 and earlier lose the password each time LHM starts.** They store the password's hash, then
     hash that again when they start, so once LHM restarts (closed and opened again, or at a reboot), the
     password set there no longer matches. The player's note then says LHM refused it.
     - **Each time,** open Options > Remote Web Server > Authentication, type the password again (the
       user name is still there) and click OK. That takes at once, with no restart of the web server.
     - **Or use a build that has the fix** (95e97cf, May 2026; 0.9.6, February 2026, is the latest
       release): a nightly build, linked under "Nightly builds" in LHM's README, or the next release.
   - **Check it:** open <http://127.0.0.1:8085/> (or the address you chose) in a browser. It should ask
     for the user name and password. A browser that has logged in can send the login with other pages' requests to LHM, so
     close it afterwards.
5. **Extra: keep other machines out.** Authentication is enough on its own, but HTTP Basic sends the
   password unencrypted, so it's safe to read only on this PC (`lhm_url` at 127.0.0.1, the default).
   - **The aim** would be to listen on 127.0.0.1 only, never on 0.0.0.0. 0.0.0.0 publishes the server to
     every network the PC is on.
   - **LHM 0.9.6 can't do it by itself.** Its Interface list offers only the PC's own network addresses
     and 0.0.0.0. When the address it's set to isn't one of those, including 127.0.0.1 typed into its
     `.config`, it listens on every interface instead.
   - **LHM's code can since 47287b6** (October 2026, in no release yet). Its Options > Remote Web Server >
     Interface / Port list also offers 127.0.0.1. With a build that has it, choose 127.0.0.1 there: that
     is the aim above, other machines can't even connect, and `lhm_url` stays at the default.
   - **What helps with 0.9.6:**
     - **Windows Firewall on for the Private profile**, with no allow rule for port 8085, keeps other
       machines out. This PC's own requests always get through.
     - **Choosing the PC's LAN address** limits the server to that one network. `lhm_url` must then be
       that address, and it breaks if the address changes.
6. **Tick Options > Run On Windows Startup while LHM is running as administrator.**
   - Ticked like that, LHM makes a logon task that runs with highest privileges. The driver then loads
     after a restart with no prompt.
   - Ticked without elevation, LHM uses the registry Run key instead, and CPU temperatures go missing after
     a restart.
   - Start Minimized and Minimize To Tray keep it out of the way.
   - With 0.9.6 or earlier, every start of LHM loses the Authentication password (step 4), so the
     `lhm.*` values stop after every reboot until it's typed again in LHM; a build with the fix keeps it.
     Authentication is still worth that: without it, step 4's risks stay open.
7. **Turn it on in the studio:**
   - In the Data sources window, on This PC, tick **Read Libre Hardware Monitor sensors**.
   - Change **Libre Hardware Monitor address** only if the port or interface changed.
   - Type the user name from step 4 in **Libre Hardware Monitor user name**, then the password in
     **Libre Hardware Monitor password**, last: changing the address or the user name clears a saved
     password. The password box then shows "Set"; it never shows the password. When a change clears it,
     the box goes back to "Not set" and the window says so.
   - **Test** lists the `lhm.*` keys, or, under the other values, why LHM didn't answer, or that it asks for
     a login, or refused this one.

## The player

- **Polling:**
  - Each enabled source is polled on its own thread, at its own interval.
  - data.json is watched like the playlist: changes are picked up once they settle for about 1.5 s.
  - Failures are logged briefly (never with tokens or passwords), and the values just go stale.
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
  - Static text is drawn into the prepared frames as now.
  - Live text is drawn while playing, over the prepared frames. It keeps its place in the stack when
    nothing above it is animated; otherwise it's drawn on top of everything. The exact rule is in
    scenes.md ("Live text").
  - **When to redraw:** the live text is redrawn only when some live text object's resolved text changes.
    That's checked every 500 ms, so `{time:%H:%M:%S}` redraws about once a second and `{time}` once a
    minute. Over an animation, redraws are spaced out to stay cheap (scenes.md).
  - **Building frames:** each frame is the cached JPEG decoded, the live text blended over it, then
    re-encoded, rotated as before.
  - **Caching:** only the frames of the current text are kept, so an animation is re-encoded once per
    change, not every loop, and a still is re-encoded once.
  - **No live text:** exactly today's path, with zero extra cost.

## The studio

- **A "Data" section:**
  - A card per source type, with enable, interval and the type's fields (pc: HWiNFO, Libre Hardware
    Monitor and its address, user name and password; HA: URL, token, entities).
  - The HA token and LHM password boxes are write-only: they show "set" and let you replace it.
  - A saved password only ever goes back to where it was typed for. Changing any text setting of its
    source (HA's URL; LHM's address or user name) drops it, so it has to be typed again. The box then
    shows "Not set" again, and the window says the password was cleared. The Test button uses the saved
    one only while those are unchanged, and the player checks that again.
  - Text settings and passwords are saved trimmed, except LHM's user name and password, which are saved
    as typed because LHM compares them exactly. A password that's all spaces counts as blank.
  - A **Test** button shows the values that source returns now, or its error, and its notes on parts it
    skipped (LHM not answering, say).
  - Changes save to data.json.
- **A live values table:** key and current value, filterable, with stale ones greyed out. Clicking a key
  inserts `{key}` into the selected text object's text at the cursor.
- **Preview:** text objects show their resolved text, via the player's `/resolve`. It refreshes when the text
  changes and about every 10 s. If the player isn't running, the raw placeholders show, and a note says so.
- **Endpoints:**
  - `GET/POST /api/data/config`: the GET leaves each password out (`token`, `lhm_password`) and gives
    `token_set`, `lhm_password_set` instead; the POST keeps the saved one unless a new one is given, as
    long as the source's text settings are unchanged.
  - `GET /api/data/values`, `POST /api/data/resolve` and `POST /api/data/test`: proxies to the player API.
    They answer `{"player": false}` when it's down.
