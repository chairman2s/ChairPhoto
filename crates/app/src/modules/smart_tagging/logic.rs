//! The Smart Tagging module's lines, ported from smartTagging.tsx (unit-tested).

use chairphoto_core::app::smarttags::SmarttagsTrainResult;
use chairphoto_core::app::SmarttagsIndexDone;

/// How an index run ended (`indexDoneMessage`), from its honest counters.
pub fn index_done_message(d: &SmarttagsIndexDone) -> String {
    if d.total == 0 {
        return "Indexing: nothing to do — all photos are already indexed.".into();
    }
    let mut skips = Vec::new();
    if d.offline > 0 {
        skips.push(format!("{} offline (connect the NAS and re-run)", d.offline));
    }
    if d.failed > 0 {
        skips.push(format!("{} unreadable", d.failed));
    }
    let skip_note = if skips.is_empty() { String::new() } else { format!(" Skipped: {} — still queued.", skips.join(", ")) };
    if d.aborted {
        return format!("Indexing cancelled at {} of {} photos.{skip_note}", d.done, d.total);
    }
    if d.done < d.total {
        return format!("Indexing finished: {} of {} photos processed.{skip_note}", d.done, d.total);
    }
    format!("Indexing complete: {} photo{} processed.", d.total, if d.total == 1 { "" } else { "s" })
}

/// The Download button while it runs (`formatDownloadProgress`).
pub fn download_label(progress: Option<(u64, Option<u64>)>) -> String {
    const MB: f64 = 1024. * 1024.;
    match progress {
        None => "Downloading… (~350 MB)".into(),
        Some((done, Some(total))) if total > 0 => {
            format!("Downloading… {}% of {} MB", (done as f64 / total as f64 * 100.).round() as u64, (total as f64 / MB).round() as u64)
        }
        Some((done, _)) => format!("Downloading… {:.1} MB", done as f64 / MB),
    }
}

/// "Index" / "Indexing…" / "Indexing… N%": a percentage only once progress has numbers.
pub fn index_label(running: bool, progress: Option<(usize, usize)>) -> String {
    match (running, progress) {
        (false, _) => "Index".into(),
        (true, Some((done, total))) if total > 0 => format!("Indexing… {}%", (done as f64 / total as f64 * 100.).round() as u64),
        (true, _) => "Indexing…".into(),
    }
}

/// The training result line.
pub fn train_line(r: &SmarttagsTrainResult) -> String {
    if r.examined == 0 {
        "No tag has enough confirmed indexed photos yet.".into()
    } else {
        format!("{} trained, {} already fresh ({} eligible).", r.trained, r.skipped, r.examined)
    }
}

/// "from N similar photo(s)".
pub fn provenance(n: usize) -> String {
    format!("from {n} similar photo{}", if n == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn done(done: usize, total: usize, offline: usize, aborted: bool) -> SmarttagsIndexDone {
        SmarttagsIndexDone { ok: true, done, total, offline, failed: 0, aborted, job: 1, error: None }
    }

    #[test]
    fn index_lines_match_the_react_app() {
        assert_eq!(index_done_message(&done(0, 0, 0, false)), "Indexing: nothing to do — all photos are already indexed.");
        assert_eq!(index_done_message(&done(1, 1, 0, false)), "Indexing complete: 1 photo processed.");
        assert_eq!(
            index_done_message(&done(3, 5, 2, false)),
            "Indexing finished: 3 of 5 photos processed. Skipped: 2 offline (connect the NAS and re-run) — still queued."
        );
        assert_eq!(index_done_message(&done(2, 5, 0, true)), "Indexing cancelled at 2 of 5 photos.");
    }

    #[test]
    fn progress_labels() {
        assert_eq!(download_label(None), "Downloading… (~350 MB)");
        assert_eq!(download_label(Some((50 << 20, Some(200 << 20)))), "Downloading… 25% of 200 MB");
        assert_eq!(download_label(Some((3 << 19, None))), "Downloading… 1.5 MB");
        assert_eq!(index_label(true, None), "Indexing…");
        assert_eq!(index_label(true, Some((1, 4))), "Indexing… 25%");
        assert_eq!(index_label(false, Some((1, 4))), "Index");
        assert_eq!(provenance(1), "from 1 similar photo");
    }
}
