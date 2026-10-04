//! The `chairphoto://` scheme handler for a development build of the app.
//!
//! An installed build registers the scheme through its `.desktop` file
//! (`packaging/chairphoto.desktop`, `MimeType=x-scheme-handler/chairphoto`). A dev build is
//! not installed, so it does not register itself on every start; it does so **only when
//! asked**: with `CHAIRPHOTO_CLAIM_SCHEME=1` it writes
//! `$XDG_DATA_HOME/applications/chairphoto-dev-handler.desktop` pointing at its own binary,
//! refreshes that directory's MIME cache, and makes itself the default (`xdg-mime default`).
//! Without the variable it writes nothing.
//!
//! **Why nothing by default.** A handler entry alone is not inert: when the user has no
//! explicit default for `x-scheme-handler/chairphoto` in `mimeapps.list`, the XDG MIME
//! Applications spec lets any installed entry that lists the type become the handler. A dev
//! build run once would then quietly take `chairphoto://` links away from the installed
//! `chairphoto.desktop` entry, or from whatever else the user has set. Its own file name
//! differs from the installed entry's, so running a dev build never overwrites the packaged
//! one; it claims the scheme only when someone opts in.
//!
//! **Exec quoting.** xdg-open (what Electron apps such as Obsidian open links with) takes the
//! first word of `Exec` verbatim and looks it up with `command -v`, so a quoted path "does not
//! exist" and it silently falls back to the browser; gio parses quoting fine. The line is
//! written unquoted whenever the path has no character the Desktop Entry spec reserves, and
//! quoted per the spec otherwise (gio then still works; xdg-open cannot).
//!
//! **Stale pre-#167 entry.** The handler file and binary were renamed from `chairphoto-gpui`
//! at #167; a developer who opted in before that rename has a `chairphoto-gpui-handler.desktop`
//! and possibly a `mimeapps.list` default pointing at it, which would otherwise outrank the
//! packaged entry forever. Registering the current handler also removes both, best-effort:
//! the file only when it is recognisably ChairPhoto's own, and the default line only when the
//! file is confirmed ChairPhoto's own (and then removed) or is simply absent — never when a
//! same-named file exists but is someone else's, since the default would then name a real,
//! foreign handler. Never runs `xdg-mime`/`xdg-settings` — `mimeapps.list` is rewritten
//! directly as text, via a temp file and rename next to the real destination (its target, if
//! it is a symlink) so a crash mid-write can't truncate it, and every other line, section,
//! and line ending is preserved byte-for-byte.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The handler entry's file name under `applications/`.
pub const HANDLER_FILE: &str = "chairphoto-dev-handler.desktop";
/// [`HANDLER_FILE`]'s name before the #167 rename (`chairphoto-gpui` → `chairphoto`). A
/// developer who opted in before that rename has this file, and possibly a `mimeapps.list`
/// default pointing at it, left behind — both stale, since nothing writes or reads this name
/// any more. [`register_dev_handler`] removes them, best-effort, when it registers the
/// current one.
const STALE_HANDLER_FILE: &str = "chairphoto-gpui-handler.desktop";
/// The MIME type of the scheme.
pub const SCHEME_MIME: &str = "x-scheme-handler/chairphoto";
/// Set to `1` to register the dev build as the scheme's handler and make it the default.
pub const CLAIM_ENV: &str = "CHAIRPHOTO_CLAIM_SCHEME";

/// The `Exec` value for `exe` with the URL placeholder: `<exe> %u`. `None` for a path with a
/// line break or another control character, which a one-line desktop-file value cannot
/// carry (it would end the `Exec=` line and start a line of its own).
pub fn exec_value(exe: &Path) -> Option<String> {
    let path = exe.to_string_lossy();
    if path.chars().any(char::is_control) {
        return None;
    }
    // The spec's reserved characters, plus `%` (field codes) and `=`/`'` for good measure:
    // anything outside this safe set gets quoted.
    let safe = |c: char| c.is_ascii_alphanumeric() || "/._-+,@:".contains(c);
    if !path.is_empty() && path.chars().all(safe) {
        return Some(format!("{path} %u"));
    }
    // Inside double quotes, `"`, `` ` ``, `$` and `\` are backslash-escaped; `%` is doubled.
    // Then the whole value is a desktop-file string, where a backslash is itself escaped.
    let mut quoted = String::from("\"");
    for c in path.chars() {
        match c {
            '"' | '`' | '$' | '\\' => {
                quoted.push('\\');
                quoted.push(c);
            }
            '%' => quoted.push_str("%%"),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    Some(format!("{} %u", quoted.replace('\\', "\\\\")))
}

/// The handler entry for `exe`; `None` when [`exec_value`] cannot express the path.
pub fn handler_entry(exe: &Path) -> Option<String> {
    Some(format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=ChairPhoto (dev build)\n\
         Exec={}\n\
         Terminal=false\n\
         NoDisplay=true\n\
         MimeType={SCHEME_MIME};\n\
         StartupWMClass={}\n",
        exec_value(exe)?,
        crate::APP_ID,
    ))
}

/// `$XDG_DATA_HOME`, else `~/.local/share`.
pub fn data_home() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
}

