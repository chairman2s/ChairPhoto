//! AI tagging backend — the `ai-tagging` plugin's Rust side. Compiled only with the
//! `ai` Cargo feature (so a build without it carries no AI code or `reqwest`).
//!
//! A vision model is shown the photo + the user's taxonomy (paths + descriptions)
//! and asked which tags apply, plus optional new-tag proposals. Provider is
//! configurable via module-scoped settings (`ai.*`); Ollama (local) and Claude
//! (cloud) are implemented; more slot in behind the same `suggest` entry point.

/// Re-export the burst grouping engine (now a top-level module) so existing
/// references via `crate::plugins::ai::burst` keep compiling.
pub use crate::burst as burst;

use crate::catalog::Catalog;
use rusqlite::OptionalExtension;

/// Resolved provider configuration (from the catalog settings KV).
pub struct Config {
    /// "ollama" (local) | "claude" | "openai" | "gemini" (cloud).
    pub provider: String,
    pub ollama_url: String,
    pub ollama_model: String,
    pub cloud_model: String, // Claude
    pub cloud_key: String,   // Claude (Anthropic)
    pub openai_model: String,
    pub openai_key: String,
    pub gemini_model: String,
    pub gemini_key: String,
    /// When true, only suggest tags already in the taxonomy (no new-tag discovery).
    pub existing_only: bool,
    /// Drop suggestions below this confidence (0.0 = keep all).
    pub min_confidence: f32,
    /// Custom prompt template (Advanced). Empty = the built-in DEFAULT_PROMPT.
    pub prompt_template: String,
}

impl Config {
    /// True when the configured provider runs on-machine (Ollama). Cloud providers
    /// (Claude/OpenAI/Gemini) are remote — private tags are withheld from them.
    pub fn is_local(&self) -> bool {
        self.provider == "ollama"
    }
}

/// One raw suggestion from the model (before resolving against the taxonomy).
/// `description`/`synonyms` are only meaningful for tags that turn out to be new.
pub struct Raw {
    pub path: String,
    pub confidence: f32,
    pub reason: String,
    pub description: String,
    pub synonyms: Vec<String>,
}

pub fn read_config(c: &Catalog) -> crate::catalog::Result<Config> {
    let get = |key: &str, default: &str| -> crate::catalog::Result<String> {
        Ok(c.get_setting(key)?
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| default.to_string()))
    };
    Ok(Config {
        provider: get("ai.provider", "ollama")?,
        ollama_url: get("ai.ollama_url", "http://localhost:11434")?,
        ollama_model: get("ai.ollama_model", "llava:latest")?,
        cloud_model: get("ai.cloud_model", "claude-sonnet-4-6")?,
        cloud_key: get("ai.cloud_api_key", "")?,
        openai_model: get("ai.openai_model", "gpt-4o")?,
        openai_key: get("ai.openai_api_key", "")?,
        gemini_model: get("ai.gemini_model", "gemini-2.0-flash")?,
        gemini_key: get("ai.gemini_api_key", "")?,
        existing_only: get("ai.existing_only", "false")? == "true",
        min_confidence: get("ai.min_confidence", "0")?.parse().unwrap_or(0.0),
        prompt_template: get("ai.prompt_template", "")?,
    })
}

// --- the plugin's own persistent suggestions store (ai__suggestions table) -----

/// One persisted suggestion row.
pub struct Stored {
    pub path: String,
    pub confidence: f32,
    pub reason: String,
    pub description: String,
    pub synonyms: Vec<String>,
    /// Provenance: when this suggestion was propagated from a burst representative
    /// (H15c), the photo id of that representative. `None` for a direct per-photo run.
    pub source_photo_id: Option<i64>,
}

/// Create the plugin-owned table if needed (prefix convention; not in core schema).
pub fn ensure_schema(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS ai__suggestions (
            photo_id        INTEGER NOT NULL,
            path            TEXT NOT NULL,
            state           TEXT NOT NULL DEFAULT 'pending',
            confidence      REAL,
            reason          TEXT,
            description     TEXT,
            synonyms        TEXT,
            source_photo_id INTEGER,
            created_at      INTEGER NOT NULL,
            PRIMARY KEY (photo_id, path)
        );",
    )?;
    // Add columns to pre-existing tables (ignore 'duplicate column' errors).
    let _ = conn.execute("ALTER TABLE ai__suggestions ADD COLUMN description TEXT", []);
    let _ = conn.execute("ALTER TABLE ai__suggestions ADD COLUMN synonyms TEXT", []);
    // H15c: provenance for burst-propagated suggestions (lazy migration).
    let _ = conn.execute("ALTER TABLE ai__suggestions ADD COLUMN source_photo_id INTEGER", []);
    Ok(())
}

fn parse_synonyms(json: Option<String>) -> Vec<String> {
    json.and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .unwrap_or_default()
}

pub fn load_pending(conn: &rusqlite::Connection, photo_id: i64) -> rusqlite::Result<Vec<Stored>> {
    let mut stmt = conn.prepare(
        "SELECT path, confidence, reason, description, synonyms, source_photo_id
         FROM ai__suggestions
         WHERE photo_id = ?1 AND state = 'pending' ORDER BY confidence DESC",
    )?;
    let rows = stmt.query_map([photo_id], |r| {
        Ok(Stored {
            path: r.get(0)?,
            confidence: r.get::<_, Option<f64>>(1)?.unwrap_or(0.0) as f32,
            reason: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            description: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
            synonyms: parse_synonyms(r.get::<_, Option<String>>(4)?),
            source_photo_id: r.get::<_, Option<i64>>(5)?,
        })
    })?;
    rows.collect()
}

