# Data sources: the catalog, plugins, and the sources window (spec)

Builds on data.md. The goal is to add sources without touching the player or studio, and to manage them
in a dedicated window, not the sidebar.

## The catalog: every source describes itself

Built-in sources (`claude`, `pc`, `homeassistant`, `time`) and plugins share one manifest shape, and the
studio builds its UI entirely from these manifests:

```json
{
  "id": "weather",
  "name": "Weather",
  "description": "Current conditions from Open-Meteo (no account needed).",
  "interval": { "default": 600, "min": 60 },
  "settings": [
    { "key": "latitude",  "label": "Latitude",  "type": "number", "default": 40.7 },
    { "key": "longitude", "label": "Longitude", "type": "number", "default": -74.0 },
    { "key": "units", "label": "Units", "type": "choice", "choices": ["celsius", "fahrenheit"], "default": "celsius" }
  ],
  "provides": [
    { "key": "weather.temp", "label": "Temperature", "unit": "°", "kind": "number" },
    { "key": "weather.desc", "label": "Conditions", "kind": "text" },
    { "key": "weather.*", "label": "Anything else it reports", "pattern": true }
  ]
}
```

- **Setting types:**
  - `text`, `number`, `bool`, `choice` (with `choices`) and `list` (one entry per line);
  - `password`: write-only in the UI, never logged, never returned to the browser, and only ever given to
    its own source.
- **Setting extras,** all optional and passed to the studio as they are: `help` (a line under the control),
  `placeholder` (grey text in an empty box), and for a number, `min` and `max` (the box's limits, as hints).
- **`provides`** documents keys up front, so the window and autocomplete can show them before any value
  arrives. A `pattern: true` entry covers dynamic keys such as `hwinfo.*`, `lhm.*` or `ha.*`; the live
  values fill those in.
- **Built-ins:** their manifests are compiled into the player. Home Assistant's `ha.*` keys come from its
  `entities` setting.

## Plugins

- **Where they live:** a plugin is a folder in `plugins/`, next to `playlist/`. It holds `plugin.json`,
  which is the manifest above plus a `run` section, and whatever program it runs:

```json
"run": { "command": ["python", "weather.py"], "mode": "poll" }
```

- **How they run:**
  - The command runs in the plugin's folder, with no window, in a Windows job that holds it and every
    program it starts. Stopping a plugin (a time-out, a settings change, **Rescan**, the player exiting by
    any means) stops the whole job, so a wrapper such as `cmd /c` never leaves a program behind.
  - Its settings arrive as one JSON object on stdin: `{"settings": {...}}`. A setting that data.json doesn't
    set gets the manifest's default, or a `choice`'s first choice, or `null`.
  - **`poll` mode:** the command runs once per interval, and prints one JSON object:
    `{"values": {"weather.temp": 21.5, "weather.desc": "Clear"}}` or `{"error": "..."}`. It's stopped if it
    takes longer than the interval, or 30 s at most, and so is anything it leaves running when it exits. It
    may print 1 MiB in all.
  - **`stream` mode:** the program keeps running and prints one such JSON object per line whenever it has
    news, each line 1 MiB at most, with no limit over its life. One that prints faster than the player takes
    its lines is held back: only a few lines wait, and then its prints wait. If it exits, it's restarted with a
    back-off.
    It isn't polled, so a 429 or 503 in its error doesn't make the player back off.
- **Rules:**
  - A plugin may only set keys that start with its own `id.`, so a plugin can't overwrite `claude.week`.
  - A plugin's id can't be a built-in's (`claude`, `pc`, `homeassistant`, `time`), the first part of keys a
    built-in sets (`ha`, `hwinfo`, `lhm`), or another plugin's: such a plugin is refused, with the reason.
  - A setting can't be called `type`, `enabled` or `interval`, end in `_set` (the studio's flag for a saved
    password), or have another setting's key. Any other key is the plugin's own, even one a built-in uses
    (`url`, `token`...).
  - An interval's `min` and `default` are 1 s to a day, and the player never waits longer than a day,
    whatever data.json says.
  - Values are numbers, text, or ISO time strings. A plugin can set 1000 keys at most: new keys past that are
    ignored, and noted.
  - `"error": null` is no error: an object with a `values` object and `"error": null` succeeds.
  - Output is read as UTF-8, a line at a time, and any byte that isn't UTF-8 becomes `�`, so it costs one
    character, never the line. stderr is decoded the same way, and its end shown when the program fails.
  - Bad output is logged briefly, and the source shows the error.
- **Security:**
  - Plugins are programs you put in that folder yourself.
  - The studio can turn plugins on and off and change their settings, but can never create a plugin or change
    its `command`. Reaching the studio, even over Tailscale, never means being able to run a new program.
  - Plugin folders are read once at start-up and when you press **Rescan**.
- **The example:** `plugins/example-weather/` is a ready-to-use plugin, and also the template for writing
  new ones.

## data.json

It keeps the same shape: `sources: [{ "type": <id>, "enabled", "interval", ...settings }]`, where plugins
use their id as the type. A source that's in the catalog but not yet in data.json counts as off, with
default settings.

## Player API additions

- `GET /catalog`: every manifest (without `run`), each with
  `status: {enabled, ok, error, notes, updated_at}`.
- `POST /catalog/rescan`: re-reads the plugins folder.
- `/test` works for plugins too: it runs the plugin once with the given settings. A plugin's run gives up
  after 30 s at most, so the studio waits longer than that (45 s) and shows the player's own error.

## The studio

- **The sidebar's Data section** shrinks to a **Data sources…** button, plus one status line, e.g.
  "3 on, 1 error".
- **The sources window** is a large dialog over the page:
  - **On the left:** every source in the catalog, each with an on/off switch and a status dot for on, off
    or error.
  - **On the right, for the selected source:**
    - its description;
    - its settings form, built from the manifest, with its interval;
    - a **Test** button;
    - a **Provides** table: each key with its label, unit and live value. Clicking a key inserts `{key}` into
      the selected text widget, if there is one, and otherwise copies it.
  - **At the bottom:** where the plugins folder is, plus **Rescan**.
  - Changes save as you make them, as now.
- **Autocomplete in the text box:**
  - Typing `{` opens a list of keys under the cursor, filtered as you type. Each entry shows its label,
    source and current value.
  - ↑/↓ moves, Enter or Tab accepts (inserting `key}`), and Esc closes.
  - After `{key:`, it suggests formats that fit the value's kind:
    - for numbers: `d` (whole) and `.1f`;
    - for times: `%H:%M`, `%a %H:%M` and `%d %b`.
  - Keys from `pattern` entries are suggested from their current live values.
