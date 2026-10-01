//! "Culling signals" (`SignalsPanel.tsx`): why the photo carries the badges it carries — its
//! sharpness against the threshold, its burst (cluster, median, cutoff, rank, the sharpest
//! frame, the frames table), and its stack and version badges. Read-only; the derivation is
//! the core's (`photo_signals::explain_photo_signals`). A stale stored flag and a truncated
//! burst are always said, never smoothed over.

use super::Load;
use crate::shell::style::Colors;
use chairphoto_core::photo_signals::{ClusterFrame, PhotoSignals};
use gpui_kit::prelude::*;
use gpui_kit::{div, px, AnyElement, Div, FontWeight, SharedString, TestSupportExt as _};

/// A verdict's words (`FLAG_LABEL`).
pub fn flag_label(flag: Option<&str>) -> String {
    match flag {
        Some("soft-in-burst") => "Soft in burst".into(),
        Some("sharpest-of-burst") => "Sharpest of burst".into(),
        Some(other) => other.into(),
        None => "no flag".into(),
    }
}

/// A score: scores span orders of magnitude, so ≥ 100 shows no decimals, else one.
pub fn score(v: Option<f64>) -> String {
    match v {
        None => "—".into(),
        Some(v) if v >= 100.0 => format!("{v:.0}"),
        Some(v) => format!("{v:.1}"),
    }
}

