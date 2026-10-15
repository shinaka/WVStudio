# example-weather: a wvplay plugin, and the template for your own

A plugin is a folder in `plugins/` (next to `playlist/`) holding `plugin.json` and the program it runs.
This one shows the current weather from [Open-Meteo](https://open-meteo.com/), which needs no account.
Turn it on in the studio's **Data sources…** window, set your latitude and longitude, and use
`{weather.temp}°`, `{weather.desc}` and so on in a text widget.

## plugin.json

The manifest: what the studio shows, and how the player runs the program.

- `id`: lowercase, the prefix of every key the plugin sets (`weather` → `weather.temp`).
- `name`, `description`: for the sources window.
- `interval`: `default` and `min` seconds between runs.
- `settings`: the form. Each has a `key`, `label`, `type` (`text`, `number`, `bool`, `choice` with
  `choices`, `list`, or `password`) and an optional `default`. A `password` is write-only in the studio
  and never logged.
- `provides`: the keys you set, each with a `label`, optional `unit` and `kind` (`number`, `text`,
  `time`). A `"pattern": true` entry like `weather.*` covers keys you make up as you go.
- `run`: `command` (a list: the program and its arguments) and `mode`, `poll` or `stream`.

## The protocol

- The command runs in the plugin's folder, with no window.
- It reads one JSON object on stdin: `{"settings": {"latitude": 40.7, ...}}`, every setting in the
  manifest, with its default when not set.
- **`poll` mode:** it runs once per interval and prints one JSON object, then exits:
  - `{"values": {"weather.temp": 21.5, "weather.desc": "Clear"}}`, or
  - `{"error": "what went wrong"}`.
  It's killed if it runs longer than the interval, or 30 s at most.
- **`stream` mode:** it keeps running and prints one such object per line whenever there's news
  (flush stdout after each line). If it exits, the player restarts it, waiting a little longer each time.
- Values are numbers, text, or ISO time strings (`2026-10-14T10:00:00Z` becomes a time, so `{key:%H:%M}` works).
- Only keys starting with `<id>.` are accepted; others are ignored and noted in the source's status.
- Values go stale after three intervals without a successful run, and the text shows its fallback.

Test a plugin without the screen: `wvplay data path\to\playlist.json` runs every enabled source once
and prints the values, and the studio's **Test** button does the same for one source.

## Writing your own

Copy this folder, rename it, change the `id` and the rest of `plugin.json`, and replace `weather.py`.
Anything that can read stdin and print JSON will do: Python, PowerShell (`"command": ["powershell",
"-NoProfile", "-ExecutionPolicy", "Bypass", "-File", "my.ps1"]`), a .cmd or a compiled program in this folder
(`"command": ["cmd", "/c", ".\\my.cmd"]`). Plugins are read when the player starts and when you press
**Rescan** in the sources window.

The full guide, with every rule, testing and troubleshooting, is [docs/plugins.md](../../docs/plugins.md).
