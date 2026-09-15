use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::audio::meta::Meta;
use eframe::egui;

/// One row of the device pickers. Flattened out of `DeviceInfo` so the UI does
/// not hold cpal `Device` handles alive across frames.
pub struct DeviceRow {
    pub id: Option<String>,
    pub name: String,
    pub supports_input: bool,
    pub can_loopback: bool,
    pub is_default_input: bool,
    pub is_default_output: bool,
}

pub enum Status {
    Idle,
    Recording,
    Finished { dir: PathBuf, meta: Box<Meta> },
    Error(String),
}

pub enum Action {
    Toggle,
    RefreshDevices,
    Reveal(PathBuf),
}

pub struct View<'a> {
    pub recording: bool,
    pub elapsed: Option<Duration>,
    pub devices: &'a [DeviceRow],
    pub mic_sel: &'a mut Option<String>,
    pub system_sel: &'a mut Option<String>,
    pub status: &'a Status,
    /// Root recordings folder, shown so the files are findable without
    /// hunting — the reason they moved out of Application Support.
    pub root: &'a Path,
}

pub fn draw(ui: &mut egui::Ui, view: View<'_>) -> Option<Action> {
    let mut action = None;

    egui::CentralPanel::default().show(ui, |ui| {
        ui.heading("Jotter");
        ui.add_space(4.0);

        ui.horizontal(|ui| {
            let label = if view.recording {
                "Stop Recording"
            } else {
                "Start Recording"
            };
            if ui.button(label).clicked() {
                action = Some(Action::Toggle);
            }

            if let Some(elapsed) = view.elapsed {
                let secs = elapsed.as_secs();
                ui.label(format!("● {:02}:{:02}", secs / 60, secs % 60));
            }
        });

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(4.0);

        // Devices cannot be changed underneath a running stream, so the
        // pickers are locked while recording rather than silently ignored.
        ui.add_enabled_ui(!view.recording, |ui| {
            device_picker(
                ui,
                "Microphone",
                view.mic_sel,
                view.devices,
                |d| d.supports_input,
                |d| d.is_default_input,
            );

            device_picker(
                ui,
                "System audio",
                view.system_sel,
                view.devices,
                |d| d.can_loopback,
                |d| d.is_default_output,
            );

            if ui.button("Refresh devices").clicked() {
                action = Some(Action::RefreshDevices);
            }
        });

        if !view.devices.iter().any(|d| d.can_loopback) {
            ui.add_space(4.0);
            ui.colored_label(
                egui::Color32::from_rgb(200, 120, 0),
                "No output-only device found. cpal can only tap system audio on a \
                 device that reports no input, so recording would capture a \
                 microphone instead.",
            );
        }

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(4.0);

        if let Some(revealed) = status(ui, view.status) {
            action = Some(Action::Reveal(revealed));
        }

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("Open recordings folder").clicked() {
                action = Some(Action::Reveal(view.root.to_path_buf()));
            }
            ui.label(
                egui::RichText::new(view.root.display().to_string())
                    .small()
                    .weak(),
            );
        });
    });

    action
}

fn device_picker(
    ui: &mut egui::Ui,
    label: &str,
    selection: &mut Option<String>,
    devices: &[DeviceRow],
    eligible: impl Fn(&DeviceRow) -> bool,
    is_default: impl Fn(&DeviceRow) -> bool,
) {
    let current = selection
        .as_ref()
        .and_then(|id| devices.iter().find(|d| d.id.as_ref() == Some(id)))
        .map(|d| d.name.clone())
        .unwrap_or_else(|| "Automatic".to_string());

    ui.horizontal(|ui| {
        ui.label(label);
        egui::ComboBox::from_id_salt(label)
            .selected_text(current)
            .show_ui(ui, |ui| {
                ui.selectable_value(selection, None, "Automatic");
                for device in devices.iter().filter(|d| eligible(d)) {
                    let Some(id) = &device.id else { continue };
                    let name = if is_default(device) {
                        format!("{} (system default)", device.name)
                    } else {
                        device.name.clone()
                    };
                    ui.selectable_value(selection, Some(id.clone()), name);
                }
            });
    });
}

/// Returns a path if the user asked to reveal it in Finder.
fn status(ui: &mut egui::Ui, status: &Status) -> Option<PathBuf> {
    let mut reveal = None;

    match status {
        Status::Idle => {
            ui.label("Idle.");
        }
        Status::Recording => {
            ui.label("Recording…");
        }
        Status::Error(message) => {
            ui.colored_label(egui::Color32::from_rgb(220, 80, 80), message);
        }
        Status::Finished { dir, meta } => {
            ui.horizontal(|ui| {
                ui.label("Saved:");
                if ui.link(dir.display().to_string()).clicked() {
                    reveal = Some(dir.clone());
                }
            });
            ui.label(format!("Duration: {:.1}s", meta.duration_secs()));

            for (label, track) in [("Mic", &meta.mic), ("System", &meta.system)] {
                let Some(track) = track else { continue };
                let secs = track.frames as f64 / track.sample_rate.max(1) as f64;
                ui.label(format!(
                    "{label}: {secs:.1}s  {} Hz  {}",
                    track.sample_rate, track.device_name
                ));

                // A track that produced no frames is the signature of a
                // permission denial or an idle device, and is otherwise
                // indistinguishable from a successful recording.
                if track.frames == 0 {
                    ui.colored_label(
                        egui::Color32::from_rgb(220, 80, 80),
                        format!("  {label} captured no audio — check permissions."),
                    );
                }
                if track.stream_errors > 0 {
                    ui.colored_label(
                        egui::Color32::from_rgb(200, 120, 0),
                        format!("  {label}: {} stream error(s)", track.stream_errors),
                    );
                }
            }
        }
    }

    reveal
}