/// Every line the panel shows, in order, as `(kind, text)` — the rendering below and the
/// tests read the same list.
#[derive(Debug, Clone, PartialEq)]
pub enum Line {
    /// A block heading with its verdict (`soft` colours it as a warning).
    Head { title: String, verdict: Option<String>, soft: bool },
    Text(String),
    Note(String),
    Warn(String),
    /// The frames table: (subject marker + name, sharpness, Δ, mark, Δ's tooltip).
    Frame { subject: bool, name: String, sharp: String, delta: String, mark: &'static str },
    Empty(String),
}

pub fn lines(s: &PhotoSignals) -> Vec<Line> {
    let mut out = Vec::new();
    let nothing = s.sharpness.is_none()
        && s.burst.is_none()
        && s.stack.child_count == 0
        && s.stack.parent_id.is_none()
        && s.version_count == 0;
    if nothing {
        out.push(Line::Empty("No signals yet — this photo has not been scored, hashed or stacked.".into()));
    }
    if let Some(sh) = &s.sharpness {
        out.push(Line::Head {
            title: "Sharpness".into(),
            verdict: Some(if sh.below_threshold { "Soft" } else { "Above the soft threshold" }.into()),
            soft: sh.below_threshold,
        });
        let mut line = format!("{} against a threshold of {}", score(Some(sh.score)), score(Some(sh.soft_threshold)));
        if let Some(m) = &sh.method {
            line.push_str(&format!(" · scored by {m}"));
        }
        out.push(Line::Text(line));
        if let Some(m) = sh.method.as_deref().filter(|m| *m != "tile") {
            let on = if m == "face" { "a detected face" } else { "the camera's AF point" };
            out.push(Line::Note(format!("Scores from different methods are not comparable — this one was measured on {on}.")));
        }
    }
    if let Some(b) = &s.burst {
        out.push(Line::Head {
            title: "Burst".into(),
            verdict: Some(flag_label(b.verdict.as_deref())),
            soft: b.verdict.as_deref() == Some("soft-in-burst"),
        });
        if b.cluster_size == 1 {
            out.push(Line::Text(format!("Not part of a burst — no frame within {}s of it.", b.time_gap_secs)));
        } else {
            let rank = b.rank.map_or("—".to_string(), |r| r.to_string());
            let mut line = format!(
                "Frame {rank} of {} by sharpness, in a burst of {}{}",
                b.scored,
                b.cluster_size,
                if b.truncated { "+" } else { "" }
            );
            if b.time_group_size > b.cluster_size {
                line.push_str(&format!(" (split from {} frames shot together)", b.time_group_size));
            }
            line.push('.');
            out.push(Line::Text(line));
        }
        if let (Some(median), Some(cutoff), true) = (b.median, b.cutoff, b.cluster_size > 1) {
            let mut line = format!(
                "Cluster median {} · soft below {} ({}% of the median)",
                score(Some(median)),
                score(Some(cutoff)),
                (b.soft_fraction * 100.0).round() as i64
            );
            if let Some(best) = &b.best {
                line.push_str(&format!(" · sharpest is {} at {}", best.file_name, score(best.sharpness)));
            }
            out.push(Line::Text(line));
        }
        if b.stale {
            out.push(Line::Warn(format!(
                "The badge on this photo says {}, but this burst now reads {}. The stored flag came from an earlier \
                 run over a different set of photos — re-run burst analysis to refresh it.",
                flag_label(b.stored_flag.as_deref()),
                flag_label(b.verdict.as_deref())
            )));
        }
        if b.truncated {
            out.push(Line::Warn(
                "This run of frames is longer than one lookup can cover, so the count, rank and median above are \
                 lower bounds on a possibly larger burst."
                    .into(),
            ));
        }
        if b.frames.len() > 1 {
            for f in &b.frames {
                out.push(frame_line(f));
            }
            if b.frames.len() < b.cluster_size {
                out.push(Line::Note(format!("Showing {} of {} frames.", b.frames.len(), b.cluster_size)));
            }
            out.push(Line::Note(format!(
                "Δ is the visual difference from this photo (0 = identical). The engine treats frames within {} as \
                 the same scene.",
                b.hamming_threshold
            )));
        }
    }
    if s.stack.child_count > 0 || s.stack.parent_id.is_some() || s.version_count > 0 {
        out.push(Line::Head { title: "Other badges".into(), verdict: None, soft: false });
        if s.stack.child_count > 0 {
            let n = s.stack.child_count;
            out.push(Line::Text(format!(
                "{n} file{} stacked under this one (e.g. the camera JPEG) — see the Stack section.",
                if n == 1 { "" } else { "s" }
            )));
        }
        if let Some(parent) = s.stack.parent_id {
            out.push(Line::Text(format!("Stacked under photo #{parent}, which is what the grid lists instead.")));
        }
        if s.version_count > 0 {
            let n = s.version_count;
            out.push(Line::Text(format!(
                "{n} edit version{} — see the Versions section.",
                if n == 1 { "" } else { "s" }
            )));
        }
    }
    out
}

fn frame_line(f: &ClusterFrame) -> Line {
    let mark = match f.verdict.as_deref() {
        Some("sharpest-of-burst") => "♛",
        Some("soft-in-burst") => "~",
        _ => "",
    };
    Line::Frame {
        subject: f.is_subject,
        name: f.file_name.clone(),
        sharp: score(f.sharpness),
        delta: f.hamming_distance.map_or("—".into(), |d| d.to_string()),
        mark,
    }
}

/// The panel for one photo's signals.
pub fn render(load: &Load<PhotoSignals>, colors: Colors) -> AnyElement {
    let body = div().id("signals").flex().flex_col().gap(px(4.)).text_size(px(11.5));
    let empty = |text: String| div().text_color(colors.mute).child(text);
    let signals = match load {
        Load::Idle | Load::Loading => return body.child(empty("Reading signals…".into())).test_support().into_any_element(),
        Load::Failed(e) => {
            return body.child(empty(format!("Could not read this photo's signals: {e}"))).test_support().into_any_element()
        }
        Load::Ready(s) => s,
    };
    let mut frames: Option<Div> = None;
    let mut out = body;
    let flush = |out: gpui_kit::Stateful<Div>, frames: &mut Option<Div>| match frames.take() {
        Some(t) => out.child(t),
        None => out,
    };
    for line in lines(signals) {
        if !matches!(line, Line::Frame { .. }) {
            out = flush(out, &mut frames);
        }
        out = match line {
            Line::Empty(t) => out.child(empty(t)),
            Line::Head { title, verdict, soft } => out.child(
                div()
                    .flex()
                    .gap(px(8.))
                    .mt(px(4.))
                    .text_size(px(10.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(colors.mute)
                    .child(title.to_uppercase())
                    .children(verdict.map(|v| {
                        div().font_weight(FontWeight::NORMAL).text_color(if soft { colors.danger } else { colors.dim }).child(v)
                    })),
            ),
            Line::Text(t) => out.child(div().text_color(colors.dim).child(t)),
            Line::Note(t) => out.child(div().text_size(px(10.5)).text_color(colors.mute).child(t)),
            Line::Warn(t) => out.child(div().text_size(px(10.5)).text_color(colors.rating).child(t)),
            Line::Frame { subject, name, sharp, delta, mark } => {
                let table = frames.take().unwrap_or_else(|| {
                    div().flex().flex_col().child(frame_row(
                        SharedString::from("Frame"),
                        "Sharp".into(),
                        "Δ".into(),
                        "",
                        colors.mute,
                        colors,
                    ))
                });
                let name: SharedString = if subject { format!("▸ {name}").into() } else { name.into() };
                let color = if subject { colors.txt } else { colors.dim };
                frames = Some(table.child(frame_row(name, sharp.into(), delta.into(), mark, color, colors)));
                out
            }
        };
    }
    flush(out, &mut frames).test_support().into_any_element()
}

fn frame_row(name: SharedString, sharp: SharedString, delta: SharedString, mark: &'static str, color: gpui_kit::Hsla, colors: Colors) -> Div {
    div()
        .flex()
        .gap(px(6.))
        .text_size(px(10.5))
        .text_color(color)
        .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child(name))
        .child(div().w(px(44.)).text_right().child(sharp))
        .child(div().w(px(26.)).text_right().child(delta))
        .child(div().w(px(14.)).text_color(colors.rating).child(mark))
}
