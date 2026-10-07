//! The Stats main view: draws [`Dashboard`] (`StatsView` in statistics.tsx). Layout follows
//! `statistics.css`: a centred column of cards — the scope chip, four stat cards, the facts
//! strip, the timeline, two-up grids — on the stage's canvas.
//!
//! Charts are gpui-component's: the timeline is an [`AreaChart`] (the hover crosshair and
//! tooltip replace React's readout line, which now always shows the peak), the 24 h clock
//! and the camera donut are [`PieChart`]s (the clock's equal slices take their length from
//! `outer_radius_fn`), and the bars are [`BarChart`]s. Rank and rate lists are plain rows.
//! React's grow-in animations are not ported (`prefers-reduced-motion` aside, they were
//! cosmetic).

use super::Statistics;
use crate::shell::style::Colors;
use chairphoto_model::statistics::{
    Bars, Clock, Dashboard, Donut, Hue, RankList, RateCard, RateMetric, Summary, Timeline, CLOCK_R_INNER,
    CLOCK_R_MAX, CLOCK_SIZE, SCOPE_HINT,
};
use gpui_kit::component::chart::{AreaChart, BarChart, PieChart};
use gpui_kit::prelude::*;
use gpui_kit::{
    div, linear_color_stop, linear_gradient, px, rgb, AnyElement, App, Context, Entity, FontWeight, Hsla,
    SharedString, Subscription, TestSupportExt as _, Window,
};
use std::rc::Rc;

/// The column's max width (`.st-inner`).
const INNER_MAX_W: f32 = 1100.;

fn hex(c: u32) -> Hsla {
    rgb(c).into()
}

fn hue_color(hue: Hue) -> Hsla {
    match hue {
        Hue::Blue => hex(0x3B82F6),
        Hue::Violet => hex(0x8B5CF6),
        Hue::Teal => hex(0x14B8A6),
    }
}

fn tip(text: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> gpui_kit::AnyView + 'static {
    let text = text.into();
    move |window, cx| gpui_kit::component::tooltip::Tooltip::new(text.clone()).build(window, cx)
}

/// One window's Stats view over the module's [`Statistics`].
pub struct StatisticsView {
    state: Entity<Statistics>,
    _observe: Subscription,
}

impl StatisticsView {
    pub fn new(state: Entity<Statistics>, cx: &mut Context<Self>) -> Self {
        let _observe = cx.observe(&state, |_, _, cx| cx.notify());
        StatisticsView { state, _observe }
    }

    pub fn state(&self) -> &Entity<Statistics> {
        &self.state
    }
}

impl Render for StatisticsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let scope = self.state.read(cx).scope_label(cx);
        let error = self.state.read(cx).session().error().map(str::to_string);
        let dashboard = self.state.update(cx, |s, _| s.dashboard());

        let mut inner = div().flex().flex_col().gap(px(14.)).w_full().max_w(px(INNER_MAX_W)).px(px(24.)).py(px(20.));
        if let Some(label) = scope {
            inner = inner.child(scope_chip(label, colors));
        }
        inner = match (error, dashboard) {
            (Some(e), _) => inner.child(
                div()
                    .id("stats-error")
                    .p(px(12.))
                    .rounded(px(8.))
                    .border_1()
                    .border_color(colors.danger)
                    .text_color(colors.danger)
                    .text_size(px(12.))
                    .child(e)
                    .test_support(),
            ),
            (None, None) => inner.child(skeleton(colors)),
            (None, Some(d)) => self.render_dashboard(inner, d, colors, cx),
        };
        div()
            .id("stats-root")
            .size_full()
            .overflow_y_scroll()
            .bg(colors.canvas)
            .flex()
            .flex_col()
            .items_center()
            .child(inner)
            .test_support()
    }
}

