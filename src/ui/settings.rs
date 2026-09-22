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
    /// A post-recording pass is running over a recording that has already been
    /// saved. A separate state from `Recording` because the audio is safe on
    /// disk by this point — nothing is at risk if the app is quit.
    ///
    /// One variant for all stages rather than one per stage: the stages differ
    /// only in what this pane calls them, so a variant each would mean another
    /// arm in every `match` on `Status` for no new information.
    Processing {
        stage: Stage,
        dir: PathBuf,
        /// How far along, for a stage that can measure it. `None` covers both
        /// "not started reporting yet" and "this stage never reports", which
        /// render the same: the label without a number.
        progress: Option<f32>,
    },
    Finished {
        dir: PathBuf,
        meta: Box<Meta>,
    },
    Error(String),
}

/// Which post-recording pass is running.
///
/// Echo cancellation, transcription and diarization so far; summarization is
/// the next one this shape exists for. The wording lives here rather than at the
/// call site so that a stage names itself once, in the module that owns the
/// pane's text.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Aec,
    Transcribe,
    Diarize,
}

impl Stage {
    /// What the pane calls the pass while it runs.
    pub fn running_label(self) -> &'static str {
        match self {
            Stage::Aec => "Removing speaker echo…",
            Stage::Transcribe => "Transcribing…",
            Stage::Diarize => "Identifying speakers…",
        }
    }

    /// How the pass is named when it fails, in "recording saved, but {} failed".
    /// A noun phrase, not a sentence, and deliberately narrow: the recording
    /// itself is intact, and the message must not imply otherwise.
    pub fn failure_label(self) -> &'static str {
        match self {
            Stage::Aec => "echo removal",
            Stage::Transcribe => "transcription",
            Stage::Diarize => "speaker identification",
        }
    }

    /// The telemetry event this pass's outcome is reported as.
    ///
    /// Here rather than at the call site so that adding a stage cannot silently
    /// report it as an existing one: the `match` is exhaustive, and a new
    /// variant fails to compile until it has been given a name of its own.
    pub fn event(self) -> &'static str {
        match self {
            Stage::Aec => crate::telemetry::events::RECORDING_PROCESSED,
            Stage::Transcribe => crate::telemetry::events::RECORDING_TRANSCRIBED,
            Stage::Diarize => crate::telemetry::events::RECORDING_DIARIZED,
        }
    }
}

pub enum Action {
    Toggle,
    RefreshDevices,
    Reveal(PathBuf),
    /// The user ticked or unticked the telemetry checkbox.
    ///
    /// An `Action` rather than a `&mut bool` through `View` — unlike the device
    /// pickers, this has to be written to disk and pushed to the telemetry
    /// worker, and those belong to `App`, not to a widget.
    SetTelemetry(bool),
    DismissTelemetryNotice,
    /// The user ticked or unticked the echo-cancellation checkbox.
    ///
    /// An `Action` for the same reason as [`Action::SetTelemetry`]: it is
    /// written to disk, and the settings file belongs to `App`, not to a widget.
    /// That is what distinguishes both from the device pickers, which are
    /// `&mut` on `View` because they are not persisted at all.
    SetAec(bool),
    /// The user ticked or unticked the transcription checkbox. An `Action` for
    /// the same reason as [`Action::SetAec`].
    SetTranscribe(bool),
    /// The user ticked or unticked the speaker-identification checkbox. An
    /// `Action` for the same reason as [`Action::SetAec`].
    SetDiarize(bool),
    /// The user changed how many people to expect on a call. An `Action` for
    /// the same reason as [`Action::SetAec`], and the only one carrying a
    /// number: unlike every other setting here, this pass cannot run at all
    /// until it has one.
    SetDiarizeSpeakers(u8),
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
    pub telemetry: TelemetryView,
    pub aec: AecView,
    pub transcribe: TranscribeView,
    pub diarize: DiarizeView,
}

