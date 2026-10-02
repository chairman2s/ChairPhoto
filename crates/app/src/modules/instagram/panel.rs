//! `InstagramForm` (instagram.tsx): the Instagram publish target. See the module docs for the
//! outcomes and the "awaiting review" confirmation.
//!
//! A Post is core's publish job in three worker steps, never on the UI thread: the claim (bound
//! to the catalog the photo came from), the 1080-px render, then handing it to Chrome (and, for
//! a confirmed post, recording it). [`Stage`] shows where it is; Cancel stops it until Chrome
//! has the render. A superseded Post's answers are dropped; a closed dialog does not stop the
//! post, whose answer then goes to the status line.

use crate::modules::dialog::{self, DialogHost};
use crate::modules::publishing::{PublishSubject, VersionPicker, NO_ROWS};
use crate::shell::style::Colors;
use crate::storage::{ui, Runner};
use chairphoto_core::app::instagram::{self as core_instagram, InstagramDriver, PostOutcome};
use chairphoto_core::app::publications::record_publications_as;
use chairphoto_core::app::uploads::{claim_upload, UPLOAD_CANCELLED};
use chairphoto_core::app::CatalogIdentity;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::prelude::*;
use gpui_kit::{div, px, AsyncApp, Context, Entity, Subscription, TestSupportExt as _, Window};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// What the panel says when a worker step died (a panic) without an answer.
const STOPPED: &str = "The post stopped unexpectedly";

pub const NEEDS_LOGIN: &str = "Log in to Instagram in the Chrome window that opened, then Post again.";
pub const AWAITING_REVIEW: &str = "Composed in Chrome — click Share in the browser, then confirm below.";
pub const POSTED: &str = "Posted to Instagram ✓";
pub const NOT_RECORDED: &str = "Post not recorded.";

/// Where a Post is. Only [`Stage::Posting`] cannot be cancelled: Chrome has the render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Preparing,
    Rendering,
    Posting,
}

impl Stage {
    fn line(self) -> &'static str {
        match self {
            Stage::Preparing => "Preparing…",
            Stage::Rendering => "Rendering…",
            Stage::Posting => "Composing the post in Chrome…",
        }
    }
}

/// The photo and version of a supervised post, kept until the user says whether they clicked
/// Share — so the confirmation records exactly what was composed, in the catalog it came from,
/// even if the picker changed meanwhile.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PendingReview {
    pub catalog: CatalogIdentity,
    pub photo_id: i64,
    pub version_id: Option<i64>,
}

/// A Post's answer: the outcome, and for a confirmed post whether its record failed.
type Outcome = Result<(PostOutcome, Option<String>), String>;

enum Go {
    Run,
    Cancelled,
    Superseded,
}

#[derive(Default)]
struct Running {
    cancelled: bool,
    abort: Option<Arc<AtomicBool>>,
}

pub struct InstagramPanel {
    host: DialogHost,
    driver: Arc<dyn InstagramDriver>,
    marker: String,
    pub subject: PublishSubject,
    pub versions: VersionPicker,
    pub caption: Entity<TextareaState>,
    /// The user edited the caption: the prefill no longer overwrites it.
    pub caption_touched: bool,
    prefilling: bool,
    /// Click Share too (off: stop before it, for the user to review).
    pub auto_publish: bool,
    pub busy: bool,
    pub stage: Option<Stage>,
    running: Option<Running>,
    attempt: u64,
    pub pending: Option<PendingReview>,
    pub status: String,
    _subscriptions: Vec<Subscription>,
}

impl InstagramPanel {
    pub fn new(host: DialogHost, driver: Arc<dyn InstagramDriver>, marker: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subject = PublishSubject::take(&host.shell, cx);
        let caption = cx.new(|cx| TextareaState::new(window, cx).rows(4).placeholder("Caption + #hashtags…"));
        let subscriptions = vec![
            dialog::close_on_switch(&host.model, window, cx),
            cx.subscribe(&caption, |this: &mut Self, _, event: &InputEvent, _| {
                if matches!(event, InputEvent::Change) && !this.prefilling {
                    this.caption_touched = true;
                }
            }),
        ];
        let mut panel = InstagramPanel {
            versions: VersionPicker::new(&subject),
            host,
            driver,
            marker,
            subject,
            caption,
            caption_touched: false,
            prefilling: false,
            auto_publish: false,
            busy: false,
            stage: None,
            running: None,
            attempt: 0,
            pending: None,
            status: String::new(),
            _subscriptions: subscriptions,
        };
        VersionPicker::load(&panel.host.app, &panel.subject, cx, |p: &mut Self, versions, cx| {
            p.versions.versions = versions;
            cx.notify();
        });
        panel.prefill_caption(window, cx);
        panel
    }

    fn photo(&self) -> Option<i64> {
        self.subject.active.map(|a| a.id)
    }

