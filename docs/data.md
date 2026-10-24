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
    { "type": "pc", "enabled": true, "interval": 60, "hwinfo": false, "pawnio": true,
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
    - `pawnio` (default `false`): read the CPU, fan, pump and motherboard sensors straight from the hardware,
      through the PawnIO driver. Windows asks for administrator rights for Wonder Vision's sensor reader when
      you turn it on and each time the player starts (see [Setting up PawnIO](#setting-up-pawnio)).
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
  - **From PawnIO**, with `"pawnio": true`, while the player plays, once PawnIO is installed and you've let
    Wonder Vision's sensor reader run as administrator ([Setting up PawnIO](#setting-up-pawnio)):
    - The CPU's temperatures and package power, as `pawnio.cpu.<type>.<sensor>`, and the motherboard sensor
      chip's fans, pump and temperatures, as `pawnio.<chip>.<type>.<sensor>`. The keys, their names and when
      a channel is left out are in [From PawnIO](#from-pawnio).
    - Problems are notes, as with LHM: the rest of the pc values come through, and the pc source doesn't
      fail or back off.
  - **`pc.cpu.temp`** is the CPU die temperature, from the first of these that gave one:
    - **PawnIO's**, when PawnIO is on and reading: `pawnio.cpu.temperature.tdie` where the CPU has one apart
      from Tctl, else `pawnio.cpu.temperature.tctl`. It's read straight from the CPU, with no other program
      running.
    - **Else HWiNFO's**, when HWiNFO is on and gave one: the reading labelled like "CPU (Tctl/Tdie)", else
      the first CPU temperature.
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

### From PawnIO

With `"pawnio": true`, This PC reads two things straight from the hardware, with no other program running:

- **The CPU:** temperatures and package power, on AMD Ryzen (families 17h to 1Ah: Zen 1 to Zen 5). Intel CPUs
  aren't read yet: they get the note "PawnIO CPU readings aren't supported on Intel CPUs yet.", and the
  motherboard's chip still comes through.
- **The motherboard's sensor chip:** fan and pump speeds, and board temperatures. Only ITE chips are read so
  far:
  - the IT8696E and the IT87952E, this PC's, checked on its hardware;
  - the IT8689E, IT8688E, IT8686E and IT8792E, which are **untested**. They're read with Libre Hardware
    Monitor's register layout for the same chips, but nobody here has run them on one. Any other chip gets a
    note naming its ID.

There are no voltages, fan duty or clocks.

**CPU keys** are our own short names. All are numbers:

| key | name | unit | when it's there |
|---|---|---|---|
| `pawnio.cpu.temperature.tctl` | CPU Tctl | °C | AMD family 17h to 1Ah. Tctl is the temperature the CPU sets its fans by |
| `pawnio.cpu.temperature.tdie` | CPU Tdie | °C | only on the family 17h CPUs that read Tctl with an offset (the Ryzen 7 1800X reads 20 °C high, say). Elsewhere Tdie is Tctl, and there's no `tdie` |
| `pawnio.cpu.temperature.ccd<n>`: `ccd1`, `ccd2`, ... | CPU CCD1, CPU CCD2, ... | °C | each CCD (core die) whose reading says it's valid at this poll, counted from 1 |
| `pawnio.cpu.temperature.ccd_max`, `pawnio.cpu.temperature.ccd_average` | CPU CCDs max, CPU CCDs average | °C | with two or more CCDs read at this poll: the hottest and the average of those |
| `pawnio.cpu.power.package` | CPU package power | W | worked out from the CPU's energy counter between two reads 0.2 s to 2 minutes apart |

- A temperature outside −20 to 130 °C, or a power outside 0 to 1000 W, is left out.
- CPU power needs the pc interval at 2 minutes or less. Longer, and the note says "Set the pc interval to 2
  minutes or less for CPU power." After the PC sleeps, the next read only starts counting again.
- A CCD that isn't found at the first read is looked for again every minute.
- This PC's Ryzen 9 9950X3D gives `tctl`, `ccd1`, `ccd2`, `ccd_max`, `ccd_average` and `package`. It has no
  `tdie`: its Tctl is the die's temperature.

**Chip keys** are `pawnio.<chip>.<type>.<sensor>`:

- `<chip>` is `ite_` and the chip's name, like `ite_it8696e`. A second chip with the same name is `<name>_2`.
- `<type>` is `temperature` or `fan`.
- `<sensor>` is `temperature_<n>` or `fan_<n>`, counted from 1.

These are the keys Libre Hardware Monitor gives on a board it has no map for, with `pawnio.` in place of
`lhm.`. This board is one, so here they're your `lhm.` keys: `lhm.ite_it87952e.fan.fan_2` is
`pawnio.ite_it87952e.fan.fan_2`. On a board LHM has a map for, LHM's keys use the header names
(`lhm.ite_it87952e.fan.system_fan_6_pump`). PawnIO's don't: the header names are labels, never keys.

On this PC (Gigabyte X870E AORUS ELITE X3D ICE), with the names the Provides table shows and what the hardware
probe read:

| key | name on this board | at the probe |
|---|---|---|
| `pawnio.ite_it8696e.temperature.temperature_1` | System #1 | 37 °C |
| `pawnio.ite_it8696e.temperature.temperature_2` | PCH | 48 °C |
| `pawnio.ite_it8696e.temperature.temperature_3` | CPU | 74 °C (it follows Tctl) |
| `pawnio.ite_it8696e.temperature.temperature_4` | PCIe x16 | 44 °C |
| `pawnio.ite_it8696e.temperature.temperature_5` | VRM MOS | 51 °C |
| `pawnio.ite_it8696e.fan.fan_1` | CPU Fan | 2377 RPM |
| `pawnio.ite_it8696e.fan.fan_2` | System Fan #1 | 1221 RPM |
| `pawnio.ite_it8696e.fan.fan_3` | System Fan #2 | 1180 RPM |
| `pawnio.ite_it8696e.fan.fan_4` | System Fan #3 | 1168 RPM |
| `pawnio.ite_it8696e.fan.fan_5` | CPU Optional Fan | 0 RPM |
| `pawnio.ite_it87952e.temperature.temperature_1` | PCIe x4 | 41 °C |
| `pawnio.ite_it87952e.temperature.temperature_3` | System #2 | 41 °C |
| `pawnio.ite_it87952e.fan.fan_1` | System Fan #5 / Pump | 0 RPM |
| `pawnio.ite_it87952e.fan.fan_2` | System Fan #6 / Pump | 6250 RPM: **the pump** |
| `pawnio.ite_it87952e.fan.fan_3` | System Fan #4 | 0 RPM |

The IT8696E's temperature 6 ("External #1") and fan 6, and the IT87952E's temperature 2 ("External #2"), are
left out here, by the rules below.

**When a channel is left out.** A channel that's left out has no key at that poll.

- **Temperatures** show only when they read 1 to 126 °C. That's how the IT8696E's temperature 6 and the
  IT87952E's temperature 2 drop out on this PC, as they do in LHM.
- **Fans:** a fan whose counter reads all ones shows as **0 RPM**. That means stopped or nothing connected:
  the two can't be told apart, and a stopped pump must show. A count of 0x3F or less is left out.
- **A fan input the chip has switched off** is left out, with no name. The IT8696E's fan 6 is off on this PC.
- **A read that fails its check is skipped for that poll**, and the last value stays until it goes stale
  (3 × the interval). The checks: the chip must echo back the register asked for, its register bank must
  still be bank 0, and the shared lock must be free (see
  [Running alongside other programs](#running-alongside-other-programs)). No read is retried within a poll.
- There's no setting to hide a channel: widgets show only the keys they name.

Units: °C, RPM and W. A fan reading is the motherboard header's tach: the same reading the BIOS and LHM show.

**Names.** The Provides table in the Data sources window lists each `pawnio.*` key that has had a reading
since PawnIO was turned on, with its name and unit, under the `pawnio.*` row, and autocomplete shows the same
names. The list only grows while PawnIO is on (a channel that's missing for one poll keeps its row), and it's
cleared when PawnIO is turned off.

- **A board with a map** shows its header names. These boards share one map, Libre Hardware Monitor's for the
  X870E AORUS PRO: Gigabyte's X870E AORUS PRO, PRO ICE, XTREME AI TOP, MASTER X3D, MASTER X3D ICE and ELITE X3D
  ICE. The board's name comes from Windows (`HKLM\HARDWARE\DESCRIPTION\System\BIOS`), matched exactly but for
  letter case and spaces at either end.
- **This PC's ELITE X3D ICE** has no map in LHM. It gets its sibling boards' names because the hardware probe
  showed the two channels that tell this wiring apart are where the map says: the IT8696E's temperature 3
  follows the CPU's Tctl, and the pump, at about 6300 RPM, is the IT87952E's fan 2. The other headers haven't
  been checked in the BIOS. They're names only: a wrong one never changes a key.
- **Any other board** gets generic names: "ITE IT8696E temperature 3", "ITE IT87952E fan 2".
- The CPU's names are the same on every board: "CPU Tctl", "CPU Tdie", "CPU CCD1", "CPU CCDs max", "CPU CCDs
  average", "CPU package power".

**How often.** PawnIO is read at each pc poll, and a whole read takes about 1 ms on this PC. For live fan
readings, set This PC's interval to 5 s. A change to PawnIO's state (turned on, allowed, declined) has the pc
source polled at once, so its values and notes follow within about a second.

**Notes** from the reader say what was skipped and why, under This PC's other values:

| note | what to do |
|---|---|
| "Another program kept the motherboard sensor chip busy, so some readings were skipped." (or "...the CPU's temperature sensors busy...") | nothing: another monitoring program held the shared lock. It's said after 3 skips in a row |
| "Another program switched the motherboard sensor chip's register bank, so its readings were skipped this time." | nothing, unless it stays: then look for a fan program that keeps the chip's other bank selected |
| "The motherboard sensor chip at 0x2E didn't answer as expected, so its fans and temperatures are missing until the player restarts." | restart the player. Each reader tries three more times (after 2, 10 and 60 s) before saying this |
| "The motherboard sensor chip stopped answering, so fan and board readings are off until the player restarts." | restart the player |
| "No supported motherboard sensor chip was found (only ITE chips are read so far), so there are no fan or board readings." | none yet: the board's chip isn't one PawnIO reads here |
| "The PawnIO module files beside the player are damaged or from another version: rebuild or reinstall the player." | rebuild the player (`cargo build --release`). If that doesn't help, turn PawnIO off, remove the reader ([below](#setting-up-pawnio)) and turn PawnIO on again, so it's set up afresh |
| "PawnIO is too old for its modules: update it from pawnio.eu." | update PawnIO |

### Setting up PawnIO

PawnIO is a free driver (<https://pawnio.eu>) that lets a program read the CPU's and the motherboard's
sensors. Libre Hardware Monitor's newer versions use it too. Reading through it needs administrator rights,
but **the player doesn't get them**: only Wonder Vision's small **sensor reader**, `wvsensors.exe`, runs as
administrator, from `C:\Program Files\Wonder Vision\`, and only while the player plays with PawnIO on.

- The player, its plugins, ffmpeg and its web API still run as you, and the studio's Stop still works.
- The reader reads none of your files, starts no other program, loads only the two PawnIO module files whose
  SHA-256 values are pinned in its code, and answers only the player that started it. That player can ask it
  for readings, or tell it to quit: nothing else.

1. **Install PawnIO:** from <https://pawnio.eu>, or `winget install namazso.PawnIO`. If Libre Hardware
   Monitor already offered to install it, it's there. It adds a driver service, `PawnIO`, that starts with
   Windows. The Data sources window says whether it's installed.
2. **Build the player** (`cargo build --release` in `player\`), which builds `wvsensors.exe` beside
   `wvplay.exe`. Stop the player before building, then restart the studio and press **Play on cooler**:
   the new player takes requests only from a studio that sends the `X-WV` header ([The player](#the-player)).
   A player built before PawnIO can't read it, and the studio says "Rebuild the player (cargo build
   --release) to read PawnIO sensors."
3. **Turn it on:** in the Data sources window, on This PC, tick **Read sensors through PawnIO (asks for
   administrator rights)**. With the player playing, the studio asks it, and Windows asks you about Wonder
   Vision's sensor reader. Choose **Yes**. The readings show within about 2 s. With no player playing, the
   studio says "Saved. Windows asks for administrator rights when the player starts."
4. **The first time**, the prompt is for the reader in the player's build folder,
   `C:\Git\CoolerVideo\player\target\release\wvsensors.exe`. It copies itself and the PawnIO module files, with
   their licence and source, into `C:\Program Files\Wonder Vision\`, where only an administrator can change
   them, then reads from there. That one prompt covers both.
5. **For live fan readings,** set This PC's interval to 5 s.

**Which path each prompt should name.** Windows' prompt shows the program's path under **Show more details**.
A routine prompt names `C:\Program Files\Wonder Vision\wvsensors.exe`. A prompt naming another path should only
appear right after you choose Ask now or press Play after an update. The reader isn't signed, so the prompt
shows no verified publisher, and any program can raise one that looks the same: answer **No** to a prompt you
didn't expect.

**Each player start, and each logon.** With PawnIO on, Windows asks each time the player starts: when you press
Play, and after you log in when **Start with Windows** is ticked, so once per logon.

- The player has no window, so the prompt may show only as a flashing shield on the taskbar: click it.
- The prompt waits until you answer. The panel plays meanwhile, without PawnIO's readings.
- Nothing can raise the prompt again, or take it back, while it's up.

**After a rebuild** that changed `wvsensors.exe`, the player doesn't ask when it starts. The studio says "The
sensor reader in Program Files is from another build of Wonder Vision: choose Ask now to update it (Windows asks
for administrator rights)." Press Play in the studio, which then asks, or choose **Ask now**. The prompt names the
build folder's reader again, and it updates the copy in Program Files. Until the reader is set up or updated,
a logon brings no prompt.

**From another device** (over Tailscale, say), Ask now works, but Windows asks on the PC's own screen: only
someone sitting at it can answer. The studio adds "Windows asks on the PC itself: only someone there can
answer." The player raises at most one prompt every 10 s, and never a second while one is up.

**Declining.** Choose **No**, and the panel plays on without PawnIO. The studio says "Administrator rights were
declined, so PawnIO sensors are off: choose Ask again, or restart the player." Nothing asks again until you
choose **Ask again**, turn PawnIO off and on, or the player restarts. Until then the `pawnio.*` values go stale
after 3 × the interval.

**Check again.** When PawnIO isn't installed, or its driver isn't running, the studio says so, with **Check
again**. Install PawnIO, or restart Windows, then choose it.

**Turning it off.** Untick the checkbox. The reader exits at once, so nothing stays running as administrator,
and the `pawnio.*` rows leave the Provides table. **Stop** does the same: the reader notices its player is gone,
finishes the step it was on with the chip, putting back what it changed, and exits within about 100 ms.

**Removing the reader.** Turn PawnIO off, or stop the player, then delete `C:\Program Files\Wonder Vision` as
administrator. There's no uninstall command. PawnIO itself is removed in Windows' Settings > Apps > Installed
apps.

**Each reader start looks for the chips once.** Turning PawnIO on, Ask again and each player start begin a new
reader, and each looks for the motherboard's sensor chips once: about 60 ms per chip on this PC. That's the
only time it uses the chips' configuration registers.

- It reads each chip's ID first, and sends the "enter configuration" key only to a chip that doesn't answer.
- It leaves each chip as it found it: a chip it put in configuration mode is taken out again, unless it's one
  of the few chips that must never be sent that.
- Polls after that read only the sensor registers.
- A chip that stops answering is only re-read, less and less often, then looked for again at most twice per
  reader.

#### Running alongside other programs

- **Libre Hardware Monitor and HWiNFO** can keep running. They share two named locks with the reader:
  `Global\Access_PCI` for the CPU's temperature registers, and `Global\Access_ISABUS.HTP.Method` for the
  motherboard's chip. So their reads and the reader's never interleave. The reader waits at most 10 ms for a
  lock at a poll (100 ms while it looks for a chip); when another program holds it, that part is skipped this
  time, and after 3 skips in a row a note says so. With PawnIO reading the same channels, LHM isn't needed for
  them.
- **Gigabyte Control Center, EasyTune and the BIOS's own ACPI code** use the same chip without any lock shared
  with ours (this PC's ACPI code uses the chip at 0x2E under a lock of its own). Their access can interleave with
  the reader's:
  - That can give a wrong reading now and then. The echo check, the bank check and the range checks catch
    most.
  - In the worst case, their write could land on a register the reader had just selected.
  - The CPU is the same: PawnIO's module leaves the CPU's register index pointing at a temperature register,
    so another program's write there that doesn't take the `Access_PCI` lock could land on a temperature
    register.
  - Libre Hardware Monitor reads the same way, so it has the same exposure today. The reader keeps each access
    short and only ever reads.
- **A second player** you start by hand while one plays never asks: it can't take the API's port, and only a
  player that has it asks.
- **A prompt you approve after Stop** does nothing: the reader finds its player gone and exits.

#### What the studio shows

The player says how PawnIO stands, and the studio shows its sentence as given: in the player box under Play,
and under the PawnIO checkbox in the Data sources window. The same sentence is This PC's note, except while
it's reading, when the note holds only the reader's own notes.

| state | the studio says | button |
|---|---|---|
| off | nothing | |
| not installed | "PawnIO isn't installed: get it from pawnio.eu (or run `winget install namazso.PawnIO`), then choose Check again." | **Check again** |
| driver not running | "PawnIO is installed but its driver isn't running: restart Windows (or reinstall PawnIO), then choose Check again." | **Check again** |
| not asked yet | "PawnIO is on: choose Ask now, and Windows asks for administrator rights for its sensor reader." Or the reader needs setting up ("...choose Ask now to install Wonder Vision's sensor reader in Program Files...") or updating ("The sensor reader in Program Files is from another build of Wonder Vision...") | **Ask now** |
| asking | "Waiting for you to allow administrator rights on this PC for PawnIO's sensor reader (look for the Windows prompt, or a flashing shield on the taskbar)." | |
| declined | "Administrator rights were declined, so PawnIO sensors are off: choose Ask again, or restart the player." | **Ask again** |
| failed | why, as below | **Ask again** |
| reading | "Reading PawnIO sensors (Wonder Vision's sensor reader runs as administrator)." | |
| standard account | "PawnIO needs administrator rights, and this Windows account isn't an administrator." A standard account gets no prompt | |
| not the player | "PawnIO sensors are read only by the player while it plays (wvplay play)." `wvplay data`, `render` and the others never ask | |

Why it failed:

- "wvsensors.exe isn't beside the player: rebuild the player (cargo build --release)."
- "The sensor reader's folder in Program Files can be changed without administrator rights, so it isn't started:
  delete C:\Program Files\Wonder Vision (as administrator) and choose Ask again." The reader is started only
  when its folder, the exe and its module files can be changed by administrators alone.
- "Another copy of the sensor reader is running, so it couldn't be updated: stop the other player and choose Ask
  again."
- "wvsensors.exe is from another build: choose Ask again to update it."
- "PawnIO's sensor reader didn't answer: choose Ask again, or restart the player."
- "PawnIO's sensor reader stopped (code N): choose Ask again, or restart the player." A failed reader is never
  started again by itself.
- "Couldn't start PawnIO's sensor reader: (Windows' reason)."
- The damaged-files note above.

**Test** in the Data sources window uses the playing player's reader, and never asks. With the PawnIO box ticked
but not saved yet, it says "PawnIO isn't on yet: save to turn it on."

**A player you run as administrator yourself** (with UAC off, or with Run as administrator) starts the reader
with no prompt. Its plugins and ffmpeg then run as administrator too, because of how it was started, and the
studio can't stop it. The player box says "The player runs as administrator: the studio can't stop it. Close it
in Task Manager, or start it normally."

**Checking the hardware.** `wvplay pawnio`, in a terminal opened with Run as administrator, prints every raw
and decoded reading as JSON: `--stage cpu` (the default) reads the CPU only, `--stage all` the chips too, and
`--compare-lhm http://127.0.0.1:8085` compares each channel with a running LHM. It's how this PC's channels
were checked. Without administrator rights it prints `{"error":"needs-admin"}`.

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
  - **Every POST needs the header `X-WV: 1`.** A web page can make the browser POST here, but it can't add
    that header, so no page you visit can make the player act. Without it the answer is 403
    `{"error": "this player needs the X-WV header: restart the studio"}`: a studio started before the player
    was updated doesn't send it yet. The studio sends it on every request, and so does `wvplay render`.
  - `GET /info` returns `{"widgets": 1, "elevated": false, "pawnio": {"state": "ok", "message": "...",
    "can_ask": false}}`:
    - `widgets`: this player draws widgets. A player from before widgets answers 404.
    - `elevated`: whether the player itself runs as administrator, which the studio can't stop.
    - `pawnio`: PawnIO's state (`off`, `not-installed`, `not-running`, `unavailable`, `needs-admin`,
      `not-asked`, `asking`, `declined`, `failed` or `ok`), its sentence as the studio shows it, and whether
      the studio offers a button for it. See [What the studio shows](#what-the-studio-shows). A player from
      before PawnIO has no `pawnio`, and the studio then shows nothing about it.
  - `POST /pawnio/ask {}` is the studio's Ask now, Ask again and Check again. It returns PawnIO's state
    afterwards, as `/info` gives it.
    - It acts only in `wvplay play`, with PawnIO on, and from `not-installed`, `not-running`, `not-asked`,
      `declined` or `failed`. It checks again, and may raise Windows' prompt.
    - It raises at most one prompt every 10 s, and none while one is up. Otherwise it returns the state as it
      is.
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
    minute. Over an animation, redraws are spaced out to stay cheap (scenes.md), except the one that fills
    in a value that was missing: that one is drawn as soon as the value comes.
  - **Building frames:** each frame is the cached JPEG decoded, the live text blended over it, then
    re-encoded, rotated as before.
  - **Caching:** only the frames of the current text are kept, so an animation is re-encoded once per
    change, not every loop, and a still is re-encoded once.
  - **No live text:** exactly today's path, with zero extra cost.

## The studio

- **A "Data" section:**
  - A card per source type, with enable, interval and the type's fields (pc: HWiNFO, PawnIO, Libre Hardware
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
- **PawnIO** ([Setting up PawnIO](#setting-up-pawnio)):
  - The player box under Play has a line with the running player's PawnIO sentence, and **Ask now**, **Ask
    again** or **Check again** when it can ask. The answer shows in the same line.
  - In the Data sources window, under This PC's PawnIO checkbox, the same line, kept up to date while the window
    is open, and above it an install line: that PawnIO isn't installed, with a link to pawnio.eu and the winget
    command; or, while no player runs, which version is installed and what Windows will ask; or that the
    player must be rebuilt to read PawnIO.
  - After a save that turns PawnIO on, and after Play with it on, the studio reads the player's state every
    500 ms for up to 6 s, and asks only when the player is ready to (`not-asked`). So Windows' prompt follows
    your own click: a change to data.json alone never raises one. What it finds is said in the Data sources
    window while it's open, else at the top of the page.
  - The **Start with Windows** checkbox's tooltip adds that Windows asks after you log in, once the player
    reads PawnIO.
  - The studio never runs anything as administrator, never starts the sensor reader, and never makes a
    scheduled task.
- **Endpoints:**
  - `GET/POST /api/data/config`: the GET leaves each password out (`token`, `lhm_password`) and gives
    `token_set`, `lhm_password_set` instead; the POST keeps the saved one unless a new one is given, as
    long as the source's text settings are unchanged.
  - `GET /api/data/values`, `POST /api/data/resolve` and `POST /api/data/test`: proxies to the player API.
    They answer `{"player": false}` when it's down.
  - `GET /api/pawnio`: `{"installed", "version", "service", "reader_built", "reader_installed"}`, the
    studio's own look at this PC, for the install line. `service` is `running`, `stopped`, `missing` or
    `unknown`. It only reads: the registry, and the service manager asked for the state alone.
  - `POST /api/pawnio/ask`: the player's `POST /pawnio/ask`, with `"remote": true` added when the page isn't
    on this PC. `{"player": false}` when it's down.
