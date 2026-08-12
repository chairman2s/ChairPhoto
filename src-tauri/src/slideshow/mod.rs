//! The slideshow **engine** (behind the `slideshow` Cargo feature): turns an ordered list of
//! still images into a single `.mp4` movie by shelling out to the external **ffmpeg** binary
//! (detected on PATH at runtime, like exiftool / Chrome — the established "shell out to a
//! trusted tool" pattern). Catalog-free: it works on file paths the caller has already
//! resolved + rendered (the `make_slideshow` command renders each photo's Original to a temp
//! full-size JPEG via the export path, then hands the paths here). See docs/slideshow.md.
//!
//! Filtergraph: one ffmpeg invocation with one looped still per clip. Each clip is
//! `scale=W:H:force_original_aspect_ratio=decrease` then `pad=W:H:-1:-1:color=black`
//! (letterbox — never crop), at the chosen `fps`; with Ken Burns a slow `zoompan` zoom over
//! `duration*fps` frames is applied. Clips are then chained with `xfade` (each transition's
//! offset accumulates: clip k starts where clip k-1's visible part ends, minus the overlap)
//! for crossfades, or simply `concat`enated when transitions are off. Encoded with `libx264`,
//! `-pix_fmt yuv420p`, `-movflags +faststart` for broad compatibility.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// All knobs for one slideshow render. The output path + the resolved still paths are passed
/// separately to [`build_args`] / [`render`]; this is the user-facing option set mirrored by
/// the frontend dialog (docs/slideshow.md → Options).
#[derive(Clone, Copy, Debug)]
pub struct SlideshowOptions {
    /// Seconds each photo is shown (the full clip length, transition overlap included).
    pub duration_per_photo: f64,
    /// Whether to crossfade between clips (ffmpeg `xfade`); `false` = hard cuts (`concat`).
    pub transition: bool,
    /// Crossfade length in seconds (ignored when `transition` is false). Clamped below
    /// `duration_per_photo` so a clip always has some non-overlapping visible time.
    pub transition_duration: f64,
    /// Apply a slow Ken Burns zoom (ffmpeg `zoompan`) to each clip.
    pub ken_burns: bool,
    /// Output frame rate.
    pub fps: u32,
    /// Output width in pixels.
    pub width: u32,
    /// Output height in pixels.
    pub height: u32,
}

impl Default for SlideshowOptions {
    fn default() -> Self {
        SlideshowOptions {
            duration_per_photo: 3.0,
            transition: true,
            transition_duration: 0.5,
            ken_burns: false,
            fps: 30,
            width: 1920,
            height: 1080,
        }
    }
}

/// Locate the `ffmpeg` binary on `PATH`. Returns the resolved absolute path, or `None` when
/// it is not installed (the caller surfaces a clear "ffmpeg required" error).
pub fn ffmpeg_path() -> Option<String> {
    which("ffmpeg")
}

/// Locate the `ffprobe` binary on `PATH` (used by tests / optional duration probing).
pub fn ffprobe_path() -> Option<String> {
    which("ffprobe")
}

fn which(bin: &str) -> Option<String> {
    let out = Command::new("which").arg(bin).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if path.is_empty() {
        None
    } else {
        Some(path)
    }
}

/// The effective transition overlap (seconds): the requested `transition_duration` when
/// transitions are on, clamped to leave at least ~0.1s of non-overlapping clip. `0.0` when
/// transitions are off.
fn overlap(opts: &SlideshowOptions) -> f64 {
    if !opts.transition {
        return 0.0;
    }
    let max = (opts.duration_per_photo - 0.1).max(0.0);
    opts.transition_duration.clamp(0.0, max)
}