/// Everything the speaker-identification toggle needs to render.
#[derive(Clone, Copy)]
pub struct DiarizeView {
    /// The stored preference — what the checkbox shows.
    pub enabled: bool,
    /// Whether this build can diarize at all.
    pub available: bool,
    /// Whether both speaker models have been downloaded.
    pub models_ready: bool,
    /// People expected on a call, or `0` for not set — at which point the pass
    /// declines. Carried into the view because the pane has to show the number
    /// *and* explain that leaving it at zero stops the feature working.
    pub speakers: u8,
    /// Whether transcription is switched on.
    ///
    /// Not a third kind of unavailability, and deliberately not a reason to
    /// disable the checkbox: this pass labels a transcript, so with
    /// transcription off it has nothing to do — but that is one tick away in the
    /// same pane, and greying out the box would leave someone hunting for the
    /// reason. It is said in the detail line instead.
    pub transcribe_enabled: bool,
}

/// Everything the transcription toggle needs to render.
#[derive(Clone, Copy)]
pub struct TranscribeView {
    /// The stored preference — what the checkbox shows.
    pub enabled: bool,
    /// Whether this build can transcribe at all.
    pub available: bool,
    /// Whether the speech model has been downloaded.
    ///
    /// Separate from `available` because the two failures need different
    /// sentences: a build compiled without transcription is nothing the user
    /// can fix, and a missing model is one command away. Ticking the box with
    /// no model is allowed — the pass will decline and say why — but the pane
    /// should say so first rather than let someone discover it after a meeting.
    pub model_ready: bool,
}

/// Everything the processing section needs to render.
#[derive(Clone, Copy)]
pub struct AecView {
    /// The stored preference — what the checkbox shows.
    pub enabled: bool,
    /// Whether this build can cancel echo at all. False without the `aec`
    /// feature, in which case the checkbox is shown disabled rather than
    /// hidden — the same reasoning as [`TelemetryView::available`].
    pub available: bool,
}

/// Everything the privacy section needs to render.
#[derive(Clone, Copy)]
pub struct TelemetryView {
    /// The stored preference — what the checkbox shows.
    pub enabled: bool,
    /// Whether the first-run notice is still pending.
    pub show_notice: bool,
    /// Whether this build can collect anything at all. False for any build
    /// without an API key or without the `telemetry` feature, in which case the
    /// checkbox is shown disabled rather than hidden: a privacy control that
    /// vanishes is more unsettling than one that is visibly inapplicable.
    pub available: bool,
}

pub fn draw(ui: &mut egui::Ui, view: View<'_>) -> Option<Action> {
    let mut action = None;

    egui::CentralPanel::default().show(ui, |ui| {
        // Scrollable because the privacy section is last and `CaptureError`'s
        // messages are several lines of remediation text. Without this, a failed
        // recording pushes the telemetry opt-out below the bottom of a 380px
        // window — putting the control out of reach exactly when the app has
        // just gone wrong, which is when someone is most likely to want it.
        egui::ScrollArea::vertical().show(ui, |ui| {
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

            // In the order the passes actually run, so the pane reads as the
            // pipeline it describes: echo removal cleans the mic track,
            // transcription reads whichever track that left behind, and speaker
            // identification labels what transcription wrote.
            ui.add_space(8.0);
            if let Some(chosen) = processing(ui, view.aec) {
                action = Some(chosen);
            }

            ui.add_space(8.0);
            if let Some(chosen) = transcription(ui, view.transcribe) {
                action = Some(chosen);
            }

            ui.add_space(8.0);
            if let Some(chosen) = speakers(ui, view.diarize) {
                action = Some(chosen);
            }

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

            ui.add_space(8.0);
            ui.separator();
            ui.add_space(4.0);

            if let Some(privacy) = privacy(ui, view.telemetry) {
                action = Some(privacy);
            }
        });
    });

    action
}