impl StatisticsView {
    fn render_dashboard(
        &self,
        inner: gpui_kit::Div,
        d: Rc<Dashboard>,
        colors: Colors,
        cx: &mut Context<Self>,
    ) -> gpui_kit::Div {
        let stat = |id: &'static str, num: &str, label: &'static str, accent: u32, small: bool| {
            div()
                .id(id)
                .flex_1()
                .min_w(px(160.))
                .flex()
                .flex_col()
                .gap(px(4.))
                .p(px(14.))
                .rounded(px(10.))
                .bg(colors.panel)
                .border_1()
                .border_color(colors.border)
                .border_t_2()
                .border_color(hex(accent))
                .child(
                    div()
                        .text_size(px(if small { 15. } else { 24. }))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(colors.txt)
                        .child(num.to_string()),
                )
                .child(div().text_size(px(11.)).text_color(colors.dim).child(label))
                .test_support()
        };
        let header = div()
            .flex()
            .flex_wrap()
            .gap(px(12.))
            .child(stat("stats-photos", &d.photos, "Photos", 0x3B82F6, false))
            .child(stat("stats-date-range", &d.date_range, "Date range", 0x8B5CF6, true))
            .child(stat("stats-cameras", &d.camera_count, "Cameras", 0xF59E0B, false))
            .child(stat("stats-lenses", &d.lens_count, "Lenses", 0x14B8A6, false));

        let facts = div().flex().flex_wrap().gap(px(12.)).children(d.facts.iter().map(|f| {
            div()
                .flex_1()
                .min_w(px(160.))
                .flex()
                .flex_col()
                .gap(px(2.))
                .px(px(12.))
                .py(px(10.))
                .rounded(px(8.))
                .bg(colors.panel)
                .child(div().text_size(px(10.5)).text_color(colors.mute).child(f.label.clone()))
                .child(div().text_size(px(13.)).text_color(colors.txt).child(f.value.clone()))
        }));

        let timeline = card(d.timeline.heading(), colors)
            .child(timeline_chart(&d.timeline, colors))
            .when_some(d.invalid_dates.clone(), |c, note| c.child(footnote(note, colors)));

        let time_grid = grid()
            .child(card("Time of day", colors).child(match &d.clock {
                Some(clock) => clock_chart(clock, colors),
                None => no_data(colors),
            }))
            .child(card("Day of week", colors).child(bars(&d.weekdays, "stats-weekdays", 170., true, colors)));

        let cameras = card("Cameras", colors).child(
            div()
                .flex()
                .flex_wrap()
                .gap(px(20.))
                .items_start()
                .when_some(d.donut.as_ref(), |row, donut| row.child(donut_chart(donut, colors)))
                .child(div().flex_1().min_w(px(260.)).child(self.rank_list("cameras", &d.cameras, colors, cx))),
        );

        let lists = grid()
            .child(card("Top tags", colors).child(self.rank_list("tags", &d.tags, colors, cx)))
            .child(card("Lenses", colors).child(self.rank_list("lenses", &d.lenses, colors, cx)))
            .child(card("Focal length (mm)", colors).child(bars(&d.focal, "stats-focal", 150., false, colors)))
            .child(card("Ratings", colors).child(bars(&d.ratings, "stats-ratings", 150., false, colors)))
            .child(card("ISO", colors).child(bars(&d.iso, "stats-iso", 150., false, colors)))
            .child(card("Aperture", colors).child(bars(&d.aperture, "stats-aperture", 150., false, colors)))
            .child(card("Shutter speed", colors).child(bars(&d.shutter, "stats-shutter", 150., false, colors)));

        let summaries = grid()
            .child(summary("stats-cull-survival", &d.cull_survival, colors))
            .child(summary("stats-hit-rate", &d.hit_rate, colors));

        let metric = d.metric;
        let seg = |id: &'static str, label: &'static str, m: RateMetric, cx: &mut Context<Self>| {
            let on = metric == m;
            div()
                .id(id)
                .px(px(10.))
                .py(px(4.))
                .rounded(px(6.))
                .text_size(px(11.5))
                .cursor_pointer()
                .when(on, |b| b.bg(colors.sel).text_color(colors.accent))
                .when(!on, |b| b.text_color(colors.dim).hover(|s| s.text_color(colors.txt)))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| this.state.update(cx, |s, cx| s.set_metric(m, cx))))
                .test_support()
        };
        let keeper_head = div()
            .flex()
            .items_center()
            .justify_between()
            .pt(px(6.))
            .child(
                div()
                    .text_size(px(14.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(colors.txt)
                    .child("Keeper analysis"),
            )
            .child(
                div()
                    .flex()
                    .gap(px(2.))
                    .p(px(2.))
                    .rounded(px(8.))
                    .bg(colors.well)
                    .child(seg("stats-metric-keep", "Keep rate", RateMetric::Keep, cx))
                    .child(seg("stats-metric-hit", "≥4★ hit rate", RateMetric::Hit, cx)),
            );
        let rates = grid().children(d.rate_cards.iter().map(|c| rate_card(c, colors)));

        inner
            .child(header)
            .child(facts)
            .child(timeline)
            .child(time_grid)
            .child(cameras)
            .child(lists)
            .child(summaries)
            .child(keeper_head)
            .child(rates)
    }

    /// `RankList`: rank, name, bar, count, share. A row with a tag id filters the Library.
    fn rank_list(&self, key: &'static str, list: &RankList, colors: Colors, cx: &mut Context<Self>) -> AnyElement {
        if list.rows.is_empty() {
            return no_data(colors);
        }
        let color = hue_color(list.hue);
        let mut col = div().flex().flex_col().gap(px(2.));
        for (i, row) in list.shown().iter().enumerate() {
            let id = SharedString::from(match row.tag_id {
                Some(tag) => format!("stats-tag-row-{tag}"),
                None => format!("stats-{key}-row-{i}"),
            });
            let mut el = div()
                .id(id)
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(6.))
                .py(px(3.))
                .rounded(px(6.))
                .text_size(px(12.))
                .child(div().w(px(18.)).text_color(if i < 3 { color } else { colors.mute }).child((i + 1).to_string()))
                .child(div().w(px(150.)).truncate().text_color(colors.txt).child(row.label.clone()))
                .child(bar_track(list.fill(row), color, colors))
                .child(div().w(px(56.)).text_right().text_color(colors.dim).child(chairphoto_model::statistics::grouped(row.count)))
                .child(div().w(px(36.)).text_right().text_color(colors.mute).child(format!("{}%", list.pct(row))));
            if let Some(title) = &row.title {
                el = el.tooltip(tip(title.clone()));
            }
            if let Some(tag) = row.tag_id {
                el = el
                    .cursor_pointer()
                    .hover(|s| s.bg(colors.well))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        Statistics::filter_by_tag(&this.state, tag, cx);
                    }));
            }
            col = col.child(el.test_support());
        }
        if let Some(more) = list.more() {
            col = col.child(div().pl(px(32.)).pt(px(4.)).text_size(px(11.)).text_color(colors.mute).child(more));
        }
        col.into_any_element()
    }
}

