# example-claude-usage: a widget with no inputs

Your Claude session and weekly usage on a dark panel, 520 x 140: `Claude 62% · resets in 2h 34m` on the
first line, `this week 31%` on the second. It's the example in `docs/widgets.md`, "Making a widget".

## No inputs

This widget declares no inputs. Its two texts carry their own live values, written in the widget itself:

- `Claude {claude.session:d}% · resets in {claude.session_resets_in}`
- `this week {claude.week:d}%`

So its copies have nothing to fill in: a copy's properties say "This widget sets its own text and pictures.
Edit widget to change them." That's the simplest kind of widget, reusable content. Drag it into **Global**
and it's on every scene.

## What it needs

The **Claude usage** source, on in the studio's **Data sources…** window. It reads Claude Code's own login,
so the values go stale (and the texts show `--`) when Claude Code hasn't run for a while. The keys it uses,
and the others it provides (`claude.session_resets`, `claude.week_resets_in`, ...), are listed in
`docs/data.md`.

## Editing it

The examples open read-only in the studio, because they're tracked in git. **Duplicate to edit** makes your
own copy of the folder, under a new id: change the texts, the font or the colours there, or add an input to
make something settable per copy.
