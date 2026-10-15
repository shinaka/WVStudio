# Writing a plugin

A plugin adds live values of your own to the cooler's screen. It's a small program in its own folder under
`plugins\`. The player runs it, gives it its settings, and reads the values it prints, such as
`{"values": {"disk.free_gb": 412.5}}`. A text on the screen can then show `{disk.free_gb} GB free`.

Any language that can read stdin and print JSON will do. The examples here are in Python, and
[later sections](#writing-plugins-in-other-languages) cover batch files, PowerShell and compiled programs.

This guide starts with a tutorial and then gives the full reference. Everything in it matches the player's
code (`player/src/plugin.rs`, `catalog.rs` and `data.rs`) and the studio's, and it was all tried against the
player. The specs are [sources.md](sources.md) and [data.md](data.md), and
[`plugins/example-weather/`](../plugins/example-weather/) is a complete working plugin.

- [Quick start: a disk-space plugin](#quick-start-a-disk-space-plugin)
- [How the player runs a plugin](#how-the-player-runs-a-plugin)
- [plugin.json reference](#pluginjson-reference)
- [The output protocol](#the-output-protocol)
- [Stream mode](#stream-mode)
- [Errors, rate limits and stale values](#errors-rate-limits-and-stale-values)
- [Testing and debugging](#testing-and-debugging)
- [Using the values in text](#using-the-values-in-text)
- [Writing plugins in other languages](#writing-plugins-in-other-languages)
- [Security](#security)
- [Checklist](#checklist) and [troubleshooting](#troubleshooting)

**At a glance:**

| | |
|---|---|
| Where | `plugins\<folder>\plugin.json`, in the CoolerVideo folder next to `playlist\` |
| When it's read | when the player starts, and when you press **Rescan** in the studio |
| How it runs | `run.command`, with no window. Its working folder is the plugin's folder, but the program itself isn't looked for there, so name the interpreter: `["python", "disk.py"]` |
| Input | one line on stdin, `{"settings": {...}}`, then end of input |
| Output | `{"values": {"<id>.<name>": value, ...}}` or `{"error": "..."}` |
| `poll` mode | runs once per interval, and is killed after the interval or 30 s, whichever is shorter |
| `stream` mode | keeps running and prints one JSON object per line. Restarted 1 s after it exits, with the wait doubling each time up to 60 s |
| Values | numbers, text, and RFC 3339 times, with seconds and a time zone: `2026-10-14T10:00:00Z` |
| Stale | after 3 × the interval without a success |

## Quick start: a disk-space plugin

You'll build `disk`, a plugin that shows the free space on a drive. It needs Python 3 and nothing else.

### 1. Make the folder

Plugins live in the `plugins` folder in the CoolerVideo folder, next to `playlist`. The studio shows its exact
path at the bottom of the **Data sources…** window. Make a folder for the plugin there:

```
CoolerVideo\
  playlist\
  plugins\
    example-weather\
    disk\
      plugin.json
      disk.py
```

The folder's name doesn't matter, but the `id` in `plugin.json` does.

The player loads every plugin in this folder when it next starts or you press **Rescan**, even a half-written
one. To keep work in progress away from it, build in a folder of your own laid out the same way, such as
`C:\dev\wv\plugins\disk\`, and copy the `disk` folder into the CoolerVideo folder's `plugins` before step 6.
Steps 4 and 5 work in either place.

### 2. Write `plugins\disk\plugin.json`

The manifest says what the studio shows and how the player runs the program:

```json
{
  "id": "disk",
  "name": "Disk space",
  "description": "Free and used space on one drive.",
  "interval": { "default": 300, "min": 10 },
  "settings": [
    { "key": "drive", "label": "Drive", "type": "text", "default": "C:\\" }
  ],
  "provides": [
    { "key": "disk.free_gb", "label": "Free space", "unit": "GB", "kind": "number" },
    { "key": "disk.used_pct", "label": "Space used", "unit": "%", "kind": "number" },
    { "key": "disk.drive", "label": "Drive", "kind": "text" },
    { "key": "disk.checked", "label": "Checked at", "kind": "time" }
  ],
  "run": { "command": ["python", "disk.py"], "mode": "poll" }
}
```

- **`id`** names the source, and starts every key it sets: `disk.free_gb`, `disk.used_pct`, and so on.
- **`interval`:** runs every 300 s unless the user sets another interval, and never more often than every 10 s.
- **`settings`** become the form in the studio. This plugin has one setting, a text box for the drive.
- **`provides`** lists the keys up front, so the studio can offer them before the first value arrives.
- **`run`:** `poll` mode means "run this once per interval". The program starts with the plugin's folder as
  its working folder, so `disk.py` is found there. The program, `python`, isn't looked for in that folder
  but on `PATH` (see [Starting the program](#starting-the-program)).

Save it as UTF-8 **without** a byte-order mark: the player can't read a `plugin.json` that starts with one.

### 3. Write `plugins\disk\disk.py`

```python
"""Free space on one drive: a wvplay plugin in poll mode."""
import json
import shutil
import sys
from datetime import datetime, timezone

# The player writes {"settings": {...}} to stdin, as UTF-8.
settings = json.load(sys.stdin.buffer)["settings"]
drive = settings.get("drive") or "C:\\"

try:
    usage = shutil.disk_usage(drive)
except OSError as e:
    print(json.dumps({"error": f"can't read {drive}: {e}"}))
    sys.exit()

print(json.dumps({"values": {
    "disk.free_gb": round(usage.free / 1e9, 1),
    "disk.used_pct": round(100 * usage.used / usage.total, 1),
    "disk.drive": drive,
    "disk.checked": datetime.now(timezone.utc).isoformat(timespec="seconds"),
}}))
```

It reads its settings, prints one JSON object, and exits. That's the whole contract in poll mode. It reads
`sys.stdin.buffer` (bytes) instead of `sys.stdin`, so a setting like `Zürich` arrives intact whatever
Windows' code page is (see [Encoding](#encoding)).

### 4. Try it by hand

In PowerShell, from the folder that holds `plugins` (the CoolerVideo folder, or your own):

```powershell
cd plugins\disk
'{"settings": {"drive": "C:\\"}}' | python disk.py
'{"settings": {"drive": "Q:\\"}}' | python disk.py
```

```
{"values": {"disk.free_gb": 1910.8, "disk.used_pct": 52.2, "disk.drive": "C:\\", "disk.checked": "2026-10-15T07:08:38+00:00"}}
{"error": "can't read Q:\\: [WinError 3] The system cannot find the path specified"}
```

### 5. Run it through the player

`wvplay data` runs the plugin through the player once, without the screen or the studio, and leaves your
running player alone. Back in the folder that holds `plugins` (`cd ..\..`), in PowerShell, set `$wvplay` to
the player, `player\target\release\wvplay.exe` in the CoolerVideo folder:

```powershell
$wvplay = "C:\Git\CoolerVideo\player\target\release\wvplay.exe"
New-Item -ItemType Directory -Force plugin-test | Out-Null
'{ "sources": [ { "type": "disk", "enabled": true } ] }' | Set-Content plugin-test\data.json

