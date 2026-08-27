//! "Follow Omarchy" appearance: read the Omarchy 4 runtime theme and watch it for switches.
//!
//! ChairPhoto has two appearance modes (docs/appearance.md): "ChairPhoto Standard", an
//! app-owned palette that never touches this module, and "Follow Omarchy", which fills the
//! same semantic tokens from the palette Omarchy keeps under its XDG state directory. This
//! module is the whole Rust side of the latter — the palette parser, the current-theme
//! reader, and a polling watcher that broadcasts [`THEME_CHANGED_EVENT`] when the user
//! switches themes. The thin command surface is `commands::appearance`.
//!
//! Absent Omarchy is a normal, non-degraded state: [`read_current_theme`] answers
//! `available: false` (never `Err`) and [`start_watcher`] starts nothing, so a machine
//! without Omarchy pays zero polling. Broken state — malformed TOML, an invalid color, a
//! half-swapped theme that never settles — gets the same answer, because the frontend's
//! fallback (ChairPhoto Standard) must engage immediately rather than keep a stale palette.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};
use tauri::Emitter;

/// Broadcast whenever the settled Omarchy state changes. Payload: a [`SystemThemeResult`] —
/// the switched-to theme, or `available: false` when the theme vanished or broke and the
/// frontend must fall back to ChairPhoto Standard.
pub const THEME_CHANGED_EVENT: &str = "appearance:theme_changed";

/// Whether the theme wants light-on-dark or dark-on-light chrome. `mode` in `colors.toml`,
/// spelled `"light"` / `"dark"` both in the TOML and in the outbound JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OmarchyMode {
    Light,
    Dark,
}

/// One Omarchy theme's palette, as its `colors.toml` declares it.
///
/// The TOML keys are snake_case; the rename is serialize-only so the same struct crosses
/// IPC as camelCase JSON. Unknown keys are ignored, deliberately: a theme carrying
/// extension colors (`orange`, `brown`, app-specific sections) must still parse. Every
/// color that *is* present must be `#rgb`/`#rrggbb`/`#rrggbbaa` (case-insensitive) —
/// [`parse_palette`] rejects the whole palette otherwise.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all(serialize = "camelCase"))]
pub struct OmarchyPalette {
    pub mode: OmarchyMode,
    // Required: a theme without these cannot fill ChairPhoto's semantic tokens.
    pub accent: String,
    pub selection: String,
    pub muted: String,
    pub background: String,
    pub foreground: String,
    // Optional background/foreground variants.
    pub dark_background: Option<String>,
    pub darker_background: Option<String>,
    pub lighter_background: Option<String>,
    pub dark_foreground: Option<String>,
    pub light_foreground: Option<String>,
    pub bright_foreground: Option<String>,
    // Optional terminal colors.
    pub red: Option<String>,
    pub yellow: Option<String>,
    pub green: Option<String>,
    pub cyan: Option<String>,
    pub blue: Option<String>,
    pub magenta: Option<String>,
    pub bright_red: Option<String>,
    pub bright_yellow: Option<String>,
    pub bright_green: Option<String>,
    pub bright_cyan: Option<String>,
    pub bright_blue: Option<String>,
    pub bright_magenta: Option<String>,
}

impl OmarchyPalette {
    /// Every color on this palette with its TOML key — the five required fields, then
    /// whichever optional ones are present.
    fn colors(&self) -> Vec<(&'static str, &str)> {
        let mut out: Vec<(&'static str, &str)> = vec![
            ("accent", self.accent.as_str()),
            ("selection", self.selection.as_str()),
            ("muted", self.muted.as_str()),
            ("background", self.background.as_str()),
            ("foreground", self.foreground.as_str()),
        ];
        let optional: [(&'static str, &Option<String>); 18] = [
            ("dark_background", &self.dark_background),
            ("darker_background", &self.darker_background),
            ("lighter_background", &self.lighter_background),
            ("dark_foreground", &self.dark_foreground),
            ("light_foreground", &self.light_foreground),
            ("bright_foreground", &self.bright_foreground),
            ("red", &self.red),
            ("yellow", &self.yellow),
            ("green", &self.green),
            ("cyan", &self.cyan),
            ("blue", &self.blue),
            ("magenta", &self.magenta),
            ("bright_red", &self.bright_red),
            ("bright_yellow", &self.bright_yellow),
            ("bright_green", &self.bright_green),
            ("bright_cyan", &self.bright_cyan),
            ("bright_blue", &self.bright_blue),
            ("bright_magenta", &self.bright_magenta),
        ];
        for (key, value) in optional {
            if let Some(v) = value {
                out.push((key, v.as_str()));
            }
        }
        out
    }
}

/// What `get_system_theme` returns and [`THEME_CHANGED_EVENT`] carries. Failure of any
/// kind — no Omarchy on this machine, missing files, malformed TOML, an invalid color —
/// is `available: false` with both fields `None`, never an `Err`: a broken or absent theme
/// is a normal answer, and the frontend's reaction (fall back to ChairPhoto Standard) is
/// the same for every cause.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemThemeResult {
    pub available: bool,
    pub theme_name: Option<String>,
    pub palette: Option<OmarchyPalette>,
}

