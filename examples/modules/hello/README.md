# hello — a minimal external module

The smallest module the host will load, kept as a starting point for writing another one and
as a check that the external-module path still works.

## Install

```bash
mkdir -p ~/.local/share/chairphoto/modules/hello
cp chairphoto-module.json index.js ~/.local/share/chairphoto/modules/hello/
```

Restart ChairPhoto — modules are discovered only at startup — then enable **Hello** in the
Modules settings panel. `onLoad` writes a `hello.loaded_at` timestamp into the catalog's
settings, so a load can be confirmed without watching the UI:

```bash
sqlite3 ~/.local/share/chairphoto/default.chairphoto \
  "SELECT key, value FROM settings WHERE key LIKE 'hello.%';"
```

The key is `hello.loaded_at` rather than `loaded_at`: the host namespaces settings by module
id, so one module cannot read or overwrite another's.

To try it without touching a real catalog, point the whole app at a scratch profile —
`app_data_dir()` honours `XDG_DATA_HOME`:

```bash
XDG_DATA_HOME=/tmp/cp-test chairphoto
```

Modules then load from `/tmp/cp-test/chairphoto/modules/`.

## Why it renders a string

`render()` returns a bare string rather than elements. That is the current ceiling, not a
simplification: an external module is imported straight from disk and has no way to reach the
host's React instance — no global, no import map, and a second React copy would break hooks
and context. `ReactNode` includes strings, so text is all that can be contributed today.

See `docs/module-capabilities.md` for what would lift that, and for what a module can and
cannot reach through the host API.

## Manifest fields

`id`, `name`, `version`, `description`, `entrypoint`, and `minHostVersion` are required, and
`id` must match the `id` on the exported module object or the loader rejects it.
`minHostVersion` is a floor: a module declaring a version newer than the running host is
listed in the Modules panel with the reason, but its code is never imported.

`backendFeature` and `requires` are optional; `docs/plugin-system.md` documents both.

## Failure modes

A module that throws while importing, or whose default export is the wrong shape, is skipped
with a console error. It never breaks discovery of other modules and never crashes the app.
