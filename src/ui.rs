pub mod settings;
pub mod tray;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::audio::{self, RecordConfig, Sources, devices::DeviceChoice};
use crate::config::Settings;
use crate::telemetry::{Surface, Telemetry, events};

use tray::MenuAction;

/// An in-progress recording.
///
/// `RecordingHandle` owns the cpal streams, so it stays on the thread that
/// created it — which is the UI thread, since that is where the tray events
/// that start and stop it are handled.
struct Active {
    handle: audio::RecordingHandle,
    started: Instant,
    dir: PathBuf,
}

pub struct App {
    tray: tray::Tray,
    recording: Option<Active>,
    /// Devices are cached rather than re-enumerated per frame: listing walks
    /// the CoreAudio device list and copies strings across FFI every call.
    devices: Vec<settings::DeviceRow>,
    mic_sel: Option<String>,
    system_sel: Option<String>,
    status: settings::Status,
    settings: Settings,
    telemetry: Telemetry,
    /// For `app_exited`'s session length. `Instant` rather than wall clock
    /// because it is a duration, and the clock can jump.
    started: Instant,
    recordings_this_session: u32,
    /// The post-recording pass that is currently running, and the channel it
    /// reports on. One channel for all stages rather than one per stage: the
    /// passes run over a recording that is already saved, one at a time, so at
    /// most one is ever live — and a second channel would mean a second thing to
    /// poll, and a second way to forget to.
    ///
    /// The stage is held here rather than read back off `self.status`, which any
    /// other error can overwrite while a pass is running; the message about a
    /// failed pass has to name the right pass regardless.
    ///
    /// Off the egui thread because `RecordingHandle::stop` is called from it and
    /// a pass takes seconds on a long meeting — blocking here would freeze the
    /// window *and* the tray. Drained in `logic`, not `ui`: `logic` runs while
    /// the window is hidden, which is the normal case for a tray app, and it
    /// already re-arms its own repaint.
    processing: Option<(settings::Stage, std::sync::mpsc::Receiver<StageEvent>)>,
}

/// What a post-recording pass sends back.
///
/// An event stream rather than a single terminal value, because the passes do
/// not all take the same order of time: echo cancellation is tens of seconds and
/// has nothing useful to say in the middle, but transcribing a long meeting runs
/// for minutes, and a channel that yields exactly one message can only say
/// "still going" by saying nothing at all.
///
/// `pub` rather than private because this is the contract a stage's worker
/// fills in — and with the `aec` feature off nothing in the crate constructs
/// one, which would make a private enum dead code in a build CI compiles with
/// `-D warnings`.
pub enum StageEvent {
    /// How far the pass has got, in `0.0..=1.0`. Optional for a stage: one that
    /// cannot measure its own progress sends nothing until it is done.
    Progress(f32),
    /// The pass is over, one way or the other. The `Meta` is re-read from disk
    /// by the pass, so it carries whatever block the pass wrote for the status
    /// pane to show.
    Done {
        dir: PathBuf,
        result: Result<Box<audio::meta::Meta>, String>,
        props: Vec<crate::telemetry::Prop>,
    },
}

impl App {
    /// Settings and telemetry are constructed in [`run`] and handed in, rather
    /// than loaded here: `eframe::run_native` only calls this once the window
    /// exists, and a launch that never gets that far is exactly the failure
    /// worth hearing about.
    fn new(tray: tray::Tray, settings: Settings, telemetry: Telemetry) -> Self {
        let first_run = !settings.telemetry_notice_seen;

        let mut app = Self {
            tray,
            recording: None,
            devices: Vec::new(),
            mic_sel: None,
            system_sel: None,
            status: settings::Status::Idle,
            settings,
            telemetry,
            started: Instant::now(),
            recordings_this_session: 0,
            processing: None,
        };
        app.refresh_devices();

        // After `refresh_devices`, so the device summary is already known: the
        // most useful thing about a launch is what hardware it found.
        app.telemetry.track(
            events::APP_STARTED,
            &[
                ("is_first_run", first_run.into()),
                ("has_loopback_device", app.has_loopback_device().into()),
                ("device_count", app.devices.len().into()),
            ],
        );

        app
    }

    fn has_loopback_device(&self) -> bool {
        self.devices.iter().any(|d| d.can_loopback)
    }

