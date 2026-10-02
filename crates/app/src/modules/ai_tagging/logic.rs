//! The AI tagging module's pure parts, ported from aiTagging.tsx: the providers and their
//! setting keys, the curated cloud model lists, the bulk-cost table, grouping suggestions by
//! provenance, the region box, and the cloud opt-in check.

use chairphoto_core::app::ai::{AiSuggestion, Region};

/// The selectable engines (`PROVIDERS`). `ollama` is local and private; the rest send the
/// image to a cloud vision API — an explicit, per-provider opt-in (its own API key).
pub const PROVIDERS: [(&str, &str); 4] = [
    ("ollama", "Local — Ollama (private, on this machine)"),
    ("claude", "Cloud — Claude (Anthropic)"),
    ("openai", "Cloud — OpenAI (GPT)"),
    ("gemini", "Cloud — Gemini (Google)"),
];

/// The inspector picker's short label for a provider ("Ollama (local)", "Claude", …).
pub fn provider_short(id: &str) -> String {
    if id == "ollama" {
        return "Ollama (local)".into();
    }
    let mut c = id.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

pub fn is_cloud(provider: &str) -> bool {
    provider != "ollama"
}

/// The setting key holding each provider's model (`MODEL_KEY`).
pub fn model_key(provider: &str) -> &'static str {
    match provider {
        "claude" => "cloud_model",
        "openai" => "openai_model",
        "gemini" => "gemini_model",
        _ => "ollama_model",
    }
}

/// The setting key holding a cloud provider's API key; `None` for Ollama.
pub fn api_key_key(provider: &str) -> Option<&'static str> {
    match provider {
        "claude" => Some("cloud_api_key"),
        "openai" => Some("openai_api_key"),
        "gemini" => Some("gemini_api_key"),
        _ => None,
    }
}

/// Known vision models per cloud provider (`CURATED_MODELS`, unchanged from the React app); a
/// saved custom value is shown on top. Ollama's list is fetched live from the server.
pub fn curated_models(provider: &str) -> &'static [&'static str] {
    match provider {
        "claude" => &["claude-opus-4-8", "claude-sonnet-4-6", "claude-haiku-4-5-20251001"],
        "openai" => &["gpt-4o", "gpt-4o-mini", "gpt-4.1", "gpt-4.1-mini"],
        "gemini" => &["gemini-2.0-flash", "gemini-2.5-flash", "gemini-1.5-flash", "gemini-1.5-pro"],
        _ => &[],
    }
}

/// Per-provider settings fields (`PROVIDER_FIELDS`): (key, label), shown for the selected one.
pub fn provider_fields(provider: &str) -> [(&'static str, &'static str); 2] {
    match provider {
        "claude" => [("cloud_model", "Claude model"), ("cloud_api_key", "Claude API key")],
        "openai" => [("openai_model", "OpenAI model"), ("openai_api_key", "OpenAI API key")],
        "gemini" => [("gemini_model", "Gemini model"), ("gemini_api_key", "Gemini API key")],
        _ => [("ollama_url", "Ollama URL"), ("ollama_model", "Ollama model")],
    }
}

/// Every `ai.*` key the settings panel saves, with its default (`DEFAULTS`). A blank stored
/// value reads as the default, as the backend's `read_config` does.
pub const DEFAULTS: [(&str, &str); 13] = [
    ("provider", "ollama"),
    ("ollama_url", "http://localhost:11434"),
    // The non-loopback Ollama URL the user allowed photos to go to (blank: none).
    ("ollama_remote_url", ""),
    ("ollama_model", "llava:latest"),
    ("cloud_model", "claude-sonnet-4-6"),
    ("cloud_api_key", ""),
    ("openai_model", "gpt-4o"),
    ("openai_api_key", ""),
    ("gemini_model", "gemini-2.0-flash"),
    ("gemini_api_key", ""),
    ("existing_only", "false"),
    ("min_confidence", "0"),
    ("prompt_template", ""),
];

pub fn default_of(key: &str) -> &'static str {
    DEFAULTS.iter().find(|(k, _)| *k == key).map_or("", |(_, v)| v)
}