/// The echo-cancellation toggle.
///
/// Worth spelling out in the UI that the original is untouched: "remove" sounds
/// destructive, and someone recording a meeting they cannot re-record wants to
/// know that before they find out.
fn processing(ui: &mut egui::Ui, view: AecView) -> Option<Action> {
    let mut action = None;

    ui.add_enabled_ui(view.available, |ui| {
        let mut enabled = view.enabled;
        if ui
            .checkbox(&mut enabled, "Remove speaker echo from my microphone track")
            .changed()
        {
            action = Some(Action::SetAec(enabled));
        }

        let detail = if view.available {
            "On by default. Only matters on speakers — with headphones there is \
             no echo to remove. Writes a second file; your original recording is \
             never modified."
        } else {
            "This build was compiled without echo cancellation."
        };
        ui.label(egui::RichText::new(detail).small().weak());
    });

    action
}

/// The transcription toggle.
///
/// Off by default, unlike echo removal, and the pane has to say why or the
/// asymmetry looks arbitrary: this one needs a download before it can do
/// anything. The box stays tickable without the model — the pass declines and
/// records the reason rather than failing — but someone should learn that here
/// rather than after a meeting they cannot re-record.
fn transcription(ui: &mut egui::Ui, view: TranscribeView) -> Option<Action> {
    let mut action = None;

    ui.add_enabled_ui(view.available, |ui| {
        let mut enabled = view.enabled;
        if ui
            .checkbox(&mut enabled, "Transcribe recordings when they finish")
            .changed()
        {
            action = Some(Action::SetTranscribe(enabled));
        }

        let detail = if !view.available {
            "This build was compiled without transcription."
        } else if view.model_ready {
            "Runs on your machine — nothing is uploaded. Your microphone and \
             everyone else's audio are transcribed separately, so the transcript \
             already knows who was who."
        } else {
            "Needs a speech model, which is not downloaded yet. Run \
             `jotter models pull` in a terminal (about 630 MB, once). Until then \
             this is ticked but every recording will say the model is missing."
        };
        ui.label(egui::RichText::new(detail).small().weak());
    });

    action
}

/// The speaker-identification toggle, and the count it cannot run without.
///
/// Four things can stop this working and they need four different sentences,
/// because only some are something the user can act on here. The box stays
/// tickable in every case — the pass declines and records why, as every stage
/// does — but the pane says so first.
///
/// The count sits next to the checkbox rather than behind an "advanced"
/// disclosure, because it is not a refinement: at zero this feature does
/// nothing at all. A control that is required to make the thing above it work
/// belongs beside it.
fn speakers(ui: &mut egui::Ui, view: DiarizeView) -> Option<Action> {
    let mut action = None;

    ui.add_enabled_ui(view.available, |ui| {
        let mut enabled = view.enabled;
        if ui
            .checkbox(&mut enabled, "Identify who is speaking")
            .changed()
        {
            action = Some(Action::SetDiarize(enabled));
        }

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("People on the call:").small());
            let mut count = view.speakers;
            // Capped well above any meeting this is useful for. The ceiling is
            // not a judgement about meeting sizes, it is to keep a dragged
            // value from silently becoming 200.
            if ui
                .add(
                    egui::DragValue::new(&mut count)
                        .speed(0.1)
                        .range(0..=32)
                        .custom_formatter(|n, _| {
                            if n < 1.0 {
                                "not set".to_string()
                            } else {
                                format!("{n:.0}")
                            }
                        }),
                )
                .changed()
            {
                action = Some(Action::SetDiarizeSpeakers(count));
            }
            ui.label(egui::RichText::new("including you").small().weak());
        });

        let detail = if !view.available {
            "This build was compiled without speaker identification."
        } else if !view.models_ready {
            "Needs two speaker models, which are not downloaded yet. Run \
             `jotter models pull` in a terminal (about 44 MB on top of the \
             speech model). Until then this is ticked but every recording will \
             say the models are missing."
        } else if !view.transcribe_enabled {
            "Nothing to label until recordings are transcribed — tick \
             Transcribe above, and this will run straight after it."
        } else if view.speakers == 0 {
            // Stated as a requirement, not as a preference, because that is
            // what it is. Asking is the honest option: working the number out
            // from the audio was tried and got it badly wrong on exactly the
            // meetings people actually have.
            "Set the number of people above. Counting them from the audio is \
             unreliable once people talk over each other, so Jotter asks \
             instead of guessing — and does nothing until you say."
        } else {
            // The asymmetry is the first thing anyone notices in the file, and
            // it is a feature rather than a gap: your own track needs no
            // guessing, so it gets none.
            "Runs on your machine — nothing is uploaded. Everyone else's audio \
             is split into speaker_01, speaker_02 and so on. Your own track is \
             already known to be you, so it is left unlabelled rather than \
             guessed at."
        };
        ui.label(egui::RichText::new(detail).small().weak());
    });

    action
}