    /// Device counts, as shapes rather than names.
    ///
    /// Device names are personal — "Nick's AirPods" is the normal case, not the
    /// exception — so only the counts leave the machine.
    fn device_props(&self) -> Vec<crate::telemetry::Prop> {
        vec![
            ("total", self.devices.len().into()),
            (
                "input_capable",
                self.devices
                    .iter()
                    .filter(|d| d.supports_input)
                    .count()
                    .into(),
            ),
            (
                "loopback_capable",
                self.devices
                    .iter()
                    .filter(|d| d.can_loopback)
                    .count()
                    .into(),
            ),
            (
                "has_default_output",
                self.devices.iter().any(|d| d.is_default_output).into(),
            ),
        ]
    }

    fn refresh_devices(&mut self) {
        match audio::devices::list_devices() {
            Ok(list) => {
                self.devices = list
                    .into_iter()
                    .map(|(_, info)| settings::DeviceRow {
                        id: info.id.clone(),
                        name: info.name.clone(),
                        supports_input: info.supports_input,
                        can_loopback: info.can_loopback(),
                        is_default_input: info.is_default_input,
                        is_default_output: info.is_default_output,
                    })
                    .collect();
                self.telemetry
                    .track(events::DEVICES_REFRESHED, &self.device_props());
            }
            Err(e) => {
                // Recoverable — the app keeps running with a stale list — so
                // this is reported as an event rather than an exception.
                self.telemetry.track(
                    events::DEVICE_LIST_FAILED,
                    &[("error_kind", e.kind().into())],
                );
                self.status = settings::Status::Error(format!("device list failed: {e}"));
            }
        }
    }

    fn start_recording(&mut self) {
        if self.recording.is_some() {
            return;
        }

        let dir = recordings_root().join(audio::meta::timestamp_dir_name());

        let config = RecordConfig {
            sources: Sources::Both,
            mic: choice(&self.mic_sel),
            system: choice(&self.system_sel),
            out_dir: dir.clone(),
            allow_duplex_system: false,
        };

        match audio::start(config) {
            Ok(handle) => {
                self.recording = Some(Active {
                    handle,
                    started: Instant::now(),
                    dir,
                });
                self.status = settings::Status::Recording;
                self.tray.set_recording(true);

                self.telemetry.track(
                    events::RECORDING_STARTED,
                    &[
                        ("mic_is_default", self.mic_sel.is_none().into()),
                        ("system_is_default", self.system_sel.is_none().into()),
                        ("has_loopback_device", self.has_loopback_device().into()),
                    ],
                );
            }
            Err(e) => {
                self.report_recording_failure("start", &e);
                // Surfaced in full rather than summarised: the failures that
                // matter here (duplex device, permission denial) each carry
                // their own remedy in the message.
                self.status = settings::Status::Error(e.to_string());
                self.tray.set_recording(false);
            }
        }
    }

    fn stop_recording(&mut self) {
        let Some(active) = self.recording.take() else {
            return;
        };
        self.tray.set_recording(false);
        self.status = match active.handle.stop() {
            Ok(meta) => {
                self.recordings_this_session += 1;
                self.telemetry
                    .track(events::RECORDING_COMPLETED, &events::recording_props(&meta));
                #[cfg(feature = "aec")]
                if self.start_processing(&active.dir, &meta) {
                    return;
                }
                settings::Status::Finished {
                    dir: active.dir,
                    meta: Box::new(meta),
                }
            }
            Err(e) => {
                self.report_recording_failure("stop", &e);
                settings::Status::Error(e.to_string())
            }
        };
    }

    /// Spawns the echo-cancellation pass, returning whether it started.
    ///
    /// Declines cheaply and silently when there is nothing to do — no point
    /// spinning up a thread to read two files and conclude that one of them is
    /// empty. `audio::process` re-checks all of this properly.
    #[cfg(feature = "aec")]
    fn start_processing(&mut self, dir: &std::path::Path, meta: &audio::meta::Meta) -> bool {
        if !self.settings.aec_enabled {
            return false;
        }
        let both_have_audio = [meta.mic.as_ref(), meta.system.as_ref()]
            .iter()
            .all(|t| t.is_some_and(|t| t.frames > 0));
        if !both_have_audio {
            return false;
        }

        let (tx, rx) = std::sync::mpsc::channel();
        let dir = dir.to_path_buf();
        let thread_dir = dir.clone();
        std::thread::spawn(move || {
            let options = audio::process::ProcessOptions::default();
            // `process::run` is one blocking call with nothing to report from
            // inside it, so this stage sends no `Progress` and the pane shows
            // its label alone — better than a percentage that never moves,
            // which reads as stuck.
            let event = match audio::process::run(&thread_dir, options) {
                Ok(report) => {
                    let props = events::aec_props(&report, false);
                    let meta = audio::meta::Meta::read(&thread_dir.join("meta.json"))
                        .map(Box::new)
                        .map_err(|e| e.to_string());
                    StageEvent::Done {
                        dir: thread_dir,
                        result: meta,
                        props,
                    }
                }
                Err(e) => StageEvent::Done {
                    dir: thread_dir,
                    result: Err(e.to_string()),
                    props: vec![("failed", true.into())],
                },
            };
            // A closed receiver means the app is shutting down, which is not an
            // error: the audio is already on disk and `jotter process` can redo
            // the pass.
            let _ = tx.send(event);
        });

        self.processing = Some((settings::Stage::Aec, rx));
        self.status = settings::Status::Processing {
            stage: settings::Stage::Aec,
            dir,
            progress: None,
        };
        true
    }

