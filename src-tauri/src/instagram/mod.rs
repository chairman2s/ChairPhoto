//! Post an exported image to Instagram by driving a real Chrome over the DevTools
//! Protocol (the `instagram` Cargo feature). There is no public Instagram "post a local
//! file" web hook, so we automate the web composer: open the create dialog, set the file
//! input (CDP — the one thing page JS can't do), type the caption, and optionally click
//! Share.
//!
//! Design choices that keep this as robust as a brittle target allows:
//! - **Detached Chrome + a persistent profile**: we *launch* Chrome ourselves with a
//!   dedicated `--user-data-dir` and `--remote-debugging-port`, then *connect* over CDP.
//!   Connecting (vs. chromiumoxide launching) means the window survives after we're done,
//!   so login persists and a supervised post stays on screen for the final click.
//! - **Text/aria selectors**: Instagram's CSS class names are obfuscated and rotate, so we
//!   click by visible text ("Next", "Share") and ARIA labels ("New post", "Write a
//!   caption…") via injected JS, which is far stabler.
//! - **Supervised by default**: with `publish = false` we compose everything and stop
//!   before Share; the caller surfaces "review and click Share".
//!
//! This is inherently fragile against Instagram UI changes and may need selector tuning.

use chromiumoxide::cdp::browser_protocol::dom::SetFileInputFilesParams;
use chromiumoxide::Browser;
use futures_util::StreamExt;
use std::path::Path;
use std::time::Duration;

/// What happened to the post attempt — surfaced to the UI.
#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PostOutcome {
    /// Composed and shared; confirmation seen.
    Posted,
    /// Composed (image + caption) but stopped before Share for the user to review.
    AwaitingReview,
    /// Not logged in — the Chrome window is open at the login page; log in and retry.
    NeedsLogin,
}

/// Post `image` with `caption`. `profile_dir` is a persistent Chrome profile (keeps the
/// Instagram login); `chrome_bin` is the Chrome/Chromium executable; `publish` clicks the
/// final Share when true, otherwise stops before it.
pub async fn post(
    image: &Path,
    caption: &str,
    profile_dir: &Path,
    chrome_bin: &str,
    publish: bool,
) -> Result<PostOutcome, String> {
    let image = image
        .to_str()
        .ok_or_else(|| "image path is not valid UTF-8".to_string())?
        .to_string();

    let ws = ensure_chrome(chrome_bin, profile_dir, DEBUG_PORT).await?;
    let (browser, mut handler) = Browser::connect(&ws)
        .await
        .map_err(|e| format!("couldn't connect to Chrome: {e}"))?;
    // The handler future drives the CDP connection; it must be polled for the session to
    // work. It ends when the browser/connection drops.
    let pump = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let result = run_flow(&browser, &image, caption, publish).await;

    // Disconnect without killing Chrome (we connected, we don't own the process), so a
    // supervised post stays on screen.
    drop(browser);
    pump.abort();
    result
}

const DEBUG_PORT: u16 = 9333;