/// A model picker's options: `choices`, with a saved value that is not among them on top
/// (so a custom model stays visible).
pub fn model_options(current: &str, choices: &[String]) -> Vec<String> {
    if current.is_empty() || choices.iter().any(|c| c == current) {
        choices.to_vec()
    } else {
        std::iter::once(current.to_string()).chain(choices.iter().cloned()).collect()
    }
}

/// Rough per-image cost (USD) per model (`COST_PER_IMAGE_USD`): ~1 500 input tokens + ~300
/// output tokens at each provider's list price as of 2026-07. Display only — the confirm
/// says so.
pub fn cost_per_image(model: &str) -> Option<f64> {
    Some(match model {
        "claude-opus-4-8" => 0.045,
        "claude-sonnet-4-6" => 0.009,
        "claude-haiku-4-5-20251001" => 0.0024,
        "gpt-4o" => 0.0068,
        "gpt-4o-mini" => 0.0004,
        "gpt-4.1" => 0.0054,
        "gpt-4.1-mini" => 0.0011,
        "gemini-2.0-flash" => 0.00027,
        "gemini-2.5-flash" => 0.0004,
        "gemini-1.5-flash" => 0.0002,
        "gemini-1.5-pro" => 0.0034,
        _ => return None,
    })
}

/// The estimate's display (`estimateBulkCost`): `$0.0042` under a cent, `$1.23` above;
/// `None` for an unknown model.
pub fn estimate_bulk_cost(model: &str, photos: usize) -> Option<String> {
    let total = cost_per_image(model)? * photos as f64;
    Some(if total < 0.01 { format!("${total:.4}") } else { format!("${total:.2}") })
}

/// One group of suggestions: a direct run (`source` `None`) or a propagated cluster.
#[derive(Debug, Clone, PartialEq)]
pub struct SuggestionGroup {
    pub source: Option<i64>,
    pub source_filename: Option<String>,
    pub items: Vec<AiSuggestion>,
}

/// Partition by provenance (`groupSuggestions`): direct suggestions first, then each
/// representative's group in first-seen order.
pub fn group_suggestions(list: &[AiSuggestion]) -> Vec<SuggestionGroup> {
    let mut groups: Vec<SuggestionGroup> = Vec::new();
    let direct: Vec<AiSuggestion> = list.iter().filter(|s| s.source_photo_id.is_none()).cloned().collect();
    if !direct.is_empty() {
        groups.push(SuggestionGroup { source: None, source_filename: None, items: direct });
    }
    for s in list.iter().filter(|s| s.source_photo_id.is_some()) {
        match groups.iter_mut().find(|g| g.source == s.source_photo_id) {
            Some(g) => g.items.push(s.clone()),
            None => groups.push(SuggestionGroup {
                source: s.source_photo_id,
                source_filename: s.source_photo_filename.clone(),
                items: vec![s.clone()],
            }),
        }
    }
    groups
}

/// The box between a drag's two normalized points, clamped to the image.
pub fn region_between(a: (f32, f32), b: (f32, f32)) -> Region {
    let (ax, ay) = (a.0.clamp(0., 1.), a.1.clamp(0., 1.));
    let (bx, by) = (b.0.clamp(0., 1.), b.1.clamp(0., 1.));
    Region { x: ax.min(bx), y: ay.min(by), w: (ax - bx).abs(), h: (ay - by).abs() }
}

/// A box smaller than 2 % on either side is an accidental tap, discarded on release.
pub fn is_tap(r: &Region) -> bool {
    r.w < 0.02 || r.h < 0.02
}

/// The follow-up "↳ more specific" asks (`refineLeaf`).
pub fn refine_question(path: &str) -> String {
    let leaf = path.rsplit('/').next().filter(|l| !l.is_empty()).unwrap_or(path);
    format!("What specific kind of {leaf} is this? Suggest a more specific tag.")
}

/// The batch result line.
pub fn batch_done_line(total: usize, representatives: usize, propagated: usize) -> String {
    format!("Done — {total} photos → {representatives} representatives, {propagated} suggestions. Review each photo.")
}