/// The AI-proposed description + synonyms for one suggestion (for accept).
pub fn get_proposed(
    conn: &rusqlite::Connection,
    photo_id: i64,
    path: &str,
) -> rusqlite::Result<(String, Vec<String>)> {
    let row = conn
        .query_row(
            "SELECT description, synonyms FROM ai__suggestions WHERE photo_id = ?1 AND path = ?2",
            rusqlite::params![photo_id, path],
            |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                    parse_synonyms(r.get::<_, Option<String>>(1)?),
                ))
            },
        )
        .optional()?;
    Ok(row.unwrap_or_default())
}

pub fn rejected_paths(conn: &rusqlite::Connection, photo_id: i64) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn
        .prepare("SELECT path FROM ai__suggestions WHERE photo_id = ?1 AND state = 'rejected'")?;
    let rows = stmt.query_map([photo_id], |r| r.get::<_, String>(0))?;
    rows.collect()
}

/// Insert a pending suggestion from a **direct** per-photo run, but never resurrect one
/// already accepted/rejected. `source_photo_id` is `NULL` (this photo tagged itself).
pub fn upsert_pending(
    conn: &rusqlite::Connection,
    photo_id: i64,
    raw: &Raw,
    now: i64,
) -> rusqlite::Result<()> {
    let synonyms = serde_json::to_string(&raw.synonyms).unwrap_or_else(|_| "[]".into());
    conn.execute(
        "INSERT INTO ai__suggestions(photo_id, path, state, confidence, reason, description, synonyms, source_photo_id, created_at)
         VALUES(?1, ?2, 'pending', ?3, ?4, ?5, ?6, NULL, ?7)
         ON CONFLICT(photo_id, path) DO NOTHING",
        rusqlite::params![
            photo_id,
            raw.path,
            raw.confidence as f64,
            raw.reason,
            raw.description,
            synonyms,
            now
        ],
    )?;
    Ok(())
}

/// Insert a pending suggestion **propagated** from a burst representative (H15c),
/// stamping `source_photo_id` for provenance and (already-)reduced confidence. Like
/// [`upsert_pending`], it never resurrects an accepted/rejected row or overwrites an
/// existing direct suggestion for the same (photo, path) — those are more authoritative.
pub fn upsert_pending_propagated(
    conn: &rusqlite::Connection,
    photo_id: i64,
    raw: &Raw,
    source_photo_id: i64,
    now: i64,
) -> rusqlite::Result<()> {
    let synonyms = serde_json::to_string(&raw.synonyms).unwrap_or_else(|_| "[]".into());
    conn.execute(
        "INSERT INTO ai__suggestions(photo_id, path, state, confidence, reason, description, synonyms, source_photo_id, created_at)
         VALUES(?1, ?2, 'pending', ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(photo_id, path) DO NOTHING",
        rusqlite::params![
            photo_id,
            raw.path,
            raw.confidence as f64,
            raw.reason,
            raw.description,
            synonyms,
            source_photo_id,
            now
        ],
    )?;
    Ok(())
}

/// Confidence multiplier applied when a representative's suggestion is propagated to the
/// rest of its burst cluster — the propagated frame wasn't itself looked at by the model,
/// so its suggestions are slightly less certain than the representative's own.
pub const PROPAGATION_CONFIDENCE_FACTOR: f32 = 0.85;

/// Supersede any burst-**propagated** pending rows for `photo_id` before a direct
/// per-photo run stores its own suggestions (H15c). Only pending rows carrying a
/// `source_photo_id` are removed; the photo's accepted/rejected history and any direct
/// pending rows are untouched. Returns the number of rows removed.
pub fn clear_propagated_pending(
    conn: &rusqlite::Connection,
    photo_id: i64,
) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM ai__suggestions
         WHERE photo_id = ?1 AND state = 'pending' AND source_photo_id IS NOT NULL",
        rusqlite::params![photo_id],
    )
}