    /// Picks up whatever the running pass has sent, if anything.
    ///
    /// Stage-agnostic on purpose: a pass that wants a progress bar gets one by
    /// sending `Progress`, not by adding a field here and a poll site in
    /// `logic`.
    fn poll_processing(&mut self) {
        let Some((stage, rx)) = self.processing.as_ref() else {
            return;
        };
        let stage = *stage;
        let event = match rx.try_recv() {
            Ok(event) => event,
            // Disconnected without a `Done` means the thread panicked. Report
            // it rather than leaving the pane saying "Removing speaker echo…"
            // for the rest of the session.
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.processing = None;
                self.status = settings::Status::Error(format!(
                    "{} stopped unexpectedly",
                    stage.failure_label()
                ));
                return;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
        };

        match event {
            // Only the number moves: the stage and the folder are already on the
            // status, and a progress update says how far along the pass is, not
            // what it is working on.
            StageEvent::Progress(fraction) => {
                if let settings::Status::Processing { progress, .. } = &mut self.status {
                    *progress = Some(fraction);
                }
            }
            StageEvent::Done { dir, result, props } => {
                self.processing = None;
                self.telemetry.track(events::RECORDING_PROCESSED, &props);
                self.status = match result {
                    Ok(meta) => settings::Status::Finished { dir, meta },
                    // The recording itself is fine — only the extra pass failed
                    // — so the message says so rather than implying the audio is
                    // lost.
                    Err(e) => settings::Status::Error(format!(
                        "recording saved, but {} failed: {e}",
                        stage.failure_label()
                    )),
                };
            }
        }
    }

    /// Persist a change to the echo-cancellation preference.
    ///
    /// Simpler than [`Self::set_telemetry`]: there is no worker to notify, so no
    /// ordering subtlety. Read at *stop* time rather than start, which is why no
    /// `Settings` plumbing into `RecordConfig` is needed.
    fn set_aec(&mut self, on: bool) {
        self.settings.aec_enabled = on;
        self.persist_settings();
    }

    #[cfg(feature = "aec")]
    fn aec_view(&self) -> settings::AecView {
        settings::AecView {
            enabled: self.settings.aec_enabled,
            available: true,
        }
    }

    #[cfg(not(feature = "aec"))]
    fn aec_view(&self) -> settings::AecView {
        settings::AecView {
            enabled: false,
            available: false,
        }
    }

    /// Report a capture failure as both an event and an exception.
    ///
    /// Note what is *not* passed: `e.to_string()`. The `Display` impl embeds the
    /// device name and is written for the settings pane; `kind` and `cpal_kind`
    /// are the `&'static str` classifications meant to leave the machine.
    fn report_recording_failure(&self, phase: &'static str, e: &audio::capture::CaptureError) {
        let props: Vec<crate::telemetry::Prop> = vec![
            ("phase", phase.into()),
            ("error_kind", e.kind().into()),
            ("cpal_kind", e.cpal_kind().into()),
            ("permission_shaped", e.is_permission_shaped().into()),
            ("has_loopback_device", self.has_loopback_device().into()),
        ];

        self.telemetry.track(events::RECORDING_FAILED, &props);
        self.telemetry.report_error(e.kind(), e.cpal_kind(), &props);
    }

    fn toggle_recording(&mut self) {
        if self.recording.is_some() {
            self.stop_recording();
        } else {
            self.start_recording();
        }
    }

    fn elapsed(&self) -> Option<Duration> {
        self.recording.as_ref().map(|r| r.started.elapsed())
    }