impl SystemThemeResult {
    /// The uniform "no usable Omarchy theme" answer.
    pub fn unavailable() -> Self {
        SystemThemeResult {
            available: false,
            theme_name: None,
            palette: None,
        }
    }
}

/// `#rgb`, `#rrggbb`, or `#rrggbbaa`, case-insensitive.
fn is_hex_color(value: &str) -> bool {
    let Some(digits) = value.strip_prefix('#') else {
        return false;
    };
    matches!(digits.len(), 3 | 6 | 8) && digits.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Parse and validate one theme's `colors.toml`. Unknown keys pass through unrejected;
/// any *present* color that is not a valid hex value fails the whole palette — a theme
/// that half-parses would paint a half-broken UI.
pub fn parse_palette(text: &str) -> Result<OmarchyPalette, String> {
    let palette: OmarchyPalette = toml::from_str(text).map_err(|e| e.to_string())?;
    for (key, value) in palette.colors() {
        if !is_hex_color(value) {
            return Err(format!("{key}: invalid color {value:?}"));
        }
    }
    Ok(palette)
}

// ── The Omarchy state directory ──────────────────────────────────────────────

/// The two files under the state root that define the current theme. Omarchy 4 switches
/// themes by removing `current/theme`, atomically replacing it, and then rewriting
/// `theme.name` — so either file can be momentarily absent mid-swap, which is a normal
/// transitional state, not an error.
const COLORS_FILE: &str = "current/theme/colors.toml";
const NAME_FILE: &str = "current/theme.name";

/// The Omarchy state root: `$XDG_STATE_HOME/omarchy`, else `~/.local/state/omarchy`.
/// Resolved through `std::env::var_os` on every call (not a cached home API) so tests can
/// point it at a fixture. `None` — neither variable set — simply means "not available".
fn omarchy_state_root() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
        })?;
    Some(base.join("omarchy"))
}

/// Read the current Omarchy theme from the default state root. Every failure — including
/// "there is no Omarchy here" — is the `available: false` answer, per the module contract.
pub fn read_current_theme() -> SystemThemeResult {
    match omarchy_state_root() {
        Some(root) => read_theme_at(&root),
        None => SystemThemeResult::unavailable(),
    }
}

/// Read the current theme under an explicit state root. A missing or unparsable
/// `colors.toml` makes the whole result unavailable; a missing `theme.name` costs only
/// the name (`theme_name: None`, palette intact).
pub fn read_theme_at(root: &Path) -> SystemThemeResult {
    let Ok(text) = std::fs::read_to_string(root.join(COLORS_FILE)) else {
        return SystemThemeResult::unavailable();
    };
    let Ok(palette) = parse_palette(&text) else {
        return SystemThemeResult::unavailable();
    };
    SystemThemeResult {
        available: true,
        theme_name: read_theme_name(root),
        palette: Some(palette),
    }
}

fn read_theme_name(root: &Path) -> Option<String> {
    std::fs::read_to_string(root.join(NAME_FILE))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

// ── The theme watcher ────────────────────────────────────────────────────────
//
// A singleton, process-lifetime daemon thread (like the video server), not a JobRegistry
// job and not on the catalog-scoped AppState: the Omarchy theme is per-machine state,
// unrelated to which catalog is open, and it never needs aborting or a status slot.

/// How often the watcher stats the two theme files. Cheap (two `stat` calls against local
/// disk), so a switch is noticed within two seconds without measurable idle cost.
const WATCH_INTERVAL: Duration = Duration::from_secs(2);

/// The settle-read cadence and budget: re-read both files every 100 ms until two
/// consecutive reads agree byte-for-byte *and* parse cleanly, giving Omarchy's
/// remove-then-replace swap up to ~2 s to finish (same shape as
/// `rapidraw::wait_for_stable_output`, which settles on size instead of bytes).
const SETTLE_INTERVAL: Duration = Duration::from_millis(100);
const SETTLE_MAX_TRIES: u32 = 20;

/// What one poll tick can observe cheaply: (mtime, len) of each theme file, `None` for a
/// file that is currently absent — normal mid-swap, so absence is a fingerprint value,
/// not an error.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Fingerprint {
    colors: Option<(SystemTime, u64)>,
    name: Option<(SystemTime, u64)>,
}