fn grid() -> gpui_kit::Div {
    div().flex().flex_wrap().gap(px(14.))
}

fn card(title: &'static str, colors: Colors) -> gpui_kit::Div {
    div()
        .flex_1()
        .min_w(px(320.))
        .flex()
        .flex_col()
        .gap(px(10.))
        .p(px(14.))
        .rounded(px(10.))
        .bg(colors.panel)
        .border_1()
        .border_color(colors.border)
        .child(div().text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).text_color(colors.dim).child(title))
}

fn footnote(text: String, colors: Colors) -> impl IntoElement {
    div().text_size(px(11.)).text_color(colors.mute).child(text)
}

fn no_data(colors: Colors) -> AnyElement {
    div().py(px(18.)).text_size(px(12.)).text_color(colors.mute).child("No data").into_any_element()
}

fn bar_track(fill: f64, color: Hsla, colors: Colors) -> impl IntoElement {
    div()
        .flex_1()
        .h(px(6.))
        .rounded(px(3.))
        .bg(colors.well)
        .child(div().h_full().rounded(px(3.)).bg(color).w(gpui_kit::relative(fill.clamp(0., 1.) as f32)))
}

fn scope_chip(label: String, colors: Colors) -> impl IntoElement {
    div()
        .id("stats-scope-chip")
        .flex()
        .items_center()
        .gap(px(10.))
        .px(px(12.))
        .py(px(6.))
        .rounded(px(999.))
        .border_1()
        .border_color(colors.accent_border())
        .child(div().text_size(px(12.)).text_color(colors.accent).child(label))
        .child(div().text_size(px(11.)).text_color(colors.mute).child(SCOPE_HINT))
        .test_support()
}

/// The dashboard's shape with placeholders: only on a cold load (`StatsSkeleton`).
fn skeleton(colors: Colors) -> impl IntoElement {
    let block = |h: f32| div().h(px(h)).w_full().rounded(px(8.)).bg(colors.well);
    div()
        .id("stats-skeleton")
        .flex()
        .flex_col()
        .gap(px(14.))
        .child(div().flex().gap(px(12.)).children((0..4).map(|_| {
            div()
                .flex_1()
                .flex()
                .flex_col()
                .gap(px(6.))
                .p(px(14.))
                .rounded(px(10.))
                .bg(colors.panel)
                .child(block(26.).w(gpui_kit::relative(0.55)))
                .child(block(11.).w(gpui_kit::relative(0.38)))
        })))
        .child(div().p(px(14.)).rounded(px(10.)).bg(colors.panel).child(block(200.)))
        .child(
            grid()
                .child(div().flex_1().p(px(14.)).rounded(px(10.)).bg(colors.panel).child(block(170.)))
                .child(div().flex_1().p(px(14.)).rounded(px(10.)).bg(colors.panel).child(block(170.))),
        )
        .test_support()
}