    /// The caption from the photo's title and keywords, until the user edits it.
    fn prefill_caption(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(photo), Some(catalog)) = (self.photo(), self.subject.catalog) else { return };
        let app = self.host.app.clone();
        let rx = Runner::get(cx).run(move || core_instagram::caption(&app, Some(catalog), photo));
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(caption)) = rx.await else { return };
            this.update_in(cx, |p, window, cx| {
                if p.caption_touched {
                    return;
                }
                p.prefilling = true;
                p.caption.update(cx, |t, cx| t.set_value(caption, window, cx));
                p.prefilling = false;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Post: claim, render, hand to Chrome; a confirmed post is recorded in the same step.
    pub fn post(&mut self, cx: &mut Context<Self>) {
        let Some(photo) = self.photo() else { return };
        let Some(catalog) = self.subject.catalog else {
            self.status = NO_ROWS.into();
            cx.notify();
            return;
        };
        if self.busy || self.pending.is_some() {
            return;
        }
        self.attempt += 1;
        let attempt = self.attempt;
        self.busy = true;
        self.running = Some(Running::default());
        self.stage = Some(Stage::Preparing);
        self.status = Stage::Preparing.line().into();
        let version = self.versions.chosen;
        let caption = self.caption.read(cx).value().to_string();
        let publish = self.auto_publish;
        let (app, driver, marker) = (self.host.app.clone(), self.driver.clone(), self.marker.clone());
        let claim = {
            let app = app.clone();
            Runner::get(cx).run(move || claim_upload(&app, Some(catalog), core_instagram::SERVICE, photo, version))
        };
        let host = self.host.clone();
        let pending = PendingReview { catalog, photo_id: photo, version_id: version };
        cx.spawn(async move |this, cx| {
            let land = |result: Outcome, cx: &mut AsyncApp| {
                if this.update(cx, |p, cx| p.land(attempt, pending, result.clone(), cx)).is_err() {
                    // The dialog closed: the answer goes to the status line.
                    let line = match &result {
                        Ok((PostOutcome::Posted, None)) => "Posted to Instagram.".to_string(),
                        Ok((PostOutcome::Posted, Some(e))) => unrecorded(e),
                        Ok((PostOutcome::AwaitingReview, _)) => "Instagram: the post is composed in Chrome — click Share there.".into(),
                        Ok((PostOutcome::NeedsLogin, _)) => format!("Instagram: {NEEDS_LOGIN}"),
                        Err(e) => format!("Instagram: {e}"),
                    };
                    cx.update(|cx| host.status(line, cx));
                }
            };
            let job = match claim.await.unwrap_or_else(|_| Err(STOPPED.into())) {
                Ok(job) => job,
                Err(e) => return land(Err(e), cx),
            };
            match this.update(cx, |p, cx| p.step(attempt, Stage::Rendering, Some(job.abort_handle()), cx)).unwrap_or(Go::Run) {
                Go::Run => {}
                Go::Cancelled => return land(Err(UPLOAD_CANCELLED.into()), cx),
                Go::Superseded => return,
            }
            let render = cx.update(|cx| Runner::get(cx).run(move || core_instagram::render(job)));
            let rendered = match render.await.unwrap_or_else(|_| Err(STOPPED.into())) {
                Ok(rendered) => rendered,
                Err(e) => return land(Err(e), cx),
            };
            match this.update(cx, |p, cx| p.step(attempt, Stage::Posting, None, cx)).unwrap_or(Go::Run) {
                Go::Run => {}
                Go::Cancelled => return land(Err(UPLOAD_CANCELLED.into()), cx),
                Go::Superseded => return,
            }
            let posted = cx.update(|cx| {
                Runner::get(cx).run(move || -> Outcome {
                    let outcome = core_instagram::post(&*driver, rendered, &caption, publish)?;
                    // Recorded only on a confirmed post; a supervised one waits for the user.
                    let recorded = match outcome {
                        PostOutcome::Posted => record_publications_as(&app, catalog, &[(photo, version)], &marker, None).err(),
                        _ => None,
                    };
                    Ok((outcome, recorded))
                })
            });
            land(posted.await.unwrap_or_else(|_| Err(STOPPED.into())), cx);
        })
        .detach();
        cx.notify();
    }

    fn step(&mut self, attempt: u64, stage: Stage, abort: Option<Arc<AtomicBool>>, cx: &mut Context<Self>) -> Go {
        if attempt != self.attempt {
            return Go::Superseded;
        }
        let Some(running) = self.running.as_mut() else { return Go::Superseded };
        if abort.is_some() {
            running.abort = abort;
        }
        if running.cancelled {
            return Go::Cancelled;
        }
        self.stage = Some(stage);
        self.status = stage.line().into();
        cx.notify();
        Go::Run
    }

    fn land(&mut self, attempt: u64, pending: PendingReview, result: Outcome, cx: &mut Context<Self>) {
        if attempt != self.attempt {
            return;
        }
        self.busy = false;
        self.stage = None;
        self.running = None;
        self.status = match result {
            Ok((PostOutcome::NeedsLogin, _)) => NEEDS_LOGIN.into(),
            Ok((PostOutcome::AwaitingReview, _)) => {
                self.pending = Some(pending);
                AWAITING_REVIEW.into()
            }
            Ok((PostOutcome::Posted, None)) => {
                self.posted(cx);
                POSTED.into()
            }
            Ok((PostOutcome::Posted, Some(e))) => {
                let line = unrecorded(&e);
                self.host.status(line.clone(), cx);
                line
            }
            Err(e) => e,
        };
        cx.notify();
    }

    /// A recorded post: the toast, and the catalog-derived views re-read.
    fn posted(&self, cx: &mut Context<Self>) {
        self.host.model.update(cx, |m, cx| {
            m.set_status("Posted to Instagram.", cx);
            m.refresh(cx);
        });
    }

    /// Cancel: until Chrome has the render, the post stops there.
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.stage == Some(Stage::Posting) {
            return;
        }
        if let Some(r) = self.running.as_mut() {
            r.cancelled = true;
            if let Some(abort) = &r.abort {
                abort.store(true, Ordering::Relaxed);
            }
        }
        cx.notify();
    }

    /// "Yes, I posted it": record the composed photo and version, in the catalog it came from.
    pub fn confirm_posted(&mut self, cx: &mut Context<Self>) {
        let Some(p) = self.pending.take() else { return };
        let (app, marker) = (self.host.app.clone(), self.marker.clone());
        let rx = Runner::get(cx).run(move || record_publications_as(&app, p.catalog, &[(p.photo_id, p.version_id)], &marker, None));
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Err(STOPPED.into()));
            this.update(cx, |v, cx| {
                v.status = match result {
                    Ok(()) => {
                        v.posted(cx);
                        POSTED.into()
                    }
                    Err(e) => unrecorded(&e),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// "No, skip": record nothing.
    pub fn dismiss_review(&mut self, cx: &mut Context<Self>) {
        self.pending = None;
        self.status = NOT_RECORDED.into();
        cx.notify();
    }
}

/// A post Instagram has but the catalog could not record: say both, so it is not posted twice.
fn unrecorded(error: &str) -> String {
    format!("Posted to Instagram, but couldn't record it as published: {error}. It is on Instagram — don't post it again.")
}

impl Render for InstagramPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let body = ui::body().id("instagram-panel");
        if self.photo().is_none() {
            return body.child(ui::empty("instagram-empty", "Select a photo", colors)).test_support();
        }
        let this = cx.entity().downgrade();
        let mut body = body
            .child(ui::label("Version", colors))
            .child(div().child(self.versions.menu("instagram-version", this, |p: &mut Self, v, _| p.versions.chosen = v)))
            .child(ui::label("Caption", colors))
            .child(div().id("instagram-caption").child(Textarea::new(&self.caption)).test_support())
            .child(
                Checkbox::new("instagram-auto-publish")
                    .label("Publish automatically (otherwise stop before Share so you can review)")
                    .checked(self.auto_publish)
                    .on_change(cx.listener(|p, checked: &bool, _, cx| {
                        p.auto_publish = *checked;
                        cx.notify();
                    })),
            )
            .child(ui::sub(
                "Posts the selected image (1080px) by driving Chrome — a window opens; log in to Instagram there once. \
                 Browser automation can break when Instagram changes its site.",
                colors,
            ));
        let can = !self.busy && self.pending.is_none();
        let label = if self.busy { "Posting…" } else { "Post to Instagram" };
        let mut actions = ui::row().child(ui::clickable(ui::primary("instagram-post", label, can, colors), can, cx.listener(|p, _, _, cx| p.post(cx))));
        if self.busy && self.stage != Some(Stage::Posting) {
            actions = actions.child(ui::clickable(ui::chip("instagram-cancel", "Cancel", true, colors), true, cx.listener(|p, _, _, cx| p.cancel(cx))));
        }
        body = body.child(actions.child(div().id("instagram-status").child(ui::sub(self.status.clone(), colors)).test_support()));
        if self.pending.is_some() {
            body = body.child(
                div()
                    .id("instagram-review")
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .pt(px(8.))
                    .border_t_1()
                    .border_color(colors.line)
                    .child(ui::sub("Did you click Share in the Instagram tab?", colors))
                    .child(
                        ui::row()
                            .child(ui::clickable(ui::primary("instagram-review-yes", "Yes, I posted it", true, colors), true, cx.listener(|p, _, _, cx| p.confirm_posted(cx))))
                            .child(ui::clickable(ui::chip("instagram-review-no", "No, skip", true, colors), true, cx.listener(|p, _, _, cx| p.dismiss_review(cx)))),
                    )
                    .test_support(),
            );
        }
        body.test_support()
    }
}
