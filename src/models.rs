//! The model catalogue: what has to be on disk before a stage can run.
//!
//! Speech models are too large to ship inside the binary — Parakeet's encoder
//! alone is 652 MB, against a whole `jotter` binary of a few tens — so they are
//! data the user acquires once and the passes then find. This module owns both
//! halves of that: the catalogue of what exists, and the rule for where it
//! lives on disk.
//!
//! Deliberately **not** under `audio`. `docs/ARCHITECTURE.md`'s one rule is that
//! `audio` knows nothing about the layers above it, and a model catalogue that
//! will grow a downloader is not audio code. `audio::transcribe` depends on this
//! module; nothing here knows a stage exists.
//!
//! **Nothing here touches the network.** Fetching is a separate, explicit act —
//! see `models::fetch` and `jotter models pull`. A stage that wanted a model and
//! did not find one *declines*; it does not quietly start a 660 MB download
//! because a meeting ended. That keeps the property the `telemetry` feature was
//! built around: with reporting off, this binary opens no sockets at all unless
//! the user asked it to.
//!
//! Adding a model is a `const` in this file and nothing else — that is the whole
//! point of the shape. A second engine (Whisper, SenseVoice) is a new [`Family`]
//! arm and a new catalogue entry, not new code in the stage.

use std::path::{Path, PathBuf};

pub mod fetch;

/// What a file is *for*, as opposed to what it is called.
///
/// Filenames are the model publisher's business and vary between repositories
/// (`encoder.int8.onnx`, `encoder.fp16.onnx`, `model.onnx`); the role is the
/// contract the stage codes against, so a re-quantised drop-in is a catalogue
/// edit rather than a code change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Encoder,
    Decoder,
    Joiner,
    /// The token inventory the decoder's output indexes into.
    Tokens,
    /// The voice-activity model, which is a model in its own right rather than
    /// part of the recogniser.
    Vad,
    /// Speaker segmentation: which regions of a recording contain speech, and
    /// where one voice gives way to another. Says *that* the speaker changed,
    /// never who it changed to.
    Segmentation,
    /// The speaker embedding model, which turns a stretch of one voice into a
    /// vector that can be compared with another. This is the half that decides
    /// two segments are the same person.
    SpeakerEmbedding,
}

/// Which runtime shape a model has, and therefore how a stage must configure it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// Encoder/decoder/joiner, configured as a sherpa-onnx offline transducer.
    /// NVIDIA's Parakeet is one of these.
    NemoTransducer,
    /// Voice activity detection, not recognition.
    Vad,
    /// A pyannote segmentation model, configured as the segmentation half of a
    /// sherpa-onnx offline speaker diarizer.
    SpeakerSegmentation,
    /// A speaker embedding extractor, the other half of that pair.
    SpeakerEmbedding,
}

/// One file belonging to a model.
///
/// `sha256` and `bytes` are both recorded because they answer different
/// questions cheaply. The size is checkable with a `stat` on every stage run;
/// the hash costs a full read and so belongs to the download, where a truncated
/// or tampered file is the risk worth paying for.
#[derive(Debug, Clone, Copy)]
pub struct Asset {
    pub role: Role,
    /// File name on disk, inside the model's own directory.
    pub name: &'static str,
    pub url: &'static str,
    /// Lowercase hex SHA-256.
    pub sha256: &'static str,
    pub bytes: u64,
}

/// A model as the catalogue knows it.
#[derive(Debug, Clone, Copy)]
pub struct Model {
    /// Stable identifier, used as the directory name, recorded in `meta.json`,
    /// and accepted by `--model`. Never free-form: it reaches telemetry.
    pub id: &'static str,
    /// The inference engine that can run it.
    pub engine: &'static str,
    pub family: Family,
    /// One-line description for `jotter models list`.
    pub description: &'static str,
    pub assets: &'static [Asset],
}

