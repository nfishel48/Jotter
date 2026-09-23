//! Replay a recording through live transcription, as if it were happening now.
//!
//! ```text
//! cargo run -p jotter --example live_from_wav -- <dir> [--speed N] [--out DIR]
//! ```
//!
//! `dir` holds `mic.wav`, `system.wav`, or both. `speed` is how fast to feed
//! the audio: `1` is real time, and `0` is as fast as the recogniser can take
//! it. When `meta.json` is present, the tracks are aligned by the callback
//! instants it records, exactly as a live recording would be; without it they
//! are taken to have started together.
//!
//! Segments are printed as they land, and the run ends with how far behind the
//! room each one was — the time from the audio at a segment's end being fed to
//! its line appearing in `live.jsonl`.

use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use jotter::audio::Sources;
use jotter::audio::live::{self, LiveConfig, LiveState, LiveTranscriber};
use jotter::audio::meta::{MIC_NAME, Meta, SYSTEM_NAME};
use jotter::audio::stage::wav;
use jotter::audio::transcript::Track;

/// How much audio is handed over at a time. A capture callback delivers less;
/// this is coarse enough that pacing it is cheap and fine enough that the
/// latency figure is meaningful.
const CHUNK_SECS: f64 = 0.02;

struct Args {
    dir: PathBuf,
    out: PathBuf,
    speed: f64,
}

struct Feed {
    track: Track,
    samples: Vec<i16>,
    rate: u32,
    origin: Option<u128>,
    feed: live::LiveFeed,
}

fn main() {
    let args = match parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("error: {e}");
            eprintln!(
                "usage: live_from_wav <dir> [--speed N] [--out DIR]\n\
                 \n  speed   1 is real time, 0 is as fast as possible (default 1)"
            );
            std::process::exit(2);
        }
    };

    let tracks = load(&args.dir);
    if tracks.is_empty() {
        eprintln!(
            "error: {} has neither {MIC_NAME} nor {SYSTEM_NAME}",
            args.dir.display()
        );
        std::process::exit(1);
    }

    std::fs::create_dir_all(&args.out).expect("create output dir");

    let live = LiveTranscriber::start(
        &args.out,
        &LiveConfig {
            tracks: Sources::Both,
            ..LiveConfig::default()
        },
        Sources::Both,
    );

    // A decline is decided before the thread exists, so it is visible at once.
    let status = live.status();
    if status.state == LiveState::Declined {
        eprintln!(
            "live transcription declined: {}",
            status.reason.unwrap_or_else(|| "no reason given".into())
        );
        std::process::exit(1);
    }

    let mut feeds = Vec::new();
    for (track, samples, rate, origin) in tracks {
        let Some(feed) = live.feed(track, rate) else {
            continue;
        };
        feeds.push(Feed {
            track,
            samples,
            rate,
            origin,
            feed,
        });
    }
    let started = Instant::now();
    // Audio time fed so far, paired with the wall clock moment it was fed, so
    // a segment's latency is measurable when its line appears.
    let mut fed: Vec<(f64, Instant)> = Vec::new();
    let mut cursor = None;
    let mut latency_ms: Vec<f64> = Vec::new();
    let mut position = [0usize; 2];

    loop {
        let mut any = false;
        let mut fed_until = 0.0f64;
        for track in &mut feeds {
            let at = position[index(track.track)];
            if at >= track.samples.len() {
                continue;
            }
            any = true;
            let n = chunk_samples(track.rate).min(track.samples.len() - at);
            let chunk = track.samples[at..at + n].to_vec();
            position[index(track.track)] = at + n;
            // The instant only matters on the first chunk of a track: it is
            // what puts the two on one timeline.
            let origin = track.origin.take();
            if args.speed == 0.0 {
                track.feed.push(chunk, origin);
            } else {
                track.feed.try_push(chunk, origin);
            }
            fed_until = fed_until.max((at + n) as f64 / track.rate as f64);
        }
        if !any {
            break;
        }

        let now = Instant::now();
        fed.push((fed_until, now));
        report(&args.out, &mut cursor, &fed, &mut latency_ms);

        if args.speed > 0.0 {
            let due = started + Duration::from_secs_f64(fed_until / args.speed);
            if let Some(wait) = due.checked_duration_since(Instant::now()) {
                thread::sleep(wait);
            }
        }
    }

    let info = live.stop();
    // The tail is transcribed during stop, so read once more.
    report(&args.out, &mut cursor, &fed, &mut latency_ms);

    println!();
    match latency_ms.as_slice() {
        [] => println!("no segments"),
        ms => {
            let mut sorted = ms.to_vec();
            sorted.sort_by(|a, b| a.total_cmp(b));
            println!(
                "{} segments, latency median {:.0} ms, min {:.0} ms, max {:.0} ms (speed {})",
                sorted.len(),
                sorted[sorted.len() / 2],
                sorted[0],
                sorted[sorted.len() - 1],
                args.speed,
            );
        }
    }
    println!(
        "wrote {} — {} lines, {} dropped as echo, {:.2}s of audio dropped",
        args.out.join(live::FILENAME).display(),
        info.segments,
        info.echo_dropped,
        info.dropped_secs,
    );
}

