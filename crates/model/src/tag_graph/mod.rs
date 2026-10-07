//! The Tag Graph module's logic (`src/modules/plugins/tagGraph.tsx` and `tagGraphBundle.ts`),
//! without a toolkit: the GPUI module (`chairphoto-app`, `modules::tag_graph`) paints it.
//!
//! - [`bundle`]: the radial edge-bundling layout, ported one to one from `tagGraphBundle.ts`.
//! - [`graph`]: `library_graph`'s answer as nodes and links, scoped to a branch.
//! - [`scene`]: the ring plus every drawn edge's curve and the base layer's buckets — built
//!   off the UI thread.
//! - [`labels`]: horizontal label placement and hit testing.
//! - [`view`]: the pan/zoom transform.
//! - [`session`]: the state machine the view drives (load, scene, focus, branch, cards).
//! - [`synthetic`]: a generated library at the real one's scale, for tests and the bench.
//!
//! **Communities only.** The legacy Photo ↔ tag (force-directed) mode was dropped by the
//! owner in "Tag graph in GPUI: which trade-offs does the owner accept?" (#120), so nothing
//! here simulates forces; the bipartite `photo_tag_graph` projection has no reader.

pub mod bundle;
pub mod graph;
pub mod labels;
pub mod scene;
pub mod session;
pub mod synthetic;
pub mod view;

/// A colour as `0xRRGGBB`.
pub type Rgb = u32;

/// Community colours (families), cycled in size order (`PALETTE`).
pub const PALETTE: [Rgb; 6] = [0x3B82F6, 0x8B5CF6, 0x10B981, 0xEC4899, 0x14B8A6, 0xF97316];
/// Reserved for cameras, so their arc is unmistakable.
pub const CAMERA_COLOR: Rgb = 0xF59E0B;

/// The ring radius, graph units (the view scales it).
pub const RING_R: f64 = 400.;
/// Pixels reserved outside the ring for tag labels.
pub const LABEL_EXTENT: f64 = 100.;
/// Labels longer than this are cut with an ellipsis.
pub const LABEL_MAX_CHARS: usize = 16;
/// 1 = follow the hierarchy exactly, 0 = straight chords.
pub const BUNDLE_BETA: f64 = 0.85;

/// The last segment of a tag path (`leaf`).
pub fn leaf(path: &str) -> &str {
    match path.rsplit('/').next() {
        Some(l) if !l.is_empty() => l,
        _ => path,
    }
}

/// The first segment of a tag path (`topLevel`).
pub fn top_level(path: &str) -> &str {
    match path.split('/').next() {
        Some(t) if !t.is_empty() => t,
        _ => path,
    }
}

/// Cut a label to [`LABEL_MAX_CHARS`] with an ellipsis (`truncate`). Counts chars where the
/// TypeScript counted UTF-16 units; the two differ only for characters outside the BMP.
pub fn truncate(s: &str) -> String {
    if s.chars().count() > LABEL_MAX_CHARS {
        let mut out: String = s.chars().take(LABEL_MAX_CHARS - 1).collect();
        out.push('…');
        out
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaf_top_level_and_truncate_follow_the_typescript() {
        assert_eq!(leaf("Animals/Bird/Seagull"), "Seagull");
        assert_eq!(leaf("Animals"), "Animals");
        assert_eq!(leaf("Animals/"), "Animals/", "`pop() || path`");
        assert_eq!(top_level("Animals/Bird"), "Animals");
        assert_eq!(top_level("/x"), "/x", "`[0] || path`");
        assert_eq!(truncate("Short"), "Short");
        assert_eq!(truncate("Exactly sixteen!"), "Exactly sixteen!");
        assert_eq!(truncate("Seventeen chars!!"), "Seventeen chars…");
    }
}