/// The batch line after Cancel.
pub fn batch_cancelled_line(sent: usize, representatives: usize, propagated: usize) -> String {
    format!("Cancelled — {sent} of {representatives} representatives sent, {propagated} suggestions stored.")
}

/// Whether a run may send photos to `provider` now — the core's rule
/// (`plugins::ai::opt_in`): Ollama at a loopback URL is local; a cloud provider is opted into
/// by choosing it **and** saving its API key; an Ollama server not on this machine by allowing
/// that exact URL. Without the opt-in no preview is even read: the refusal names the fix.
pub fn send_opt_in(provider: &str, api_key: &str, ollama_url: &str, ollama_remote_url: &str) -> Result<(), String> {
    chairphoto_core::plugins::ai::opt_in(provider, api_key, ollama_url, ollama_remote_url)
}

/// Whether the Ollama server at `url` is on this machine (a loopback literal; no DNS).
pub fn is_local_url(url: &str) -> bool {
    chairphoto_core::plugins::ai::is_loopback_url(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sug(path: &str, source: Option<i64>) -> AiSuggestion {
        AiSuggestion {
            path: path.into(),
            confidence: 0.5,
            reason: String::new(),
            existing_tag_id: None,
            is_new: false,
            description: String::new(),
            synonyms: Vec::new(),
            source_photo_id: source,
            source_photo_filename: source.map(|s| format!("DSC{s}.ARW")),
        }
    }

    #[test]
    fn groups_put_direct_first_then_representatives_in_order() {
        let list = [sug("A", Some(7)), sug("B", None), sug("C", Some(3)), sug("D", Some(7))];
        let g = group_suggestions(&list);
        assert_eq!(g.iter().map(|g| g.source).collect::<Vec<_>>(), vec![None, Some(7), Some(3)]);
        assert_eq!(g[1].items.iter().map(|s| s.path.as_str()).collect::<Vec<_>>(), vec!["A", "D"]);
        assert_eq!(g[1].source_filename.as_deref(), Some("DSC7.ARW"));
    }

    #[test]
    fn bulk_cost_formats_like_the_react_app() {
        assert_eq!(estimate_bulk_cost("claude-sonnet-4-6", 3).as_deref(), Some("$0.03"));
        assert_eq!(estimate_bulk_cost("gemini-2.0-flash", 2).as_deref(), Some("$0.0005"));
        assert_eq!(estimate_bulk_cost("my-own-model", 2), None);
    }

    #[test]
    fn region_is_normalized_and_taps_are_discarded() {
        let r = region_between((0.8, 0.9), (0.2, 0.1));
        assert!((r.x - 0.2).abs() < 1e-6 && (r.y - 0.1).abs() < 1e-6);
        assert!((r.w - 0.6).abs() < 1e-6 && (r.h - 0.8).abs() < 1e-6);
        assert!(is_tap(&region_between((0.5, 0.5), (0.51, 0.9))));
        assert!(!is_tap(&r));
        let clamped = region_between((-1., -1.), (2., 2.));
        assert_eq!((clamped.x, clamped.w), (0., 1.));
    }

    #[test]
    fn model_options_keep_a_saved_custom_model() {
        let choices = vec!["a".to_string(), "b".to_string()];
        assert_eq!(model_options("custom", &choices), vec!["custom", "a", "b"]);
        assert_eq!(model_options("b", &choices), choices);
        assert_eq!(model_options("", &choices), choices);
    }

    #[test]
    fn cloud_needs_its_key_and_local_ollama_needs_nothing() {
        assert!(send_opt_in("ollama", "", "http://localhost:11434", "").is_ok());
        assert!(send_opt_in("claude", "sk-x", "", "").is_ok());
        let e = send_opt_in("gemini", "  ", "", "").unwrap_err();
        assert!(e.contains("Gemini") && e.contains("API key"), "{e}");
        assert!(send_opt_in("ollama", "", "http://10.0.0.2:11434", "").is_err());
    }

    #[test]
    fn refine_asks_about_the_leaf() {
        assert_eq!(refine_question("Transportation/Boats"), "What specific kind of Boats is this? Suggest a more specific tag.");
    }
}