/// Persist one representative's `raw` suggestions across a burst cluster (H15c).
///
/// - `member_ids` — every photo in the cluster (includes `rep_id`).
/// - `rep_id`     — the representative that was actually dispatched to the provider.
/// - `raw`        — the provider's suggestions for the representative.
/// - `min_confidence` / `existing_only` — the same per-run filters `ai_suggest_tags`
///   applies (a suggestion is dropped for a member below the threshold, when it was
///   previously rejected for that member, or when it names a non-existent tag under
///   existing-only mode).
/// - `tag_exists` — resolves a path against the live taxonomy (authoritative).
///
/// The representative's rows are stored **direct** (`source_photo_id` NULL); every other
/// member's rows are stored **propagated** at reduced confidence and stamped with
/// `rep_id`. Before propagating to a non-representative member, its stale propagated
/// pending rows are cleared so a fresh cluster run replaces an older one (a member's
/// direct rows and accept/reject history are preserved). Returns the number of pending
/// rows stored.
pub fn propagate_cluster<Err, Exists>(
    conn: &rusqlite::Connection,
    member_ids: &[i64],
    rep_id: i64,
    raw: &[Raw],
    min_confidence: f32,
    existing_only: bool,
    now: i64,
    tag_exists: Exists,
) -> std::result::Result<usize, Err>
where
    Err: From<rusqlite::Error>,
    Exists: Fn(&str) -> std::result::Result<bool, Err>,
{
    let mut stored = 0usize;
    for &member_id in member_ids {
        let is_rep = member_id == rep_id;
        if !is_rep {
            clear_propagated_pending(conn, member_id)?;
        }
        let member_rejected = rejected_paths(conn, member_id)?;
        for r in raw {
            let conf = if is_rep {
                r.confidence
            } else {
                r.confidence * PROPAGATION_CONFIDENCE_FACTOR
            };
            if conf < min_confidence || member_rejected.contains(&r.path) {
                continue;
            }
            if existing_only && !tag_exists(&r.path)? {
                continue;
            }
            if is_rep {
                upsert_pending(conn, member_id, r, now)?;
            } else {
                let propagated = Raw {
                    path: r.path.clone(),
                    confidence: conf,
                    reason: r.reason.clone(),
                    description: r.description.clone(),
                    synonyms: r.synonyms.clone(),
                };
                upsert_pending_propagated(conn, member_id, &propagated, rep_id, now)?;
            }
            stored += 1;
        }
    }
    Ok(stored)
}

/// Set a suggestion's state (accepted/rejected), inserting the row if absent.
pub fn set_state(
    conn: &rusqlite::Connection,
    photo_id: i64,
    path: &str,
    state: &str,
    now: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO ai__suggestions(photo_id, path, state, created_at)
         VALUES(?1, ?2, ?3, ?4)
         ON CONFLICT(photo_id, path) DO UPDATE SET state = excluded.state",
        rusqlite::params![photo_id, path, state, now],
    )?;
    Ok(())
}

/// Render the taxonomy as a compact list for the prompt: "- Full/Path: description".
/// When `include_private` is false (a cloud provider), tags marked private (people's
/// names etc.) are omitted so they never leave the machine. See `Catalog::tag_private`.
pub fn taxonomy_text(c: &Catalog, include_private: bool) -> crate::catalog::Result<String> {
    let mut s = String::new();
    for t in c.list_tags_with_counts()? {
        if !include_private && t.tag.private {
            continue;
        }
        s.push_str("- ");
        s.push_str(&t.tag.full_path);
        if !t.tag.description.is_empty() {
            s.push_str(": ");
            s.push_str(&t.tag.description);
        }
        s.push('\n');
    }
    Ok(s)
}

/// The default prompt template. Placeholders are filled by [`build_prompt`]:
/// `{taxonomy}` (the tag list), `{new_tags}` (whether to propose new tags), `{rejected}`
/// (tags the user already rejected). Editable by the user in AI settings → Advanced.
pub const DEFAULT_PROMPT: &str = "You are tagging a photograph. Available tags (Parent/Child paths, with descriptions):\n\n{taxonomy}\nLook at the image and do BOTH: (1) pick the tags that genuinely apply, and (2) classify the photo's genre and style.\n- SUBJECT — the most specific applicable concept(s); do NOT also list parent tags (ancestors are implied, e.g. \"Transportation/Boats/Ship\" already counts as Boats and Transportation).\n- GENRE — the kind of photography (e.g. Landscape, Portrait, Street, Wildlife, Macro, Architecture, Still life, Documentary, Abstract, Astrophotography). Tag it under \"Genre/<name>\".\n- STYLE — the aesthetic treatment (e.g. Minimalist, Moody, High-key, Low-key, Black & white, Long exposure, Golden hour, Bokeh, Candid, Symmetrical, Vintage). Tag it under \"Style/<name>\".\nUse an existing Genre/Style tag when one fits; otherwise add it under that branch if new tags are allowed.\nTaxonomy rules:\n- Keep facets separate: what something IS, WHERE it is, WHEN/occasion, WHO made it, and Genre and Style are all DIFFERENT branches — never combine facets in one path.\n- Nesting is an is-a relationship: a tag must be a KIND OF its parent. {new_tags}{rejected}\nRespond with ONLY this JSON shape, no prose:\n{\"tags\":[{\"path\":\"Full/Path\",\"confidence\":0.0,\"reason\":\"short\",\"description\":\"only for new tags\",\"synonyms\":[\"optional\"]}]}";