/// `$XDG_CONFIG_HOME`, else `~/.config` — where `xdg-mime default` records the scheme's
/// default handler, in `mimeapps.list`'s `[Default Applications]` section (the "generic",
/// non-KDE/LXQt case `/usr/bin/xdg-mime`'s `make_default_generic` takes; the one this app's
/// own `claim_default` goes through).
pub fn config_home() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
}

/// Whether the environment opts the dev build in ([`CLAIM_ENV`] is `1`).
pub fn opted_in() -> bool {
    std::env::var(CLAIM_ENV).is_ok_and(|v| v == "1")
}

/// Register the dev build as the scheme's handler — only when `opted_in`; otherwise do
/// nothing at all and return `Ok(None)`. Registering first removes a stale pre-#167
/// [`STALE_HANDLER_FILE`] entry and its `mimeapps.list` default if either is ChairPhoto's own
/// ([`clean_stale_handler`], best-effort), then writes the entry for `exe` under
/// `data_home/applications` (only when it changed), refreshes that directory's MIME cache,
/// and calls `claim_default` (in the app: [`claim_default`], `xdg-mime default`; tests pass
/// their own so they never touch the user's `mimeapps.list`). Returns the entry's path.
///
/// `update-desktop-database` and `xdg-mime` are optional: a missing tool is logged and skipped.
pub fn register_dev_handler(
    data_home: &Path,
    config_home: &Path,
    exe: &Path,
    opted_in: bool,
    claim_default: impl FnOnce(),
) -> io::Result<Option<PathBuf>> {
    if !opted_in {
        return Ok(None);
    }
    clean_stale_handler(data_home, config_home);
    let entry = handler_entry(exe).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, format!("{} cannot go in a desktop entry", exe.display()))
    })?;
    let dir = data_home.join("applications");
    std::fs::create_dir_all(&dir)?;
    let file = dir.join(HANDLER_FILE);
    if std::fs::read_to_string(&file).ok().as_deref() != Some(entry.as_str()) {
        std::fs::write(&file, &entry)?;
        run_tool(Command::new("update-desktop-database").arg(&dir));
    }
    claim_default();
    Ok(Some(file))
}

/// Remove a stale pre-#167 [`STALE_HANDLER_FILE`] entry under `data_home/applications` and,
/// if `config_home/mimeapps.list` still defaults the scheme to it, that default line too —
/// each only when it is recognisably ChairPhoto's own, never merely a file or line that
/// happens to share the name. Best-effort: a missing file, an unreadable `mimeapps.list`, or
/// any I/O error just means nothing to clean up here; this never fails registration.
///
/// The default line is dropped only when [`STALE_HANDLER_FILE`] is confirmed ChairPhoto's own
/// (and then removed) **or is simply absent** — never when a same-named `.desktop` file
/// exists but is someone else's: the default would then point at a real, foreign handler, not
/// a stale one.
///
/// Never runs `xdg-mime`/`xdg-settings` — the default is edited directly as text, so no
/// external tool touches the real `mimeapps.list` outside what this function reads/writes
/// itself (tests point `config_home` at a scratch directory).
fn clean_stale_handler(data_home: &Path, config_home: &Path) {
    let stale = data_home.join("applications").join(STALE_HANDLER_FILE);
    // `symlink_metadata` does not follow a symlink, so a symlink planted at this exact name
    // is left alone rather than removed or read through — and, since its content is never
    // read, it counts as "not confirmed ours" below, same as an unreadable file.
    let stale_is_own_or_absent = match std::fs::symlink_metadata(&stale) {
        Ok(meta) if meta.is_file() => match std::fs::read_to_string(&stale) {
            Ok(content) => {
                let own = is_own_entry(&content);
                if own {
                    let _ = std::fs::remove_file(&stale);
                }
                own
            }
            Err(_) => false,
        },
        Ok(_) => false, // present but not a plain file (e.g. a symlink): not confirmed ours
        Err(_) => true, // absent: nothing foreign to protect, the stale default can go too
    };
    if !stale_is_own_or_absent {
        return;
    }

    let mimeapps = config_home.join("mimeapps.list");
    if let Ok(content) = std::fs::read_to_string(&mimeapps) {
        if let Some(updated) = remove_stale_default(&content) {
            let _ = write_mimeapps(&mimeapps, &updated);
        }
    }
}