/// NVIDIA Parakeet TDT 0.6b v2, int8-quantised, as repackaged for sherpa-onnx.
///
/// The default because it is the strongest English model that fits the budget:
/// a 0.6b transducer quantised to int8 runs comfortably on a laptop CPU, which
/// is where this has to run — a meeting transcript that requires a GPU is a
/// transcript most users never get.
pub const PARAKEET_TDT_0_6B_V2_INT8: Model = Model {
    id: "parakeet-tdt-0.6b-v2-int8",
    engine: "sherpa-onnx",
    family: Family::NemoTransducer,
    description: "NVIDIA Parakeet TDT 0.6b v2 (English), int8",
    assets: &[
        Asset {
            role: Role::Encoder,
            name: "encoder.int8.onnx",
            url: "https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8/resolve/main/encoder.int8.onnx",
            sha256: "a32b12d17bbbc309d0686fbbcc2987b5e9b8333a7da83fa6b089f0a2acd651ab",
            bytes: 652_184_296,
        },
        Asset {
            role: Role::Decoder,
            name: "decoder.int8.onnx",
            url: "https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8/resolve/main/decoder.int8.onnx",
            sha256: "b6bb64963457237b900e496ee9994b59294526439fbcc1fecf705b31a15c6b4e",
            bytes: 7_257_753,
        },
        Asset {
            role: Role::Joiner,
            name: "joiner.int8.onnx",
            url: "https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8/resolve/main/joiner.int8.onnx",
            sha256: "7946164367946e7f9f29a122407c3252b680dbae9a51343eb2488d057c3c43d2",
            bytes: 1_739_080,
        },
        Asset {
            role: Role::Tokens,
            name: "tokens.txt",
            url: "https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8/resolve/main/tokens.txt",
            sha256: "ec182b70dd42113aff6c5372c75cac58c952443eb22322f57bbd7f53977d497d",
            bytes: 9_384,
        },
    ],
};

/// Silero VAD, as published with sherpa-onnx's ASR models.
///
/// A separate catalogue entry rather than a fifth asset of the recogniser: it is
/// not Parakeet's, every future recognition model needs the same one, and
/// bundling it into each would mean downloading it again per model.
pub const SILERO_VAD: Model = Model {
    id: "silero-vad",
    engine: "sherpa-onnx",
    family: Family::Vad,
    description: "Silero voice activity detection",
    assets: &[Asset {
        role: Role::Vad,
        name: "silero_vad.onnx",
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx",
        sha256: "9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6",
        bytes: 643_854,
    }],
};

/// Pyannote segmentation 3.0, exported to ONNX for sherpa-onnx.
///
/// Diarization needs two models rather than one, and they are separate
/// catalogue entries for the reason [`SILERO_VAD`] is: they are independently
/// replaceable. This one finds the speaker *changes* — a better segmenter can
/// be swapped in without touching the embeddings, and vice versa.
pub const PYANNOTE_SEGMENTATION_3_0: Model = Model {
    id: "pyannote-segmentation-3-0",
    engine: "sherpa-onnx",
    family: Family::SpeakerSegmentation,
    description: "Pyannote 3.0 speaker segmentation",
    assets: &[Asset {
        role: Role::Segmentation,
        name: "model.onnx",
        url: "https://huggingface.co/csukuangfj/sherpa-onnx-pyannote-segmentation-3-0/resolve/main/model.onnx",
        sha256: "220ad67ca923bef2fa91f2390c786097bf305bceb5e261d4af67b38e938e1079",
        bytes: 5_992_913,
    }],
};

/// NVIDIA TitaNet small, as repackaged for sherpa-onnx.
///
/// Chosen by measurement, and it is worth recording what the measurement was,
/// because the obvious candidate loses it. On a control of three distinct
/// LibriSpeech speakers read back to back — clean studio audio, no overlap, no
/// crosstalk, the easiest separation there is — this model puts each block in
/// its own cluster. WeSpeaker CAM++ (the model the original prototype used),
/// WeSpeaker ResNet34-LM and 3D-Speaker CAM++ all merged two of the three.
///
/// English, matching the default recogniser. 38 MB, against CAM++'s 28: the
/// extra 10 MB is the difference between a feature that works and one that
/// quietly attributes two people to one name, which is the expensive kind of
/// wrong here — a reader cannot tell it happened.
pub const NEMO_EN_TITANET_SMALL: Model = Model {
    id: "nemo-en-titanet-small",
    engine: "sherpa-onnx",
    family: Family::SpeakerEmbedding,
    description: "NVIDIA TitaNet small speaker embeddings (English)",
    assets: &[Asset {
        role: Role::SpeakerEmbedding,
        name: "nemo_en_titanet_small.onnx",
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/nemo_en_titanet_small.onnx",
        sha256: "ad4a1802485d8b34c722d2a9d04249662f2ece5d28a7a039063ca22f515a789e",
        bytes: 40_257_283,
    }],
};

