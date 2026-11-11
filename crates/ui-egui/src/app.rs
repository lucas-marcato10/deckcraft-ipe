use super::clock::{monotonic_secs, now_ms};
use super::ime_hooks::ime_in_ui_pass_key;
use super::services::{InboxEvent, Services};
use super::state::{FilePlace, FilePurpose, ImeDescriptorKey, ImeDestination, ImeTerminalStamp, LocatedPlace, RECENT_MAX, UiState, ViewMode};
#[cfg(not(target_arch = "wasm32"))]
use super::updates_ui;
use super::{
    AnimPreview, ControlRequest, FileTarget, HashMap, ImeCancelled, ImeComposition, ImeResetPhase, ImeTerminalPending, InputTarget, Mods,
    PendingFile, Perf, Receiver, Session, SlideApp, Target, UI_COMMANDS, UiRequest, Value, backstage, canvas, capture, dialogs, doc_window,
    eyedropper, json, media, menus, notes, otel_pacing, panes, preview, qat, repaint, ribbon, selpane, show, sorter, status, textures, theme, thumbs,
    toast, uifonts,
};
use deckcraft_telemetry::sc;
use opentelemetry::KeyValue;

mod clipboard;
mod commands;
mod control;
pub(crate) mod files;
mod frame;
mod ime;
mod input;
mod keys;
mod playback;

#[cfg(test)]
pub(crate) use clipboard::{OBJECTS_MARKER, system_clipboard_text};
#[cfg(test)]
pub(crate) use files::web_image_files;
pub use keys::key_name;

/// How long a transient status message stays visible.
const STATUS_LINGER_MS: f64 = 6000.0;

impl SlideApp {
    /// Remove temporary PDFs created for printing.
    pub fn cleanup_print_previews(&mut self) {
        self.print_previews.cleanup();
    }

    /// Build application UI state, taking ownership of audio output and initialising media, dialogs,
    /// synthetic input, and timing fields.
    pub fn new(session: Session, mut services: Services) -> Self {
        // Tests need the in-memory OTel pipeline installed *before* the first frame binds the
        // shared instrument handles: a handle built while the global meter is the default no-op
        // provider stays no-op for the whole process. Nothing runs here in a shipped build.
        #[cfg(test)]
        crate::otel_frame_tests::pipeline();
        let audio_out = services.audio_out.take();
        SlideApp {
            last_fill: serde_json::json!("accent1"),
            last_outline: serde_json::json!("tx1"),
            table_pen: serde_json::json!({"width": 1.0}),
            session,
            services,
            ui: UiState::default(),
            textures: textures::Textures::default(),
            control_rx: None,
            squiggles: Default::default(),
            dialog: None,
            thesaurus_query: String::new(),
            show: None,
            anim_preview: None,
            reuse_target: None,
            media: media::MediaHost::new(audio_out),
            status: None,
            opening: None,
            toast: None,
            fonts_noted: Default::default(),
            perf: Perf { frame_ms: 0.0, render_ms: 0.0, fps: 0.0 },
            last_frame_ms: None,
            frame_id: 0,
            display_interval_ms: otel_pacing::interval_ms(),
            process_start: None,
            startup_frame_sampled: false,
            startup_input_sampled: false,
            interactions: Vec::new(),
            interaction_seq: 0,
            synthetic: vec![],
            synthetic_pending: false,
            synthetic_next: vec![],
            synthetic_next_target: None,
            synthetic_injected: vec![],
            synthetic_injected_at: None,
            synthetic_generation: 0,
            synthetic_target: None,
            synthetic_focus: None,
            synthetic_injected_time: None,
            ribbon_scroll: HashMap::new(),
            show_instance: 0,
            opening_generation: 0,
            canvas_rect: None,
            eyedropper: None,
            canvas_scale: 1.0,
            slide_rect: None,
            canvas_region: None,
            palette: None,
            integrated_titlebar: false,
            backstage: None,
            pending_urls: vec![],
            print_previews: files::PrintPreviews::default(),
            #[cfg(not(target_arch = "wasm32"))]
            updates_rt: updates_ui::Runtime::default(),
            pending_copy: None,
            capture: capture::Capture::default(),
            styled: false,
            restyle: true,
            fonts_ready: false,
            last_time: 0.0,
            last_recovery: 0.0,
            quit_confirmed: false,
            notes_buf: (0, usize::MAX, notes::Draft::default()),
            ui_fonts: uifonts::UiFonts::default(),
            thumb_drag: None,
            selpane: selpane::State::default(),
            align_to: None,
            doc_window: None,
            sorter: sorter::State::default(),
            slides_focused: false,
            v_seen: false,
            recent_checked: HashMap::new(),
            twisting: false,
            next_file_id: 1,
            pending_files: Vec::new(),
            host_start: None,
            ime_tracker: egui::ime_context::ImeContextTracker::new(),
            ime_pending: None,
            ime_frame_viewport: None,
            ime_viewport: None,
            ime_composition: None,
            ime_terminal_pending: None,
            ime_cancelled: None,
            ime_generation: 0,
            ime_settles_seen: 0,
            ime_interrupt_pending: false,
            ime_reset: HashMap::new(),
        }
    }