/// Overwrite `path`'s content with `content`: a temp file next to the real destination, then
/// a rename, so a crash mid-write never leaves a truncated `mimeapps.list` behind (unlike a
/// plain truncating `fs::write`). If `path` is a symlink, the real destination is resolved
/// through it first, so the rename lands on the symlink's *target* and the symlink itself is
/// left exactly as it was — the same "edit the target" behaviour `xdg-mime` itself has.
fn write_mimeapps(path: &Path, content: &str) -> io::Result<()> {
    let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = real.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
    let tmp = dir.join(format!(".mimeapps.list.chairphoto-tmp-{}-{nanos}", std::process::id()));
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, &real)
}

/// Whether a desktop-entry's content is recognisably ChairPhoto's own dev handler: either the
/// shape [`handler_entry`] writes today, or the pre-#167 shape (`Name=ChairPhoto (GPUI dev
/// build)`, before the `chairphoto-gpui` → `chairphoto` rename) that [`STALE_HANDLER_FILE`]
/// is the only file this module can ever name. Not merely a file that happens to share a name.
fn is_own_entry(content: &str) -> bool {
    (content.contains("\nName=ChairPhoto (dev build)\n") || content.contains("\nName=ChairPhoto (GPUI dev build)\n"))
        && content.contains(SCHEME_MIME)
}

/// Remove `x-scheme-handler/chairphoto=chairphoto-gpui-handler.desktop` from
/// `[Default Applications]` in a `mimeapps.list`'s text, if present there; `None` if nothing
/// changed (including when the key is present but points elsewhere — someone else's default
/// is left alone, and a same-named key outside `[Default Applications]`, e.g. in `[Added
/// Associations]`, is left alone too). Every other line, section and ordering is preserved
/// byte-for-byte, CRLF line endings and a missing final newline included.
fn remove_stale_default(content: &str) -> Option<String> {
    let target = format!("{SCHEME_MIME}={STALE_HANDLER_FILE}");
    let mut in_default = false;
    let mut changed = false;
    let mut out = String::with_capacity(content.len());
    for (line, terminator) in lines_with_terminators(content) {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_default = trimmed == "[Default Applications]";
        } else if in_default && trimmed == target {
            changed = true;
            continue;
        }
        out.push_str(line);
        out.push_str(terminator);
    }
    changed.then_some(out)
}

/// Splits `content` into `(line, terminator)` pairs — `terminator` is `"\r\n"`, `"\n"`, or
/// `""` for a final line with no trailing newline — such that concatenating `line` +
/// `terminator` for every pair reproduces `content` exactly. `str::lines` alone can't do
/// this: it discards both the line-ending style and whether the last line had one.
fn lines_with_terminators(content: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    let mut rest = content;
    while !rest.is_empty() {
        match rest.find('\n') {
            Some(idx) => {
                let (line, after) = rest.split_at(idx);
                rest = &after[1..]; // skip the '\n'
                match line.strip_suffix('\r') {
                    Some(line) => out.push((line, "\r\n")),
                    None => out.push((line, "\n")),
                }
            }
            None => {
                out.push((rest, ""));
                rest = "";
            }
        }
    }
    out
}

/// Make [`HANDLER_FILE`] the scheme's default handler (`xdg-mime default`, which writes the
/// user's `mimeapps.list`).
pub fn claim_default() {
    run_tool(Command::new("xdg-mime").args(["default", HANDLER_FILE, SCHEME_MIME]));
}

