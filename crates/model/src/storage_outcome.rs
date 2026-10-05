//! What a storage verb says when it finishes — the inspector's status line after Back up,
//! Offload and Restore, and the Library's "Retrieve from NAS".
//!
//! Port of `stackOutcome` and the `onBackup`/`onOffload`/`onRestore` wording in React's
//! `PhotoInspector.tsx` (origin/main 60c5034, #82). Each verb acts on a stack — the photo the
//! user named and the frames under it — so each reports what it took and what it left: a
//! burst offloaded as "Local copy freed" when 4 of 7 frames went was #82's whole complaint,
//! and a count the user can read is what makes the per-frame gate visible rather than
//! mysterious. A lone photo keeps the plain verb it always had.

use chairphoto_core::catalog::{BackupReport, OffloadReport, RestoreReport, SkippedPhoto};

/// How a verb reports acting on a stack: "Backed up 4 of 7 — no local copy to back up". A
/// lone photo (nothing skipped, one photo touched) keeps the plain verb.
///
/// `total` is the whole moment the user named — the photo and every frame under it, members
/// already in the wanted state included — so "4 of 7" counts the stack, not just the members
/// this call attempted (a frame already archived is neither done nor skipped by an offload).
///
/// Reasons are de-duplicated (first occurrence wins the order) because a stack usually fails
/// for one reason at a time, and seven copies of the same sentence would bury the count.
pub fn stack_outcome(verb: &str, done: &[i64], skipped: &[SkippedPhoto], total: usize) -> String {
    outcome(verb, "", done, skipped, total)
}

/// [`stack_outcome`] with `place` (" from NAS") after the count and before the reasons.
fn outcome(verb: &str, place: &str, done: &[i64], skipped: &[SkippedPhoto], total: usize) -> String {
    if skipped.is_empty() {
        return if done.len() > 1 {
            format!("{verb} {} in the stack{place}", done.len())
        } else {
            format!("{verb}{place}")
        };
    }
    let mut reasons: Vec<&str> = Vec::new();
    for skip in skipped {
        if !reasons.contains(&skip.reason.as_str()) {
            reasons.push(&skip.reason);
        }
    }
    format!("{verb} {} of {total}{place} — {}", done.len(), reasons.join("; "))
}

/// The status line after Back up.
pub fn backup_message(report: &BackupReport) -> String {
    stack_outcome("Backed up", &report.backed_up, &report.skipped, report.total)
}

/// The status line after Offload, including what it deliberately left: a sidecar backup is
/// the only record of what this copy's sidecar looked like before ChairPhoto first wrote it,
/// so offload leaves it in place — and says so.
pub fn offload_message(report: &OffloadReport) -> String {
    let mut parts = vec![if report.freed.len() == 1 && report.skipped.is_empty() {
        "Local copy freed".to_string()
    } else {
        stack_outcome("Freed", &report.freed, &report.skipped, report.total)
    }];
    let n = report.sidecar_backups_left;
    if n > 0 {
        parts.push(format!("left {n} sidecar backup{} in place", if n == 1 { "" } else { "s" }));
    }
    parts.join(" — ")
}

/// The status line after the inspector's Restore.
pub fn restore_message(report: &RestoreReport) -> String {
    if report.restored.len() == 1 && report.skipped.is_empty() {
        "Restored to local".to_string()
    } else {
        stack_outcome("Restored", &report.restored, &report.skipped, report.total)
    }
}

/// The Library's status line after "Retrieve from NAS" (the same restore, worded as that
/// menu item is).
pub fn retrieve_message(report: &RestoreReport) -> String {
    if report.restored.len() == 1 && report.skipped.is_empty() {
        "Retrieved from NAS.".to_string()
    } else {
        format!("{}.", outcome("Retrieved", " from NAS", &report.restored, &report.skipped, report.total))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skipped(items: &[(i64, &str)]) -> Vec<SkippedPhoto> {
        items.iter().map(|(id, why)| SkippedPhoto { photo_id: *id, reason: why.to_string() }).collect()
    }

    #[test]
    fn a_lone_photo_keeps_the_plain_verb() {
        assert_eq!(stack_outcome("Backed up", &[1], &[], 1), "Backed up");
        assert_eq!(offload_message(&OffloadReport { freed: vec![1], ..Default::default() }), "Local copy freed");
        assert_eq!(restore_message(&RestoreReport { restored: vec![1], ..Default::default() }), "Restored to local");
        assert_eq!(retrieve_message(&RestoreReport { restored: vec![1], ..Default::default() }), "Retrieved from NAS.");
    }

    #[test]
    fn a_whole_stack_says_how_many_went() {
        let report = BackupReport { backed_up: vec![1, 2, 3], ..Default::default() };
        assert_eq!(backup_message(&report), "Backed up 3 in the stack");
        let report = OffloadReport { freed: vec![1, 2], ..Default::default() };
        assert_eq!(offload_message(&report), "Freed 2 in the stack");
        let report = RestoreReport { restored: vec![1, 2], ..Default::default() };
        assert_eq!(retrieve_message(&report), "Retrieved 2 in the stack from NAS.");
    }

    #[test]
    fn a_partial_stack_says_what_it_left_and_why_once() {
        let report = OffloadReport {
            freed: vec![1, 2, 3, 4],
            skipped: skipped(&[(5, "no verified backup"), (6, "no verified backup"), (7, "a.xmp differs")]),
            total: 7,
            sidecar_backups_left: 0,
        };
        assert_eq!(offload_message(&report), "Freed 4 of 7 — no verified backup; a.xmp differs");
    }

    /// The count is of the whole moment, from the report's `total` — not done + skipped,
    /// which leaves out members that needed nothing (port of React's
    /// `PhotoInspector.storageOutcome` case, origin/main 227c87e).
    #[test]
    fn the_count_is_of_the_whole_moment() {
        let report = OffloadReport {
            freed: vec![1, 2, 3, 4],
            skipped: skipped(&[(7, "no verified backup yet")]),
            total: 7,
            sidecar_backups_left: 0,
        };
        assert_eq!(offload_message(&report), "Freed 4 of 7 — no verified backup yet");
    }

    #[test]
    fn offload_says_what_it_left_in_place() {
        let one = OffloadReport { freed: vec![1], skipped: vec![], total: 1, sidecar_backups_left: 1 };
        assert_eq!(offload_message(&one), "Local copy freed — left 1 sidecar backup in place");
        let two = OffloadReport { freed: vec![1, 2], skipped: vec![], total: 2, sidecar_backups_left: 2 };
        assert_eq!(offload_message(&two), "Freed 2 in the stack — left 2 sidecar backups in place");
    }

    #[test]
    fn a_restore_that_left_a_frame_away_names_it() {
        let report =
            RestoreReport { restored: vec![1], skipped: skipped(&[(2, "no reachable backup to restore")]), total: 2 };
        assert_eq!(restore_message(&report), "Restored 1 of 2 — no reachable backup to restore");
        assert_eq!(retrieve_message(&report), "Retrieved 1 of 2 from NAS — no reachable backup to restore.");
    }
}