$keep = $env:LOCALAPPDATA
$env:LOCALAPPDATA = "$env:TEMP\wvplay-test"
Remove-Item "$env:TEMP\wvplay-test" -Recurse -ErrorAction SilentlyContinue
& $wvplay data plugin-test\data.json
$env:LOCALAPPDATA = $keep
```

```
[07:20:31 UTC] plugins: 1 in plugins
disk: ok
  disk.checked = 03:20  [disk]
  disk.drive = C:\  [disk]
  disk.free_gb = 1910.5  [disk]
  disk.used_pct = 52.2  [disk]
  time = 03:20  [time]
```

`disk: ok` and its four values mean the player loaded and ran it. `time` is a built-in that's always listed.
[Testing from the command line](#testing-from-the-command-line) explains each line, and how to test
settings.

### 6. Load it in the studio

The plugin must now be in the CoolerVideo folder's `plugins`. The player reads that folder, so it must be
running: press **Play on cooler** if it isn't.

1. Open **Data sources…** and press **Rescan** at the bottom. The note says how many plugins it found, and
   lists any it skipped and why.
2. Select **Disk space** in the list (it's tagged "plugin") and switch it on.
3. Press **Test**. It runs the plugin once with the settings in the form, without saving anything, and
   shows the four values, or the error.

Once it has run, the status above its settings says when it last updated, and the **Provides** table shows
each key with its live value.

### 7. Show it on the screen

Select a text and type `{disk.free_gb:d} GB free`, or click a key in the **Provides** table to insert it.
Typing `{` in the text box also suggests every key, with its current value. Some more you could write:

| written | shows |
|---|---|
| `{disk.free_gb:d} GB free` | `1911 GB free` |
| `C: {disk.used_pct:d}% full` | `C: 52% full` |
| `checked {disk.checked:%H:%M}` | the time of the last check, in local time |
| `{disk.free_gb\|?} GB` | `?` instead of the value while it's missing or stale |

The studio's preview fills in the text with the running player's values, so you can check it there before it
reaches the screen. (`wvplay data` shows the values, not your text.)

That's a working plugin. Delete the `plugin-test` folder when you're done with it. The rest of this guide
explains each part in detail.

## How the player runs a plugin

### Finding plugins

- The player's plugins folder is `plugins`, next to the folder that holds the playlist. The player plays
  `playlist\playlist.json`, so that's `CoolerVideo\plugins`.
- Every folder in it that holds a `plugin.json` is a plugin. Other folders and files are ignored.
- The folder is read when the player starts and when you press **Rescan**. After you change `plugin.json`,
  press **Rescan**.
  - A change to the program itself needs no rescan in poll mode: each run starts the file afresh.
  - A streaming plugin keeps running its old code until it restarts. **Rescan** restarts every plugin.
- A plugin whose `plugin.json` has a problem is skipped. The reason goes to the player's log and to the
  **Rescan** note: see [troubleshooting](#when-the-plugin-isnt-loaded).
- Ids must be unique:
  - The built-in sources' ids are taken: `claude`, `pc`, `homeassistant` and `time`.
  - If two folders have the same id, the folder whose name sorts first wins.
  - Don't use `ha` or `hwinfo` either, or any other first part of a key in the built-ins' `provides` in
    `player/src/builtins.json`. The player accepts them, but Home Assistant sets keys starting with `ha.` and
    the PC source sets keys starting with `hwinfo.`, so the two would overwrite each other's values.

### Starting the program

`run.command` is a list: the program, then its arguments.

- **No shell:** the program is started directly, so `|`, `>`, `&&`, `%VARIABLES%` and wildcards mean
  nothing in it.
- **You choose the interpreter:** the player doesn't pick one from the file's extension. Name it:
  `["python", "disk.py"]`, not `["disk.py"]`.
- **Where the program is looked for:**

| `command` | works? |
|---|---|
| `["python", "disk.py"]`, `["py", "-3", "disk.py"]`, `["powershell", ...]`, `["pwsh", ...]`, `["cmd", ...]` | yes: a bare name is looked up in the player's own folder, then Windows' `System32` and Windows folders, then the player's `PATH`. For a program Windows also has (`bash`, `curl`, `tar`), give the full path to the one you mean |
| `["tool.exe"]`, `["run.cmd"]` (files in your plugin's folder) | **no**: `couldn't run "tool.exe": program not found`. Your plugin's folder isn't searched |
| `[".\\tool.exe"]`, `[".\\run.cmd"]` | **no**: a relative path is taken from the *player's* working folder, not your plugin's |
| `["C:\\Tools\\tool.exe"]` | yes: a full path |
| `["cmd", "/c", ".\\tool.exe"]`, `["cmd", "/c", ".\\run.cmd"]` | yes: `cmd` starts in your plugin's folder and finds the file there. But in stream mode, `cmd` mustn't start another program, such as `tool.exe`: see [Stream mode](#stream-mode) |

- **Arguments:** relative paths in the arguments are fine, because the program starts in the plugin's
  folder. That's why `["python", "disk.py"]` works.
- **Write the `.\`** in `cmd /c .\run.cmd`. Without it, `cmd` finds the file only when the environment
  variable `NoDefaultCurrentDirectoryInExePath` isn't set, and otherwise says
  `'run.cmd' is not recognized as an internal or external command`.
- **Working folder:** the plugin's folder.
- **Environment:** the player's own, including `PATH`.
  - A player started from the studio has the studio's environment. One started with Windows has your
    login's.
  - If you install Python or change `PATH`, close the studio and start it again from a new terminal, then
    press **Stop** and **Play on cooler** in it. Restarting the player alone isn't enough: it gets the
    studio's environment, which was fixed when the studio started. A player started with Windows sees the
    change after you next sign in.
- **No window:** the program is started without a console window, so `python`, `powershell` and `cmd` run
  invisibly.

### What it reads on stdin

One line of JSON, then a newline, then the end of input:

```json
{"settings":{"api_key":"hunter2","city":"Oslo","items":["a"],"n":null,"on":true,"units":"c"}}
```

- `settings` holds every setting in the manifest, and nothing else. The interval, `enabled` and `type` aren't
  included.
- Each value is the one in `playlist\data.json`. When that's missing, the manifest's `default` is used, and
  when there's no default either, `null`.
- Passwords are included, in plain text: see [Passwords and other secrets](#passwords-and-other-secrets).
- Don't rely on the order of the keys.

To read it in Python, use `json.load(sys.stdin.buffer)` in poll mode, or
`json.loads(sys.stdin.buffer.readline())` in stream mode. A program that needs no settings doesn't have to
read stdin at all.

### When it runs (poll mode)

- **The first run** is at player start, unless its last success (see
  [last values](#last-values-are-kept-across-restarts)) is younger than its interval. Then it waits out the
  rest of the interval first.
- **After that,** it runs again one interval after each run *ends*.
- **After a settings change:** changing a setting or the interval in the studio restarts the source, and the
  plugin runs at once with the new settings.
- **Time limit:** the run is killed if it takes longer than its interval, or 30 s, whichever is shorter. The
  source then shows `took longer than 30 s`, or whatever the limit was.
  - Only the process the player started is killed. If your command is a wrapper (`cmd /c ...`,
    `powershell ...`) that starts another program, that program keeps running to its end.
  - Give your own network calls a timeout well under the limit. The example-weather plugin uses 15 s.

### When the player stops

When the player exits, it doesn't kill a running plugin. A streaming plugin finds out at its next print,
which fails because nobody is reading. Python then exits with a `BrokenPipeError`. In another language,
exit when a write to stdout fails.

## plugin.json reference

```json
{
  "id": "myplugin",
  "name": "My plugin",
  "description": "One sentence for the sources window.",
  "interval": { "default": 60, "min": 5 },
  "settings": [ { "key": "...", "label": "...", "type": "text", "default": "..." } ],
  "provides": [ { "key": "myplugin.value", "label": "...", "unit": "...", "kind": "number" } ],
  "run": { "command": ["python", "myplugin.py"], "mode": "poll" }
}
```

| field | type | required | default | rules |
|---|---|---|---|---|
| `id` | text | yes | | lowercase `a`–`z`, digits, `_` and `-`. Unique, and not `claude`, `pc`, `homeassistant` or `time`. Avoid `ha` and `hwinfo` too ([why](#finding-plugins)). It starts every key you set, and it's the `type` in `data.json` |
| `name` | text | yes | | not blank. Shown in the sources list |
| `description` | text | no | `""` | shown above the settings |
| `interval` | object | no | `{"default": 60, "min": 5}` | [below](#interval) |
| `settings` | list | no | `[]` | [below](#settings) |
| `provides` | list | no | `[]` | [below](#provides) |
| `run` | object | yes | | [below](#run) |

Fields the player doesn't know are ignored. The file must be valid JSON, saved as UTF-8 without a
byte-order mark.

### interval

`{"default": 600, "min": 60}`, in seconds:

- If you give `interval`, give both fields. Each is a number (fractions are allowed), and `min` is at least 1.
- **The source's interval** is the one set in the studio (saved in `data.json`), or else `default`. It's never
  less than `min`: the studio won't save less, and the player won't run more often.
- **What it does:**
  - **poll mode:** the time between runs, and the time limit of each run (30 s at most);
  - **both modes:** values go stale after 3 × the interval without a success.
- The interval isn't passed to the plugin.

### settings

Each setting is `{"key", "label", "type", "default", "choices"}`:

- **`key`** (required): the name in `data.json` and on stdin. It can't be empty, `type`, `enabled` or
  `interval`.
  - Avoid `hwinfo`, `url`, `token` and `entities`, and any other setting key of the built-in sources in
    `player/src/builtins.json`: the player reads those with fixed types.
  - `hwinfo` is always passed as `true` or `false`, whatever its default.
  - `url` and `token` must be text, and `entities` a list of text. A value of another type in `data.json`
    stops the player reading the file at all.
- **`label`** (required): the text by the control.
- **`type`** (required): one of the types in the table below.
- **`default`** (optional): any JSON value. It isn't checked against the type, so make it fit.
- **`choices`** (for `choice`): a non-empty list of text.
- Any other field (such as `help` or `placeholder`) is ignored.

| `type` | the studio shows | saved in `data.json` as | if there's no `default`, the studio saves |
|---|---|---|---|
| `text` | a text box | text | `""` |
| `number` | a number box (any decimals) | a number | `0` |
| `bool` | a checkbox, with the label beside it | `true` / `false` | `false` |
| `choice` | a drop-down of `choices` | the chosen text | `""`, which it then refuses: **always give a choice a default** |
| `list` | a multi-line box, one entry per line | a list of text, trimmed, with blank lines dropped | `[]` |
| `password` | a password box | text | nothing |

**How defaults reach your plugin:**

- When the studio first adds a source to `data.json` (when you switch it on, or change one of its settings),
  it writes all of its settings. It uses the manifest's defaults, or the empty values above.
- From then on, `data.json` wins: changing a default in `plugin.json` later doesn't change sources that are
  already set up.
- The player falls back to the manifest's default only when `data.json` lacks the key, and passes `null`
  when there's no default either.

So treat `null`, `""` and `0` as "not set" where that makes sense, as `disk.py` does with
`settings.get("drive") or "C:\\"`.

### Passwords and other secrets

A setting of type `password` is for API keys and tokens:

- **In the studio:**
  - It's write-only: it shows as "Set: type a new one to replace it", and the studio never sends it back
    to the browser.
  - Settings whose key ends in `token`, `password`, `secret`, `api_key` or `apikey` (the whole key, or
    after a `_`) are treated as secrets in the studio whatever their type, but give them type `password`
    anyway.
- **Stored** in plain text in `playlist\data.json`. The `playlist` folder is ignored by git.
- **Passed** to your plugin only, on stdin, like any other setting. The player never logs settings.
- **Test:** the **Test** button uses the password typed in the form, or else the saved one, as long as the
  text settings are as saved.
- **Changing any `text` setting drops the saved password**, so it has to be typed again. The studio assumes a
  text setting might be the server the secret is sent to, and a secret only ever goes back to the server it
  was entered for. That holds even when your text setting isn't an address.
- **Your plugin must not print the secret.** Error messages, and the end of stderr when the plugin crashes,
  are logged and shown in the studio. See [Security](#security).

### provides

Each entry documents a key: `{"key", "label", "unit", "kind", "pattern"}`.

- **`key`** (required) must start with `<id>.`, or the whole plugin is refused.
- **`label`** (required): shown in the **Provides** table and in the text box's suggestions.
- **`unit`** (optional): shown in the **Provides** table only. Your text adds its own unit:
  `{disk.free_gb} GB`.
- **`kind`** (optional): `number`, `text` or `time`. It decides which formats the text box suggests after
  `{key:`.
- **`pattern: true`** marks a key with a `*`, such as `myplugin.*`. It covers keys you make up as you go: the
  **Provides** table lists the live keys that match it under its label, and `*` matches anything, dots
  included.

`provides` is documentation for the studio. The player accepts any key under your id, listed or not.

### run

`{"command": [...], "mode": "poll"}`:

- **`command`** (required): a non-empty list of text: the program and its arguments. See
  [Starting the program](#starting-the-program).
- **`mode`**: `poll` (the default) or `stream`.

The studio never shows or changes `run`.

## The output protocol

### Values, or an error

Print one JSON object:

```json
{"values": {"disk.free_gb": 412.5, "disk.drive": "C:\\", "disk.checked": "2026-10-15T07:08:22+00:00"}}
```

or, when you couldn't get the values:

```json
{"error": "can't reach the server: timed out"}
```

- **When `error` is there,** it wins, whatever it holds: even `"error": null` fails, with the error `null`.
  Leave the key out when you succeed, rather than setting it to `None` or `null`.
- **An error that isn't text** (such as `{"error": {"code": 7}}`) is shown as its JSON, `{"code":7}`.
- **A JSON object with neither** gives `the output has neither "values" nor "error"`, and so does a
  `values` that isn't an object.
- **`{"values": {}}`** is a success with no values.
- **The exit code** doesn't matter once the JSON is printed. Without any JSON it does: see
  [Errors](#errors-rate-limits-and-stale-values).

### Value types

| you print | becomes | notes |
|---|---|---|
| a JSON number | a number | stored as a 64-bit float, so integers beyond about 9 × 10¹⁵ lose precision |
| text in RFC 3339 form: a date, `T`, hours, minutes **and seconds**, then a time zone of `Z` or `±hh:mm`: `"2026-10-14T10:00:00Z"`, `"2026-10-14T12:00:00+02:00"` | a time | fractions of a second are fine, and so is a space instead of the `T` |
| any other text | text | `"21.5"` stays text, so print numbers as numbers. These stay text too: `"2026-10-14T10:00:00"` (no time zone), `"2026-10-14T10:00Z"` (no seconds), `"2026-10-14T10:00:00+0200"` (no colon in the zone) and `"2026-10-14"` |
| `true`, `false`, `null`, a list, an object | ignored, and noted | send `1`/`0` or text instead |

- **Times:**
  - In Python, `datetime.now(timezone.utc).isoformat()` gives a time, while a naive `datetime.now().isoformat()`
    gives text. So does `strftime("%Y-%m-%dT%H:%M:%S%z")`, because `%z` writes `+0200`.
  - In PowerShell, `.ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")` gives a time.
  - Times are shown in the PC's local time.
- **Not a number:** `NaN` and `Infinity` aren't JSON. Python's `json.dumps` writes them anyway, and then the
  *whole* output is rejected with `didn't print JSON`. Leave such values out, or use
  `json.dumps(..., allow_nan=False)` to find them.
- **No countdowns:** there's no "time until" type. To show `2h 34m`, work the text out yourself.

### Keys

- **The prefix:** every key must start with your id and a dot, exactly, with something after the dot:
  `disk.free_gb`.
  - The player doesn't add the prefix for you.
  - Case matters: `Disk.x` is refused for the id `disk`.
- **Keys that break the rules:**
  - Keys outside your id (`pc.cpu.load`, `disk`, `disk.`) are ignored, and so are values of the wrong type.
  - Each one is noted in the source's status: `ignored pc.cpu.load isn't under disk.`, or
    `ignored disk.flag isn't a number, text or ISO time`. The first five are listed, then a count of the
    rest, such as `and 3 more ignored`.
  - If nothing usable is left, the run fails with `nothing usable: ...`.
- **The rest of the key** isn't checked, but stick to lowercase letters, digits, `_` and dots, such as
  `weather.wind.gust`:
  - a placeholder can't name a key that holds `:`, `|`, `{`, `}` or a line break;
  - spaces at either end of a name in a placeholder are trimmed;
  - every other key is lowercase.

### Stray prints

**In poll mode,** the player looks for the last `{` that begins a line from which the rest of the output reads
as one JSON object. So:

- lines printed *before* the JSON are ignored, even ones that start with `{`, as long as they're valid UTF-8.
  The player stops reading at the first line that isn't, so the JSON after it is lost: `didn't print JSON`.
  In Python, a plain `print("café starting")` writes such a line (see [Encoding](#encoding));
- the JSON can be pretty-printed over several lines (`json.dumps(..., indent=2)` is fine);
- anything printed *after* the JSON breaks it: `didn't print JSON: {"values": {"after.n": 1}} | done`.
  Print the JSON last. Trailing spaces and newlines are fine.

**In stream mode,** every line is read on its own:

- each object must be on one line;
- a line that isn't JSON, including a blank line, marks the source as failing until the next good line;
- a line that isn't valid UTF-8 stops the player reading the plugin at all, with no error. The program keeps
  running and printing, but its values just go stale, until it has printed [1 MiB](#size-limits) in all and
  is cut off and restarted.

Print diagnostics to **stderr**, not stdout. The player shows the end of stderr when the program fails (see
[Errors](#errors-rate-limits-and-stale-values)), as long as it's UTF-8.

### Encoding

The settings come in as UTF-8, and stdout and stderr must be UTF-8:

- **Input:**
  - In Python, read `sys.stdin.buffer`, not `sys.stdin`. On a normal Windows setup, `sys.stdin` decodes
    the pipe with the ANSI code page, and a setting of `Zürich` arrives as `ZÃ¼rich`.
  - In PowerShell, set `[Console]::InputEncoding` to UTF-8 first (see
    [the PowerShell example](#powershell)).
- **stdout:**
  - Python's `json.dumps` writes plain ASCII unless you pass `ensure_ascii=False`, so by default its output is
    safe.
  - Other text is written in the ANSI code page when stdout is a pipe, so `print("café")` writes a line that
    isn't UTF-8. The player stops reading at that line: in poll mode the JSON after it is lost
    (`didn't print JSON`), and in stream mode the values just stop (see [Stray prints](#stray-prints)).
- **stderr:** if any of it isn't UTF-8, the player drops all of it and shows just `exited with code N`.
  Python writes stderr in the ANSI code page too, so a traceback that mentions `Zürich`, or a user folder
  such as `C:\Users\Jürgen`, shows nothing.
- **In Python, `-X utf8` fixes both:** start it with `["python", "-X", "utf8", "disk.py"]`, which makes stdin,
  stdout and stderr UTF-8, so a `print()` or a traceback of any text is safe. The examples here print only
  `json.dumps` output, so without it they lose nothing but a traceback that holds such a character.

### Size limits

- **How much is read:** the player reads the first 1 MiB of stdout and the first 1 MiB of stderr *of one run
  of the program*. It then stops reading, and the program's next write to that stream fails:
  - In poll mode, a run that prints more than 1 MiB fails (Python exits with a `BrokenPipeError`).
  - In stream mode, the 1 MiB counts over the whole life of the process. A streaming plugin that has printed
    1 MiB in all is cut off and restarted. That's after about 3 hours of a 100-byte line every second. Logging
    a lot to stderr does the same.
- **Error messages** show the last 300 characters of the program's output, with its lines joined by ` | `.
- **Keep text that isn't JSON short** (stdout that isn't JSON, a bad stream line, and stderr when it
  crashes). If it's longer than 300 characters and the character just before its last 300 isn't ASCII, such
  as `ü` or `°`, a bug in the player stops it polling the source until the source restarts (a settings
  change, **Rescan**, or a new start). No error is shown, the values go stale, and `wvplay data` leaves the
  source out.

### What happens to the values

- Each success updates the keys it gives. A key you stop sending keeps its last value until it goes stale.
  Nothing removes a key.
- Values go stale 3 × the interval after their last success. Then `{key}` shows `--`, or its fallback.
- The values are saved for the next start: see [last values](#last-values-are-kept-across-restarts).

## Stream mode

Use `"mode": "stream"` when values come as events, or when starting the program for each poll is too slow.
The player starts the program once and keeps it running, and it prints one JSON object per line whenever it
has news.

This plugin counts the files in a folder, and prints the count as soon as it changes.

`plugins\files\plugin.json`:

```json
{
  "id": "files",
  "name": "Files in a folder",
  "description": "How many files are in a folder, updated as soon as that changes.",
  "interval": { "default": 60, "min": 30 },
  "settings": [
    { "key": "folder", "label": "Folder", "type": "text", "default": "%USERPROFILE%\\Downloads" }
  ],
  "provides": [
    { "key": "files.count", "label": "Files", "kind": "number" }
  ],
  "run": { "command": ["python", "files.py"], "mode": "stream" }
}
```

`plugins\files\files.py`:

```python
"""How many files are in a folder, printed when that changes: a wvplay plugin in stream mode."""
import json
import os
import sys
import time

settings = json.loads(sys.stdin.buffer.readline())["settings"]
folder = os.path.expandvars(settings.get("folder") or r"%USERPROFILE%\Downloads")

last = printed_at = None
while True:
    try:
        count = sum(1 for entry in os.scandir(folder) if entry.is_file())
        out = {"values": {"files.count": count}}
    except OSError as e:
        count, out = None, {"error": f"can't read {folder}: {e}"}
    # Print when the count changes, and at least once a minute so the value never goes stale.
    if printed_at is None or count != last or time.monotonic() - printed_at > 60:
        print(json.dumps(out), flush=True)
        last, printed_at = count, time.monotonic()
    time.sleep(2)
```

The rules:

- **Settings arrive once,** when the program starts. Changing a setting or the interval in the studio
  restarts it with the new ones, and so does **Rescan**.
- **Start the program directly, not through a wrapper.** A settings change, **Rescan** or **Test** kills
  only the process the player started.
  - With `["cmd", "/c", ...]` starting another program, or PowerShell starting one, the program behind it
    keeps running with its old settings next to the new copy. The player still drains its output, so its
    prints don't fail, and each restart leaves one more copy running, still doing its work, until the player
    exits or the copy has printed 1 MiB.
  - Use `["python", "myplugin.py"]`, or the full path of an `.exe`. A venv's Python works by its full path
    too: see [Python with its own packages](#python-with-its-own-packages).
- **Flush after every line** (`print(..., flush=True)`). Python holds back output sent to a pipe, and without
  the flush the player sees nothing, possibly for minutes.
- **One object per line:** `{"values": ...}` or `{"error": ...}`. A line that isn't JSON, or an error, marks
  the source as failing until the next good line, but the program keeps running. A line that isn't valid
  UTF-8 is worse: see [Stray prints](#stray-prints).
- **On an error you can't recover from** (no API key, a bad setting), print `{"error": ...}` and keep running:
  sleep, and print it again now and then. Don't exit. When the program exits, its status becomes
  `exited with code 0; restarting in 1 s`, with the end of stderr after the code if there is any. That
  replaces your error, which is left only in the log, and the player starts the program again and again, up
  to every 60 s. If you do exit, write the reason to stderr too.
- **The interval doesn't schedule anything here,** but values still go stale after 3 × the interval. Print
  at least that often, even when nothing has changed. The example prints at least once a minute, with an
  interval of 60 s.
- **Print when something changes, not nonstop.**
  - The player checks whether to stop a streaming plugin only after a quarter of a second without output,
    so a plugin that prints nonstop can't be stopped or restarted cleanly.
  - Nonstop printing also uses up the [1 MiB per run](#size-limits) sooner.
- **Restarts:** if the program exits, the source shows `exited with code 1: <end of stderr>; restarting in 1 s`
  and the player starts it again.
  - The wait doubles after each exit in a row: 1, 2, 4, 8, 16, 32 and 60 s.
  - It goes back to 1 s once a run prints a good line.
- **No back-off for rate limits:** the player doesn't pause a streaming plugin that reports a 429 or 503. The
  log says `backing off`, but the program keeps running, so waiting is up to you.
- **The Test button** starts the program, takes its first line, and stops it. If no line comes within the
  interval or 30 s, whichever is shorter, it shows `printed nothing in N s`. But the studio waits only 20 s
  for a test, so with an interval of 20 s or more you get "The player isn't running" instead. Either way, it's
  usually a missing flush.

## Errors, rate limits and stale values

### Reporting a failure

When you can't get the values, print `{"error": "what went wrong"}`, short and specific, and exit (in stream
mode, [keep running](#stream-mode) instead):

- **Where it shows:** in the studio, the dot by the source's name turns to its error colour and
  `Error: ...` shows above the settings. The message also goes to the player's log.
- **The log:** it's logged once when it starts or changes, and `data: <id> is working again` is logged when
  the source recovers.
- **The values** stay as they were until they go stale.

If the program crashes before it prints any JSON, the player makes the message itself:

| what happened | the source's error |
|---|---|
| it exited with a non-zero code | `exited with code 1: <the end of stderr>`, such as the last lines of a Python traceback. If stderr isn't all UTF-8, just `exited with code 1` (see [Encoding](#encoding)) |
| it exited with code 0 but printed no JSON | `didn't print JSON: <the end of stdout>`, or the end of stderr when stdout was empty |
| it ran too long | `took longer than 30 s`, or less when the interval is shorter |
| it couldn't be started | `couldn't run "python": program not found` |

Catching exceptions and printing an `error`, as `disk.py` does, gives a clearer message than a traceback.

### Rate limits: 429 and 503

When a poll plugin's error contains **429** or **503** as a number on its own (`HTTP 429 Too Many Requests`,
`503 Service Unavailable`), the player backs off:

- **The waits:** 2 minutes, then 4, 8, 16 and 30 minutes at most, but never less than the interval. A success
  resets it.
- **The log:** it says so once, such as
  `data: myplugin: HTTP 429 Too Many Requests; backing off, trying again in 2m`.
- **No `Retry-After`:** a plugin can't pass on a server's `Retry-After`. If the server sends one, wait
  yourself, or report the error.
- **Only 429 and 503** count, as whole numbers. `4290` doesn't, but an exit code of 429 does, and so does a
  `503` anywhere in the end of a crash's stderr.
- **Only poll mode** backs off: see [stream mode](#stream-mode).

So when your server says "slow down", put the status code in the error: `{"error": "HTTP 429 from the API"}`.

### Stale values and fallbacks

- **When values go stale:** 3 × the interval after their last success, such as 15 minutes for a 300 s interval.
- **What text shows then:** `{key|fallback}` shows the fallback, and `{key}` shows `--`.
- **In the studio,** stale values are shown faded.

### Last values are kept across restarts

The player saves every source's latest values, and when they came, in `%LOCALAPPDATA%\wvplay\last-values.json`.
It saves no settings and no passwords:

- **When it's written:** after each successful poll; for a streaming plugin, at most every 10 s.
- **At start,** the player loads the values of the sources that are still switched on, with their real age.
  A value that was stale stays stale.
- **The first run waits** if a poll plugin's last success is younger than its interval: the plugin doesn't run
  until the rest of the interval has passed. This matters when [testing](#testing-from-the-command-line).

## Testing and debugging

### By hand

Pipe the settings in yourself, from the plugin's folder:

```powershell
'{"settings": {"drive": "D:\\"}}' | python disk.py
```

- The program runs in the same folder as under the player, but with *your* terminal's environment.
- **A streaming plugin** doesn't exit: it prints each line as it comes, until you press **Ctrl+C**. To check
  that it flushes, send its output into a pipe, as the player does:
  `'{"settings": {}}' | python files.py | ForEach-Object { $_ }`. Each line should appear as soon as it's
  printed. (Straight to the terminal, Python shows each line at once even without a flush.)
- **Text like `Zürich`:** Windows PowerShell 5.1 (`powershell`) sends text piped to a program as ASCII, so
  `Zürich` arrives as `Z?rich`, even in a plugin that reads UTF-8 correctly. Test such settings in
  PowerShell 7 (`pwsh`), or run `$OutputEncoding = [Text.UTF8Encoding]::new($false)` first.

### In the studio

The **Data sources…** window needs the player to be running.

- **Rescan** re-reads the plugins folder, and lists the plugins it skipped, with the reason.
- **Test** runs the selected source once with the settings in the form, without saving. It shows the values,
  "It worked, but gave no values", or the error.
  - The studio waits 20 s for a test. A plugin that takes longer shows
    "The player isn't running: start it with Play on cooler to test", even though the player is running.
    So does a streaming plugin with an interval of 20 s or more that prints no line.
- **The status line** above the settings shows `Error: ...`, or when it last updated, followed by the
  ignored-key notes after a `·`.
- **The Provides table** shows each key's live value.

### Testing from the command line

`wvplay data <folder>\data.json` runs every source that's switched on in that `data.json` once, prints what
each did and every value, and exits:

- it waits until every source has answered once, or 30 s at most;
- it doesn't touch the screen, and without `--serve` it serves no API;
- it reads plugins from the `plugins` folder next to `<folder>`, so put the test `data.json` in a folder
  beside `plugins`. Any folder works, not just the CoolerVideo folder: `<yours>\plugins\disk\` and
  `<yours>\plugin-test\data.json`, with `wvplay.exe` called by its full path;
- the usage text says `<playlist.json>`, but either file works, because only its folder counts.

The commands, and what they print, are in [step 5 of the quick start](#5-run-it-through-the-player).

**The test `data.json`** has one entry per source in `"sources"`. Settings sit next to `type`, not under a
`"settings"` key, and so does the interval:

```json
{ "sources": [ { "type": "disk", "enabled": true, "interval": 60, "drive": "D:\\" } ] }
```

- A key the plugin doesn't have is ignored without a warning, `"settings": {...}` included, and the plugin
  then runs with its defaults.
- `enabled` can be left out: a source is on unless it says `false`.
- A password goes here too, in plain text. Use a throwaway key, or keep the test folder outside the
  CoolerVideo folder: `plugin-test\` isn't ignored by git, so a real key there could be committed.
- Save it as UTF-8 without a byte-order mark, like `plugin.json`. PowerShell 7's `Set-Content` does. Windows
  PowerShell 5.1's `Set-Content` writes the ANSI code page, and with `-Encoding utf8` it adds a byte-order
  mark, so for text like `Zürich` use `[IO.File]::WriteAllText("$PWD\plugin-test\data.json", '...')` there.
  A `data.json` the player can't read gives a log line, `... data.json isn't valid (...)`, and then only
  `time`.

What else to know:

- **What the output shows:**
  - Each source shows `ok`, `ok (<notes>)` or `failed: <error>`.
  - A source that hasn't answered within the 30 s isn't listed at all: a streaming plugin that doesn't flush
    just doesn't appear.
  - Values are in their default form, with `(stale)` after stale ones. `time` is a built-in that's always
    listed.
  - Log lines, such as plugins that were skipped and why, come first.
  - The exit code is always 0, even when a source failed or `data.json` isn't valid: read the lines.
- **Why the `LOCALAPPDATA` lines:** `wvplay data` reads and writes the same `last-values.json` as the running
  player.
  - With the real one, a plugin that's on in your player may not run at all: the test just shows the player's
    saved values (see [last values](#last-values-are-kept-across-restarts)). It still says `ok`, so the
    signs are that it returns at once and the times are old.
  - The test would also overwrite the file with its own values.
  - The lines in step 5 give each test run a fresh, separate file.
- **Turn on only your plugin** in the test `data.json`.
  - Leave out the built-in sources: the player polls them already.
  - `claude`'s endpoint rate-limits a login that polls too often.
- **A streaming plugin** shows its first line. When `wvplay data` exits, the plugin keeps running until its next
  print fails.
  - So don't pipe `wvplay data` into another command (`| Select-String`, `| Out-String`) while a streaming
    plugin is on: the plugin holds the pipe open, and the command waits until the plugin exits.
- **Clean up:** `plugin-test\` isn't ignored by git, so delete it when you're done.

### The player's log

The player started by the studio logs to `.cache\wvplay.log` in the CoolerVideo folder, a new file each time it
starts. For plugins it shows:

- how many plugins it found, and where: `plugins: 2 in C:\...\plugins`;
- every plugin it skipped, and why;
- each failure once, when it starts or changes, and `... is working again` when the source recovers.

## Using the values in text

Any text can hold placeholders, which the player fills in with live values. The full rules are in
[data.md](data.md#placeholders), and in short:

| written | shows |
|---|---|
| `{key}` | the value in its default form |
| `{key:fmt}` | formatted: `d` (a whole number), `.2f` (2 decimals), or a `strftime` pattern such as `%H:%M` or `%a %d %b` for times |
| `{key\|fallback}` | `fallback` while the value is missing or stale |
| `{key:fmt\|fallback}` | both |
| `{{` and `}}` | literal braces |

- **Default forms:**
  - whole numbers have no decimals, and other numbers are rounded to 1 decimal (`21.5`, `0.3`);
  - times are `%H:%M`, in local time;
  - text is as it is.
- **A format that doesn't fit** the value is ignored: `{disk.drive:d}` just shows the text.
- **Missing or stale** values with no fallback show `--`.
- **Text that looks like a number isn't one,** so send numbers as numbers or `:d` won't work.

## Writing plugins in other languages

The contract is the same in any language:

- read stdin to its end, or read its first line;
- print one JSON object (poll mode), or one per line with a flush after each (stream mode);
- write UTF-8, to stdout and stderr alike;
- exit when done (poll mode), or keep running, even after an error (stream mode).

Name the interpreter in `command`, and reach programs in the plugin's folder through `cmd /c .\...` (poll
mode) or by their full path, as in [Starting the program](#starting-the-program). In stream mode, start the
program [directly](#stream-mode), not through `cmd`.

### A batch file

`plugins\host\plugin.json`:

```json
{
  "id": "host",
  "name": "This computer's name",
  "description": "The computer and user name, from a batch file.",
  "interval": { "default": 3600, "min": 60 },
  "provides": [
    { "key": "host.name", "label": "Computer name", "kind": "text" },
    { "key": "host.user", "label": "User name", "kind": "text" }
  ],
  "run": { "command": ["cmd", "/c", ".\\host.cmd"] }
}
```

`plugins\host\host.cmd`:

```bat
@echo off
rem The settings arrive on stdin as one line of JSON. This plugin has none, so it doesn't read them.
echo {"values": {"host.name": "%COMPUTERNAME%", "host.user": "%USERNAME%"}}
```

`echo` prints the JSON as written. A value holding `&`, `|`, `<`, `>`, `^` or `%` needs batch escaping, so for
anything beyond simple text use another language. To read the settings line, use `set /p settings=`.

### PowerShell

`plugins\uptime\plugin.json`:

```json
{
  "id": "uptime",
  "name": "Uptime",
  "description": "How long Windows has been running, from PowerShell.",
  "interval": { "default": 300, "min": 60 },
  "provides": [
    { "key": "uptime.hours", "label": "Up for", "unit": "h", "kind": "number" },
    { "key": "uptime.since", "label": "Started at", "kind": "time" }
  ],
  "run": { "command": ["powershell", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", "uptime.ps1"] }
}
```

`plugins\uptime\uptime.ps1`:

```powershell
# Read and write UTF-8, whatever the console's code page is.
[Console]::InputEncoding = [Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
$settings = ([Console]::In.ReadToEnd() | ConvertFrom-Json).settings   # unused here

try {
    $boot = (Get-CimInstance Win32_OperatingSystem).LastBootUpTime
    $values = @{
        "uptime.hours" = [math]::Round(((Get-Date) - $boot).TotalHours, 1)
        "uptime.since" = $boot.ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
    }
    @{ values = $values } | ConvertTo-Json -Compress
} catch {
    @{ error = $_.Exception.Message } | ConvertTo-Json -Compress
}
```

- **Windows PowerShell** (`powershell`) needs `-ExecutionPolicy Bypass`. Without it, Windows' default policy
  stops the script, with `running scripts is disabled on this system`.
- **PowerShell 7** works with `["pwsh", "-NoProfile", "-File", "uptime.ps1"]`.
- **Use `-Compress`**, so the JSON is one line, and **print only the JSON** to stdout. Anything else you output
  goes to stdout too.

### A compiled program

Put the `.exe` in the plugin's folder, and start it through `cmd`:

```json
"run": { "command": ["cmd", "/c", ".\\myplugin.exe", "--some-option"] }
```

or by its full path, which you must use in stream mode, because when the player stops a program that `cmd`
started, it stops only `cmd` (see [Stream mode](#stream-mode)):

```json
"run": { "command": ["C:\\Git\\CoolerVideo\\plugins\\myplugin\\myplugin.exe", "--some-option"], "mode": "stream" }
```

`["myplugin.exe"]` and `[".\\myplugin.exe"]` don't work: see [Starting the program](#starting-the-program).

### Python with its own packages

To use packages without installing them for everyone, give the plugin its own virtual environment
(`python -m venv .venv` in its folder, then `.venv\Scripts\pip install ...`), and start that Python through
`cmd`:

```json
"run": { "command": ["cmd", "/c", ".\\.venv\\Scripts\\python.exe", "myplugin.py"] }
```

When `cmd` starts another program, as here, a [time-out](#when-it-runs-poll-mode) kills only `cmd`, so keep
the plugin's own work well inside the time limit.

**In stream mode, don't go through `cmd`:** a restart would leave the old Python running (see
[Stream mode](#stream-mode)). Give the full path of the venv's Python instead. Stopping it stops the Python
it starts too:

```json
"run": { "command": ["C:\\Git\\CoolerVideo\\plugins\\myplugin\\.venv\\Scripts\\python.exe", "myplugin.py"], "mode": "stream" }
```

## Security

- **No sandbox:** a plugin is a program that runs as you, with your files, your network and your environment.
  Only install plugins you trust. Read `plugin.json`'s `command` and the code first.
- **The studio can't add programs:** it can switch plugins on and off and change their settings, but it can't
  create a plugin or change its command. So reaching the studio, even over Tailscale, never means running a
  new program.
- **Settings are untrusted input:** anyone who can use the studio can change them. Check them, and never paste
  them into a shell command line.
- **Keep secrets out of output:**
  - Never print a password or token, to stdout or stderr. Errors and the end of stderr are logged to
    `.cache\wvplay.log` and shown in the studio.
  - Put API keys in request headers rather than URLs, because exception messages often include the URL.
  - Catch exceptions, and report your own message.
- **Passwords are stored in plain text** in `playlist\data.json`. See
  [Passwords and other secrets](#passwords-and-other-secrets).

## Checklist

- [ ] A folder in `plugins\`, with a `plugin.json` that's valid JSON in UTF-8 without a byte-order mark.
- [ ] `id`:
  - lowercase letters, digits, `_` or `-`;
  - unique, and not `ha` or `hwinfo`;
  - the start of every `provides` key and every key you print.
- [ ] `name` is set, and every setting has `key`, `label` and `type`. Every `choice` has `choices` and a `default`.
- [ ] No setting called `type`, `enabled`, `interval`, `hwinfo`, `url`, `token` or `entities`.
- [ ] `command`:
  - names the interpreter;
  - refers to files in the plugin's folder only in arguments, through `cmd /c .\...`, or by their full path;
  - in stream mode, starts the program that does the work directly: no `cmd /c` starting another program.
- [ ] Reads stdin as UTF-8 (`sys.stdin.buffer` in Python), and copes with `null`, `""` and `0`.
- [ ] Output:
  - poll mode: one JSON object, printed last;
  - stream mode: one object per line, flushed, and it keeps running after an error;
  - diagnostics go to stderr;
  - stdout and stderr are UTF-8: Python that prints anything but `json.dumps` output runs with `-X utf8`.
- [ ] Values:
  - numbers are JSON numbers;
  - times have seconds and a time zone (`Z` or `+02:00`);
  - no `NaN`, booleans, lists or `null`;
  - no `"error"` key when it succeeds.
- [ ] Failures print `{"error": "..."}`, with `429` or `503` in it when the server says to slow down, and
      never a secret.
- [ ] Poll mode finishes well within the interval or 30 s, whichever is shorter. Network calls have their own
      timeouts.
- [ ] Stream mode prints at least once every 3 intervals, and not nonstop.
- [ ] Tried by hand, with `wvplay data`, and with **Test** in the studio.

## Troubleshooting

### When the plugin isn't loaded

These go to the player's log and the **Rescan** note, as `<folder>: <problem>`.

| message | fix |
|---|---|
| `plugin.json isn't valid: expected value at line 1 column 1` | the file is empty, isn't JSON, or starts with a byte-order mark: save it as UTF-8 without BOM |
| ``plugin.json isn't valid: missing field `label` at line ...`` | a required field is missing. Every setting and `provides` entry needs a `label`, and the manifest needs an `id` and a `name` |
| ``plugin.json isn't valid: missing field `min` at line ...`` | `interval` needs both `default` and `min` |
| `plugin.json isn't valid: key must be a string at ...` (or another JSON error) | a JSON syntax error at that position, such as a trailing comma or a missing quote |
| `the id "My Plugin" should be lowercase letters, digits, _ or -` | fix the `id` |
| `there's already a source called "pc"` | the id is a built-in's or another plugin's: choose another |
| `the manifest has no name` | give it a `name` |
| `plugin.json has no "run" section` / `"run" has no command` | add `"run": {"command": [...]}` |
| `"run" mode should be "poll" or "stream", not "push"` | fix `mode` |
| `provides "other.x", which isn't under myplugin.` | every `provides` key must start with your id and a dot |
| `setting "units" is a choice with no choices` | add `choices` |
| `setting "s" has an unknown type "slider"` | use `text`, `number`, `bool`, `choice`, `list` or `password` |
| `setting "interval" can't have that key` | rename it: `type`, `enabled` and `interval` are taken |
| `the interval's min and default should be numbers, at least 1` | fix `interval` |

A plugin that doesn't appear at all, with no message, has no `plugin.json` directly in its folder, or hasn't been
rescanned.

### When it fails

These show as the source's error in the studio, in `wvplay data` and in the log.

| message | cause and fix |
|---|---|
| `couldn't run "python": program not found` | not on the player's `PATH`. Install it, or fix `PATH`, then close the studio, start it again from a new terminal, and press **Stop** and **Play on cooler** (see [Starting the program](#starting-the-program)). For a file in your plugin's folder, use `["cmd", "/c", ".\\file"]` or its full path |
| `exited with code 9009: Python was not found; run without arguments to install from the Microsoft Store, ...` | Windows' `python` shortcut ran instead, because no real Python is on the player's `PATH`: the same fix as the row above |
| `couldn't run ".\\tool.exe": The system cannot find the file specified. (os error 2)` | relative paths aren't taken from your plugin's folder: use `["cmd", "/c", ".\\tool.exe"]` or a full path |
| `exited with code 1: 'run.cmd' is not recognized as an internal or external command, ...` | write `.\run.cmd`, not `run.cmd`, after `cmd /c`, and check the name |
| `exited with code 1: ... running scripts is disabled on this system ...` | add `"-ExecutionPolicy", "Bypass"` before `-File` |
| `exited with code N: <traceback or message>` | the program crashed before printing JSON: the end of its stderr says why. Run it [by hand](#by-hand) |
| `exited with code N`, with nothing after it | it wrote nothing to stderr, or text that isn't UTF-8, such as a Python traceback with `ü` in it: start Python with `-X utf8` (see [Encoding](#encoding)), or run it [by hand](#by-hand) |
| `didn't print JSON: <text>` | it printed something other than a JSON object: a stray line *after* the JSON, `NaN`, or plain text |
| `didn't print JSON`, with nothing after it | it printed nothing at all, or a line that isn't UTF-8 before its JSON, such as Python's `print("café")` without `-X utf8` |
| `null` | it printed `"error": null` beside its values: leave the `error` key out when it succeeds |
| `took longer than N s` | the run passed its time limit (the interval or 30 s, whichever is shorter). Add timeouts, or do less per run |
| `printed nothing in N s` | (Test, stream mode, an interval under 20 s) no line in time: flush after each line |
| `the output has neither "values" nor "error"` | the JSON object lacks both, or `values` isn't an object |
| `nothing usable: ignored x.y isn't under myplugin.; ...` | every key was outside your id or of the wrong type: see [Keys](#keys) |
| `ok (ignored ...)` (a note, not an error) | some keys or values were ignored: the note names them |
| `exited with code N: ...; restarting in N s` | (stream mode) the program exited, and the player restarts it, waiting up to 60 s |
| `exited with code 0; restarting in N s`, and your own error only in the log | (stream mode) it printed `{"error": ...}` and exited: keep running instead, and print the error again now and then (see [Stream mode](#stream-mode)) |
| `... BrokenPipeError ...` | it printed more than [1 MiB](#size-limits) in one run (or the player went away) |
| `unknown source type "myplugin"` | `data.json` names an id that the player didn't load: a typo, a skipped plugin, or no rescan yet |
| `data: ...\data.json isn't valid (Data at line 1, column 60)` (log) | a value in `data.json` has the wrong type, such as a setting named `hwinfo` or `url` holding the wrong kind of value. Nothing is polled until it's fixed |

### Other symptoms

| symptom | cause |
|---|---|
| The text shows `--` | the value is missing or stale: check the source's status, and its interval (stale after 3 intervals) |
| A setting shows up as `ZÃ¼rich` | the plugin decoded stdin with the ANSI code page: read it as UTF-8 (`sys.stdin.buffer`) |
| A number won't take `:d` or `.1f` | it was sent as text (`"21.5"`): send a JSON number |
| A time shows as text | it has no time zone, no seconds, or a zone with no colon (`+0200`): write `2026-10-14T10:00:00Z` or `...+02:00` |
| `wvplay data` shows old values, and the plugin didn't run | its saved values are younger than its interval: use a fresh `LOCALAPPDATA`, as [above](#testing-from-the-command-line) |
| A streaming plugin's values arrive late or never | it isn't flushing stdout after each line |
| A streaming plugin's values go stale while it's running | it prints less often than every 3 intervals, or it printed a line that isn't UTF-8 (see [Stray prints](#stray-prints)) |
| A streaming plugin seems to run twice, or keeps using old settings | it's started through `cmd /c` (or another wrapper), so each restart left the old copy running: start it [directly](#stream-mode) |
| A source shows no error but stopped updating, and `wvplay data` leaves it out | a long message with non-ASCII text hit a player bug: see [Size limits](#size-limits) |
| `wvplay data` doesn't list a source at all | it gave no answer within 30 s: a streaming plugin that doesn't flush, or that printed a line that isn't UTF-8 first, or the player bug in the row above |
| A test `data.json` setting is ignored | settings sit next to `type`, not under `"settings"`: see [Testing from the command line](#testing-from-the-command-line) |
| Changing a default in `plugin.json` changes nothing | `data.json` already holds the old value: change it in the studio |
| The studio says "Couldn't save the data sources: ... must be one of ..." | a `choice` setting has no `default`, so the studio saved `""`: add a default |
| The password is "Not set" again | a `text` setting was changed, which drops a saved password: type it again |
| Test says "The player isn't running" while it is | the test took longer than the studio's 20 s: a slow poll plugin, or a streaming plugin that printed no line (check that it flushes) |
| A setting arrives as `Z?rich` when you test by hand | Windows PowerShell 5.1 piped it as ASCII: see [By hand](#by-hand) |
