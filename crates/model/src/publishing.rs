//! The pure parts of the publish targets: the Snapchat 9:16 pre-flight (`snapchat.tsx`), the
//! LocalSend device choice (`SendToDevicePanel.tsx`) and the album default and URL rule of
//! the shared publish panel (`publishing.tsx`). See docs/localsend.md and docs/publications.md.

/// Snapchat story aspect: vertical 9:16 (1080×1920).
pub const SNAP_ASPECT: f64 = 9.0 / 16.0;
/// Tolerance for "close enough to 9:16" (~±3 %).
pub const ASPECT_TOLERANCE: f64 = 0.03;
/// The warning the Snapchat target shows above Send. Non-blocking: the user can still send.
pub const SNAPCHAT_WARNING: &str =
    "Snapchat stories are vertical 9:16 (1080×1920) — make a 9:16 crop for best results.";

/// The effective width/height aspect of what would be sent.
///
/// Priority: the selected version's crop, read generically from its `edit_json` — an
/// `aspect` label like "9:16" (W:H) wins, else a normalized crop `w`/`h` applied to the
/// photo's pixel dimensions; then the photo's own dimensions. `None` when nothing usable is
/// known.
pub fn effective_aspect(width: Option<i64>, height: Option<i64>, edit_json: Option<&str>) -> Option<f64> {
    let dims = match (width, height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => Some((w as f64, h as f64)),
        _ => None,
    };
    if let Some(crop) = edit_json.and_then(parse_crop) {
        if let Some(a) = crop.get("aspect").and_then(|a| a.as_str()).and_then(parse_aspect_label) {
            return Some(a);
        }
        let num = |k: &str| crop.get(k).and_then(|v| v.as_f64());
        if let (Some(cw), Some(ch), Some((w, h))) = (num("w"), num("h"), dims) {
            if cw > 0.0 && ch > 0.0 {
                let aspect = (w * cw) / (h * ch);
                if aspect.is_finite() && aspect > 0.0 {
                    return Some(aspect);
                }
            }
        }
    }
    dims.map(|(w, h)| w / h)
}

/// Whether `aspect` is within ±`tolerance` (fractional) of 9:16.
pub fn is_near_snap_aspect(aspect: Option<f64>, tolerance: f64) -> bool {
    match aspect {
        Some(a) if a.is_finite() && a > 0.0 => (a - SNAP_ASPECT).abs() <= SNAP_ASPECT * tolerance,
        _ => false,
    }
}

/// The Snapchat pre-flight: [`SNAPCHAT_WARNING`] when the effective aspect is known and not
/// ~9:16, else `None`.
pub fn snapchat_preflight(width: Option<i64>, height: Option<i64>, edit_json: Option<&str>) -> Option<&'static str> {
    let aspect = effective_aspect(width, height, edit_json);
    if aspect.is_none() || is_near_snap_aspect(aspect, ASPECT_TOLERANCE) {
        None
    } else {
        Some(SNAPCHAT_WARNING)
    }
}

/// The `crop` object of an edit record; malformed JSON or no crop → `None`.
fn parse_crop(edit_json: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    let v: serde_json::Value = serde_json::from_str(edit_json).ok()?;
    v.get("crop")?.as_object().cloned()
}

/// "W:H" (e.g. "9:16") as width / height.
fn parse_aspect_label(label: &str) -> Option<f64> {
    let (w, h) = label.trim().split_once(':')?;
    let (w, h): (f64, f64) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
    (w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0).then(|| w / h)
}

/// LocalSend's well-known port: a manual address's default.
pub const DEFAULT_PORT: u16 = 53317;

/// The manual port field as a port: anything unparsable (or 0) is [`DEFAULT_PORT`]
/// (`Number(manualPort) || DEFAULT_PORT`).
pub fn manual_port(text: &str) -> u16 {
    text.trim().parse::<u16>().ok().filter(|p| *p != 0).unwrap_or(DEFAULT_PORT)
}

/// A device's line in the picker: "alias (model) — ip".
pub fn device_label(alias: &str, model: Option<&str>, ip: &str) -> String {
    match model.filter(|m| !m.is_empty()) {
        Some(m) => format!("{alias} ({m}) — {ip}"),
        None => format!("{alias} — {ip}"),
    }
}

/// The send's result line: "Sent 3 to Phone, 1 skipped."
pub fn sent_line(sent: usize, failed: usize, alias: &str) -> String {
    let skipped = if failed > 0 { format!(", {failed} skipped") } else { String::new() };
    format!("Sent {sent} to {alias}{skipped}.")
}

/// A send that stopped: "Sent 3 of 5 to Phone, then stopped: why" — or just `why` when nothing
/// had arrived.
pub fn stopped_line(sent: usize, requested: usize, alias: &str, why: &str) -> String {
    if sent == 0 {
        why.to_string()
    } else {
        format!("Sent {sent} of {requested} to {alias}, then stopped: {why}")
    }
}

/// "Sending d/t…".
pub fn progress_line(done: usize, total: usize) -> String {
    format!("Sending {done}/{total}…")
}

