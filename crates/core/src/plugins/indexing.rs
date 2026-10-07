//! Shared indexing-job settings for optional ONNX-backed plugins.
//!
//! Faces and Smart Tagging are independently gated Cargo features, but both use the same
//! `indexing.speed` preference to size model session pools. Keeping that setting here avoids
//! making either plugin compile through the other one just to read a shared knob.

use rusqlite::Connection;

/// Setting key for how hard indexing jobs may push the machine.
pub const INDEXING_SPEED_SETTING: &str = "indexing.speed";

/// How aggressively background index jobs consume the machine. Chosen by the user in
/// Preferences; resolves to a concrete [`IndexingPlan`] against the host's core count.
///
/// - [`Background`](IndexingSpeed::Background) — today's throttle: a couple of workers,
///   a couple of intra-op threads each, so the desktop stays responsive while indexing.
/// - [`Full`](IndexingSpeed::Full) — saturate: pool + parallelism scale with the available
///   CPUs so a big re-index finishes as fast as the CPU allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexingSpeed {
    Background,
    Full,
}

impl Default for IndexingSpeed {
    fn default() -> Self {
        IndexingSpeed::Background
    }
}

impl IndexingSpeed {
    /// Parse the `indexing.speed` setting string. Anything unrecognized (or unset) falls back
    /// to the responsive default.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "full" => IndexingSpeed::Full,
            _ => IndexingSpeed::Background,
        }
    }

    /// The canonical string stored in `settings`.
    pub fn as_str(self) -> &'static str {
        match self {
            IndexingSpeed::Background => "background",
            IndexingSpeed::Full => "full",
        }
    }

    /// Resolve this speed to a concrete plan against `available_cpus` (pass the host core
    /// count; a real caller passes [`std::thread::available_parallelism`]).
    ///
    /// The split between *photo* parallelism (concurrent sessions) and *intra-op* threads
    /// (threads inside one ONNX run) was tuned by measuring face indexing on a 24-thread dev
    /// machine. The ONNX Runtime intra-op pool already parallelizes each detect/embed across
    /// cores, so stacking many concurrent sessions on top *oversubscribes* and slows
    /// throughput. The win comes from giving a **few** sessions **generous** intra-op threads
    /// (`parallelism x intra ~= cpus`), which keeps two photos overlapping (one's preload/IO
    /// hides behind the other's compute) while each run still uses the whole CPU.
    pub fn plan(self, available_cpus: usize) -> IndexingPlan {
        let cpus = available_cpus.max(1);
        match self {
            // Two workers, two intra-op threads each — matches the old
            // `DETECT_PARALLELISM = 2` throttle and leaves headroom for the UI.
            IndexingSpeed::Background => IndexingPlan {
                parallelism: 2.min(cpus),
                intra_threads: 2.min(cpus),
            },
            // Saturate: keep just two concurrent photos so they overlap, but hand each ONNX
            // session ~half the cores of intra-op threads (2 x cpus/2 ~= cpus). Measured the
            // fastest of every split tried; pushing parallelism higher regressed on both
            // sparse- and dense-face folders.
            IndexingSpeed::Full => {
                let parallelism = 2.min(cpus);
                let intra_threads = (cpus / parallelism).max(1);
                IndexingPlan {
                    parallelism,
                    intra_threads,
                }
            }
        }
    }
}

/// Concrete, resolved parallelism for one index run: how many photos to process at once and
/// how many intra-op threads each ONNX session gets. Session pools are sized to
/// `parallelism` so every worker can hold a distinct session with no lock contention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexingPlan {
    /// Concurrent model calls.
    pub parallelism: usize,
    /// Intra-op thread count configured on each ONNX session.
    pub intra_threads: usize,
}

/// Load the effective [`IndexingPlan`] from the catalog `settings` table and the host's core
/// count. Falls back to [`IndexingSpeed::default`] for a missing/blank/unknown value.
pub fn load_indexing_plan(conn: &Connection) -> IndexingPlan {
    let speed = get_speed_setting(conn)
        .map(|s| IndexingSpeed::parse(&s))
        .unwrap_or_default();
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    speed.plan(cpus)
}