/// Every model this build knows how to fetch and run.
pub const CATALOGUE: &[&Model] = &[
    &PARAKEET_TDT_0_6B_V2_INT8,
    &SILERO_VAD,
    &PYANNOTE_SEGMENTATION_3_0,
    &NEMO_EN_TITANET_SMALL,
];

/// What the transcription stage uses unless told otherwise.
pub const DEFAULT_TRANSCRIPTION_MODEL: &Model = &PARAKEET_TDT_0_6B_V2_INT8;

/// The segmentation half of what the diarization stage uses unless told
/// otherwise.
pub const DEFAULT_SEGMENTATION_MODEL: &Model = &PYANNOTE_SEGMENTATION_3_0;

/// The embedding half of the same pair.
pub const DEFAULT_EMBEDDING_MODEL: &Model = &NEMO_EN_TITANET_SMALL;

/// Look a model up by [`Model::id`].
pub fn find(id: &str) -> Option<&'static Model> {
    CATALOGUE.iter().copied().find(|m| m.id == id)
}

/// Where models are kept.
///
/// Must be absolute, for the same reason `config::path` must be: a macOS
/// bundle's working directory is `/`.
///
/// On Linux this is `XDG_DATA_HOME`, **not** the `XDG_CONFIG_HOME` the settings
/// file uses. That is not an inconsistency: config is small, hand-edited and
/// routinely synced between machines, and dropping 660 MB of model weights into
/// a synced directory is a genuinely unpleasant surprise. macOS has one
/// Application Support directory for both, so models get a subdirectory of it.
pub fn models_root() -> PathBuf {
    // An escape hatch for tests and for anyone keeping models on another
    // volume — 660 MB is enough that "somewhere else" is a reasonable ask.
    if let Some(dir) = std::env::var_os("JOTTER_MODELS_DIR") {
        let dir = PathBuf::from(dir);
        if dir.is_absolute() {
            return dir;
        }
    }

    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);

    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Jotter/models")
    } else if cfg!(target_os = "windows") {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData/Local"))
            .join("Jotter/models")
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home.join(".local/share"))
            .join("jotter/models")
    }
}

/// A model whose every asset was found on disk, with the paths to them.
///
/// Constructed only by [`Model::resolve`], so holding one is evidence the files
/// existed a moment ago — which is as strong a guarantee as a filesystem
/// offers, and stops every call site re-checking.
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub model: &'static Model,
    paths: Vec<(Role, PathBuf)>,
}

impl ResolvedModel {
    /// The file filling `role`, if this model has one.
    pub fn path(&self, role: Role) -> Option<&Path> {
        self.paths
            .iter()
            .find(|(r, _)| *r == role)
            .map(|(_, p)| p.as_path())
    }
}

/// Why a model could not be used from disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssetProblem {
    Absent,
    /// Present but the wrong length. Nearly always an interrupted download from
    /// before writes went through a temporary sibling, or a half-copied model
    /// directory — either way the file is not the model and must not be loaded.
    WrongSize {
        found: u64,
        expected: u64,
    },
}

/// Everything wrong with a model's files, not merely the first thing.
///
/// Reporting one missing file at a time turns "fetch the model" into four
/// rounds of the same error, so the whole list is collected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingAssets {
    pub model_id: &'static str,
    pub problems: Vec<(&'static str, AssetProblem)>,
}

impl std::fmt::Display for MissingAssets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "model {} is not ready: ", self.model_id)?;
        for (i, (name, problem)) in self.problems.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            match problem {
                AssetProblem::Absent => write!(f, "{name} is missing")?,
                AssetProblem::WrongSize { found, expected } => {
                    write!(f, "{name} is {found} bytes, expected {expected}")?
                }
            }
        }
        write!(f, " — run `jotter models pull --model {}`", self.model_id)
    }
}

impl std::error::Error for MissingAssets {}

