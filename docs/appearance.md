---
title: "Appearance — ChairPhoto Standard and Follow Omarchy"
description: "Two appearance modes: the app-owned palette, or the same semantic tokens filled from the Omarchy runtime theme."
tags:
  - chairphoto/core
  - chairphoto/platform
aliases:
  - "Appearance"
  - "Omarchy"
  - "Theming"
---

# Appearance — ChairPhoto Standard and Follow Omarchy

ChairPhoto's UI is painted from one set of semantic color tokens. Two modes fill them:

- **ChairPhoto Standard** — the app-owned palette. Frontend-only: no backend involvement,
  no files read, identical on every platform. The default, and the fallback whenever the
  Omarchy mode has nothing valid to offer.
- **Follow Omarchy** — the same tokens filled from the palette of the currently active
  [Omarchy](https://omarchy.org) theme, so ChairPhoto matches the desktop and follows
  theme switches live.

Only the Omarchy half has a Rust side, and that is what this document describes: parser
and watcher in `crates/core/src/appearance/`, called directly by the GPUI app (no command
wrapper) through `read_current_theme()`.

## The Omarchy 4 contract

Omarchy keeps its runtime state under `$XDG_STATE_HOME/omarchy` (default
`~/.local/state/omarchy`). Two files define the active theme:

| File | Content |
|---|---|
| `current/theme/colors.toml` | The active theme's palette (TOML, snake_case keys). |
| `current/theme.name` | The theme's display name; trimmed contents used verbatim. |

`colors.toml` must carry `mode` (`"light"` or `"dark"`) and five required colors:
`accent`, `selection`, `muted`, `background`, `foreground`. Beyond those, ChairPhoto
understands the optional background/foreground variants (`dark_background`,
`darker_background`, `lighter_background`, `dark_foreground`, `light_foreground`,
`bright_foreground`) and the twelve terminal colors (`red`, `yellow`, `green`, `cyan`,
`blue`, `magenta`, and their `bright_*` variants). Every color present must be `#rgb`,
`#rrggbb`, or `#rrggbbaa` (case-insensitive); one invalid value invalidates the whole
palette — a theme that half-parses would paint a half-broken UI. Unknown keys are
**ignored, never rejected**: a theme carrying `orange`, `brown`, or an app-specific
section still parses. A missing `theme.name` costs only the name.

### Theme switches are atomic swaps

Omarchy 4 switches themes by *removing* `current/theme`, atomically replacing it, and
then rewriting `theme.name`. There is a window in which either file — or the whole theme
directory — is absent. That mid-swap state is normal and transient, never treated as an
error by itself: readers ride it out (below) and only report the settled outcome.

## Reading the theme: `read_current_theme`

`read_current_theme()` returns a `SystemThemeResult`:

```rust
SystemThemeResult { available: true, theme_name: Some("tokyo-night".into()), palette: Some(palette) }
```

Failure of *any* kind — no Omarchy on this machine, missing files, malformed TOML, an
invalid color — is `SystemThemeResult { available: false, theme_name: None, palette: None }`,
never an `Err`. A broken theme and an absent theme demand the same caller reaction, so they
get the same shape. The palette's optional fields are `None` when the theme omits them;
`colors.toml`'s keys stay snake_case, matched one to one by the struct's own fields (the
`camelCase` serialization on [`SystemThemeResult`] and [`OmarchyPalette`] is for the handful
of tests that round-trip it through JSON, not for a wire this struct crosses at runtime).

## Watching for switches

At startup (`app::boot_with`, which the GPUI app calls) the backend starts a singleton
watcher thread — **only if the Omarchy state root exists**. It polls every 2 seconds with
a cheap fingerprint (mtime + length of both files, absence included as a value). On a
fingerprint change it settle-reads: both files re-read every 100 ms until two consecutive
reads agree byte-for-byte *and* parse cleanly, up to ~20 tries (~2 s), riding out the
atomic swap above. The settled outcome is broadcast as:

- **Event**: `appearance:theme_changed`
- **Payload**: the same `SystemThemeResult` shape as `read_current_theme` — the new theme,
  or `available: false` when the theme vanished or never settled on something valid.

The watcher emits only when the settled outcome *differs* from the last known one, so the
2-second tick never spams events, and a rewrite that changes nothing stays silent.

## Fallback semantics

`available: false` — from `read_current_theme` or the event — means **switch to ChairPhoto
Standard now**. GPUI must never keep painting a stale Omarchy palette while the state on
disk is broken or gone; the next valid settled theme arrives as a fresh
`appearance:theme_changed` and re-enables following.

Absence of Omarchy is a supported, non-degraded state, not an error: `read_current_theme`
answers `available: false`, no watcher thread starts, and nothing polls. ChairPhoto
Standard is simply the only mode with something to show.