/// Ensure a Chrome with our profile is running with the CDP endpoint open, and return its
/// WebSocket URL. Reuses an already-running instance (the profile is a singleton, so we
/// can't launch a second one against it).
async fn ensure_chrome(chrome_bin: &str, profile_dir: &Path, port: u16) -> Result<String, String> {
    if let Some(ws) = ws_endpoint(port).await {
        return Ok(ws);
    }
    std::fs::create_dir_all(profile_dir).map_err(|e| e.to_string())?;
    std::process::Command::new(chrome_bin)
        .arg(format!("--remote-debugging-port={port}"))
        .arg(format!("--user-data-dir={}", profile_dir.display()))
        // Reduce Instagram's automation detection (proven in the photo-insta poster).
        .arg("--disable-blink-features=AutomationControlled")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("https://www.instagram.com/")
        .spawn()
        .map_err(|e| format!("couldn't launch Chrome ({chrome_bin}): {e}"))?;

    // Poll the DevTools endpoint until Chrome is ready (~10s).
    for _ in 0..40 {
        if let Some(ws) = ws_endpoint(port).await {
            return Ok(ws);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Err("Chrome did not open its debugging endpoint in time".into())
}

/// Fetch the browser-level WebSocket debugger URL from Chrome's HTTP endpoint, if up.
async fn ws_endpoint(port: u16) -> Option<String> {
    let url = format!("http://127.0.0.1:{port}/json/version");
    let resp = reqwest::get(&url).await.ok()?;
    let json: serde_json::Value = resp.json().await.ok()?;
    json.get("webSocketDebuggerUrl")?.as_str().map(String::from)
}

/// The composer flow on an existing connection.
async fn run_flow(
    browser: &Browser,
    image: &str,
    caption: &str,
    publish: bool,
) -> Result<PostOutcome, String> {
    let page = browser
        .new_page("https://www.instagram.com/")
        .await
        .map_err(|e| format!("couldn't open Instagram: {e}"))?;
    page.wait_for_navigation()
        .await
        .map_err(|e| format!("Instagram didn't load: {e}"))?;

    // Logged in? The logged-out page shows a username field.
    if eval_bool(&page, r#"!!document.querySelector('input[name="username"]')"#).await {
        return Ok(PostOutcome::NeedsLogin);
    }

    // Open the create dialog (the "New post" entry in the left nav). Some sessions then
    // show a small "Post / Reel" menu — click Post if it appears.
    if !click_create(&page).await {
        return Err("couldn't find Instagram's New-post button (UI may have changed)".into());
    }
    let _ = click_text(&page, "Post").await; // best-effort; absent in the direct flow

    // The composer's hidden file input. Set it via CDP (page JS can't assign .files).
    // Use the *last* file input — Instagram has more than one and the composer's is last
    // (proven in the photo-insta poster).
    let input = wait_for_last(&page, r#"input[type="file"]"#, 20)
        .await
        .ok_or("the upload field never appeared")?;
    // No Element helper for this in chromiumoxide 0.9 — issue the CDP command directly
    // (the file input is hidden, so a normal click won't help; this is the reliable path).
    let params = SetFileInputFilesParams {
        files: vec![image.to_string()],
        node_id: None,
        backend_node_id: Some(input.backend_node_id),
        object_id: None,
    };
    page.execute(params)
        .await
        .map_err(|e| format!("couldn't attach the image: {e}"))?;
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // On the crop screen Instagram applies a default crop/zoom. Pick "Original" so the
    // full (already IG-sized) frame is kept. Best-effort — skip if the UI differs.
    select_original_crop(&page).await;

    // Crop screen → Next, filters screen → Next.
    wait_click_text(&page, "Next", 20).await?;
    wait_click_text(&page, "Next", 20).await?;

    // Caption screen: type into the caption box (real keystrokes, like the reference
    // poster — more reliably triggers Instagram's editor than a synthetic value set).
    if !wait_type_caption(&page, caption, 20).await {
        return Err("couldn't find the caption field".into());
    }

    if !publish {
        return Ok(PostOutcome::AwaitingReview);
    }

    wait_click_text(&page, "Share", 15).await?;
    // Confirmation copy varies ("Your post has been shared." / "Post shared").
    for _ in 0..60 {
        if eval_bool(
            &page,
            r#"/post has been shared|post shared/i.test(document.body.innerText)"#,
        )
        .await
        {
            return Ok(PostOutcome::Posted);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err("shared, but no confirmation seen — check Instagram".into())
}

/// Click the "New post" nav entry (its clickable ancestor), via its stable ARIA label.
async fn click_create(page: &chromiumoxide::Page) -> bool {
    let js = r#"(() => {
        const svg = document.querySelector('svg[aria-label="New post"]');
        if (svg) {
            let el = svg;
            while (el && el !== document.body) {
                if (el.tagName === 'A' || el.getAttribute('role') === 'link'
                    || el.getAttribute('role') === 'button') { el.click(); return true; }
                el = el.parentElement;
            }
            svg.parentElement && svg.parentElement.click();
            return true;
        }
        const byText = [...document.querySelectorAll('a,div[role="link"],span')]
            .find(e => e.textContent.trim() === 'Create');
        if (byText) { byText.click(); return true; }
        return false;
    })()"#;
    eval_bool(page, js).await
}

/// Click the first clickable element whose visible text equals `text` (case-insensitive).
async fn click_text(page: &chromiumoxide::Page, text: &str) -> bool {
    let want = serde_json::to_string(&text.to_lowercase()).unwrap();
    let js = format!(
        r#"(() => {{
            const want = {want};
            const els = [...document.querySelectorAll('button,div[role="button"],a,[role="button"]')];
            const el = els.find(e => (e.textContent || '').trim().toLowerCase() === want);
            if (el) {{ el.click(); return true; }}
            return false;
        }})()"#
    );
    eval_bool(page, &js).await
}