/// Print every line appended since the last call, with how long after its
/// audio it appeared.
fn report(
    dir: &Path,
    cursor: &mut Option<live::Cursor>,
    fed: &[(f64, Instant)],
    latency_ms: &mut Vec<f64>,
) {
    let chunk = live::read(dir, *cursor).expect("read live.jsonl");
    let now = Instant::now();
    for segment in &chunk.segments {
        let late = fed
            .iter()
            .find(|(t, _)| *t >= segment.end)
            .map(|(_, at)| now.duration_since(*at).as_secs_f64() * 1e3);
        if let Some(late) = late {
            latency_ms.push(late);
        }
        println!(
            "[{:>7.2}-{:>6.2}s {:>6}] {}{}",
            segment.start,
            segment.end,
            segment.track.as_str(),
            segment.text,
            late.map(|l| format!("  (+{l:.0} ms)")).unwrap_or_default(),
        );
    }
    *cursor = Some(chunk.cursor);
}

fn load(dir: &Path) -> Vec<(Track, Vec<i16>, u32, Option<u128>)> {
    let meta = Meta::read(&dir.join("meta.json")).ok();
    let origin = |name: &str| {
        meta.as_ref()
            .and_then(|m| match name {
                MIC_NAME => m.mic.as_ref(),
                _ => m.system.as_ref(),
            })
            .and_then(|t| t.first_callback_nanos)
    };

    [Track::Mic, Track::System]
        .into_iter()
        .filter_map(|track| {
            let name = match track {
                Track::Mic => MIC_NAME,
                Track::System => SYSTEM_NAME,
            };
            let audio = wav::read_track(&dir.join(name)).ok()?;
            Some((track, audio.samples, audio.sample_rate, origin(name)))
        })
        .collect()
}

fn chunk_samples(rate: u32) -> usize {
    (rate as f64 * CHUNK_SECS).round().max(1.0) as usize
}

fn index(track: Track) -> usize {
    match track {
        Track::Mic => 0,
        Track::System => 1,
    }
}

fn parse(args: impl Iterator<Item = String>) -> Result<Args, String> {
    let args: Vec<String> = args.collect();
    let mut dir = None;
    let mut out = None;
    let mut speed = 1.0;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--speed" => {
                i += 1;
                speed = args
                    .get(i)
                    .ok_or("--speed needs a number")?
                    .parse()
                    .map_err(|_| "--speed needs a number")?;
                if speed < 0.0 {
                    return Err("speed cannot be negative".into());
                }
            }
            "--out" => {
                i += 1;
                out = Some(PathBuf::from(args.get(i).ok_or("--out needs a directory")?));
            }
            other if other.starts_with('-') => return Err(format!("unknown option {other}")),
            other => dir = Some(PathBuf::from(other)),
        }
        i += 1;
    }
    let dir = dir.ok_or("missing recording directory")?;
    let out = out.unwrap_or_else(|| dir.join("live-out"));
    Ok(Args { dir, out, speed })
}
