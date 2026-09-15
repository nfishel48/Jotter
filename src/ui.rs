pub mod settings;
pub mod tray;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::audio::{self, devices::DeviceChoice, RecordConfig, Sources};

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
}

impl App {
    fn new(tray: tray::Tray) -> Self {
        let mut app = Self {
            tray,
            recording: None,
            devices: Vec::new(),
            mic_sel: None,
            system_sel: None,
            status: settings::Status::Idle,
        };
        app.refresh_devices();
        app
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
            }
            Err(e) => self.status = settings::Status::Error(format!("device list failed: {e}")),
        }
    }

    fn start_recording(&mut self) {
        if self.recording.is_some() {
            return;
        }

        let dir = recordings_root().join(recording_dir_name());

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
            }
            Err(e) => {
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
            Ok(meta) => settings::Status::Finished {
                dir: active.dir,
                meta: Box::new(meta),
            },
            Err(e) => settings::Status::Error(e.to_string()),
        };
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
            show_window(ctx);
        }

        for action in tray::handle_menu_events() {
            match action {
                MenuAction::ToggleRecord => self.toggle_recording(),
                MenuAction::ShowSettings => show_window(ctx),
                MenuAction::Quit => {
                    // Stop first: dropping the process mid-stream leaves an
                    // unfinalized WAV with a placeholder RIFF header.
                    self.stop_recording();
                    std::process::exit(0);
                }
            }
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

/// Folder name for a new recording, e.g. `2026-09-15_14-32-08`.
///
/// Local time, and zero-padded so lexical order matches chronological order.
fn recording_dir_name() -> String {
    chrono::Local::now().format("%Y-%m-%d_%H-%M-%S").to_string()
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

        // eframe only calls `logic` when a repaint is pending, so the polling
        // loop has to keep itself alive. Without this the tray stops
        // responding as soon as the window is hidden.
        ctx.request_repaint_after(Duration::from_millis(if self.recording.is_some() {
            200
        } else {
            500
        }));
    }

    /// Finalize any in-progress recording before the process goes away.
    ///
    /// With a Dock icon the app is Cmd-Q-able, which bypasses the tray menu's
    /// Quit. An abandoned stream leaves a WAV whose RIFF header still holds a
    /// placeholder length, which many tools refuse to open — so the last hour
    /// of a meeting would be unreadable.
    fn on_exit(&mut self) {
        self.stop_recording();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

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
            },
        );

        match action {
            Some(settings::Action::Toggle) => self.toggle_recording(),
            Some(settings::Action::RefreshDevices) => self.refresh_devices(),
            Some(settings::Action::Reveal(path)) => reveal_in_file_manager(&path),
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

pub fn run(icon: &std::path::Path, native_options: eframe::NativeOptions) -> eframe::Result<()> {
    let icon = icon.to_path_buf();
    eframe::run_native(
        "Jotter",
        native_options,
        Box::new(move |_cc| {
            let tray = tray::build_tray(tray::load_icon(&icon));
            Ok(Box::new(App::new(tray)))
        }),
    )
}