/// The telemetry notice and opt-out.
///
/// Placed last: it is a thing you go looking for once, not something to put
/// between the user and the record button.
fn privacy(ui: &mut egui::Ui, view: TelemetryView) -> Option<Action> {
    let mut action = None;

    if view.show_notice && view.available {
        // A frame rather than a plain label so first-run text reads as a notice
        // to acknowledge, not as more settings chrome to skim past.
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label(
                "Jotter sends anonymous usage and crash reports, which is how recording \
                 failures get found and fixed.",
            );
            ui.label(
                egui::RichText::new("Never audio, file names, folder names, or device names.")
                    .small()
                    .weak(),
            );
            if ui.button("Got it").clicked() {
                action = Some(Action::DismissTelemetryNotice);
            }
        });
        ui.add_space(4.0);
    }

    ui.add_enabled_ui(view.available, |ui| {
        let mut enabled = view.enabled;
        if ui
            .checkbox(&mut enabled, "Send anonymous usage and crash reports")
            .changed()
        {
            action = Some(Action::SetTelemetry(enabled));
        }
    });

    let detail = if view.available {
        "App version, OS, how long recordings run, and whether a track captured \
         audio. Never audio, file names, or device names."
    } else {
        // Either built without the `telemetry` feature or without an API key —
        // a self-built or packaged binary. Say so, rather than showing a dead
        // checkbox with no explanation.
        "This build has telemetry compiled out and sends nothing."
    };
    ui.label(egui::RichText::new(detail).small().weak());

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
        Status::Processing {
            stage,
            dir,
            progress,
        } => {
            ui.horizontal(|ui| {
                ui.label("Saved:");
                if ui.link(dir.display().to_string()).clicked() {
                    reveal = Some(dir.clone());
                }
            });
            ui.label(match progress {
                Some(fraction) => format!("{} {:.0}%", stage.running_label(), fraction * 100.0),
                None => stage.running_label().to_string(),
            });
            ui.label(
                egui::RichText::new("The recording is already safe on disk.")
                    .small()
                    .weak(),
            );
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

            // The same "say what went wrong rather than looking successful"
            // reasoning as the zero-frames warning above: a pass that declined,
            // or one that ran and achieved nothing, must not be silent about it.
            if let Some(aec) = meta.aec.as_ref() {
                ui.add_space(4.0);
                match (&aec.bypassed, aec.erle_db) {
                    (Some(_), _) => {
                        ui.label(
                            egui::RichText::new(
                                "Echo removal skipped — the microphone track is unchanged.",
                            )
                            .small()
                            .weak(),
                        );
                    }
                    (None, Some(erle)) => {
                        ui.label(format!("Echo removed: {erle:.0} dB"));
                        // Negative means the canceller cut into the user's own
                        // voice, which is the one outcome worth a warning: the
                        // cancelled track is then worse than the original.
                        if aec.near_gain_db.is_some_and(|g| g < -1.0) {
                            ui.colored_label(
                                egui::Color32::from_rgb(200, 120, 0),
                                "  Your voice was affected too — the original mic.wav \
                                 is still the safe choice.",
                            );
                        }
                    }
                    (None, None) => {
                        ui.label(
                            egui::RichText::new(
                                "Echo removal ran, but there was no system audio to \
                                 measure against.",
                            )
                            .small()
                            .weak(),
                        );
                    }
                }
            }
        }
    }

    reveal
}