fn timeline_chart(t: &Timeline, colors: Colors) -> AnyElement {
    if t.points.is_empty() {
        return no_data(colors);
    }
    let accent = hex(0x3B82F6);
    // `plotted`, not `points`: a lone month becomes a level line (#179).
    let chart = AreaChart::new(t.plotted())
        .id("stats-timeline-chart")
        .x(|p| SharedString::from(p.tick.clone()))
        .y(|p| p.value as f64)
        .natural()
        .stroke(accent)
        .fill(linear_gradient(180., linear_color_stop(accent.opacity(0.45), 0.), linear_color_stop(accent.opacity(0.), 1.)))
        .tick_margin(1)
        // Pinned to zero so the curve's overshoot between sparse months is clipped, not
        // drawn below the baseline.
        .y_domain(0., t.peak as f64)
        .tooltip_title(|p| SharedString::from(p.readout.clone()))
        .tooltip_value(|_, _, _| SharedString::default());
    div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(div().h(px(200.)).w_full().child(chart))
        .child(div().text_size(px(11.)).text_color(colors.dim).child(t.peak_readout()))
        .into_any_element()
}

/// The radial 24 h clock: equal slices, each as long as its share of the busiest hour.
fn clock_chart(clock: &Clock, colors: Colors) -> AnyElement {
    let chart = PieChart::new(clock.hours.clone())
        .id("stats-clock-chart")
        .value(|_| 1.)
        .inner_radius(CLOCK_R_INNER)
        .outer_radius(CLOCK_R_MAX)
        .outer_radius_fn(|arc| arc.data.outer_radius())
        .pad_angle(2. * std::f32::consts::PI / 24. * 0.12)
        .color(|s| hex(s.color))
        .tooltip_name(|s| SharedString::from(s.title.clone()))
        .tooltip_value(|_, _, _| SharedString::default());
    let ring = |h: f32, text: &'static str| {
        // Midnight at the top, clockwise; the label sits mid-slice just outside the ring.
        let a = -std::f32::consts::FRAC_PI_2 + (h + 0.5) * (2. * std::f32::consts::PI / 24.);
        let r = CLOCK_R_MAX + 12.;
        let (x, y) = (CLOCK_SIZE / 2. + r * a.cos(), CLOCK_SIZE / 2. + r * a.sin());
        div()
            .absolute()
            .left(px(x - 10.))
            .top(px(y - 7.))
            .w(px(20.))
            .text_center()
            .text_size(px(10.))
            .text_color(colors.mute)
            .child(text)
    };
    div()
        .flex()
        .justify_center()
        .child(
            div()
                .relative()
                .size(px(CLOCK_SIZE))
                .child(div().absolute().inset_0().child(chart))
                .child(ring(0., "0"))
                .child(ring(6., "6"))
                .child(ring(12., "12"))
                .child(ring(18., "18"))
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .text_size(px(18.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(colors.txt)
                                .child(clock.peak.clone()),
                        )
                        .child(div().text_size(px(10.)).text_color(colors.mute).child("favorite hour")),
                ),
        )
        .into_any_element()
}

/// The camera donut (top five + Other) and its legend.
fn donut_chart(donut: &Donut, colors: Colors) -> AnyElement {
    let size = 150.;
    let chart = PieChart::new(donut.slices.clone())
        .id("stats-donut-chart")
        .value(|s| s.count as f32)
        .inner_radius(49.)
        .outer_radius(71.)
        .color(|s| hex(s.color))
        .tooltip_name(|s| SharedString::from(s.title.clone()))
        .tooltip_value(|_, _, _| SharedString::default());
    let legend = div().flex().flex_col().gap(px(4.)).children(donut.slices.iter().enumerate().map(|(i, s)| {
        div()
            .id(("stats-donut-legend", i))
            .flex()
            .items_center()
            .gap(px(6.))
            .text_size(px(11.5))
            .child(div().size(px(8.)).rounded_full().bg(hex(s.color)))
            .child(div().max_w(px(160.)).truncate().text_color(colors.txt).child(s.name.clone()))
            .child(div().text_color(colors.mute).child(format!("{}%", s.pct)))
            .tooltip(tip(format!("{} · {}", s.name, chairphoto_model::statistics::grouped(s.count))))
    }));
    div()
        .flex()
        .items_center()
        .gap(px(14.))
        .child(
            div()
                .relative()
                .size(px(size))
                .child(div().absolute().inset_0().child(chart))
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .text_size(px(18.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(colors.txt)
                                .child(donut.top_share.clone()),
                        )
                        .child(div().text_size(px(10.)).text_color(colors.mute).child("top camera")),
                ),
        )
        .child(legend)
        .into_any_element()
}