/// Build the per-clip filter chain (everything after the input label, up to its output label)
/// for one still: scale to fit, letterbox-pad, set SAR/fps, optional Ken Burns zoom. The clip
/// is `duration_per_photo` seconds long (`duration*fps` frames).
fn clip_filter(opts: &SlideshowOptions) -> String {
    let SlideshowOptions {
        width: w,
        height: h,
        fps,
        duration_per_photo: dur,
        ..
    } = *opts;
    let frames = ((dur * fps as f64).round() as i64).max(1);
    let mut chain = String::new();
    if opts.ken_burns {
        // Slow zoom from 1.0 to ~1.15 over the clip. zoompan needs an oversized input so the
        // zoomed crop has pixels to sample: upscale to 2x the target first, zoompan emits the
        // target size. `d` = frames per still, `fps` sets the clip rate. CRITICAL: zoompan
        // emits `d` frames *per input frame*, and the looped `-i` still feeds a stream of
        // frames — so we `select='eq(n,0)'` to pass exactly ONE frame in, making the clip
        // exactly `d/fps` seconds long (otherwise the duration blows up by the loop rate).
        chain.push_str(&format!(
            "scale={w2}:{h2}:force_original_aspect_ratio=decrease,\
             pad={w2}:{h2}:-1:-1:color=black,\
             select='eq(n\\,0)',\
             zoompan=z='min(zoom+0.0015,1.15)':d={frames}:x='iw/2-(iw/zoom/2)':y='ih/2-(ih/zoom/2)':s={w}x{h}:fps={fps},\
             setsar=1",
            w2 = w * 2,
            h2 = h * 2,
            w = w,
            h = h,
            frames = frames,
            fps = fps,
        ));
    } else {
        chain.push_str(&format!(
            "scale={w}:{h}:force_original_aspect_ratio=decrease,\
             pad={w}:{h}:-1:-1:color=black,\
             fps={fps},setsar=1",
            w = w,
            h = h,
            fps = fps,
        ));
    }
    chain
}

/// Build the `filter_complex` graph for `n` clips. Returns the graph string and the label of
/// the final video stream to map (`[vN]`, or `[outv]` for the xfade/concat result).
fn filter_complex(n: usize, opts: &SlideshowOptions) -> (String, String) {
    let mut parts: Vec<String> = Vec::new();
    let per = clip_filter(opts);
    for i in 0..n {
        parts.push(format!("[{i}:v]{per}[v{i}]", i = i, per = per));
    }

    if n == 1 {
        return (parts.join(";"), "[v0]".to_string());
    }

    if opts.transition {
        let ov = overlap(opts);
        let dur = opts.duration_per_photo;
        // xfade chains pairwise. Clip i contributes (dur - ov) of unique visible time before
        // the next transition begins, so offset_k = k*(dur - ov) for the k-th xfade (k>=1).
        let mut prev = "[v0]".to_string();
        let mut acc = 0.0_f64;
        for i in 1..n {
            acc += dur - ov;
            let out = if i == n - 1 {
                "[outv]".to_string()
            } else {
                format!("[x{i}]")
            };
            parts.push(format!(
                "{prev}[v{i}]xfade=transition=fade:duration={ov}:offset={off}{out}",
                prev = prev,
                i = i,
                ov = fmt_secs(ov),
                off = fmt_secs(acc),
                out = out,
            ));
            prev = out;
        }
        (parts.join(";"), "[outv]".to_string())
    } else {
        // Hard cuts: concat the per-clip streams.
        let labels: String = (0..n).map(|i| format!("[v{i}]")).collect();
        parts.push(format!(
            "{labels}concat=n={n}:v=1:a=0[outv]",
            labels = labels,
            n = n
        ));
        (parts.join(";"), "[outv]".to_string())
    }
}

/// Format seconds without trailing-zero noise / locale issues (ffmpeg wants a `.`-decimal).
fn fmt_secs(s: f64) -> String {
    // Up to 3 decimals, trim trailing zeros.
    let mut t = format!("{:.3}", s);
    while t.contains('.') && (t.ends_with('0') || t.ends_with('.')) {
        t.pop();
    }
    t
}

/// Build the full ffmpeg argument vector (everything after the binary name) for the given
/// ordered `frame_paths`, options, and output file. Each still is a looped input held for
/// `duration_per_photo` seconds; the `filter_complex` does scale/pad/fps (+ optional zoompan)
/// and chains the clips with xfade (or concat). Encodes libx264 / yuv420p / +faststart.
///
/// `-progress pipe:1` is appended so the caller can parse progress from stdout.
pub fn build_args(frame_paths: &[PathBuf], opts: &SlideshowOptions, out: &Path) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    args.push("-y".to_string());

    let dur = fmt_secs(opts.duration_per_photo);
    for p in frame_paths {
        args.push("-loop".to_string());
        args.push("1".to_string());
        args.push("-t".to_string());
        args.push(dur.clone());
        args.push("-i".to_string());
        args.push(p.to_string_lossy().to_string());
    }

    let (graph, map) = filter_complex(frame_paths.len(), opts);
    args.push("-filter_complex".to_string());
    args.push(graph);
    args.push("-map".to_string());
    args.push(map);

    args.push("-r".to_string());
    args.push(opts.fps.to_string());
    args.push("-c:v".to_string());
    args.push("libx264".to_string());
    args.push("-pix_fmt".to_string());
    args.push("yuv420p".to_string());
    args.push("-movflags".to_string());
    args.push("+faststart".to_string());
    // `-progress`/`-nostats` must come BEFORE the output file — ffmpeg ignores options that
    // follow the output filename, which would silently disable progress reporting.
    args.push("-progress".to_string());
    args.push("pipe:1".to_string());
    args.push("-nostats".to_string());
    args.push(out.to_string_lossy().to_string());

    args
}