fn stat_entry(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

fn fingerprint(root: &Path) -> Fingerprint {
    Fingerprint {
        colors: stat_entry(&root.join(COLORS_FILE)),
        name: stat_entry(&root.join(NAME_FILE)),
    }
}

/// Raw bytes of (colors.toml, theme.name); `None` for an absent file.
type Snapshot = (Option<Vec<u8>>, Option<Vec<u8>>);

fn read_snapshot(root: &Path) -> Snapshot {
    (
        std::fs::read(root.join(COLORS_FILE)).ok(),
        std::fs::read(root.join(NAME_FILE)).ok(),
    )
}

/// Interpret a settled snapshot; `None` when it cannot yield an available theme
/// (colors.toml absent, non-UTF-8, malformed, or carrying an invalid color).
fn snapshot_result(snap: &Snapshot) -> Option<SystemThemeResult> {
    let text = std::str::from_utf8(snap.0.as_deref()?).ok()?;
    let palette = parse_palette(text).ok()?;
    let theme_name = snap
        .1
        .as_deref()
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Some(SystemThemeResult {
        available: true,
        theme_name,
        palette: Some(palette),
    })
}

/// Ride out a theme swap: re-read both files until two consecutive reads (100 ms apart)
/// agree byte-for-byte and parse cleanly, then return that theme plus the fingerprint
/// captured alongside the confirming read. A state that never settles on a valid theme
/// within the budget — vanished files, a broken palette — returns `unavailable()`, so the
/// frontend falls back instead of keeping a stale palette; the next fingerprint change
/// re-evaluates from scratch.
fn settle_read(root: &Path) -> (SystemThemeResult, Fingerprint) {
    let mut prev: Option<Snapshot> = None;
    for _ in 0..SETTLE_MAX_TRIES {
        let fp = fingerprint(root);
        let snap = read_snapshot(root);
        if prev.as_ref() == Some(&snap) {
            if let Some(result) = snapshot_result(&snap) {
                return (result, fp);
            }
            // Stable but not (yet) a valid theme — mid-swap can pause with the theme
            // directory missing entirely. Keep watching until the budget runs out.
        }
        prev = Some(snap);
        std::thread::sleep(SETTLE_INTERVAL);
    }
    (SystemThemeResult::unavailable(), fingerprint(root))
}

/// The watcher's memory between ticks: the last stat fingerprint, and the last outcome
/// the frontend knows (emitted here, or fetched itself via `get_system_theme` at startup).
struct WatchState {
    fingerprint: Fingerprint,
    last: SystemThemeResult,
}

impl WatchState {
    fn capture(root: &Path) -> Self {
        WatchState {
            fingerprint: fingerprint(root),
            last: read_theme_at(root),
        }
    }
}

/// One poll tick, factored out of the thread loop so the whole transition is testable
/// without a Tauri app. Compares the cheap fingerprint against the last one; on change,
/// settle-reads the new state and returns the outcome to broadcast — `Some` only when the
/// settled outcome differs from the last known one, so a 2-second tick never spams events.
fn poll_tick(root: &Path, state: &mut WatchState) -> Option<SystemThemeResult> {
    let fp = fingerprint(root);
    if fp == state.fingerprint {
        return None;
    }
    let (result, settled_fp) = settle_read(root);
    state.fingerprint = settled_fp;
    if result == state.last {
        return None;
    }
    state.last = result.clone();
    Some(result)
}

/// One watcher for the app's lifetime — a second `start_watcher` is a no-op. Module-local
/// (mirroring rapidraw's cancel registry) so the feature stays self-contained.
static WATCHER_STARTED: AtomicBool = AtomicBool::new(false);

/// Start the theme watcher: a detached, process-lifetime thread that polls the Omarchy
/// state root every [`WATCH_INTERVAL`] and emits [`THEME_CHANGED_EVENT`] when the settled
/// outcome changes. Returns whether it started. It does not start when Omarchy's state
/// root is absent — the supported no-Omarchy state, which must cost zero polling — or when
/// a watcher is already running.
pub fn start_watcher(app: tauri::AppHandle) -> bool {
    let Some(root) = omarchy_state_root() else {
        return false;
    };
    if !root.exists() {
        return false;
    }
    if WATCHER_STARTED.swap(true, Ordering::SeqCst) {
        return false;
    }
    std::thread::spawn(move || {
        let mut state = WatchState::capture(&root);
        loop {
            std::thread::sleep(WATCH_INTERVAL);
            if let Some(result) = poll_tick(&root, &mut state) {
                let _ = app.emit(THEME_CHANGED_EVENT, &result);
            }
        }
    });
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::test_env_helpers::EnvGuard;
    use crate::test_support::TestTmpDir;

    /// A complete dark palette: every required and optional field, mixed-case hex.
    const FULL_DARK: &str = r##"
mode = "dark"
accent = "#7AA2F7"
selection = "#283457"
muted = "#565f89"
background = "#1a1b26"
foreground = "#c0caf5"
dark_background = "#16161e"
darker_background = "#0f0f14"
lighter_background = "#24283b"
dark_foreground = "#a9b1d6"
light_foreground = "#e0e6ff"
bright_foreground = "#ffffff"
red = "#f7768e"
yellow = "#e0af68"
green = "#9ece6a"
cyan = "#7dcfff"
blue = "#7aa2f7"
magenta = "#bb9af7"
bright_red = "#ff899d"
bright_yellow = "#faba4a"
bright_green = "#9fe044"
bright_cyan = "#a4daff"
bright_blue = "#8db0ff"
bright_magenta = "#c7a9ff"
"##;

    /// Only the required fields, light mode, one 3-digit color.
    const MINIMAL_LIGHT: &str = r##"
mode = "light"
accent = "#26a"
selection = "#c8d3f5"
muted = "#9099b2"
background = "#ffffff"
foreground = "#1b1d2b"
"##;

    fn write_theme(root: &Path, name: Option<&str>, colors: &str) {
        let theme = root.join("current/theme");
        std::fs::create_dir_all(&theme).unwrap();
        std::fs::write(theme.join("colors.toml"), colors).unwrap();
        if let Some(name) = name {
            std::fs::write(root.join("current/theme.name"), format!("{name}\n")).unwrap();
        }
    }

    /// Remove the swapped pieces the way Omarchy does mid-switch: the whole theme
    /// directory, then the name file.
    fn remove_theme(root: &Path) {
        let _ = std::fs::remove_dir_all(root.join("current/theme"));
        let _ = std::fs::remove_file(root.join("current/theme.name"));
    }

    // ── Parsing ──────────────────────────────────────────────────────────────

    #[test]
    fn parses_a_full_dark_palette() {
        let p = parse_palette(FULL_DARK).unwrap();
        assert_eq!(p.mode, OmarchyMode::Dark);
        assert_eq!(p.accent, "#7AA2F7");
        assert_eq!(p.darker_background.as_deref(), Some("#0f0f14"));
        assert_eq!(p.bright_magenta.as_deref(), Some("#c7a9ff"));
    }

    #[test]
    fn parses_a_light_palette_with_optionals_omitted() {
        let p = parse_palette(MINIMAL_LIGHT).unwrap();
        assert_eq!(p.mode, OmarchyMode::Light);
        assert_eq!(p.background, "#ffffff");
        assert!(p.dark_background.is_none());
        assert!(p.red.is_none());
        assert!(p.bright_red.is_none());
    }

    #[test]
    fn ignores_unknown_extension_fields() {
        let text = format!(
            "{MINIMAL_LIGHT}orange = \"#ff9e64\"\nbrown = \"#8f5536\"\n\n[some_app]\nx = 3\n"
        );
        let p = parse_palette(&text).expect("extension keys must not reject a theme");
        assert_eq!(p.mode, OmarchyMode::Light);
    }

    #[test]
    fn accepts_all_three_hex_forms() {
        for color in ["#abc", "#AABBCC", "#aabbccdd", "#AaBbCcDd"] {
            let text = MINIMAL_LIGHT.replace("\"#26a\"", &format!("\"{color}\""));
            assert!(parse_palette(&text).is_ok(), "{color} must parse");
        }
    }

    #[test]
    fn rejects_invalid_colors() {
        for color in ["7aa2f7", "#7aa2g7", "#7aa2f", "#", "red", "#aabbccddee"] {
            let text = MINIMAL_LIGHT.replace("\"#26a\"", &format!("\"{color}\""));
            assert!(parse_palette(&text).is_err(), "{color} must be rejected");
        }
    }

    #[test]
    fn rejects_an_invalid_optional_color() {
        let text = format!("{MINIMAL_LIGHT}red = \"ff0000\"\n");
        assert!(parse_palette(&text).is_err());
    }

    // ── Reading the current theme ────────────────────────────────────────────

    #[test]
    fn reads_a_complete_theme() {
        let dir = TestTmpDir::new("appearance-read");
        let _env = EnvGuard::set("XDG_STATE_HOME", &dir.to_string_lossy());
        write_theme(&dir.join("omarchy"), Some("tokyo-night"), FULL_DARK);

        let result = read_current_theme();
        assert!(result.available);
        assert_eq!(result.theme_name.as_deref(), Some("tokyo-night"));
        assert_eq!(result.palette.unwrap().mode, OmarchyMode::Dark);
    }

    #[test]
    fn absent_root_is_unavailable() {
        let dir = TestTmpDir::new("appearance-absent");
        let _env = EnvGuard::set("XDG_STATE_HOME", &dir.to_string_lossy());
        // No omarchy/ under the state dir at all.
        assert_eq!(read_current_theme(), SystemThemeResult::unavailable());
    }

    #[test]
    fn missing_colors_toml_is_unavailable() {
        let dir = TestTmpDir::new("appearance-nocolors");
        let root = dir.join("omarchy");
        std::fs::create_dir_all(root.join("current/theme")).unwrap();
        std::fs::write(root.join("current/theme.name"), "tokyo-night\n").unwrap();
        assert_eq!(read_theme_at(&root), SystemThemeResult::unavailable());
    }

    #[test]
    fn missing_theme_name_keeps_the_palette() {
        let dir = TestTmpDir::new("appearance-noname");
        let root = dir.join("omarchy");
        write_theme(&root, None, FULL_DARK);
        let result = read_theme_at(&root);
        assert!(result.available);
        assert_eq!(result.theme_name, None);
        assert!(result.palette.is_some());
    }

    #[test]
    fn malformed_toml_is_unavailable() {
        let dir = TestTmpDir::new("appearance-badtoml");
        let root = dir.join("omarchy");
        write_theme(&root, Some("broken"), "mode = \"dark\"\naccent = [not toml");
        assert_eq!(read_theme_at(&root), SystemThemeResult::unavailable());
    }

    #[test]
    fn invalid_hex_is_unavailable() {
        let dir = TestTmpDir::new("appearance-badhex");
        let root = dir.join("omarchy");
        write_theme(
            &root,
            Some("bad"),
            &MINIMAL_LIGHT.replace("\"#26a\"", "\"#nothex\""),
        );
        assert_eq!(read_theme_at(&root), SystemThemeResult::unavailable());
    }

    // ── Fingerprint ──────────────────────────────────────────────────────────

    #[test]
    fn fingerprint_tracks_content_and_absence() {
        let dir = TestTmpDir::new("appearance-fp");
        let root = dir.join("omarchy");
        write_theme(&root, Some("a"), MINIMAL_LIGHT);

        let before = fingerprint(&root);
        assert_eq!(
            before,
            fingerprint(&root),
            "untouched files fingerprint identically"
        );
        assert!(before.colors.is_some());
        assert!(before.name.is_some());

        // Mid-swap absence is a fingerprint value, not a crash.
        remove_theme(&root);
        let gone = fingerprint(&root);
        assert_eq!(gone.colors, None);
        assert_eq!(gone.name, None);
        assert_ne!(before, gone);

        // Different-length replacement content registers even where the filesystem's
        // mtime granularity is coarse.
        write_theme(&root, Some("b"), FULL_DARK);
        assert_ne!(fingerprint(&root), gone);
    }

    // ── Settle-read ──────────────────────────────────────────────────────────

    #[test]
    fn settle_read_settles_on_the_final_content_through_a_swap() {
        let dir = TestTmpDir::new("appearance-settle");
        let root = dir.join("omarchy");
        write_theme(&root, Some("first"), MINIMAL_LIGHT);
        // The swap is under way before the settle-read starts (theme removed), and the
        // replacement lands while it runs — the reader must ride the gap out and settle
        // on the final content, never on the gap itself.
        remove_theme(&root);
        let writer_root = root.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(250));
            write_theme(&writer_root, Some("second"), FULL_DARK);
        });

        let (result, _fp) = settle_read(&root);
        writer.join().unwrap();
        assert!(result.available);
        assert_eq!(result.theme_name.as_deref(), Some("second"));
        assert_eq!(result.palette.unwrap().accent, "#7AA2F7");
    }

    #[test]
    fn settle_read_reports_a_vanished_theme_as_unavailable() {
        let dir = TestTmpDir::new("appearance-vanish");
        let root = dir.join("omarchy");
        write_theme(&root, Some("gone"), MINIMAL_LIGHT);
        remove_theme(&root);
        // Stable-but-invalid never satisfies the settle condition; the budget runs out
        // (~2 s) and the caller learns the theme is gone.
        let (result, _fp) = settle_read(&root);
        assert_eq!(result, SystemThemeResult::unavailable());
    }

    // ── Poll tick ────────────────────────────────────────────────────────────

    #[test]
    fn poll_tick_is_quiet_while_nothing_changes() {
        let dir = TestTmpDir::new("appearance-quiet");
        let root = dir.join("omarchy");
        write_theme(&root, Some("a"), MINIMAL_LIGHT);
        let mut state = WatchState::capture(&root);
        assert_eq!(poll_tick(&root, &mut state), None);
        assert_eq!(poll_tick(&root, &mut state), None);
    }

    #[test]
    fn poll_tick_broadcasts_a_theme_switch_once() {
        let dir = TestTmpDir::new("appearance-switch");
        let root = dir.join("omarchy");
        write_theme(&root, Some("first"), MINIMAL_LIGHT);
        let mut state = WatchState::capture(&root);

        write_theme(&root, Some("second"), FULL_DARK);
        let result = poll_tick(&root, &mut state).expect("a switch must be broadcast");
        assert!(result.available);
        assert_eq!(result.theme_name.as_deref(), Some("second"));

        // The settled state is the new baseline: the next tick has nothing to say.
        assert_eq!(poll_tick(&root, &mut state), None);
    }

    #[test]
    fn poll_tick_swallows_a_touch_that_changes_nothing() {
        let dir = TestTmpDir::new("appearance-touch");
        let root = dir.join("omarchy");
        write_theme(&root, Some("a"), MINIMAL_LIGHT);
        let mut state = WatchState::capture(&root);

        // Rewrite identical content: the fingerprint may change (fresh mtime), the
        // settled outcome does not — no event.
        std::thread::sleep(Duration::from_millis(20));
        write_theme(&root, Some("a"), MINIMAL_LIGHT);
        assert_eq!(poll_tick(&root, &mut state), None);
    }

    #[test]
    fn poll_tick_tolerates_a_file_absent_mid_swap() {
        let dir = TestTmpDir::new("appearance-midswap");
        let root = dir.join("omarchy");
        write_theme(&root, Some("first"), MINIMAL_LIGHT);
        let mut state = WatchState::capture(&root);

        // The tick fires while the swap is mid-flight: theme already removed, the
        // replacement arriving only while the settle-read runs.
        remove_theme(&root);
        let writer_root = root.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(250));
            write_theme(&writer_root, Some("second"), FULL_DARK);
        });
        let result = poll_tick(&root, &mut state).expect("the settled swap must be broadcast");
        writer.join().unwrap();
        assert_eq!(result.theme_name.as_deref(), Some("second"));
        assert_eq!(poll_tick(&root, &mut state), None);
    }

    #[test]
    fn poll_tick_reports_a_vanished_theme_and_only_once() {
        let dir = TestTmpDir::new("appearance-gone");
        let root = dir.join("omarchy");
        write_theme(&root, Some("a"), MINIMAL_LIGHT);
        let mut state = WatchState::capture(&root);

        remove_theme(&root);
        assert_eq!(
            poll_tick(&root, &mut state),
            Some(SystemThemeResult::unavailable()),
            "the frontend must be told to fall back"
        );
        assert_eq!(poll_tick(&root, &mut state), None, "and told exactly once");
    }

    // ── IPC shape ────────────────────────────────────────────────────────────

    #[test]
    fn ipc_payload_is_camel_case() {
        let result = SystemThemeResult {
            available: true,
            theme_name: Some("tokyo-night".into()),
            palette: Some(parse_palette(FULL_DARK).unwrap()),
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["available"], true);
        assert_eq!(json["themeName"], "tokyo-night");
        assert_eq!(json["palette"]["mode"], "dark");
        assert_eq!(json["palette"]["darkBackground"], "#16161e");
        assert_eq!(json["palette"]["brightMagenta"], "#c7a9ff");
        // The rename is serialize-only: outbound JSON has no snake_case keys.
        assert!(json["palette"].get("dark_background").is_none());
    }
}
