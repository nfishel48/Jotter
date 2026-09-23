//! Downloading a model, once, on purpose.
//!
//! Kept apart from the catalogue because it is the only code in this binary
//! that opens a socket for a reason other than telemetry, and that is worth
//! being able to point at. Nothing calls it except `jotter models pull`: the
//! transcription stage declines when a model is absent rather than fetching one
//! (see the module doc on [`super`]).
//!
//! Two properties matter more than speed here.
//!
//! **A file is verified before it becomes a model file.** The download is
//! hashed as it streams and the result is only moved into place if the digest
//! matches, so an interrupted, truncated, or tampered transfer never occupies
//! the name the stage will later load. This is the one place the SHA-256 in the
//! catalogue is checked, which is what lets [`super::Model::resolve`] get away
//! with a size check on every run.
//!
//! **A failed download leaves nothing behind.** 652 MB of orphaned temporary
//! file is its own kind of bug, so the temporary sibling is removed on every
//! failure path.

use std::io::{self, Read, Write};
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::audio::stage::{tmp_path, write_atomic};

use super::{Asset, Model};

/// How far along a pull is.
///
/// A callback rather than printing from in here: this module has no business
/// knowing whether it is being driven by a CLI, a progress bar or a test.
#[derive(Debug, Clone, Copy)]
pub enum Progress<'a> {
    /// The file is already present at its recorded size, so nothing was fetched.
    Skipped {
        asset: &'a Asset,
    },
    Started {
        asset: &'a Asset,
    },
    /// `done` of `asset.bytes` transferred.
    Bytes {
        asset: &'a Asset,
        done: u64,
    },
    Finished {
        asset: &'a Asset,
    },
}

#[derive(Debug)]
pub enum FetchError {
    /// The server answered, but not with the file.
    Status {
        asset: &'static str,
        status: u16,
    },
    /// The transfer never completed: DNS, TLS, a dropped connection.
    Transport {
        asset: &'static str,
        message: String,
    },
    /// The bytes arrived and are not the file the catalogue describes. Treated
    /// as an error rather than a retry: a mirror serving the wrong content is
    /// not something a second attempt fixes, and loading it would be worse.
    Corrupt {
        asset: &'static str,
        expected: &'static str,
        found: String,
    },
    Io(io::Error),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Status { asset, status } => {
                write!(f, "{asset}: the server answered {status}")
            }
            Self::Transport { asset, message } => write!(f, "{asset}: {message}"),
            Self::Corrupt {
                asset,
                expected,
                found,
            } => write!(
                f,
                "{asset}: downloaded file does not match the expected checksum \
                 (expected {expected}, got {found}) — it was discarded"
            ),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for FetchError {}

impl From<io::Error> for FetchError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Fetch every asset of `model` that is not already present.
///
/// Assets already on disk at their recorded size are skipped, which is what
/// makes a re-run after a failure cheap: only the asset that failed is fetched
/// again. Within a single asset there is no resume — a connection that drops at
/// 600 MB costs the whole 652 MB again. Worth fixing only if it turns out to
/// happen; range requests need the server to honour them and a partial file to
/// be trusted enough to append to, and neither is free.
pub fn fetch(model: &'static Model, progress: &mut dyn FnMut(Progress)) -> Result<(), FetchError> {
    let dir = model.dir();
    std::fs::create_dir_all(&dir)?;

    for asset in model.assets {
        let target = dir.join(asset.name);
        if std::fs::metadata(&target).is_ok_and(|m| m.len() == asset.bytes) {
            progress(Progress::Skipped { asset });
            continue;
        }

        progress(Progress::Started { asset });
        match download(asset, &target, progress) {
            Ok(()) => progress(Progress::Finished { asset }),
            Err(e) => {
                // A partial 652 MB file is not something to leave lying around,
                // and `write_atomic` only guarantees it never reaches `target` —
                // clearing the sibling is this function's job.
                let _ = std::fs::remove_file(tmp_path(&target));
                return Err(e);
            }
        }
    }

    Ok(())
}

/// One asset, streamed to a temporary sibling and hashed on the way past.
fn download(
    asset: &'static Asset,
    target: &Path,
    progress: &mut dyn FnMut(Progress),
) -> Result<(), FetchError> {
    let mut response = ureq::get(asset.url).call().map_err(|e| match &e {
        ureq::Error::StatusCode(status) => FetchError::Status {
            asset: asset.name,
            status: *status,
        },
        _ => FetchError::Transport {
            asset: asset.name,
            message: e.to_string(),
        },
    })?;

    write_atomic(target, |tmp| {
        let mut file = std::fs::File::create(tmp)?;
        // `as_reader` streams. Not `read_to_vec`, which would both hold 652 MB
        // in memory and silently stop at ureq's 10 MB default body limit.
        let mut body = response.body_mut().as_reader();

        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 256 * 1024];
        let mut done: u64 = 0;

        loop {
            let read = body.read(&mut buffer).map_err(|e| FetchError::Transport {
                asset: asset.name,
                message: e.to_string(),
            })?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            file.write_all(&buffer[..read])?;
            done += read as u64;
            progress(Progress::Bytes { asset, done });
        }
        file.flush()?;

        let found = hex(&hasher.finalize());
        if found != asset.sha256 {
            return Err(FetchError::Corrupt {
                asset: asset.name,
                expected: asset.sha256,
                found,
            });
        }
        Ok(())
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The digest the catalogue's hashes are in, so a formatting slip here would
    /// make every download look corrupt.
    #[test]
    fn hashes_render_as_lowercase_hex() {
        let digest = Sha256::digest(b"");
        assert_eq!(
            hex(&digest),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    /// Every `FetchError` ends up in front of a user, so each one has to say
    /// what failed and which file it was — an anonymous "checksum mismatch" is
    /// no use when four files are being pulled.
    #[test]
    fn errors_name_the_asset_they_are_about() {
        let cases: Vec<FetchError> = vec![
            FetchError::Status {
                asset: "encoder.int8.onnx",
                status: 404,
            },
            FetchError::Transport {
                asset: "encoder.int8.onnx",
                message: "connection closed".into(),
            },
            FetchError::Corrupt {
                asset: "encoder.int8.onnx",
                expected: "abc",
                found: "def".into(),
            },
        ];

        for case in cases {
            let message = case.to_string();
            assert!(
                message.contains("encoder.int8.onnx"),
                "unnamed asset in {message:?}"
            );
        }

        // And the corrupt case says the file was thrown away, not kept — the
        // whole point of hashing before the rename.
        let message = FetchError::Corrupt {
            asset: "encoder.int8.onnx",
            expected: "abc",
            found: "def".into(),
        }
        .to_string();
        assert!(message.contains("discarded"), "{message}");
    }
}