/// Build the final prompt by filling `template`'s placeholders. An empty `template`
/// falls back to [`DEFAULT_PROMPT`]. `{new_tags}`/`{rejected}` are computed from the
/// current run (existing-only mode, this photo's rejected list).
fn build_prompt(
    template: &str,
    taxonomy: &str,
    rejected: &[String],
    existing_only: bool,
    question: Option<&str>,
) -> String {
    let new_clause = if existing_only {
        "Do NOT invent new tags; use ONLY tags from the list above."
    } else {
        "You may also propose NEW tags as full paths. Nest a new tag under an existing \
branch ONLY when it is genuinely a narrower KIND of that branch (an is-a relationship): \
e.g. \"Transportation/Watercraft/Kayak\" is valid because a kayak IS a kind of \
watercraft. If no existing branch is a true broader category, start a NEW branch \
instead — do NOT force-fit a tag under a loosely-related branch. For example, a sunset \
is NOT a kind of public place, so use a branch like \"Nature/Sunset\", never \
\"Public place/Sunset\". For each NEW tag, include a short \"description\" and optional \
\"synonyms\"."
    };
    let rejected_clause = if rejected.is_empty() {
        String::new()
    } else {
        format!(
            " The user already REJECTED these tags for this photo — do not suggest them \
again: {}.",
            rejected.join(", ")
        )
    };
    let tmpl = if template.trim().is_empty() { DEFAULT_PROMPT } else { template };
    let base = tmpl
        .replace("{taxonomy}", taxonomy)
        .replace("{new_tags}", new_clause)
        .replace("{rejected}", &rejected_clause);
    // A follow-up question takes priority — drill into the user's ask (e.g. "what kind of
    // boat?" → a specific Transportation/Boats/<Type>), then add any other clear tags.
    match question {
        Some(q) if !q.trim().is_empty() => format!(
            "The user is refining the tags and asks a FOLLOW-UP QUESTION: \"{}\". Answer it \
with the most specific applicable tag(s) — e.g. if asked what kind of boat, give the \
specific type as \"Transportation/Boats/<Type>\". Then also include any other clearly \
applicable tags.\n\n{base}",
            q.trim()
        ),
        _ => base,
    }
}

/// JSON schema for Ollama structured output — forces valid JSON even from small
/// local models. Mirrors the suggestion contract.
fn ollama_format_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "tags": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "confidence": { "type": "number" },
                        "reason": { "type": "string" },
                        "description": { "type": "string" },
                        "synonyms": { "type": "array", "items": { "type": "string" } }
                    },
                    "required": ["path", "confidence"]
                }
            }
        },
        "required": ["tags"]
    })
}

/// Run a suggestion against the configured provider. `question` (Some) refines toward a
/// user follow-up (e.g. "what kind of boat?").
pub async fn suggest(
    cfg: &Config,
    image_b64: &str,
    taxonomy: &str,
    rejected: &[String],
    question: Option<&str>,
) -> Result<Vec<Raw>, String> {
    let prompt = build_prompt(&cfg.prompt_template, taxonomy, rejected, cfg.existing_only, question);
    match cfg.provider.as_str() {
        "claude" => call_claude(cfg, &prompt, image_b64).await,
        "openai" => {
            let content = call_openai(cfg, &prompt, image_b64).await?;
            Ok(parse_suggestions(&content))
        }
        "gemini" => {
            let content = call_gemini(cfg, &prompt, image_b64).await?;
            Ok(parse_suggestions(&content))
        }
        _ => {
            let content = call_ollama(cfg, &prompt, image_b64).await?;
            Ok(parse_suggestions(&content))
        }
    }
}

/// List the models installed on the Ollama server (its `/api/tags`), for the model
/// picker. Returns an empty list if the server is unreachable.
pub async fn list_ollama_models(url: &str) -> Result<Vec<String>, String> {
    let resp = reqwest::Client::new()
        .get(format!("{}/api/tags", url.trim_end_matches('/')))
        .send()
        .await
        .map_err(|e| format!("Ollama request failed: {e}"))?;
    let value: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    Ok(value
        .get("models")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("name").and_then(|n| n.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default())
}

async fn call_ollama(cfg: &Config, prompt: &str, image_b64: &str) -> Result<String, String> {
    let body = serde_json::json!({
        "model": cfg.ollama_model,
        "stream": false,
        "format": ollama_format_schema(),
        "messages": [{ "role": "user", "content": prompt, "images": [image_b64] }],
    });
    let resp = reqwest::Client::new()
        .post(format!("{}/api/chat", cfg.ollama_url.trim_end_matches('/')))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Ollama request failed: {e}"))?;
    let value: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    value
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .map(str::to_string)
        .ok_or_else(|| "Ollama returned no content".to_string())
}

/// The JSON input_schema for the `suggest_tags` tool passed to the Claude Messages API.
/// Mirrors the tag-suggestion contract from `docs/ai-tagging.md` exactly so Claude is
/// forced to emit valid, parseable JSON via tool_use.
pub fn claude_tool_definition() -> serde_json::Value {
    serde_json::json!({
        "name": "suggest_tags",
        "description": "Return photo-tag suggestions as structured JSON.",
        "input_schema": {
            "type": "object",
            "properties": {
                "tags": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path":        { "type": "string" },
                            "confidence":  { "type": "number" },
                            "reason":      { "type": "string" },
                            "description": { "type": "string" },
                            "synonyms":    { "type": "array", "items": { "type": "string" } }
                        },
                        "required": ["path", "confidence"]
                    }
                }
            },
            "required": ["tags"]
        }
    })
}

/// Build the Claude Messages API request body (with tool-use enforced).
/// Extracted so unit tests can inspect it without making network calls.
pub fn build_claude_request(cfg: &Config, prompt: &str, image_b64: &str) -> serde_json::Value {
    serde_json::json!({
        "model": cfg.cloud_model,
        "max_tokens": 1024,
        "tools": [claude_tool_definition()],
        "tool_choice": { "type": "tool", "name": "suggest_tags" },
        "messages": [{
            "role": "user",
            "content": [
                { "type": "image", "source": {
                    "type": "base64", "media_type": "image/jpeg", "data": image_b64 } },
                { "type": "text", "text": prompt },
            ],
        }],
    })
}