fn get_speed_setting(conn: &Connection) -> Option<String> {
    conn.query_row(
        "SELECT value FROM settings WHERE key = ?1",
        [INDEXING_SPEED_SETTING],
        |r| r.get::<_, String>(0),
    )
    .ok()
    .filter(|s: &String| !s.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        conn
    }

    /// Unknown / blank / unset speed strings fall back to the responsive default.
    #[test]
    fn indexing_speed_parse_defaults_to_background() {
        assert_eq!(IndexingSpeed::parse("full"), IndexingSpeed::Full);
        assert_eq!(IndexingSpeed::parse("FULL"), IndexingSpeed::Full);
        assert_eq!(
            IndexingSpeed::parse("background"),
            IndexingSpeed::Background
        );
        assert_eq!(IndexingSpeed::parse(""), IndexingSpeed::Background);
        assert_eq!(IndexingSpeed::parse("turbo"), IndexingSpeed::Background);
        assert_eq!(IndexingSpeed::default(), IndexingSpeed::Background);
        // Round-trips through the canonical string.
        assert_eq!(
            IndexingSpeed::parse(IndexingSpeed::Full.as_str()),
            IndexingSpeed::Full
        );
        assert_eq!(
            IndexingSpeed::parse(IndexingSpeed::Background.as_str()),
            IndexingSpeed::Background
        );
    }

    /// Background keeps the old 2-worker/2-thread throttle regardless of core count; Full
    /// keeps two concurrent photos but hands each session ~half the cores of intra-op threads,
    /// so `parallelism * intra <= cpus` (no oversubscription).
    #[test]
    fn indexing_plan_scales_with_speed_and_cpus() {
        // Background is flat at 2/2 (or fewer on a 1-core box).
        assert_eq!(
            IndexingSpeed::Background.plan(24),
            IndexingPlan {
                parallelism: 2,
                intra_threads: 2
            }
        );
        assert_eq!(
            IndexingSpeed::Background.plan(1),
            IndexingPlan {
                parallelism: 1,
                intra_threads: 1
            }
        );

        // Full on the 24-thread dev machine: 2 sessions x 12 intra-op threads = the whole CPU.
        let full = IndexingSpeed::Full.plan(24);
        assert_eq!(
            full,
            IndexingPlan {
                parallelism: 2,
                intra_threads: 12
            }
        );
        assert!(
            full.parallelism * full.intra_threads <= 24,
            "no oversubscription"
        );
        // Full gives each session far more intra-op threads than background does.
        assert!(full.intra_threads > IndexingSpeed::Background.plan(24).intra_threads);

        // Degenerate core counts still yield a runnable plan.
        assert_eq!(
            IndexingSpeed::Full.plan(1),
            IndexingPlan {
                parallelism: 1,
                intra_threads: 1
            }
        );
        assert_eq!(
            IndexingSpeed::Full.plan(0),
            IndexingPlan {
                parallelism: 1,
                intra_threads: 1
            }
        );
    }

    /// `load_indexing_plan` reads the `indexing.speed` setting and resolves a plan; an unset
    /// key yields the background default.
    #[test]
    fn load_indexing_plan_reads_setting() {
        let conn = mem_conn();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT);",
        )
        .unwrap();

        // Unset -> background (parallelism 2 on any multi-core host).
        let plan = load_indexing_plan(&conn);
        assert_eq!(
            plan.parallelism,
            IndexingSpeed::Background
                .plan(
                    std::thread::available_parallelism()
                        .map(|n| n.get())
                        .unwrap_or(4)
                )
                .parallelism
        );

        // Set to full -> each session gets more intra-op threads than background (>=, equal
        // only on a <=2-core host).
        conn.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, 'full')",
            [INDEXING_SPEED_SETTING],
        )
        .unwrap();
        let full = load_indexing_plan(&conn);
        assert!(full.intra_threads >= plan.intra_threads);
    }
}