/// Total expected output frame count (used to turn ffmpeg's `frame=` counter into a fraction).
fn total_frames(n: usize, opts: &SlideshowOptions) -> u32 {
    if n == 0 {
        return 0;
    }
    let per = opts.duration_per_photo * opts.fps as f64;
    let ov_frames = overlap(opts) * opts.fps as f64;
    // n clips, each `per` frames, overlapped by `ov_frames` at (n-1) joins.
    let total = per * n as f64 - ov_frames * (n.saturating_sub(1)) as f64;
    total.round().max(1.0) as u32
}

/// Render `frame_paths` to the `.mp4` at `out` using ffmpeg. Reports progress as
/// `(done_frames, total_frames)` via `on_progress` while encoding (parsed from ffmpeg's
/// `-progress pipe:1` `frame=` lines). Returns `Err` with a tail of ffmpeg's stderr on
/// failure, or a clear message when ffmpeg is not installed / no frames were given.
pub fn render(
    frame_paths: &[PathBuf],
    opts: &SlideshowOptions,
    out: &Path,
    on_progress: impl Fn(u32, u32),
) -> Result<(), String> {
    if frame_paths.is_empty() {
        return Err("slideshow: no photos to render".to_string());
    }
    let bin = ffmpeg_path()
        .ok_or_else(|| "ffmpeg not found on PATH — install ffmpeg to render slideshows".to_string())?;

    let total = total_frames(frame_paths.len(), opts);
    let args = build_args(frame_paths, opts, out);

    let mut child = Command::new(&bin)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to launch ffmpeg: {e}"))?;

    // Drain stderr on a separate thread so a full stderr pipe can never deadlock the
    // stdout/progress loop (ffmpeg writes verbose diagnostics to stderr on long renders).
    let stderr_handle = child.stderr.take().map(|mut err| {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = String::new();
            let _ = err.read_to_string(&mut buf);
            buf
        })
    });

    // Parse `-progress pipe:1` from stdout. ffmpeg writes blocks of `key=value` lines; we
    // care about `frame=` (current output frame) and the terminal `progress=end`.
    if let Some(stdout) = child.stdout.take() {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            if let Some(v) = line.strip_prefix("frame=") {
                if let Ok(f) = v.trim().parse::<u32>() {
                    on_progress(f.min(total), total);
                }
            } else if line == "progress=end" {
                on_progress(total, total);
            }
        }
    }

    let stderr_buf = stderr_handle.and_then(|h| h.join().ok()).unwrap_or_default();

    let status = child
        .wait()
        .map_err(|e| format!("ffmpeg wait failed: {e}"))?;

    if !status.success() {
        let tail: String = stderr_buf
            .lines()
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        return Err(format!(
            "ffmpeg exited with {} —\n{}",
            status.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into()),
            tail
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> SlideshowOptions {
        SlideshowOptions {
            duration_per_photo: 3.0,
            transition: false,
            transition_duration: 0.5,
            ken_burns: false,
            fps: 30,
            width: 1920,
            height: 1080,
        }
    }

    fn paths(n: usize) -> Vec<PathBuf> {
        (0..n).map(|i| PathBuf::from(format!("/tmp/f{i}.jpg"))).collect()
    }

    #[test]
    fn clip_filter_letterboxes_without_crop() {
        let f = clip_filter(&opts());
        assert!(f.contains("scale=1920:1080:force_original_aspect_ratio=decrease"));
        assert!(f.contains("pad=1920:1080:-1:-1:color=black"));
        assert!(f.contains("fps=30"));
        assert!(!f.contains("zoompan"));
    }

    #[test]
    fn clip_filter_ken_burns_uses_zoompan() {
        let mut o = opts();
        o.ken_burns = true;
        let f = clip_filter(&o);
        assert!(f.contains("zoompan="));
        assert!(f.contains("s=1920x1080"));
        // Oversampled input so the zoom crop has pixels.
        assert!(f.contains("scale=3840:2160:force_original_aspect_ratio=decrease"));
        // Exactly one frame fed into zoompan (otherwise the looped input multiplies duration).
        assert!(f.contains("select='eq(n\\,0)'"));
    }

    #[test]
    fn concat_graph_no_transition() {
        let (graph, map) = filter_complex(3, &opts());
        assert_eq!(map, "[outv]");
        assert!(graph.contains("[0:v]"));
        assert!(graph.contains("[v0][v1][v2]concat=n=3:v=1:a=0[outv]"));
        assert!(!graph.contains("xfade"));
    }

    #[test]
    fn xfade_graph_accumulates_offsets() {
        let mut o = opts();
        o.transition = true;
        o.transition_duration = 0.5; // overlap 0.5, unique = 2.5
        let (graph, map) = filter_complex(3, &o);
        assert_eq!(map, "[outv]");
        // First xfade: offset = 2.5, into intermediate [x1].
        assert!(
            graph.contains("[v0][v1]xfade=transition=fade:duration=0.5:offset=2.5[x1]"),
            "graph was: {graph}"
        );
        // Second xfade: offset = 5.0, into [outv].
        assert!(
            graph.contains("[x1][v2]xfade=transition=fade:duration=0.5:offset=5[outv]"),
            "graph was: {graph}"
        );
    }

    #[test]
    fn xfade_two_inputs_single_join() {
        let mut o = opts();
        o.transition = true;
        let (graph, map) = filter_complex(2, &o);
        assert_eq!(map, "[outv]");
        assert!(graph.contains("[v0][v1]xfade=transition=fade:duration=0.5:offset=2.5[outv]"));
    }

    #[test]
    fn single_clip_maps_v0() {
        let (_graph, map) = filter_complex(1, &opts());
        assert_eq!(map, "[v0]");
    }

    #[test]
    fn build_args_loops_each_input_and_encodes() {
        let a = build_args(&paths(2), &opts(), Path::new("/tmp/out.mp4"));
        assert_eq!(a.iter().filter(|x| *x == "-loop").count(), 2);
        assert_eq!(a.iter().filter(|x| *x == "-i").count(), 2);
        assert!(a.iter().any(|x| x == "libx264"));
        assert!(a.iter().any(|x| x == "yuv420p"));
        assert!(a.iter().any(|x| x == "+faststart"));
        assert!(a.iter().any(|x| x == "/tmp/out.mp4"));
        assert!(a.iter().any(|x| x == "-progress"));
    }

    #[test]
    fn render_rejects_empty_input() {
        // No frames → a clear error, and ffmpeg is never launched (checked before detection).
        let err = render(&[], &opts(), Path::new("/tmp/out.mp4"), |_, _| {})
            .expect_err("empty input must error");
        assert!(err.contains("no photos"), "error was: {err}");
    }

    #[test]
    fn fmt_secs_trims_trailing_zeros() {
        assert_eq!(fmt_secs(2.5), "2.5");
        assert_eq!(fmt_secs(5.0), "5");
        assert_eq!(fmt_secs(0.0), "0");
        assert_eq!(fmt_secs(0.123), "0.123");
        // Always a `.`-decimal regardless of locale (ffmpeg requires it).
        assert!(!fmt_secs(2.5).contains(','));
    }

    #[test]
    fn build_args_single_input_maps_v0_and_no_xfade() {
        // A one-photo slideshow still encodes (maps [v0], no transition graph).
        let a = build_args(&paths(1), &opts(), Path::new("/tmp/out.mp4"));
        let i = a.iter().position(|x| x == "-map").expect("has -map");
        assert_eq!(a[i + 1], "[v0]");
        let fc = a.iter().position(|x| x == "-filter_complex").unwrap();
        assert!(!a[fc + 1].contains("xfade"));
        assert!(!a[fc + 1].contains("concat"));
    }

    #[test]
    fn build_args_holds_each_still_for_duration() {
        // Every looped input is held for `-t duration_per_photo`.
        let a = build_args(&paths(2), &opts(), Path::new("/tmp/out.mp4"));
        let t_vals: Vec<&String> = a
            .iter()
            .enumerate()
            .filter(|(i, x)| *x == "-t" && *i + 1 < a.len())
            .map(|(i, _)| &a[i + 1])
            .collect();
        assert_eq!(t_vals.len(), 2);
        assert!(t_vals.iter().all(|v| *v == "3"), "got {t_vals:?}");
    }

    #[test]
    fn total_frames_zero_inputs_is_zero() {
        assert_eq!(total_frames(0, &opts()), 0);
    }

    #[test]
    fn overlap_clamped_below_clip_duration() {
        let mut o = opts();
        o.transition = true;
        o.duration_per_photo = 1.0;
        o.transition_duration = 5.0;
        // Cannot exceed dur - 0.1 = 0.9.
        assert!((overlap(&o) - 0.9).abs() < 1e-9);
    }

    #[test]
    fn total_frames_accounts_for_overlap() {
        let mut o = opts();
        o.transition = false;
        // 3 clips * 3s * 30fps = 270.
        assert_eq!(total_frames(3, &o), 270);
        o.transition = true;
        o.transition_duration = 0.5;
        // 270 - 0.5*30*2 = 270 - 30 = 240.
        assert_eq!(total_frames(3, &o), 240);
    }

    /// Real ffmpeg render — skips gracefully when ffmpeg/ffprobe are not installed.
    #[test]
    fn real_render_produces_correct_mp4() {
        let (Some(_ff), Some(probe)) = (ffmpeg_path(), ffprobe_path()) else {
            eprintln!(
                "SKIPPED: real_render_produces_correct_mp4 — ffmpeg/ffprobe not on PATH, so no \
                 real render was exercised"
            );
            return;
        };

        let dir = std::env::temp_dir().join(format!("chairphoto_slideshow_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Write 3 solid-color PNGs.
        let colors = [[255u8, 0, 0], [0, 255, 0], [0, 0, 255]];
        let mut frame_paths = Vec::new();
        for (i, c) in colors.iter().enumerate() {
            let p = dir.join(format!("c{i}.png"));
            let img = image::RgbImage::from_pixel(640, 480, image::Rgb(*c));
            img.save(&p).unwrap();
            frame_paths.push(p);
        }

        let opts = SlideshowOptions {
            duration_per_photo: 2.0,
            transition: true,
            transition_duration: 0.5,
            ken_burns: true,
            fps: 30,
            width: 1280,
            height: 720,
        };
        let out = dir.join("slideshow.mp4");

        let last = std::sync::atomic::AtomicU32::new(0);
        render(&frame_paths, &opts, &out, |done, _total| {
            last.store(done, std::sync::atomic::Ordering::SeqCst);
        })
        .expect("render should succeed");

        assert!(out.exists(), "output mp4 should exist");
        assert!(std::fs::metadata(&out).unwrap().len() > 0, "output mp4 should be non-empty");
        // Progress must actually be reported (guards against -progress being placed where
        // ffmpeg ignores it, which would leave the UI bar stuck at 0%).
        assert!(
            last.load(std::sync::atomic::Ordering::SeqCst) > 0,
            "render should report progress via on_progress"
        );

        // Probe dimensions (exact) and duration (~ within 0.4s).
        let dims = Command::new(&probe)
            .args([
                "-v", "error", "-select_streams", "v:0",
                "-show_entries", "stream=width,height",
                "-of", "csv=p=0:s=x",
            ])
            .arg(&out)
            .output()
            .unwrap();
        let dims_s = String::from_utf8_lossy(&dims.stdout);
        assert_eq!(dims_s.trim(), "1280x720", "dimensions should match preset");

        let durout = Command::new(&probe)
            .args([
                "-v", "error", "-show_entries", "format=duration",
                "-of", "default=noprint_wrappers=1:nokey=1",
            ])
            .arg(&out)
            .output()
            .unwrap();
        let dur: f64 = String::from_utf8_lossy(&durout.stdout).trim().parse().unwrap();
        // Expected: 3*2 - 0.5*2 = 5.0s.
        assert!((dur - 5.0).abs() < 0.5, "duration {dur} should be ~5.0s");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