/// Parse the structured `tool_use` block out of a Claude Messages API response.
/// Returns the `input` object (which should contain the `tags` array) as a JSON
/// string, or `None` if the response cannot be parsed as a valid tool-use result.
pub fn extract_claude_tool_input(response: &serde_json::Value) -> Option<String> {
    // The Messages API returns: { "content": [ { "type": "tool_use", "input": {...} } ] }
    response
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|arr| {
            arr.iter().find(|block| {
                block.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                    && block.get("name").and_then(|n| n.as_str()) == Some("suggest_tags")
            })
        })
        .and_then(|block| block.get("input"))
        .map(|input| input.to_string())
}

/// Call the Claude Messages API using the `suggest_tags` tool (forced tool_choice).
/// On a missing or unparseable tool-use block, retries once, then returns an error.
async fn call_claude(cfg: &Config, prompt: &str, image_b64: &str) -> Result<Vec<Raw>, String> {
    if cfg.cloud_key.is_empty() {
        return Err("No cloud API key configured".into());
    }

    let do_request = |body: serde_json::Value| {
        let key = cfg.cloud_key.clone();
        async move {
            let resp = reqwest::Client::new()
                .post("https://api.anthropic.com/v1/messages")
                .header("x-api-key", &key)
                .header("anthropic-version", "2023-06-01")
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("Claude request failed: {e}"))?;
            let value: serde_json::Value =
                resp.json().await.map_err(|e| format!("Claude response parse failed: {e}"))?;
            Ok::<serde_json::Value, String>(value)
        }
    };

    let body = build_claude_request(cfg, prompt, image_b64);

    // First attempt.
    let response = do_request(body.clone()).await?;
    if let Some(json_str) = extract_claude_tool_input(&response) {
        // A valid tool_use block — even one with an empty `tags` array — is a
        // well-formed response, not a parse failure. Return immediately; do NOT retry.
        return Ok(parse_suggestions(&json_str));
    }

    // Retry once only when the response contained no recognisable tool_use block
    // (e.g. the model returned plain text, a different tool, or an API error body).
    let response2 = do_request(body).await?;
    match extract_claude_tool_input(&response2) {
        Some(json_str) => Ok(parse_suggestions(&json_str)),
        None => Err(format!(
            "Claude did not return a valid suggest_tags tool call after retry. \
Response: {response2}"
        )),
    }
}

/// OpenAI vision via the Chat Completions API (the image rides as a base64 data URL).
async fn call_openai(cfg: &Config, prompt: &str, image_b64: &str) -> Result<String, String> {
    if cfg.openai_key.is_empty() {
        return Err("No OpenAI API key configured".into());
    }
    let body = serde_json::json!({
        "model": cfg.openai_model,
        "max_tokens": 1024,
        "response_format": { "type": "json_object" },
        "messages": [{
            "role": "user",
            "content": [
                { "type": "text", "text": prompt },
                { "type": "image_url", "image_url": {
                    "url": format!("data:image/jpeg;base64,{image_b64}") } },
            ],
        }],
    });
    let resp = reqwest::Client::new()
        .post("https://api.openai.com/v1/chat/completions")
        .header("Authorization", format!("Bearer {}", cfg.openai_key))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("OpenAI request failed: {e}"))?;
    let value: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    value
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|m| m.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("OpenAI returned no text: {value}"))
}

/// Google Gemini vision via generateContent (the image rides as inline base64 data).
async fn call_gemini(cfg: &Config, prompt: &str, image_b64: &str) -> Result<String, String> {
    if cfg.gemini_key.is_empty() {
        return Err("No Gemini API key configured".into());
    }
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent",
        cfg.gemini_model
    );
    let body = serde_json::json!({
        "contents": [{
            "parts": [
                { "text": prompt },
                { "inline_data": { "mime_type": "image/jpeg", "data": image_b64 } },
            ],
        }],
        "generationConfig": { "response_mime_type": "application/json" },
    });
    let resp = reqwest::Client::new()
        .post(&url)
        .header("x-goog-api-key", &cfg.gemini_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Gemini request failed: {e}"))?;
    let value: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    value
        .get("candidates")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|c| c.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array())
        .and_then(|a| a.first())
        .and_then(|p| p.get("text"))
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("Gemini returned no text: {value}"))
}

/// Parse the model's JSON (tolerant of surrounding prose) into raw suggestions.
/// Primary shape is `{"tags":[…]}`; falls back to the older `{"existing":[],"new":[]}`.
fn parse_suggestions(content: &str) -> Vec<Raw> {
    let json = extract_json(content);
    let value: serde_json::Value = match serde_json::from_str(&json) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    if let Some(arr) = value.get("tags").and_then(|v| v.as_array()) {
        out.extend(arr.iter().filter_map(parse_item));
    } else {
        for key in ["existing", "new"] {
            if let Some(arr) = value.get(key).and_then(|v| v.as_array()) {
                out.extend(arr.iter().filter_map(parse_item));
            }
        }
    }
    out
}