/// Poll until `click_text` succeeds or the timeout elapses.
async fn wait_click_text(page: &chromiumoxide::Page, text: &str, secs: u64) -> Result<(), String> {
    for _ in 0..(secs * 2) {
        if click_text(page, text).await {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err(format!("couldn't find the “{text}” button (Instagram UI may have changed)"))
}

/// Open Instagram's crop selector and pick "Original" so the image isn't auto-zoomed.
/// Entirely best-effort — silently continues if the UI differs or it's already original.
async fn select_original_crop(page: &chromiumoxide::Page) {
    let opened = eval_bool(
        page,
        r#"(() => {
            const svg = document.querySelector('svg[aria-label="Select crop"]')
                || document.querySelector('[aria-label="Select crop"]');
            if (!svg) return false;
            let el = svg;
            while (el && el !== document.body) {
                if (el.tagName === 'BUTTON' || el.getAttribute('role') === 'button') { el.click(); return true; }
                el = el.parentElement;
            }
            svg.parentElement && svg.parentElement.click();
            return true;
        })()"#,
    )
    .await;
    if opened {
        tokio::time::sleep(Duration::from_millis(600)).await;
        let _ = click_text(page, "Original").await;
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
}

/// Poll until the caption field exists, then type `caption` into it with real keystrokes,
/// falling back to a JS insert if typing fails.
async fn wait_type_caption(page: &chromiumoxide::Page, caption: &str, secs: u64) -> bool {
    let selector =
        r#"div[aria-label^="Write a caption"],textarea[aria-label^="Write a caption"],div[role="textbox"]"#;
    for _ in 0..(secs * 2) {
        if let Ok(el) = page.find_element(selector).await {
            let _ = el.click().await;
            if el.type_str(caption).await.is_ok() {
                return true;
            }
            if set_caption_js(page, caption).await {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    false
}

/// Fallback caption insert via JS (execCommand fires the input events IG listens for).
async fn set_caption_js(page: &chromiumoxide::Page, caption: &str) -> bool {
    let text = serde_json::to_string(caption).unwrap();
    let js = format!(
        r#"(() => {{
            const el = document.querySelector(
                'div[aria-label^="Write a caption"],textarea[aria-label^="Write a caption"],div[role="textbox"]'
            );
            if (!el) return false;
            el.focus();
            try {{ document.execCommand('insertText', false, {text}); }}
            catch (e) {{ el.textContent = {text}; }}
            el.dispatchEvent(new Event('input', {{ bubbles: true }}));
            return true;
        }})()"#
    );
    eval_bool(page, &js).await
}

/// Poll until a CSS selector matches at least one element, returning the *last* one.
async fn wait_for_last(
    page: &chromiumoxide::Page,
    selector: &str,
    secs: u64,
) -> Option<chromiumoxide::element::Element> {
    for _ in 0..(secs * 2) {
        if let Ok(els) = page.find_elements(selector).await {
            if let Some(el) = els.into_iter().last() {
                return Some(el);
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    None
}

/// Evaluate a JS expression and coerce the result to bool (false on any error).
async fn eval_bool(page: &chromiumoxide::Page, js: &str) -> bool {
    match page.evaluate(js).await {
        Ok(v) => v.into_value::<bool>().unwrap_or(false),
        Err(_) => false,
    }
}

/// Build an Instagram caption from a photo's title/description + keywords, mirroring the
/// photo-insta pipeline: optional title and description lines, then a blank line, then the
/// keywords as `#hashtags` (de-duped, capped — Instagram allows at most 30).
pub fn build_caption(
    title: &str,
    description: &str,
    keywords: &[String],
    max_hashtags: usize,
) -> String {
    let mut lines: Vec<String> = Vec::new();
    if !title.trim().is_empty() {
        lines.push(title.trim().to_string());
    }
    if !description.trim().is_empty() {
        lines.push(description.trim().to_string());
    }

    let mut seen = std::collections::HashSet::new();
    let mut tags: Vec<String> = Vec::new();
    for kw in keywords {
        let h = to_hashtag(kw);
        if h.is_empty() || !seen.insert(h.clone()) {
            continue;
        }
        tags.push(format!("#{h}"));
        if tags.len() >= max_hashtags {
            break;
        }
    }

    let body = lines.join("\n");
    let hashline = tags.join(" ");
    match (body.is_empty(), hashline.is_empty()) {
        (true, _) => hashline,
        (false, true) => body,
        (false, false) => format!("{body}\n\n{hashline}"),
    }
}

/// Normalize a keyword to an Instagram hashtag body: keep Unicode letters/digits/`_`
/// (Instagram supports ø/å/æ/ü/…), drop spaces and punctuation, lowercase.
fn to_hashtag(keyword: &str) -> String {
    keyword
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_')
        .collect::<String>()
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::build_caption;

    #[test]
    fn caption_has_title_then_deduped_capped_hashtags() {
        let kw = vec![
            "Golden Hour".into(),
            "Harbor".into(),
            "harbor".into(), // dup after normalization
            "Sea".into(),
        ];
        let out = build_caption("Sunset at the dock", "", &kw, 2);
        assert_eq!(out, "Sunset at the dock\n\n#goldenhour #harbor");
    }

    #[test]
    fn caption_hashtags_only_when_no_title() {
        let out = build_caption("", "", &["Boats".into()], 30);
        assert_eq!(out, "#boats");
    }
}
