//! Tag graph measurements (#121) on a generated library at the designed scale — 1,400 tags
//! and 8,600 co-occurrence edges by default (`chairphoto_model::tag_graph::synthetic`). No
//! catalog is opened, so it never touches the user's data.
//!
//! ```sh
//! # Headless: shaping, scene build, raster, label placement — no window, no GPU.
//! cargo run --release -p chairphoto-app --example tag_graph_bench -- --headless
//! # A window with the real view over the same data, tags and cameras on. Every 2 s it prints
//! # the canvas's prepaint+paint cost and the frame-to-frame interval of the frames painted
//! # in that window; hover the ring and wheel-zoom to measure them.
//! cargo run --release -p chairphoto-app --example tag_graph_bench
//! # Another scale: --tags N --edges M --cameras K
//! ```

use chairphoto_app::model::AppModel;
use chairphoto_app::modules::tag_graph::raster::{rasterize, region_for, strips_for, RasterJob};
use chairphoto_app::modules::tag_graph::view::TagGraphView;
use chairphoto_app::modules::tag_graph::GraphSource;
use chairphoto_app::shell::ShellState;
use chairphoto_core::app::AppState;
use chairphoto_core::appearance::SystemThemeResult;
use chairphoto_model::tag_graph::graph::Graph;
use chairphoto_model::tag_graph::labels::place_labels;
use chairphoto_model::tag_graph::scene::Scene;
use chairphoto_model::tag_graph::session::GraphSession;
use chairphoto_model::tag_graph::synthetic::{library, Scale};
use chairphoto_model::tag_graph::view::{Size, View};
use gpui_kit::{px, size, AppContext as _, Bounds, WindowBounds, WindowOptions};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn summary(label: &str, mut v: Vec<Duration>) {
    if v.is_empty() {
        println!("{label:<40} (no samples)");
        return;
    }
    v.sort();
    let p = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
    println!(
        "{label:<40} n={:<4} p50 {:>7.2} ms  p95 {:>7.2} ms  max {:>7.2} ms",
        v.len(),
        ms(p(0.5)),
        ms(p(0.95)),
        ms(*v.last().unwrap())
    );
}

fn time<T>(runs: usize, mut f: impl FnMut() -> T) -> (Vec<Duration>, T) {
    let mut out = Vec::new();
    let mut last = None;
    for _ in 0..runs {
        let t = Instant::now();
        last = Some(f());
        out.push(t.elapsed());
    }
    (out, last.unwrap())
}

fn arg(name: &str) -> Option<usize> {
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok())
}

fn headless(scale: Scale) {
    let data = library(scale, 1);
    let (t, graph) = time(5, || Graph::build(&data));
    summary("shape library_graph (Graph::build)", t);
    let mut s = GraphSession::new();
    let g = s.begin_load();
    s.apply_load(g, Ok(graph));
    s.toggle_tags();
    s.toggle_cameras();
    let input = s.take_scene_request().unwrap();
    let (t, scene) = time(5, || Scene::build(&input));
    println!("ring: {} nodes, {} drawn edges, {} buckets", scene.layout.order.len(), scene.edges.len(), scene.buckets.len());
    summary("scene build (layout + curves)", t);
    let scene = Arc::new(scene);
    let canvas = Size { w: 1400., h: 900. };
    let fit = View::fit(canvas);
    // ×3 about the ring's 3 o'clock point: the ring crosses the canvas, labels stack there.
    let near = fit.zoom_at(3., (700. + 400. * fit.k, 450.));
    for (name, view) in [("fit", fit), ("×3 at 3 o'clock", near)] {
        for dpr in [1., 2.] {
            let region = region_for(view, canvas).unwrap();
            let strips = strips_for((region.h * dpr) as u32);
            let job = RasterJob { scene: scene.clone(), view, size: canvas, region, scale: dpr, strips };
            let (t, px) = time(5, || rasterize(&job).unwrap());
            summary(&format!("raster {name} @{dpr}x {}×{} ({strips} strips)", px.width, px.height), t);
        }
        let (t, labels) = time(20, || place_labels(&scene, view, canvas, None, &[], |_| 70.));
        summary(&format!("labels {name} ({} placed)", labels.len()), t);
    }
    let hub = scene.layout.label_order[0];
    let lit = scene.lit_edges(&[hub].into()).len();
    println!("lit edges when hovering the biggest tag: {lit}");
}

fn main() {
    let scale = Scale {
        tags: arg("--tags").unwrap_or(1400),
        edges: arg("--edges").unwrap_or(8600),
        cameras: arg("--cameras").unwrap_or(6),
    };
    println!("synthetic library: {scale:?}");
    if std::env::args().any(|a| a == "--headless") {
        headless(scale);
        return;
    }
    let data = library(scale, 1);
    gpui_kit::application().with_assets(chairphoto_app::assets::Assets).run(move |cx| {
        let _ = chairphoto_app::assets::load_fonts(cx);
        gpui_kit::init(cx);
        chairphoto_app::theme::apply_system_theme(&SystemThemeResult::unavailable(), cx);
        cx.bind_keys(chairphoto_app::keymap::bindings());
        let model = cx.new(|_| AppModel::new(AppState::default(), None));
        let shell = cx.new(|cx| ShellState::new(&model, cx));
        let source: GraphSource = Arc::new(move || Ok(data.clone()));
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1400.), px(900.)), cx))),
            app_id: Some("chairphoto-tag-graph-bench".into()),
            ..Default::default()
        };
        let m = model.clone();
        let (_window, view) = gpui_kit::open_window(options, cx, move |window, cx| {
            cx.new(|cx| TagGraphView::new(&m, shell, None, source, window, cx))
        })
        .expect("window");
        view.update(cx, |v, cx| {
            v.update_session(cx, |s| {
                s.toggle_tags();
                s.toggle_cameras();
            })
        });
        let cache = view.read(cx).paint_cache();
        cx.spawn(async move |cx| loop {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            let (costs, at) = {
                let mut c = cache.borrow_mut();
                (std::mem::take(&mut c.timings), std::mem::take(&mut c.painted_at))
            };
            if !costs.is_empty() {
                summary("canvas prepaint+paint (last 2 s)", costs);
                // Painted frames back to back: idle gaps over 250 ms are not frames of a gesture.
                let gaps: Vec<Duration> = at.windows(2).map(|w| w[1] - w[0]).filter(|d| *d < Duration::from_millis(250)).collect();
                summary("frame interval while painting", gaps);
            }
        })
        .detach();
    });
}