fn run_tool(command: &mut Command) {
    let name = command.get_program().to_string_lossy().to_string();
    match command.status() {
        Ok(status) if status.success() => {}
        Ok(status) => eprintln!("deep-link registration: {name} exited with {status}"),
        Err(e) => eprintln!("deep-link registration: {name} unavailable: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// A scratch data home, removed on drop.
    struct Home(PathBuf);

    impl Home {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            Home(std::env::temp_dir().join(format!("cp-desktop-{tag}-{}-{nanos}", std::process::id())))
        }
    }

    impl Drop for Home {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_plain_path_is_written_unquoted_for_xdg_open() {
        assert_eq!(
            exec_value(Path::new("/home/me/src/chairphoto/target/debug/chairphoto-gpui")).as_deref(),
            Some("/home/me/src/chairphoto/target/debug/chairphoto-gpui %u")
        );
    }

    #[test]
    fn a_path_with_reserved_characters_is_quoted_per_the_spec() {
        assert_eq!(exec_value(Path::new("/home/me/My Photos/cp")).as_deref(), Some("\"/home/me/My Photos/cp\" %u"));
        // `$` is escaped inside the quotes (`\$`), then the backslash for the desktop file.
        assert_eq!(exec_value(Path::new("/opt/a$b")).as_deref(), Some("\"/opt/a\\\\$b\" %u"));
        assert_eq!(exec_value(Path::new("/opt/100%")).as_deref(), Some("\"/opt/100%%\" %u"));
    }

    /// A line break in the path would end the `Exec=` line and start a line of its own
    /// (`MimeType=…`, say): such a path is refused, never written.
    #[test]
    fn a_path_with_a_line_break_is_refused() {
        assert_eq!(exec_value(Path::new("/opt/x\nMimeType=text/html;/cp")), None);
        assert_eq!(exec_value(Path::new("/opt/x\r/cp")), None);
        assert_eq!(exec_value(Path::new("/opt/x\t/cp")), None);
        assert_eq!(handler_entry(Path::new("/opt/x\n/cp")), None);
        let home = Home::new("newline");
        let result = register_dev_handler(&home.0, &home.0, Path::new("/opt/x\n/cp"), true, || panic!("claimed"));
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert!(!home.0.join("applications").join(HANDLER_FILE).exists());
    }

    #[test]
    fn the_entry_registers_the_scheme() {
        let entry = handler_entry(Path::new("/x/chairphoto")).unwrap();
        assert!(entry.contains("\nExec=/x/chairphoto %u\n"), "{entry}");
        assert!(entry.contains("\nMimeType=x-scheme-handler/chairphoto;\n"), "{entry}");
        assert!(entry.contains("\nNoDisplay=true\n"), "{entry}");
    }

    /// The installed entry declares the scheme and passes the URL with `%u`.
    #[test]
    fn the_packaged_entry_declares_the_scheme() {
        let entry = include_str!("../../../packaging/chairphoto.desktop");
        let lines: Vec<&str> = entry.lines().collect();
        assert!(lines.contains(&"Exec=chairphoto %u"), "Exec");
        assert!(lines.contains(&"MimeType=x-scheme-handler/chairphoto;"), "MimeType");
        assert!(lines.contains(&"StartupWMClass=chairphoto"), "StartupWMClass");
    }

    /// Without the opt-in a dev build writes nothing and claims nothing: no `applications/`
    /// directory appears in the data home.
    #[test]
    fn without_the_opt_in_nothing_is_registered() {
        let home = Home::new("no-opt-in");
        let claimed = Cell::new(false);
        let result = register_dev_handler(&home.0, &home.0, Path::new("/a/chairphoto"), false, || claimed.set(true));
        assert_eq!(result.unwrap(), None);
        assert!(!home.0.join("applications").exists(), "a handler entry was written without the opt-in");
        assert!(!claimed.get(), "the default was claimed without the opt-in");
    }

    /// With the opt-in: the entry lands under `applications/`, the default is claimed (here
    /// a stand-in, so the user's `mimeapps.list` is never touched), a rerun leaves the entry
    /// alone, and a different binary rewrites it.
    #[test]
    fn with_the_opt_in_the_entry_is_written_and_the_default_claimed() {
        let home = Home::new("opt-in");
        let claims = Cell::new(0);
        let claim = || claims.set(claims.get() + 1);
        let file = register_dev_handler(&home.0, &home.0, Path::new("/a/chairphoto"), true, claim).unwrap().unwrap();
        assert_eq!(file, home.0.join("applications").join(HANDLER_FILE));
        assert_eq!(Some(std::fs::read_to_string(&file).unwrap()), handler_entry(Path::new("/a/chairphoto")));
        assert_eq!(claims.get(), 1);
        let modified = std::fs::metadata(&file).unwrap().modified().unwrap();
        register_dev_handler(&home.0, &home.0, Path::new("/a/chairphoto"), true, claim).unwrap();
        assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), modified, "unchanged entry is not rewritten");
        register_dev_handler(&home.0, &home.0, Path::new("/b/chairphoto"), true, claim).unwrap();
        assert!(std::fs::read_to_string(&file).unwrap().contains("Exec=/b/chairphoto %u"));
    }

    // --- stale pre-#167 handler + mimeapps default cleanup (L6) --------------------------

    /// The literal pre-#167 handler entry text, copied from `handler_entry` as it read at
    /// `aa9438f^:crates/app/src/desktop.rs` (before the `chairphoto-gpui` → `chairphoto`
    /// rename changed the `Name=` line this module writes). Built from the literal text, not
    /// by calling the *current* `handler_entry()`, so this fixture still matches what a real
    /// pre-rename dev build actually wrote even if `handler_entry`'s shape changes again later.
    const STALE_PRE_RENAME_ENTRY: &str = "[Desktop Entry]\n\
         Type=Application\n\
         Name=ChairPhoto (GPUI dev build)\n\
         Exec=/old/chairphoto-gpui %u\n\
         Terminal=false\n\
         NoDisplay=true\n\
         MimeType=x-scheme-handler/chairphoto;\n\
         StartupWMClass=chairphoto\n";

    /// A ChairPhoto-shaped stale entry, and a `mimeapps.list` default pointing at it, are
    /// both removed when the current handler registers — and the new entry still gets
    /// written and claimed, same as any other registration.
    #[test]
    fn a_stale_own_entry_and_its_default_are_removed_on_registration() {
        let home = Home::new("stale-own");
        let apps = home.0.join("applications");
        std::fs::create_dir_all(&apps).unwrap();
        let stale = apps.join(STALE_HANDLER_FILE);
        std::fs::write(&stale, STALE_PRE_RENAME_ENTRY).unwrap();
        let config = home.0.join("config");
        std::fs::create_dir_all(&config).unwrap();
        let mimeapps = config.join("mimeapps.list");
        std::fs::write(
            &mimeapps,
            "[Added Associations]\ntext/plain=kate.desktop;\n\n\
             [Default Applications]\nx-scheme-handler/chairphoto=chairphoto-gpui-handler.desktop\n\
             text/html=firefox.desktop\n",
        )
        .unwrap();

        let claims = Cell::new(0);
        let file = register_dev_handler(&home.0, &config, Path::new("/a/chairphoto"), true, || {
            claims.set(claims.get() + 1)
        })
        .unwrap()
        .unwrap();

        assert!(!stale.exists(), "the stale pre-rename entry should be removed");
        assert!(file.exists(), "the current entry is still written");
        assert_eq!(claims.get(), 1, "the current default is still claimed");
        let updated = std::fs::read_to_string(&mimeapps).unwrap();
        assert!(!updated.contains(STALE_HANDLER_FILE), "the stale default line is removed:\n{updated}");
        assert!(updated.contains("text/plain=kate.desktop;"), "other associations survive:\n{updated}");
        assert!(updated.contains("text/html=firefox.desktop"), "other defaults survive:\n{updated}");
    }

    /// A same-named file that is not ChairPhoto's own entry (no recognisable content) is
    /// left alone — the cleanup never deletes a file merely because of its name.
    #[test]
    fn a_same_named_entry_that_is_not_our_own_is_left_alone() {
        let home = Home::new("stale-foreign");
        let apps = home.0.join("applications");
        std::fs::create_dir_all(&apps).unwrap();
        let stale = apps.join(STALE_HANDLER_FILE);
        std::fs::write(&stale, "[Desktop Entry]\nName=Someone Else's App\nExec=other %u\n").unwrap();

        register_dev_handler(&home.0, &home.0, Path::new("/a/chairphoto"), true, || {}).unwrap();

        assert!(stale.exists(), "a foreign entry sharing the stale name must survive");
    }

    /// A `mimeapps.list` default for the scheme that points somewhere else entirely (not the
    /// stale handler) is left alone — the cleanup only ever removes its own stale line.
    #[test]
    fn a_default_pointing_elsewhere_is_left_alone() {
        let home = Home::new("stale-other-default");
        let config = home.0.join("config");
        std::fs::create_dir_all(&config).unwrap();
        let mimeapps = config.join("mimeapps.list");
        let original = "[Default Applications]\nx-scheme-handler/chairphoto=some-other-app.desktop\n";
        std::fs::write(&mimeapps, original).unwrap();

        register_dev_handler(&home.0, &config, Path::new("/a/chairphoto"), true, || {}).unwrap();

        assert_eq!(std::fs::read_to_string(&mimeapps).unwrap(), original, "an unrelated default is untouched");
    }

    /// Without the opt-in, nothing is cleaned up either — the whole function is a no-op,
    /// matching [`without_the_opt_in_nothing_is_registered`].
    #[test]
    fn without_the_opt_in_stale_cleanup_does_not_run() {
        let home = Home::new("stale-no-opt-in");
        let apps = home.0.join("applications");
        std::fs::create_dir_all(&apps).unwrap();
        let stale = apps.join(STALE_HANDLER_FILE);
        std::fs::write(&stale, handler_entry(Path::new("/old/chairphoto-gpui")).unwrap()).unwrap();

        register_dev_handler(&home.0, &home.0, Path::new("/a/chairphoto"), false, || panic!("claimed")).unwrap();

        assert!(stale.exists(), "nothing is cleaned up without the opt-in");
    }

    /// A default line for the stale scheme that sits outside `[Default Applications]` (e.g.
    /// under `[Added Associations]`, which uses the same `key=value.desktop;` shape) must
    /// survive: the cleanup only ever touches the default, not an association list. Mutation
    /// check: dropping `remove_stale_default`'s `in_default &&` guard makes this fail (the
    /// line would be stripped regardless of section).
    #[test]
    fn a_matching_line_outside_default_applications_is_left_alone() {
        let home = Home::new("stale-outside-section");
        let apps = home.0.join("applications");
        std::fs::create_dir_all(&apps).unwrap();
        std::fs::write(&apps.join(STALE_HANDLER_FILE), STALE_PRE_RENAME_ENTRY).unwrap();
        let config = home.0.join("config");
        std::fs::create_dir_all(&config).unwrap();
        let mimeapps = config.join("mimeapps.list");
        let original = "[Added Associations]\nx-scheme-handler/chairphoto=chairphoto-gpui-handler.desktop\n\
             \n[Default Applications]\ntext/html=firefox.desktop\n";
        std::fs::write(&mimeapps, original).unwrap();

        register_dev_handler(&home.0, &config, Path::new("/a/chairphoto"), true, || {}).unwrap();

        let updated = std::fs::read_to_string(&mimeapps).unwrap();
        assert_eq!(updated, original, "a same-shaped line outside [Default Applications] must survive:\n{updated}");
    }

    /// CRLF line endings in the original `mimeapps.list` survive the rewrite — the file is
    /// reassembled from each line's own terminator, not rewritten with a blanket `\n`.
    #[test]
    fn a_crlf_mimeapps_list_keeps_its_line_endings() {
        let home = Home::new("stale-crlf");
        let config = home.0.join("config");
        std::fs::create_dir_all(&config).unwrap();
        let mimeapps = config.join("mimeapps.list");
        let original = "[Added Associations]\r\ntext/plain=kate.desktop;\r\n\r\n\
             [Default Applications]\r\nx-scheme-handler/chairphoto=chairphoto-gpui-handler.desktop\r\n\
             text/html=firefox.desktop\r\n";
        std::fs::write(&mimeapps, original).unwrap();

        register_dev_handler(&home.0, &config, Path::new("/a/chairphoto"), true, || {}).unwrap();

        let updated = std::fs::read_to_string(&mimeapps).unwrap();
        assert_eq!(
            updated,
            "[Added Associations]\r\ntext/plain=kate.desktop;\r\n\r\n[Default Applications]\r\ntext/html=firefox.desktop\r\n",
            "the stale line is removed and every surviving line keeps its \\r\\n:\n{updated:?}"
        );
    }

    /// A `mimeapps.list` with no trailing newline keeps it that way after the stale line (not
    /// the last line) is removed.
    #[test]
    fn a_mimeapps_list_without_a_final_newline_keeps_it_that_way() {
        let home = Home::new("stale-no-final-newline");
        let config = home.0.join("config");
        std::fs::create_dir_all(&config).unwrap();
        let mimeapps = config.join("mimeapps.list");
        // No trailing '\n' after the last line.
        let original = "[Default Applications]\nx-scheme-handler/chairphoto=chairphoto-gpui-handler.desktop\ntext/html=firefox.desktop";
        std::fs::write(&mimeapps, original).unwrap();

        register_dev_handler(&home.0, &config, Path::new("/a/chairphoto"), true, || {}).unwrap();

        let updated = std::fs::read_to_string(&mimeapps).unwrap();
        assert_eq!(updated, "[Default Applications]\ntext/html=firefox.desktop", "no newline is added:\n{updated:?}");
    }

    /// A same-named `.desktop` file that is foreign (not ChairPhoto's own) protects its
    /// `mimeapps.list` default line too: the default genuinely points at a real, foreign
    /// handler, so removing the line would be wrong, not just removing the file. Mutation
    /// check: always treating the stale file as "own or absent" for this gate (i.e. removing
    /// the `!stale_is_own_or_absent { return; }` early return in `clean_stale_handler`) makes
    /// this fail.
    #[test]
    fn a_foreign_entrys_mimeapps_default_line_is_also_left_alone() {
        let home = Home::new("stale-foreign-default");
        let apps = home.0.join("applications");
        std::fs::create_dir_all(&apps).unwrap();
        let stale = apps.join(STALE_HANDLER_FILE);
        std::fs::write(&stale, "[Desktop Entry]\nName=Someone Else's App\nExec=other %u\n").unwrap();
        let config = home.0.join("config");
        std::fs::create_dir_all(&config).unwrap();
        let mimeapps = config.join("mimeapps.list");
        let original = "[Default Applications]\nx-scheme-handler/chairphoto=chairphoto-gpui-handler.desktop\n";
        std::fs::write(&mimeapps, original).unwrap();

        register_dev_handler(&home.0, &config, Path::new("/a/chairphoto"), true, || {}).unwrap();

        assert!(stale.exists(), "the foreign file must survive");
        assert_eq!(
            std::fs::read_to_string(&mimeapps).unwrap(),
            original,
            "the default line naming a real, foreign handler must survive"
        );
    }

    /// A `mimeapps.list` that doesn't exist at all is simply left missing — matching the
    /// pre-existing `read_to_string` short-circuit — and the "absent" branch of the ownership
    /// gate is exercised without a stale `.desktop` file either.
    #[test]
    fn a_missing_mimeapps_list_is_not_created() {
        let home = Home::new("stale-missing-mimeapps");
        let config = home.0.join("config");
        std::fs::create_dir_all(&config).unwrap();

        register_dev_handler(&home.0, &config, Path::new("/a/chairphoto"), true, || {}).unwrap();

        assert!(!config.join("mimeapps.list").exists(), "a missing mimeapps.list is not created");
    }

    /// A symlinked `mimeapps.list` stays a symlink: the rewrite edits the target file, not
    /// the link, matching `xdg-mime`'s own "edit the target" behaviour.
    #[test]
    fn a_symlinked_mimeapps_list_keeps_the_symlink_and_edits_its_target() {
        let home = Home::new("stale-symlink");
        let config = home.0.join("config");
        std::fs::create_dir_all(&config).unwrap();
        let real = home.0.join("real-mimeapps.list");
        let original = "[Default Applications]\nx-scheme-handler/chairphoto=chairphoto-gpui-handler.desktop\ntext/html=firefox.desktop\n";
        std::fs::write(&real, original).unwrap();
        let link = config.join("mimeapps.list");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        register_dev_handler(&home.0, &config, Path::new("/a/chairphoto"), true, || {}).unwrap();

        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "mimeapps.list must stay a symlink");
        assert_eq!(std::fs::read_link(&link).unwrap(), real, "the symlink must still point at the same target");
        let updated = std::fs::read_to_string(&real).unwrap();
        assert!(!updated.contains("chairphoto-gpui-handler.desktop"), "the real target is the one that gets edited:\n{updated}");
        assert!(updated.contains("text/html=firefox.desktop"), "other defaults in the target survive:\n{updated}");
    }
}
