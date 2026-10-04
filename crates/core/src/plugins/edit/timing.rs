//! Opt-in per-stage timings for the render path (increment 1 of the GPU-smoothness
//! work, docs/plans/darkroom/00-status.md). With `CHAIRPHOTO_EDIT_TIMING=1` in the
//! environment every render prints one `[edit-timing]` line to stderr: the stages in
//! order with their milliseconds, the total, and the build profile — a debug `cargo run`
//! runs this crate at opt-level 0, so a debug number is not a release number, and the line
//! says which it is. Off (the default) a render costs one atomic load and takes no
//! clocks; nothing here touches pixels.
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// The build profile a timing line was measured in.
pub(crate) const PROFILE: &str = if cfg!(debug_assertions) { "debug" } else { "release" };

/// Whether timing lines are wanted — read once per process from the environment.
pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        matches!(std::env::var("CHAIRPHOTO_EDIT_TIMING").as_deref(), Ok(v) if !v.is_empty() && v != "0")
    })
}

/// One render's stage clock. `start` samples the clock only when timing is enabled, so
/// the disabled path is a couple of `None` checks per stage.
pub(crate) struct Stages {
    label: String,
    t0: Option<Instant>,
    last: Option<Instant>,
    marks: Vec<(&'static str, f64)>,
}

impl Stages {
    pub(crate) fn start(label: impl Into<String>) -> Self {
        Self::new(label, enabled())
    }

    fn new(label: impl Into<String>, on: bool) -> Self {
        let now = on.then(Instant::now);
        Stages {
            label: label.into(),
            t0: now,
            last: now,
            marks: Vec::new(),
        }
    }

    /// Record the time since the previous mark (or the start) under `stage`.
    pub(crate) fn mark(&mut self, stage: &'static str) {
        if let Some(last) = self.last {
            let now = Instant::now();
            self.marks.push((stage, ms(now - last)));
            self.last = Some(now);
        }
    }

    /// Print the line; `extra` (sizes, ids) is appended verbatim. A no-op when disabled.
    pub(crate) fn report(self, extra: &str) {
        let Some(t0) = self.t0 else { return };
        let total = ms(t0.elapsed());
        let mut line = format!("[edit-timing] {} profile={PROFILE}", self.label);
        for (stage, v) in &self.marks {
            line.push_str(&format!(" {stage}={v:.2}"));
        }
        line.push_str(&format!(" total={total:.2}"));
        if !extra.is_empty() {
            line.push(' ');
            line.push_str(extra);
        }
        eprintln!("{line}");
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_by_default_and_marks_accumulate() {
        // The test process does not set CHAIRPHOTO_EDIT_TIMING: a started clock is inert.
        let mut off = Stages::start("off");
        off.mark("a");
        off.mark("b");
        assert!(off.t0.is_none());
        assert!(off.marks.is_empty(), "a disabled clock must not take samples");

        // Forced on, marks land in order with non-negative durations.
        let mut on = Stages::new("on", true);
        on.mark("parse");
        std::thread::sleep(Duration::from_millis(2));
        on.mark("look");
        let names: Vec<&str> = on.marks.iter().map(|(s, _)| *s).collect();
        assert_eq!(names, ["parse", "look"]);
        assert!(on.marks.iter().all(|(_, v)| *v >= 0.0));
        assert!(on.marks[1].1 >= 1.0, "the slept stage must measure at least ~1 ms");
        on.report("out=1x1"); // prints to stderr; must not panic
    }
}
