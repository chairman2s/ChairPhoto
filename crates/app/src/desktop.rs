//! The `chairphoto://` scheme handler for a development build of the GPUI app.
//!
//! An installed build registers the scheme through its `.desktop` file
//! (`packaging/chairphoto-gpui.desktop`, `MimeType=x-scheme-handler/chairphoto`). A dev build
//! is not installed, so — as the Tauri shell does in `src-tauri/src/lib.rs` with
//! `deep_link().register_all()` — it writes a handler entry pointing at its own binary on
//! startup: `$XDG_DATA_HOME/applications/chairphoto-gpui-handler.desktop`.
//!
//! **Coexisting with the Tauri app until the cutover.** The Tauri dev build writes
//! `chairphoto-handler.desktop` and makes it the scheme's default (`xdg-mime default`) on
//! every start. This handler has its own file name, so neither overwrites the other, and it
//! does **not** make itself the default unless `CHAIRPHOTO_GPUI_CLAIM_SCHEME=1` is set: the
//! user's (or the Tauri build's) choice stands, and links keep opening the Tauri app until
//! someone opts the GPUI build in. Without the opt-in the entry still lists the GPUI build as
//! a handler, so `gio open` / "Open with" can pick it.
//!
//! **Exec quoting.** xdg-open (what Electron apps such as Obsidian open links with) takes the
//! first word of `Exec` verbatim and looks it up with `command -v`, so a quoted path "does not
//! exist" and it silently falls back to the browser; gio parses quoting fine. The Tauri shell
//! strips the quotes `register_all` writes after the fact. Here the line is written unquoted
//! from the start whenever the path has no character the Desktop Entry spec reserves, and
//! quoted per the spec otherwise (gio then still works; xdg-open cannot).

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The handler entry's file name under `applications/`.
pub const HANDLER_FILE: &str = "chairphoto-gpui-handler.desktop";
/// The MIME type of the scheme.
pub const SCHEME_MIME: &str = "x-scheme-handler/chairphoto";
/// Set to `1` to make the dev build the scheme's default handler.
pub const CLAIM_ENV: &str = "CHAIRPHOTO_GPUI_CLAIM_SCHEME";

/// The `Exec` value for `exe` with the URL placeholder: `<exe> %u`.
pub fn exec_value(exe: &Path) -> String {
    let path = exe.to_string_lossy();
    // The spec's reserved characters, plus `%` (field codes) and `=`/`'` for good measure:
    // anything outside this safe set gets quoted.
    let safe = |c: char| c.is_ascii_alphanumeric() || "/._-+,@:".contains(c);
    if !path.is_empty() && path.chars().all(safe) {
        return format!("{path} %u");
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
    format!("{} %u", quoted.replace('\\', "\\\\"))
}

/// The handler entry for `exe`.
pub fn handler_entry(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=ChairPhoto (GPUI dev build)\n\
         Exec={}\n\
         Terminal=false\n\
         NoDisplay=true\n\
         MimeType={SCHEME_MIME};\n\
         StartupWMClass={}\n",
        exec_value(exe),
        crate::APP_ID,
    )
}

/// `$XDG_DATA_HOME`, else `~/.local/share` — where the Tauri shell's `data_dir()` writes too.
pub fn data_home() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
}

/// Write the handler entry for `exe` under `data_home/applications` (only when it changed),
/// refresh that directory's MIME cache, and — only with `claim_default` — make it the
/// scheme's default handler. Returns the entry's path.
///
/// `update-desktop-database` and `xdg-mime` are optional: a missing tool is logged and skipped.
pub fn register_dev_handler(data_home: &Path, exe: &Path, claim_default: bool) -> io::Result<PathBuf> {
    let dir = data_home.join("applications");
    std::fs::create_dir_all(&dir)?;
    let file = dir.join(HANDLER_FILE);
    let entry = handler_entry(exe);
    if std::fs::read_to_string(&file).ok().as_deref() != Some(entry.as_str()) {
        std::fs::write(&file, &entry)?;
        run_tool(Command::new("update-desktop-database").arg(&dir));
    }
    if claim_default {
        run_tool(Command::new("xdg-mime").args(["default", HANDLER_FILE, SCHEME_MIME]));
    }
    Ok(file)
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

    #[test]
    fn a_plain_path_is_written_unquoted_for_xdg_open() {
        assert_eq!(
            exec_value(Path::new("/home/me/src/chairphoto/target/debug/chairphoto-gpui")),
            "/home/me/src/chairphoto/target/debug/chairphoto-gpui %u"
        );
    }

    #[test]
    fn a_path_with_reserved_characters_is_quoted_per_the_spec() {
        assert_eq!(exec_value(Path::new("/home/me/My Photos/cp")), "\"/home/me/My Photos/cp\" %u");
        // `$` is escaped inside the quotes (`\$`), then the backslash for the desktop file.
        assert_eq!(exec_value(Path::new("/opt/a$b")), "\"/opt/a\\\\$b\" %u");
        assert_eq!(exec_value(Path::new("/opt/100%")), "\"/opt/100%%\" %u");
    }

    #[test]
    fn the_entry_registers_the_scheme() {
        let entry = handler_entry(Path::new("/x/chairphoto-gpui"));
        assert!(entry.contains("\nExec=/x/chairphoto-gpui %u\n"), "{entry}");
        assert!(entry.contains("\nMimeType=x-scheme-handler/chairphoto;\n"), "{entry}");
        assert!(entry.contains("\nNoDisplay=true\n"), "{entry}");
    }

    /// The installed entries (Tauri's today, the GPUI app's for the cutover) declare the
    /// scheme and pass the URL with `%u`.
    #[test]
    fn the_packaged_entries_declare_the_scheme() {
        for (name, entry, exec) in [
            ("chairphoto-gpui.desktop", include_str!("../../../packaging/chairphoto-gpui.desktop"), "Exec=chairphoto-gpui %u"),
            ("chairphoto.desktop", include_str!("../../../packaging/chairphoto.desktop"), "Exec=chairphoto %u"),
        ] {
            let lines: Vec<&str> = entry.lines().collect();
            assert!(lines.contains(&exec), "{name}: {exec}");
            assert!(lines.contains(&"MimeType=x-scheme-handler/chairphoto;"), "{name}: MimeType");
            assert!(lines.contains(&"StartupWMClass=chairphoto"), "{name}: StartupWMClass");
        }
    }

    /// Registration into a scratch data home: the entry lands under `applications/`, a rerun
    /// leaves it alone, and a different binary rewrites it. Never claims the default here, so
    /// the user's `mimeapps.list` is not touched.
    #[test]
    fn registration_writes_the_entry_into_the_data_home() {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let home = std::env::temp_dir().join(format!("cp-desktop-{}-{nanos}", std::process::id()));
        let file = register_dev_handler(&home, Path::new("/a/chairphoto-gpui"), false).unwrap();
        assert_eq!(file, home.join("applications").join(HANDLER_FILE));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), handler_entry(Path::new("/a/chairphoto-gpui")));
        let modified = std::fs::metadata(&file).unwrap().modified().unwrap();
        register_dev_handler(&home, Path::new("/a/chairphoto-gpui"), false).unwrap();
        assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), modified, "unchanged entry is not rewritten");
        register_dev_handler(&home, Path::new("/b/chairphoto-gpui"), false).unwrap();
        assert!(std::fs::read_to_string(&file).unwrap().contains("Exec=/b/chairphoto-gpui %u"));
        let _ = std::fs::remove_dir_all(&home);
    }
}