    /// Poll the tray channels and act on whatever arrived.
    fn pump_tray(&mut self, ctx: &egui::Context) {
        if tray::handle_icon_events() {
            self.telemetry
                .track(events::SETTINGS_OPENED, &[("trigger", "tray_icon".into())]);
            show_window(ctx);
        }

        for action in tray::handle_menu_events() {
            self.telemetry.track(
                events::TRAY_MENU_CLICKED,
                &[("id", action.telemetry_id().into())],
            );

            match action {
                MenuAction::ToggleRecord => self.toggle_recording(),
                MenuAction::ShowSettings => {
                    self.telemetry
                        .track(events::SETTINGS_OPENED, &[("trigger", "tray_menu".into())]);
                    show_window(ctx);
                }
                MenuAction::Quit => {
                    // Stop first: dropping the process mid-stream leaves an
                    // unfinalized WAV with a placeholder RIFF header.
                    self.stop_recording();
                    // `process::exit` runs no destructors, so nothing else gets
                    // a chance — not `on_exit`, not `Drop`. Every buffered event
                    // would be lost here without an explicit drain.
                    self.finish_session("tray_quit");
                    std::process::exit(0);
                }
            }
        }
    }

    /// Record the end of the session and drain the telemetry queue.
    ///
    /// Bounded internally, so a dead network cannot hang a quit.
    fn finish_session(&mut self, reason: &'static str) {
        self.telemetry.track(
            events::APP_EXITED,
            &[
                ("reason", reason.into()),
                ("session_secs", self.started.elapsed().as_secs().into()),
                (
                    "recordings_this_session",
                    self.recordings_this_session.into(),
                ),
            ],
        );
        self.telemetry.shutdown();
    }

    /// Persist and apply a change to the telemetry preference.
    fn set_telemetry(&mut self, on: bool) {
        if on {
            // Order matters in both directions: opting in must reach the worker
            // before the event, or the event is dropped by the gate...
            self.telemetry.set_enabled(true);
            self.settings.telemetry_enabled = true;
            self.telemetry.track(events::TELEMETRY_OPTED_IN, &[]);
        } else {
            // ...and opting out must send the event first, for the same reason.
            // This is the last thing this install will send.
            self.telemetry.track(events::TELEMETRY_OPTED_OUT, &[]);
            self.telemetry.set_enabled(false);
            self.settings.telemetry_enabled = false;
        }

        self.persist_settings();
    }

    fn persist_settings(&mut self) {
        if let Err(e) = self.settings.save() {
            // Worth surfacing: silently failing to persist an opt-out would mean
            // the box quietly unticks itself on the next launch.
            self.status = settings::Status::Error(format!("could not save settings: {e}"));
        }
    }

    fn telemetry_view(&self) -> settings::TelemetryView {
        settings::TelemetryView {
            enabled: self.settings.telemetry_enabled,
            show_notice: !self.settings.telemetry_notice_seen,
            available: self.telemetry.is_configured(),
        }
    }
}

/// Where recordings are written.
///
/// Must be absolute: a macOS bundle's working directory is `/`, so a relative
/// path would try to write to `/recordings` and fail.
///
/// On macOS, Documents is TCC-gated, so the first recording triggers a one-time
/// "access files in your Documents folder" prompt. That is the deliberate trade
/// for putting the files somewhere findable; if denied, `create_dir_all` fails
/// and the error surfaces in the settings pane. Linux has no such gate.
pub fn recordings_root() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);

    // Linux desktops let the user relocate or localise Documents, so honour
    // XDG when it is set rather than assuming the English default exists.
    let documents = std::env::var_os("XDG_DOCUMENTS_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join("Documents"));

    documents.join("Jotter")
}

/// Open a path in the system file manager, creating it first if it does not
/// exist yet.
///
/// Without the create, "Open recordings folder" does nothing at all before the
/// first recording — which is exactly when someone is most likely to press it.
fn reveal_in_file_manager(path: &std::path::Path) {
    let _ = std::fs::create_dir_all(path);

    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        // Freedesktop standard, present on any desktop Linux that has a file
        // manager at all.
        "xdg-open"
    };

    let _ = std::process::Command::new(opener).arg(path).spawn();
}

fn choice(sel: &Option<String>) -> DeviceChoice {
    sel.clone().map_or(DeviceChoice::Default, DeviceChoice::Id)
}

fn show_window(ctx: &egui::Context) {
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
}