/// One bar for [`BarChart`]: its label, value, hover title and whether it is the peak.
#[derive(Clone)]
struct Bar {
    label: SharedString,
    value: i64,
    title: SharedString,
    peak: bool,
}

/// `VBars`: vertical gradient bars; the tallest is lighter, optionally with its count above.
fn bars(b: &Bars, id: &'static str, height: f32, peak_label: bool, colors: Colors) -> AnyElement {
    if b.is_empty() {
        return no_data(colors);
    }
    let peak = b.peak();
    let data: Vec<Bar> = (0..b.values.len())
        .map(|i| Bar {
            label: b.labels[i].clone().into(),
            value: b.values[i],
            title: b.titles.get(i).cloned().unwrap_or_default().into(),
            peak: Some(i) == peak,
        })
        .collect();
    let mut chart = BarChart::new(data)
        .id(id)
        .band(|d| d.label.clone())
        .value(|d| d.value as f64)
        .fill_gradient(|d, _, _| {
            let (base, tip) = if d.peak { (0x3B82F6, 0x93C5FD) } else { (0x2563EB, 0x3B82F6) };
            [linear_color_stop(hex(base), 0.), linear_color_stop(hex(tip), 1.)]
        })
        .corner_radii(px(3.))
        .max_band_width(px(64.))
        .min_length(1.)
        .grid(false)
        .tooltip_title(|d| d.title.clone())
        .tooltip_value(|_, _| SharedString::default());
    if peak_label {
        chart = chart.label(|d| if d.peak && d.value > 0 { chairphoto_model::statistics::grouped(d.value) } else { String::new() });
    }
    div().id(SharedString::from(format!("{id}-bars"))).h(px(height + 24.)).w_full().child(chart).test_support().into_any_element()
}

fn summary(id: &'static str, s: &Summary, colors: Colors) -> impl IntoElement {
    div()
        .id(id)
        .flex_1()
        .min_w(px(320.))
        .flex()
        .flex_col()
        .gap(px(4.))
        .p(px(14.))
        .rounded(px(10.))
        .bg(colors.panel)
        .border_1()
        .border_color(colors.border)
        .child(div().text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).text_color(colors.dim).child(s.title))
        .child(div().text_size(px(24.)).font_weight(FontWeight::SEMIBOLD).text_color(colors.txt).child(s.value.clone()))
        .child(div().text_size(px(11.)).text_color(colors.dim).child(s.detail.clone()))
        .when_some(s.footnote.clone(), |c, note| c.child(footnote(note, colors)))
        .test_support()
}

/// `RateList`: a rate per row; small samples are dimmed, never dropped.
fn rate_card(c: &RateCard, colors: Colors) -> impl IntoElement {
    let color = hue_color(c.hue);
    let body = if c.rows.is_empty() {
        no_data(colors)
    } else {
        div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .children(c.rows.iter().enumerate().map(|(i, r)| {
                div()
                    .id(SharedString::from(format!("stats-rate-{}-{i}", c.title)))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(6.))
                    .py(px(3.))
                    .text_size(px(12.))
                    .when(r.low_n(), |row| row.opacity(0.5))
                    .child(div().w(px(150.)).truncate().text_color(colors.txt).child(r.label.clone()))
                    .child(bar_track(r.fill(), color, colors))
                    .child(div().w(px(70.)).text_right().text_color(colors.dim).child(r.count()))
                    .child(div().w(px(36.)).text_right().text_color(colors.mute).child(format!("{}%", r.pct())))
                    .tooltip(tip(r.title()))
            }))
            .into_any_element()
    };
    card(c.title, colors).child(body)
}