    pub fn with_control(mut self, rx: Receiver<ControlRequest>) -> Self {
        self.control_rx = Some(rx);
        self
    }

    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), now_ms()));
    }

    /// Turn on the automatic check for a newer release at start-up (desktop shell only; tests and
    /// the web build never touch the network on their own).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn enable_update_checks(&mut self) {
        updates_ui::enable(self);
    }

    /// A message that must be seen wherever the user is: a toast, which also shows over the start
    /// screen and the File tab, where there is no status bar.
    pub fn notify(&mut self, msg: impl Into<String>) {
        self.toast = Some(toast::Toast { text: msg.into(), at: now_ms() });
    }

    /// The transient status message while it is fresh; clears it once stale. Both the status bar
    /// and the start screen draw it — the start screen has no status bar, and errors raised there
    /// (a missing recent file, a failed sample) used to vanish (issue #172).
    pub fn fresh_status(&mut self) -> Option<String> {
        let (msg, at) = self.status.clone()?;
        if now_ms() - at < STATUS_LINGER_MS {
            Some(msg)
        } else {
            self.status = None;
            None
        }
    }

    /// Return the current document count plus the active document uid, so callers can detect a
    /// replacement document rather than only an index change.
    pub(crate) fn docs_key(&self) -> (usize, Option<u64>) {
        // The document uid, not just its index: closing one presentation and opening another
        // must not look like the same document to the Backstage view or a queued input batch.
        (self.session.documents().len(), self.session.active().map(|d| d.uid))
    }

    /// The File tab's view is open over a presentation (not just the start screen with nothing
    /// open).
    pub fn backstage_open(&self) -> bool {
        self.backstage.is_some() && self.session.active().is_some()
    }

    /// Opens the File tab's view on `page` (keeping its state when it is already open).
    pub fn open_backstage(&mut self, page: backstage::Page) {
        let key = self.docs_key();
        match &mut self.backstage {
            Some(b) => b.go(page),
            None => self.backstage = Some(backstage::Backstage::new(page, key)),
        }
    }

    /// Back to the presentation (no effect on the start screen, where nothing is open).
    pub fn close_backstage(&mut self) {
        if self.session.active().is_some() {
            self.backstage = None;
        }
    }

    /// Keeps the File tab's view in step with the open presentations: it closes when one is
    /// opened, created or closed (the result shows), and it is the start screen when none is open.
    fn sync_backstage(&mut self) {
        let key = self.docs_key();
        let has_doc = self.session.active().is_some();
        let stale = self.backstage.as_ref().is_some_and(|b| b.docs != key);
        if has_doc && stale {
            self.backstage = None;
        } else if !has_doc && (stale || self.backstage.is_none()) {
            self.backstage = Some(backstage::Backstage::new(backstage::Page::Home, key));
        }
    }

    /// Background work, run on every logic call (visible, hidden or `run_logic`).
    ///
    /// Only consumed queues and monotonic deadlines live here: control requests, asynchronous
    /// opening/inbox results, screenshot completions/deadlines, AutoRecover pacing and media
    /// clocks. UI events are handled in [`Self::ui`], where a forced hidden paint also arrives.
    pub fn logic(&mut self, ctx: &egui::Context) {
        if !Self::root_viewport_admitted(ctx) {
            log::debug!("ignoring SlideApp::logic for render-only viewport {:?}", ctx.viewport_id());
            return;
        }
        self.tick_hidden_show(ctx, web_time::Instant::now());
        // `raw_input_hook` runs before `App::logic`. In a full pass egui has already built
        // `PointerState` and `keys_down` from every injected event, so retracting the events after
        // this pass would not stop the derived click or key from reaching the UI. Defer instead:
        // a control request, background-open result or inbox file that changes the document,
        // target or modal owner waits until the admitted batch has been consumed by the old
        // target's UI pass. A logic-only callback carries the RawInput into the next full pass
        // (no live `InputState` here), so that pass still drains normally and the next
        // `raw_input_hook` retracts a batch that went stale.
        if !self.synthetic_batch_is_live(ctx) {
            self.drain_control(ctx);
            self.poll_opening();
            #[cfg(not(target_arch = "wasm32"))]
            updates_ui::tick(self, ctx);
            // Inbox results are host file bytes and do not need fonts or a painted frame. A
            // window that starts hidden must still apply a file picked before its first UI pass.
            self.drain_inbox();
        }
        self.drain_screenshots();
        // `raw_input_hook` ran before this logic pass. This is normally a no-op now that
        // target-changing work is deferred for a live batch; it still covers a target change
        // made by other logic, and keeps a logic-only batch pending for the next hook.
        self.retract_synthetic_after_logic(ctx);
        // Queued/settling screenshots force the bounded hidden paints they need; issued ones
        // rely on the screenshot command and the completion callback, with only a deadline wake.
        self.capture.request_paint_if_needed(ctx, web_time::Instant::now());
        if !self.synthetic.is_empty() || !self.synthetic_next.is_empty() || self.synthetic_pending {
            repaint::synthetic(ctx);
        }
        // Closing the window with unsaved changes asks first.
        if ctx.input(|i| i.viewport().close_requested()) && !self.quit_confirmed && self.session.documents().iter().any(|d| d.is_dirty()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.show = None;
            if self.dialog.as_ref().is_none_or(|d| d.id != "quit") {
                self.dialog = Some(dialogs::Dialog::new("quit"));
            }
        }
        self.autosave_if_due(ctx);
        self.tick_media_clock(ctx);
        self.note_settled_composition(ctx);
        // In a full run_ui pass the UI callback after this logic half still owns the pass's final
        // IME descriptor. Only a standalone run_logic callback proves that no custom editor was
        // painted for this tick. Do not infer this from cumulative frame counters or stale
        // geometry.
        if !self.ime_in_ui_pass(ctx) {
            self.finalize_ime_logic(ctx);
        }
    }

    /// Lay out the whole window.
    ///
    /// Every return route, including fonts/layout discards, show mode, modal/Backstage and the
    /// no-document paths, reaches the same outer reset observation and finalizer below. No early
    /// route may publish or inspect IME output on its own.
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        if !Self::root_viewport_admitted(&ctx) {
            log::debug!("ignoring SlideApp::ui for render-only viewport {:?}", ctx.viewport_id());
            return;
        }
        self.ui_inner(ui);
        // The last writer (all panels, dialogs, the custom producer and deferred child windows)
        // has run. Observe the actual custom descriptor for this pass, then reconcile the reset
        // lifetime against the actual committed output. This also runs for discarded passes.
        self.ime_frame_viewport = Some(ctx.viewport_id());
        let custom = self.observe_ime(&ctx);
        self.finalize_ime_reset(&ctx, custom);
    }

    /// Run the admitted root UI pass, including setup gating, panels, canvas, dialogs, overlays, and
    /// show mode.
    fn ui_inner(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.ime_frame_viewport = Some(ctx.viewport_id());
        self.ime_pending = None;
        if !self.ui_setup(&ctx) {
            // This pass is discarded for the new fonts. The input events were already consumed
            // by egui, so keep them queued for the next frame instead of silently losing them.
            self.requeue_injected_synthetic();
            return;
        }
        let t0 = now_ms();
        self.frame_id = self.frame_id.wrapping_add(1);
        // One root span per painted frame; every phase below is a child of it, so a trace answers
        // "which part of this frame was slow" instead of requiring a timestamp correlation.
        let attrs = vec![KeyValue::new(sc::A_FRAME_ID, self.frame_id as i64), KeyValue::new(sc::A_FRAME_BUDGET_MS, sc::FRAME_BUDGET_MS)];
        // The closing work runs inside the span so the frame's own events (a tearing suspect) land
        // on it. `frame_ms` is measured at the top of `finish_frame`, so the span duration stays the
        // paint time the report uses as its exact per-frame sample; the recording that follows costs
        // microseconds.
        deckcraft_telemetry::spans::frame_span(attrs, |_cx| {
            self.frame_body(ui);
            self.finish_frame(t0);
        });
    }

    /// Everything one painted frame does, inside its `deckcraft.frame` span.
    fn frame_body(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        // An interaction queued by real input reached this pass: its handling starts here (and, in
        // an immediate-mode UI, its frame is requested in the same tick). Control-channel
        // interactions were dispatched when their request ran and keep those timestamps.
        self.interaction_frame_started(now_ms());
        let t = theme::Tokens::get(&ctx);
        let build_start = now_ms();
        let shown = deckcraft_telemetry::spans::phase_span(sc::P_BUILD, || self.frame_build(ui));
        deckcraft_telemetry::instruments().frame_phase.record(now_ms() - build_start, &[KeyValue::new(sc::A_PHASE, sc::P_BUILD)]);
        if shown {
            self.ui_end(&ctx);
            return;
        }
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(t.chrome)).show(ui, |ui| {
            if self.backstage.is_some() {
                // File tab: the Backstage view replaces everything below the title bar.
                backstage::show(self, ui);
                return;
            }
            if self.session.active().is_none() {
                return;
            }
            match self.ui.view {
                ViewMode::Sorter => sorter::show(self, ui),
                ViewMode::NotesPage => sorter::notes_page(self, ui),
                ViewMode::Outline | ViewMode::Normal | ViewMode::Reading => {
                    if self.ui.pane.is_some() {
                        panes::show(self, ui);
                    }
                    if self.ui.view == ViewMode::Outline {
                        sorter::outline_pane(self, ui);
                    } else {
                        thumbs::show(self, ui);
                    }
                    egui::CentralPanel::default().frame(egui::Frame::NONE.fill(t.canvas)).show(ui, |ui| {
                        if self.ui.notes {
                            status::notes_pane(self, ui);
                        }
                        deckcraft_telemetry::spans::phase_span(sc::P_PRESENT, || {
                            let present_start = now_ms();
                            canvas::show(self, ui);
                            deckcraft_telemetry::instruments()
                                .frame_phase
                                .record(now_ms() - present_start, &[KeyValue::new(sc::A_PHASE, sc::P_PRESENT)]);
                        });
                    });
                }
            }
        });
        toast::note_missing_fonts(self);
        toast::show(self, &ctx);
        dialogs::show(self, &ctx);
        #[cfg(not(target_arch = "wasm32"))]
        updates_ui::show(self, &ctx);
        menus::palette(self, &ctx);
        for url in std::mem::take(&mut self.pending_urls) {
            ctx.open_url(egui::OpenUrl::new_tab(url));
        }
        if let Some(t) = self.pending_copy.take() {
            ctx.copy_text(t);
        }
        self.drain_requests();
        doc_window::show(self, &ctx);
        self.ui_end(&ctx);
    }

    /// The layout half of a frame: everything before the central panel paints, plus show mode,
    /// which paints the whole screen here. Returns true when show mode handled the frame (the
    /// early return it used to take now ends this function instead of the frame).
    fn frame_build(&mut self, ui: &mut egui::Ui) -> bool {
        let ctx = ui.ctx().clone();
        self.note_ui_text(&ctx);
        self.media.sync_video_textures(&ctx);
        canvas::trackpad_twist(self, &ctx);
        eyedropper::tick(self, &ctx);
        self.opening_ui(&ctx);
        if self.show.is_some() {
            self.anim_preview = None;
            show::ui(self, ui);
            doc_window::show(self, &ctx);
            return true;
        }
        self.sync_backstage();
        self.keyboard(&ctx);
        self.sync_backstage();
        ribbon::title_bar(self, ui);
        if self.backstage.is_none() && self.session.active().is_some() {
            ribbon::show(self, ui);
            status::status_bar(self, ui);
        }
        let now = ui.input(|i| i.time);
        if self.tick_anim_preview(now) {
            repaint::anim_preview(&ctx);
        }
        false
    }

    /// Close a painted frame: `Perf` (unchanged, for `ui.inspect` and `xtask bench`) plus the
    /// frame's OpenTelemetry instruments.
    pub(crate) fn finish_frame(&mut self, t0: f64) {
        let frame_ms = (now_ms() - t0).max(0.0);
        self.perf.frame_ms = frame_ms;
        let instruments = deckcraft_telemetry::instruments();
        instruments.frame_duration.record(frame_ms, &[]);
        // The interval between two frame starts — the same number the pacing verdict reads, so
        // `frame.delta` and `pacing.error` can never disagree about it. The first frame has no
        // predecessor and records no delta rather than a fake zero.
        let delta = self.last_frame_ms.map(|previous| (t0 - previous).max(0.0));
        if let Some(delta) = delta {
            instruments.frame_delta.record(delta, &[]);
            // Only an active frame can miss a vsync. A gap this large is the app being idle or
            // throttled (no repaint was requested), not a present that arrived late — classifying
            // it would push a ~1 s sample into the histogram's tail, where the p99 lives.
            if otel_pacing::should_classify(self.display_interval_ms, delta) {
                otel_pacing::record(&otel_pacing::pacing(self.display_interval_ms, delta, otel_pacing::present_mode()));
            }
        }
        self.last_frame_ms = Some(t0);
        // One exclusive class per frame, by severity: the report adds the counters up to the number
        // of janky frames, so a 60 ms frame must not also count as a dropped frame.
        if let Some(class) = otel_pacing::jank_class(frame_ms) {
            instruments.frame_janky.add(1, &[KeyValue::new(sc::A_JANK_CLASS, class)]);
        }
        // Startup is a load measure, not a per-frame one: only the first painted frame samples it.
        self.startup_first_frame();
        // This frame painted whatever input it consumed, so it is the frame that closes those
        // interactions. Do it after the frame's own instruments: the INP total ends at frame close
        // (the backend reports no presentation callback), and the frame's cost is not part of it.
        self.interaction_painted(now_ms());
    }

    pub fn open_url(&mut self, url: &str) {
        self.pending_urls.push(url.to_string());
    }
}