impl eframe::App for App {
    /// Runs even while the window is hidden, which `ui` does not.
    ///
    /// The app starts hidden and spends most of its life that way, so tray
    /// polling has to live here — in `ui` the menu would be dead exactly when
    /// it is the only interface the user has.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump_tray(ctx);
        // Here rather than in `ui` because a tray app spends most of its life
        // with the window hidden, and a pass whose result only lands when
        // someone opens the window would leave the status pane stale.
        self.poll_processing();

        // eframe only calls `logic` when a repaint is pending, so the polling
        // loop has to keep itself alive. Without this the tray stops
        // responding as soon as the window is hidden.
        //
        // Faster while a recording or a pass is live: the elapsed clock has to
        // tick, and the pane should update promptly when a pass reports.
        let interval = if self.recording.is_some() || self.processing.is_some() {
            200
        } else {
            500
        };
        ctx.request_repaint_after(Duration::from_millis(interval));
    }

    /// Finalize any in-progress recording before the process goes away.
    ///
    /// With a Dock icon the app is Cmd-Q-able, which bypasses the tray menu's
    /// Quit. An abandoned stream leaves a WAV whose RIFF header still holds a
    /// placeholder length, which many tools refuse to open — so the last hour
    /// of a meeting would be unreadable.
    fn on_exit(&mut self) {
        self.stop_recording();
        // The Cmd-Q path. The tray's Quit does this for itself, because
        // `process::exit` never reaches here.
        self.finish_session("window_quit");
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        let telemetry_view = self.telemetry_view();
        let aec_view = self.aec_view();
        let action = settings::draw(
            ui,
            settings::View {
                recording: self.recording.is_some(),
                elapsed: self.elapsed(),
                devices: &self.devices,
                mic_sel: &mut self.mic_sel,
                system_sel: &mut self.system_sel,
                status: &self.status,
                root: &recordings_root(),
                telemetry: telemetry_view,
                aec: aec_view,
            },
        );

        match action {
            Some(settings::Action::Toggle) => self.toggle_recording(),
            Some(settings::Action::RefreshDevices) => self.refresh_devices(),
            Some(settings::Action::Reveal(path)) => {
                // Distinguishes the always-present button from the link on a
                // just-finished recording: the second means the recording is
                // being used, the first only that it was looked for.
                let source = if path == recordings_root() {
                    "button"
                } else {
                    "saved_link"
                };
                self.telemetry.track(
                    events::RECORDINGS_FOLDER_OPENED,
                    &[("source", source.into())],
                );
                reveal_in_file_manager(&path);
            }
            Some(settings::Action::SetTelemetry(on)) => self.set_telemetry(on),
            Some(settings::Action::SetAec(on)) => self.set_aec(on),
            Some(settings::Action::DismissTelemetryNotice) => {
                self.settings.telemetry_notice_seen = true;
                self.persist_settings();
            }
            None => {}
        }

        // Close hides instead of quitting, so recording survives closing the
        // window. Quit is the tray menu's job.
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
    }
}

/// Launch the tray app. Blocks until the user quits.
///
/// eframe's types stay inside this module on purpose: `src/main.rs` is shared
/// with the CLI and has to compile with the `gui` feature off, so nothing in
/// its signature may mention eframe.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut native_options = eframe::NativeOptions::default();
    native_options.viewport = native_options
        .viewport
        .clone()
        // Shown on launch: the app has a Dock icon (LSUIElement is false), and
        // a Dock icon that bounces into nothing visible reads as a failed
        // launch. Closing the window hides it; the tray reopens it.
        .with_visible(true)
        .with_inner_size([420.0, 380.0]);

    let mut settings = Settings::load();
    let telemetry = Telemetry::init(Surface::Gui, &mut settings);

    let app_telemetry = telemetry.clone();
    let result = eframe::run_native(
        "Jotter",
        native_options,
        Box::new(move |_cc| {
            let tray = tray::build_tray(tray::load_icon());
            Ok(Box::new(App::new(tray, settings, app_telemetry)))
        }),
    );

    if result.is_err() {
        // The window never opened, so `App` — and with it `on_exit` — never
        // existed. On Linux this is the GPU/Wayland class of bug report, and it
        // is otherwise entirely invisible to us. The error itself stays here:
        // eframe's message can name a display or a device path.
        telemetry.track(events::APP_STARTED, &[("launch_failed", true.into())]);
        telemetry.report_error("eframe_launch_failed", None, &[]);
    }
    telemetry.shutdown();

    result?;
    Ok(())
}