impl Model {
    /// This model's own directory inside [`models_root`].
    pub fn dir(&self) -> PathBuf {
        models_root().join(self.id)
    }

    /// Total download size, for a progress figure worth showing.
    pub fn bytes(&self) -> u64 {
        self.assets.iter().map(|a| a.bytes).sum()
    }

    /// Check every asset is present at its recorded size and hand back the paths.
    ///
    /// Size only, never the hash: this runs before every transcription, and
    /// re-reading 660 MB to prove it is still the file that was verified at
    /// download time would cost more than the transcription. The hash is checked
    /// where it earns its keep — see `fetch`.
    pub fn resolve(&'static self) -> Result<ResolvedModel, MissingAssets> {
        self.resolve_in(&self.dir())
    }

    /// [`Model::resolve`] against an explicit directory, which is what makes it
    /// testable without writing to the user's real model store.
    pub fn resolve_in(&'static self, dir: &Path) -> Result<ResolvedModel, MissingAssets> {
        let mut paths = Vec::with_capacity(self.assets.len());
        let mut problems = Vec::new();

        for asset in self.assets {
            let path = dir.join(asset.name);
            match std::fs::metadata(&path) {
                Ok(meta) if meta.len() == asset.bytes => paths.push((asset.role, path)),
                Ok(meta) => problems.push((
                    asset.name,
                    AssetProblem::WrongSize {
                        found: meta.len(),
                        expected: asset.bytes,
                    },
                )),
                Err(_) => problems.push((asset.name, AssetProblem::Absent)),
            }
        }

        if problems.is_empty() {
            Ok(ResolvedModel { model: self, paths })
        } else {
            Err(MissingAssets {
                model_id: self.id,
                problems,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique scratch directory; avoids a `tempfile` dependency, as
    /// `config::tests::scratch` does.
    fn scratch(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("jotter-models-{name}-{unique}"));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    /// Writes each asset at exactly its catalogue size, so `resolve_in` sees a
    /// complete model without a 660 MB download.
    fn lay_out(model: &Model, dir: &Path, skip: &[&str]) {
        for asset in model.assets {
            if skip.contains(&asset.name) {
                continue;
            }
            std::fs::write(dir.join(asset.name), vec![0u8; asset.bytes as usize]).expect("write");
        }
    }

    /// Small enough to fake in a test without allocating 660 MB.
    const TINY: Model = Model {
        id: "test-tiny",
        engine: "sherpa-onnx",
        family: Family::NemoTransducer,
        description: "fixture",
        assets: &[
            Asset {
                role: Role::Encoder,
                name: "encoder.onnx",
                url: "https://example.invalid/encoder.onnx",
                sha256: "00",
                bytes: 8,
            },
            Asset {
                role: Role::Tokens,
                name: "tokens.txt",
                url: "https://example.invalid/tokens.txt",
                sha256: "01",
                bytes: 4,
            },
        ],
    };

    #[test]
    fn a_complete_model_resolves_to_its_files() {
        let dir = scratch("complete");
        lay_out(&TINY, &dir, &[]);

        let resolved = TINY.resolve_in(&dir).expect("should resolve");
        assert_eq!(
            resolved.path(Role::Encoder),
            Some(dir.join("encoder.onnx").as_path())
        );
        assert_eq!(
            resolved.path(Role::Tokens),
            Some(dir.join("tokens.txt").as_path())
        );
        // A role this model does not have is absent, not an error.
        assert_eq!(resolved.path(Role::Joiner), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The bug this guards: reporting one missing file at a time, which turns
    /// "fetch the model" into as many rounds of the same error as there are
    /// assets.
    #[test]
    fn every_missing_asset_is_reported_at_once() {
        let dir = scratch("missing");

        let err = TINY.resolve_in(&dir).expect_err("nothing is there");
        assert_eq!(err.model_id, "test-tiny");
        assert_eq!(
            err.problems,
            vec![
                ("encoder.onnx", AssetProblem::Absent),
                ("tokens.txt", AssetProblem::Absent),
            ]
        );
        // And the message names the way out, not just the problem.
        assert!(err.to_string().contains("jotter models pull"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An interrupted download leaves a file that exists and is wrong. Loading
    /// it would fail somewhere deep inside onnxruntime with a message about the
    /// file format, which is a long way from "the download did not finish".
    #[test]
    fn a_truncated_asset_is_not_mistaken_for_a_present_one() {
        let dir = scratch("truncated");
        lay_out(&TINY, &dir, &["encoder.onnx"]);
        std::fs::write(dir.join("encoder.onnx"), b"half").expect("write");

        let err = TINY
            .resolve_in(&dir)
            .expect_err("truncated must not resolve");
        assert_eq!(
            err.problems,
            vec![(
                "encoder.onnx",
                AssetProblem::WrongSize {
                    found: 4,
                    expected: 8
                }
            )]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The catalogue is hand-written `const` data that reaches the network and
    /// `meta.json`, so the shape of it is worth asserting once: ids are what
    /// `--model` and telemetry use, and a duplicate would silently shadow.
    #[test]
    fn the_catalogue_is_well_formed() {
        for model in CATALOGUE {
            assert!(!model.assets.is_empty(), "{} has no assets", model.id);
            assert_eq!(find(model.id).map(|m| m.id), Some(model.id));

            for asset in model.assets {
                assert_eq!(
                    asset.sha256.len(),
                    64,
                    "{}/{} has a malformed hash",
                    model.id,
                    asset.name
                );
                assert!(
                    asset.sha256.chars().all(|c| c.is_ascii_hexdigit()),
                    "{}/{} has a non-hex hash",
                    model.id,
                    asset.name
                );
                assert!(
                    asset.url.starts_with("https://"),
                    "{}/{} is not fetched over TLS",
                    model.id,
                    asset.name
                );
                assert!(asset.bytes > 0, "{}/{} has no size", model.id, asset.name);
            }
        }

        let ids: Vec<_> = CATALOGUE.iter().map(|m| m.id).collect();
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(ids.len(), unique.len(), "duplicate model id in {ids:?}");
    }

    /// The four roles the transcription stage asks a transducer for. Getting one
    /// of these wrong in the catalogue is a runtime failure inside sherpa-onnx,
    /// a long way from the typo that caused it.
    #[test]
    fn the_default_transcription_model_fills_every_transducer_role() {
        let model = DEFAULT_TRANSCRIPTION_MODEL;
        assert_eq!(model.family, Family::NemoTransducer);

        for role in [Role::Encoder, Role::Decoder, Role::Joiner, Role::Tokens] {
            assert!(
                model.assets.iter().any(|a| a.role == role),
                "{} has no {role:?}",
                model.id
            );
        }
        assert_eq!(SILERO_VAD.family, Family::Vad);
    }

    /// Diarization needs two models and will not start with one. They are
    /// separate catalogue entries, so nothing but a test stops one of them
    /// being edited into the wrong family — at which point the stage resolves a
    /// file it cannot use and the failure surfaces from inside ONNX.
    #[test]
    fn the_diarization_pair_covers_both_of_its_roles() {
        assert_eq!(
            DEFAULT_SEGMENTATION_MODEL.family,
            Family::SpeakerSegmentation
        );
        assert!(
            DEFAULT_SEGMENTATION_MODEL
                .assets
                .iter()
                .any(|a| a.role == Role::Segmentation)
        );

        assert_eq!(DEFAULT_EMBEDDING_MODEL.family, Family::SpeakerEmbedding);
        assert!(
            DEFAULT_EMBEDDING_MODEL
                .assets
                .iter()
                .any(|a| a.role == Role::SpeakerEmbedding)
        );

        // Two entries, not one with two assets — see their doc comments.
        assert_ne!(DEFAULT_SEGMENTATION_MODEL.id, DEFAULT_EMBEDDING_MODEL.id);
    }

    /// Absolute for the same reason `config::path` is: a macOS bundle's working
    /// directory is `/`, so a relative model store would resolve against the
    /// root of the volume.
    #[test]
    fn the_model_store_is_absolute() {
        assert!(models_root().is_absolute());
        assert!(PARAKEET_TDT_0_6B_V2_INT8.dir().is_absolute());
        assert!(
            PARAKEET_TDT_0_6B_V2_INT8
                .dir()
                .ends_with("parakeet-tdt-0.6b-v2-int8")
        );
    }
}