/// Which album a publish panel starts on: the remembered one when it is still listed, else the
/// first (`last && list.some(a => a.uri === last) ? last : list[0]?.uri ?? ""`).
pub fn default_album<'a>(uris: &[&'a str], last: Option<&'a str>) -> Option<&'a str> {
    match last {
        Some(l) if uris.contains(&l) => Some(l),
        _ => uris.first().copied(),
    }
}

/// The URL a publication records: the service's answer when it is a web address (the
/// Flickr importer matches its own uploads by it), else none.
pub fn publication_url(answer: &str) -> Option<&str> {
    (answer.starts_with("http://") || answer.starts_with("https://")).then_some(answer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_crop_aspect_label_wins_over_the_rect_and_the_photo() {
        let json = r#"{"crop":{"x":0,"y":0,"w":1,"h":1,"aspect":"9:16"}}"#;
        assert_eq!(effective_aspect(Some(6000), Some(4000), Some(json)), Some(9.0 / 16.0));
    }

    #[test]
    fn a_normalized_crop_applies_to_the_photo_dimensions() {
        // A 6000×4000 landscape cropped to w 0.375, h 1 → 2250×4000 = 9:16.
        let json = r#"{"crop":{"x":0.3,"y":0,"w":0.375,"h":1}}"#;
        let a = effective_aspect(Some(6000), Some(4000), Some(json)).unwrap();
        assert!((a - 9.0 / 16.0).abs() < 1e-9, "{a}");
        assert!(snapchat_preflight(Some(6000), Some(4000), Some(json)).is_none());
    }

    #[test]
    fn without_a_crop_the_photo_dimensions_decide() {
        assert_eq!(effective_aspect(Some(6000), Some(4000), None), Some(1.5));
        assert_eq!(effective_aspect(Some(6000), Some(4000), Some("not json")), Some(1.5), "malformed → photo");
        assert_eq!(effective_aspect(Some(6000), Some(4000), Some(r#"{"exposure":1}"#)), Some(1.5));
        assert_eq!(effective_aspect(None, None, None), None);
        assert_eq!(effective_aspect(Some(0), Some(4000), None), None);
        // A crop rect without dimensions to apply it to falls through to "unknown".
        assert_eq!(effective_aspect(None, None, Some(r#"{"crop":{"w":0.5,"h":1}}"#)), None);
    }

    #[test]
    fn the_preflight_warns_only_when_the_shape_is_known_and_not_9_16() {
        assert_eq!(snapchat_preflight(Some(6000), Some(4000), None), Some(SNAPCHAT_WARNING), "3:2 landscape");
        assert_eq!(snapchat_preflight(Some(1080), Some(1920), None), None, "exactly 9:16");
        assert_eq!(snapchat_preflight(None, None, None), None, "unknown shape: no warning");
        // ±3 %: 0.5625 × 1.03 ≈ 0.579 passes, 0.6 does not.
        assert!(is_near_snap_aspect(Some(0.579), ASPECT_TOLERANCE));
        assert!(!is_near_snap_aspect(Some(0.6), ASPECT_TOLERANCE));
        assert!(!is_near_snap_aspect(None, ASPECT_TOLERANCE));
        assert!(!is_near_snap_aspect(Some(f64::NAN), ASPECT_TOLERANCE));
    }

    #[test]
    fn aspect_labels_parse_w_over_h() {
        assert_eq!(parse_aspect_label(" 9 : 16 "), Some(9.0 / 16.0));
        assert_eq!(parse_aspect_label("4.5:8"), Some(4.5 / 8.0));
        assert_eq!(parse_aspect_label("0:16"), None);
        assert_eq!(parse_aspect_label("free"), None);
    }

    #[test]
    fn the_manual_port_falls_back_to_the_well_known_one() {
        assert_eq!(manual_port("9000"), 9000);
        assert_eq!(manual_port(" 53318 "), 53318);
        assert_eq!(manual_port(""), DEFAULT_PORT);
        assert_eq!(manual_port("0"), DEFAULT_PORT);
        assert_eq!(manual_port("abc"), DEFAULT_PORT);
        assert_eq!(manual_port("70000"), DEFAULT_PORT);
    }

    #[test]
    fn lines_read_as_react_wrote_them() {
        assert_eq!(device_label("Kind Carrot", Some("iPhone"), "192.168.1.128"), "Kind Carrot (iPhone) — 192.168.1.128");
        assert_eq!(device_label("Desk", None, "10.0.0.2"), "Desk — 10.0.0.2");
        assert_eq!(sent_line(3, 0, "Phone"), "Sent 3 to Phone.");
        assert_eq!(sent_line(2, 1, "Phone"), "Sent 2 to Phone, 1 skipped.");
        assert_eq!(progress_line(1, 4), "Sending 1/4…");
    }

    #[test]
    fn the_album_default_and_the_recorded_url() {
        assert_eq!(default_album(&["a", "b"], Some("b")), Some("b"));
        assert_eq!(default_album(&["a", "b"], Some("gone")), Some("a"));
        assert_eq!(default_album(&[], Some("b")), None);
        assert_eq!(publication_url("https://flickr.com/photos/x/1"), Some("https://flickr.com/photos/x/1"));
        assert_eq!(publication_url("/api/v2/image/abc"), None);
    }
}