fn parse_item(item: &serde_json::Value) -> Option<Raw> {
    let path = item.get("path").and_then(|p| p.as_str())?.trim();
    if path.is_empty() {
        return None;
    }
    let synonyms = item
        .get("synonyms")
        .and_then(|s| s.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    Some(Raw {
        path: path.to_string(),
        confidence: item.get("confidence").and_then(|c| c.as_f64()).unwrap_or(0.5) as f32,
        reason: item.get("reason").and_then(|r| r.as_str()).unwrap_or("").to_string(),
        description: item.get("description").and_then(|d| d.as_str()).unwrap_or("").to_string(),
        synonyms,
    })
}

/// Extract the outermost JSON object from text that may have stray prose.
fn extract_json(content: &str) -> String {
    match (content.find('{'), content.rfind('}')) {
        (Some(a), Some(b)) if b > a => content[a..=b].to_string(),
        _ => content.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── H15c — grouped dispatch + propagation ────────────────────────────────

    /// A cluster as produced by the H15b engine: member ids + the representative index.
    struct FakeCluster {
        member_ids: Vec<i64>,
        rep_idx: usize,
    }
    impl FakeCluster {
        fn rep_id(&self) -> i64 {
            self.member_ids[self.rep_idx]
        }
    }

    fn raw(path: &str, conf: f32) -> Raw {
        Raw {
            path: path.into(),
            confidence: conf,
            reason: "because".into(),
            description: String::new(),
            synonyms: vec![],
        }
    }

    /// Run the grouped-dispatch flow against an in-memory table using a fake provider
    /// closure. Mirrors `commands::ai_suggest_tags_grouped`: dispatch ONLY each cluster's
    /// representative, propagate to the rest. Returns the number of provider calls.
    fn run_grouped<P>(
        conn: &rusqlite::Connection,
        clusters: &[FakeCluster],
        mut provider: P,
    ) -> usize
    where
        P: FnMut(i64) -> Vec<Raw>,
    {
        let mut calls = 0usize;
        for cluster in clusters {
            let rep_id = cluster.rep_id();
            let raws = provider(rep_id); // the fake "network call" — once per cluster
            calls += 1;
            propagate_cluster::<rusqlite::Error, _>(
                conn,
                &cluster.member_ids,
                rep_id,
                &raws,
                0.0,   // min_confidence
                false, // existing_only
                1000,
                |_path| Ok(true),
            )
            .unwrap();
        }
        calls
    }

    #[test]
    fn grouped_dispatch_three_clusters_one_call_each_and_propagates_with_provenance() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();

        // Three clusters; representatives are 1, 11, 21. Each cluster's other members must
        // receive the representative's suggestions as propagated pending rows.
        let clusters = vec![
            FakeCluster { member_ids: vec![1, 2, 3], rep_idx: 0 },
            FakeCluster { member_ids: vec![11, 12], rep_idx: 0 },
            FakeCluster { member_ids: vec![21, 22, 23, 24], rep_idx: 1 }, // rep = 22
        ];

        // The fake provider returns a distinct suggestion per representative, so we can
        // verify each member got ITS cluster's tags (not another's).
        let calls = run_grouped(&conn, &clusters, |rep_id| match rep_id {
            1 => vec![raw("Animals/Birds/Gull", 0.9)],
            11 => vec![raw("Nature/Sunset", 0.8)],
            22 => vec![raw("Transportation/Boats/Ship", 0.6)],
            other => panic!("provider dispatched to a non-representative photo {other}"),
        });

        // Exactly one provider call per cluster.
        assert_eq!(calls, 3, "one dispatch per cluster (representatives only)");

        // Representative rows are direct (source_photo_id NULL) at full confidence.
        let rep1 = load_pending(&conn, 1).unwrap();
        assert_eq!(rep1.len(), 1);
        assert_eq!(rep1[0].path, "Animals/Birds/Gull");
        assert_eq!(rep1[0].source_photo_id, None, "representative row is direct");
        assert!((rep1[0].confidence - 0.9).abs() < 1e-4);

        // Cluster-1 members 2 and 3 are propagated from rep 1 at reduced confidence.
        for member in [2, 3] {
            let rows = load_pending(&conn, member).unwrap();
            assert_eq!(rows.len(), 1, "member {member} has the propagated suggestion");
            assert_eq!(rows[0].path, "Animals/Birds/Gull");
            assert_eq!(
                rows[0].source_photo_id,
                Some(1),
                "member {member} provenance = representative 1"
            );
            let expected = 0.9 * PROPAGATION_CONFIDENCE_FACTOR;
            assert!(
                (rows[0].confidence - expected).abs() < 1e-4,
                "propagated confidence is reduced"
            );
        }

        // Cluster-2 propagation (rep 11 → member 12).
        let m12 = load_pending(&conn, 12).unwrap();
        assert_eq!(m12[0].path, "Nature/Sunset");
        assert_eq!(m12[0].source_photo_id, Some(11));

        // Cluster-3: rep is 22 (rep_idx 1). Members 21, 23, 24 are propagated from 22.
        let rep22 = load_pending(&conn, 22).unwrap();
        assert_eq!(rep22[0].source_photo_id, None, "22 is the representative");
        for member in [21, 23, 24] {
            let rows = load_pending(&conn, member).unwrap();
            assert_eq!(rows[0].path, "Transportation/Boats/Ship");
            assert_eq!(rows[0].source_photo_id, Some(22));
        }
    }

    #[test]
    fn direct_run_supersedes_propagated_rows() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();

        // Cluster [1,2,3], rep 1 → members 2,3 get propagated suggestions from photo 1.
        let clusters = vec![FakeCluster { member_ids: vec![1, 2, 3], rep_idx: 0 }];
        run_grouped(&conn, &clusters, |_| vec![raw("Animals/Birds/Gull", 0.9)]);

        let before = load_pending(&conn, 2).unwrap();
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].source_photo_id, Some(1), "starts propagated");

        // A DIRECT per-photo run on member 2: clear its propagated rows, then store direct
        // suggestions (exactly what `ai_suggest_tags` does before upserting).
        clear_propagated_pending(&conn, 2).unwrap();
        upsert_pending(&conn, 2, &raw("Nature/Sunset", 0.7), 2000).unwrap();

        let after = load_pending(&conn, 2).unwrap();
        // The propagated "Gull" is gone; only the direct "Sunset" remains, with NULL source.
        assert_eq!(after.len(), 1, "propagated row superseded by the direct run");
        assert_eq!(after[0].path, "Nature/Sunset");
        assert_eq!(after[0].source_photo_id, None, "direct suggestion has no provenance");

        // Other members are untouched by member 2's direct run.
        let m3 = load_pending(&conn, 3).unwrap();
        assert_eq!(m3[0].source_photo_id, Some(1), "member 3 still propagated");
    }

    #[test]
    fn direct_run_does_not_clobber_accepted_or_rejected_history() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();

        // Member 2 already rejected a tag; propagation must not resurrect it, and a later
        // clear_propagated_pending must not touch the rejected row.
        set_state(&conn, 2, "Animals/Birds/Gull", "rejected", 500).unwrap();

        let clusters = vec![FakeCluster { member_ids: vec![1, 2], rep_idx: 0 }];
        run_grouped(&conn, &clusters, |_| vec![raw("Animals/Birds/Gull", 0.9)]);

        // No pending row for member 2 — the rejection filtered the propagation out.
        let pending = load_pending(&conn, 2).unwrap();
        assert!(pending.is_empty(), "a rejected tag is not re-proposed by propagation");

        // The rejection survives a supersede.
        clear_propagated_pending(&conn, 2).unwrap();
        let still_rejected = rejected_paths(&conn, 2).unwrap();
        assert_eq!(still_rejected, vec!["Animals/Birds/Gull".to_string()]);
    }

    #[test]
    fn parses_model_json() {
        let content = r#"Here you go: {"existing":[{"path":"Animals/Birds","confidence":0.9,"reason":"a gull"}],"new":[{"path":"Weather/Overcast","confidence":0.6,"reason":"grey sky"}]}"#;
        let raw = parse_suggestions(content);
        assert_eq!(raw.len(), 2);
        assert_eq!(raw[0].path, "Animals/Birds");
        assert_eq!(raw[1].path, "Weather/Overcast");
    }

    #[test]
    fn tolerates_garbage() {
        assert!(parse_suggestions("not json").is_empty());
    }

    #[test]
    fn new_tag_prompt_requires_is_a_nesting() {
        let p = build_prompt("", "- Public place\n", &[], false, None);
        // Nesting is gated on a genuine is-a relationship, not a loose "fit".
        assert!(p.contains("is-a"));
        assert!(p.contains("KIND"));
        // The concrete counterexample steers the model away from the bad placement.
        assert!(p.contains("Public place/Sunset"));
        assert!(p.contains("Nature/Sunset"));
        // The old force-fit phrasing is gone.
        assert!(!p.contains("MUST be a full path that extends"));
    }

    #[test]
    fn existing_only_prompt_forbids_new_tags() {
        let p = build_prompt("", "- Public place\n", &[], true, None);
        assert!(p.contains("Do NOT invent new tags"));
        // No *new-tag* guidance when new tags are disallowed (the general is-a/leaf rules
        // still apply to picking existing tags).
        assert!(!p.contains("propose NEW tags"));
        assert!(!p.contains("start a NEW branch"));
    }

    // ------ Claude structured-output (tool-use) tests ------

    fn dummy_cfg() -> Config {
        Config {
            provider: "claude".into(),
            ollama_url: String::new(),
            ollama_model: String::new(),
            cloud_model: "claude-sonnet-4-6".into(),
            cloud_key: "test-key".into(),
            openai_model: String::new(),
            openai_key: String::new(),
            gemini_model: String::new(),
            gemini_key: String::new(),
            existing_only: false,
            min_confidence: 0.0,
            prompt_template: String::new(),
        }
    }

    /// The request body must declare exactly one tool (`suggest_tags`) and force it.
    #[test]
    fn claude_request_body_has_tool_and_forced_tool_choice() {
        let cfg = dummy_cfg();
        let body = build_claude_request(&cfg, "describe this photo", "b64data==");

        // tools array must contain exactly the suggest_tags tool
        let tools = body.get("tools").and_then(|t| t.as_array()).expect("tools array");
        assert_eq!(tools.len(), 1);
        assert_eq!(
            tools[0].get("name").and_then(|n| n.as_str()),
            Some("suggest_tags")
        );

        // tool_choice must be forced to that specific tool
        let tc = body.get("tool_choice").expect("tool_choice");
        assert_eq!(tc.get("type").and_then(|t| t.as_str()), Some("tool"));
        assert_eq!(tc.get("name").and_then(|n| n.as_str()), Some("suggest_tags"));
    }

    /// The input_schema in the tool definition must include `tags` as a required array.
    #[test]
    fn claude_tool_definition_schema_requires_tags_array() {
        let def = claude_tool_definition();
        let schema = def.get("input_schema").expect("input_schema");
        let props = schema.get("properties").expect("properties");
        assert!(props.get("tags").is_some(), "schema must have a 'tags' property");
        let required = schema
            .get("required")
            .and_then(|r| r.as_array())
            .expect("required array");
        assert!(
            required.iter().any(|v| v.as_str() == Some("tags")),
            "'tags' must be in required"
        );
        // Each tag item must require 'path' and 'confidence'.
        let items = props["tags"].get("items").expect("items");
        let item_req = items
            .get("required")
            .and_then(|r| r.as_array())
            .expect("item required array");
        assert!(item_req.iter().any(|v| v.as_str() == Some("path")));
        assert!(item_req.iter().any(|v| v.as_str() == Some("confidence")));
    }

    /// A well-formed Claude tool-use response is extracted and parsed correctly.
    #[test]
    fn extract_and_parse_valid_tool_use_response() {
        let response = serde_json::json!({
            "content": [{
                "type": "tool_use",
                "name": "suggest_tags",
                "input": {
                    "tags": [
                        { "path": "Animals/Birds/Gull", "confidence": 0.92,
                          "reason": "a gull in flight" },
                        { "path": "Weather/Overcast",   "confidence": 0.7,
                          "reason": "grey sky" }
                    ]
                }
            }]
        });

        let json_str =
            extract_claude_tool_input(&response).expect("should extract tool input");
        let raws = parse_suggestions(&json_str);
        assert_eq!(raws.len(), 2);
        assert_eq!(raws[0].path, "Animals/Birds/Gull");
        assert!((raws[0].confidence - 0.92).abs() < 1e-3);
        assert_eq!(raws[1].path, "Weather/Overcast");
    }

    /// A response that contains a text block instead of a tool_use block yields `None`.
    #[test]
    fn extract_returns_none_for_text_only_response() {
        let response = serde_json::json!({
            "content": [{ "type": "text", "text": "here are some tags: Birds" }]
        });
        assert!(extract_claude_tool_input(&response).is_none());
    }

    /// A response with the wrong tool name is not mistaken for our tool.
    #[test]
    fn extract_returns_none_for_wrong_tool_name() {
        let response = serde_json::json!({
            "content": [{
                "type": "tool_use",
                "name": "some_other_tool",
                "input": { "tags": [] }
            }]
        });
        assert!(extract_claude_tool_input(&response).is_none());
    }

    /// An empty `tags` array is valid (model found nothing applicable).
    #[test]
    fn extract_and_parse_empty_tags_array() {
        let response = serde_json::json!({
            "content": [{
                "type": "tool_use",
                "name": "suggest_tags",
                "input": { "tags": [] }
            }]
        });
        let json_str = extract_claude_tool_input(&response).expect("should extract");
        let raws = parse_suggestions(&json_str);
        assert!(raws.is_empty());
    }

    /// Retry must NOT fire when extract_claude_tool_input returns Some(_), even if
    /// parse_suggestions produces an empty vec (e.g. every item has an empty path).
    /// The contract: retry only on a missing/malformed tool-use block (None), never
    /// on a structurally valid block whose tags all fail item-level validation.
    #[test]
    fn valid_tool_use_with_all_invalid_items_does_not_indicate_malformed_response() {
        // All items have empty paths — parse_suggestions drops them → empty vec.
        // But extract_claude_tool_input must still return Some, proving the response
        // was structurally valid and call_claude should return Ok([]) not retry.
        let response = serde_json::json!({
            "content": [{
                "type": "tool_use",
                "name": "suggest_tags",
                "input": {
                    "tags": [
                        { "path": "", "confidence": 0.9, "reason": "empty path" }
                    ]
                }
            }]
        });
        // extract returns Some → the block is present; call_claude would return Ok([]).
        let json_str = extract_claude_tool_input(&response)
            .expect("structurally valid tool_use must yield Some");
        let raws = parse_suggestions(&json_str);
        // Empty because the item's path is blank, but that is NOT a malformed response.
        assert!(raws.is_empty());
    }

    /// retry fires only when the response has no recognisable tool_use block (None).
    #[test]
    fn missing_tool_use_block_indicates_retry_needed() {
        // An API-level error body has no `content` array — extract returns None.
        let api_error = serde_json::json!({
            "type": "error",
            "error": { "type": "authentication_error", "message": "invalid api key" }
        });
        assert!(extract_claude_tool_input(&api_error).is_none());

        // A text-only response also yields None.
        let text_response = serde_json::json!({
            "content": [{ "type": "text", "text": "{\"tags\":[]}" }]
        });
        assert!(extract_claude_tool_input(&text_response).is_none());
    }

    /// The request body uses the configured model name, not a hard-coded default.
    #[test]
    fn claude_request_uses_configured_model() {
        let mut cfg = dummy_cfg();
        cfg.cloud_model = "claude-opus-4-5".into();
        let body = build_claude_request(&cfg, "prompt", "img");
        assert_eq!(
            body.get("model").and_then(|m| m.as_str()),
            Some("claude-opus-4-5")
        );
    }
}
