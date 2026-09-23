//! The worker thread that turns queued audio into `live.jsonl`.
//!
//! One thread for both tracks, deliberately. The recogniser is the expensive
//! part and it does not decode two segments at once anyway, so a second thread
//! would only add a queue. Audio arrives already converted to mono `i16` — the
//! same form the WAV writer stored — and goes through the same [`Segmenter`]
//! the offline pass uses, so a live line and the final transcript are cut the
//! same way.
//!
//! The two tracks share one timeline, the mic track's, exactly as
//! `transcript.json` does. A system segment's own-timeline time is shifted by
//! the difference between the two streams' first callback instants, which is
//! why every chunk carries the instant it was captured at.

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::audio::meta::LiveInfo;
use crate::audio::stage::DeclineReason;
use crate::audio::transcribe::{
    self, ParakeetTranscriber, Segmenter, TranscribeError, Transcriber,
};
use crate::audio::transcript::{Segment, Track};
use crate::models::Model;

use super::merge::Merger;
use super::{
    Chunk, FILENAME, FORMAT_VERSION, LIVE_VERSION, LiveConfig, LiveDecline, LiveFailure, LiveFeed,
    LiveState, LiveStatus, Shared,
};

/// How long `stop()` waits for the tail to be transcribed before giving up.
///
/// The queue holds a few tens of seconds and a segment decodes in well under
/// one, so this is several times the worst case rather than a guess at the
/// typical one. Past it the recording must not be held up: the final
/// transcript covers whatever the live one missed.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the worker is given to notice it has been abandoned, which is one
/// decode plus a margin.
const ABANDON_GRACE: Duration = Duration::from_secs(5);

/// How often the worker looks up to see whether it has been asked to stop.
const POLL: Duration = Duration::from_millis(200);

/// Silence inserted in place of audio the queue had no room for, so the sample
/// count — and therefore the timeline — survives a dropped chunk.
const GAP_CHUNK: usize = 16_000;

/// What live transcription is doing, shared with the thread reporting it.
///
/// The counters the audio callback touches are atomics rather than fields of
/// the mutex, because that callback must not take the mutex: it runs on the
/// realtime thread, and a lock held across a model call is exactly the stall
/// live transcription exists to avoid causing.
pub(crate) struct Progress {
    pub(crate) state: LiveState,
    pub(crate) kind: Option<&'static str>,
    pub(crate) reason: Option<String>,
    pub(crate) segments: u32,
    pub(crate) last_end: Option<f64>,
    stopping: AtomicBool,
    abandon: AtomicBool,
    /// Shared with the capture callback, which increments it without taking
    /// the mutex — see [`LiveFeed`].
    dropped: [Arc<AtomicU64>; 2],
    rate: [AtomicU64; 2],
}

impl Progress {
    fn starting() -> Self {
        Self {
            state: LiveState::Starting,
            kind: None,
            reason: None,
            segments: 0,
            last_end: None,
            stopping: AtomicBool::new(false),
            abandon: AtomicBool::new(false),
            dropped: [Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0))],
            rate: [AtomicU64::new(0), AtomicU64::new(0)],
        }
    }

    pub(crate) fn status(&self) -> LiveStatus {
        LiveStatus {
            state: self.state,
            kind: self.kind.map(str::to_string),
            reason: self.reason.clone(),
            segments: self.segments,
            last_end_secs: self.last_end,
            dropped_secs: self.dropped_secs(),
        }
    }

    pub(crate) fn dropped_secs(&self) -> f32 {
        (0..2)
            .map(|i| {
                let rate = self.rate[i].load(Ordering::Relaxed);
                if rate == 0 {
                    0.0
                } else {
                    self.dropped[i].load(Ordering::Relaxed) as f32 / rate as f32
                }
            })
            .sum()
    }

    /// The counter the callback increments, for one track.
    pub(crate) fn drops(&self, track: Track) -> Arc<AtomicU64> {
        Arc::clone(&self.dropped[super::track_index(track)])
    }

    fn stopping(&self) -> bool {
        self.stopping.load(Ordering::Relaxed)
    }

    fn abandoned(&self) -> bool {
        self.abandon.load(Ordering::Relaxed)
    }
}

pub(crate) enum Engine {
    /// Resolved before the thread existed, so there is nothing to stop.
    Declined(LiveInfo),
    Running {
        tx: SyncSender<Chunk>,
        shared: Shared,
        done: Receiver<LiveInfo>,
        thread: std::thread::JoinHandle<()>,
    },
}

impl Engine {
    pub(crate) fn start(dir: &Path, config: &LiveConfig, sources: crate::audio::Sources) -> Self {
        let model = transcribe::model_for(config.model.as_deref());

        // Before any thread: a missing model is a stat call, and finding out
        // after the recording started would only delay the decline.
        let resolved = match transcribe::resolve(model) {
            Ok(resolved) => resolved,
            Err(files) => {
                return Self::Declined(declined(
                    model,
                    LiveDecline::ModelMissing {
                        model_id: model.id,
                        files,
                    },
                ));
            }
        };

        let shared = Arc::new(Mutex::new(Progress::starting()));
        let (tx, rx) = std::sync::mpsc::sync_channel(super::QUEUE_CAPACITY);
        let (done_tx, done) = std::sync::mpsc::channel();

        let path = dir.join(FILENAME);
        let wanted = sources.intersect(config.tracks);
        let shared_for_thread = Arc::clone(&shared);

        let thread = std::thread::Builder::new()
            .name("jotter-live".into())
            .spawn(move || {
                let info = Worker::run(&path, model, resolved, wanted, &shared_for_thread, rx);
                let _ = done_tx.send(info);
            });

        let thread = match thread {
            Ok(thread) => thread,
            Err(_) => return Self::Declined(failed(model, LiveFailure::Engine)),
        };

        Self::Running {
            tx,
            shared,
            done,
            thread,
        }
    }

    pub(crate) fn feed(&self, track: Track, sample_rate: u32) -> Option<LiveFeed> {
        let Self::Running { tx, shared, .. } = self else {
            return None;
        };
        let progress = shared.lock().expect("live progress");
        progress.rate[super::track_index(track)].store(sample_rate as u64, Ordering::Relaxed);
        Some(LiveFeed {
            tx: tx.clone(),
            track,
            sample_rate,
            position: 0,
            dropped: progress.drops(track),
        })
    }

    pub(crate) fn status(&self) -> LiveStatus {
        match self {
            Self::Declined(info) => LiveStatus {
                state: LiveState::Declined,
                kind: info.declined.as_deref().map(decline_kind),
                reason: info.declined.clone(),
                segments: 0,
                last_end_secs: None,
                dropped_secs: 0.0,
            },
            Self::Running { shared, .. } => shared.lock().expect("live progress").status(),
        }
    }

    /// Transcribe everything still queued, then stop.
    ///
    /// Waits, but not for ever: past [`DRAIN_TIMEOUT`] the worker is told to
    /// abandon whatever it is decoding and the recording moves on. The final
    /// transcript covers the tail either way.
    pub(crate) fn stop(self) -> LiveInfo {
        let Self::Running {
            tx,
            shared,
            done,
            thread,
        } = self
        else {
            let Self::Declined(info) = self else {
                unreachable!()
            };
            return info;
        };

        shared
            .lock()
            .expect("live progress")
            .stopping
            .store(true, Ordering::Relaxed);
        // The worker exits once the queue is empty *and* it has been asked to
        // stop, so this sender has to go: a feed the caller kept hold of must
        // not keep it alive past the recording.
        drop(tx);

        if let Ok(info) = done.recv_timeout(DRAIN_TIMEOUT) {
            let _ = thread.join();
            return info;
        }

        shared
            .lock()
            .expect("live progress")
            .abandon
            .store(true, Ordering::Relaxed);
        if let Ok(info) = done.recv_timeout(ABANDON_GRACE) {
            let _ = thread.join();
            return info;
        }

        // Still inside a decode, which cannot be interrupted. The thread is
        // left to finish it; it writes nothing more once it notices.
        let info = timed_out(&shared);
        drop(thread);
        info
    }
}

fn declined(model: &Model, reason: LiveDecline) -> LiveInfo {
    LiveInfo {
        declined: Some(reason.kind().into()),
        ..base(model)
    }
}

/// The stored decline reason is already its stable kind, so a status reports
/// it unchanged. Unrecognised text — a record this build does not know — is
/// reported as a plain decline rather than echoed back.
fn decline_kind(stored: &str) -> String {
    match stored {
        "unavailable" | "model_missing" | "no_tracks" => stored.to_string(),
        _ => "declined".to_string(),
    }
}

fn failed(model: &Model, failure: LiveFailure) -> LiveInfo {
    LiveInfo {
        failed: Some(failure.kind().into()),
        ..base(model)
    }
}

/// A decline decided before any model was looked at, so the record names none.
impl Engine {
    pub(crate) fn declined_only(reason: LiveDecline) -> Self {
        Self::Declined(LiveInfo {
            version: LIVE_VERSION,
            declined: Some(reason.kind().into()),
            ..LiveInfo::default()
        })
    }
}

fn base(model: &Model) -> LiveInfo {
    LiveInfo {
        version: LIVE_VERSION,
        model: model.id.into(),
        engine: model.engine.into(),
        ..LiveInfo::default()
    }
}

fn timed_out(shared: &Shared) -> LiveInfo {
    let progress = shared.lock().expect("live progress");
    LiveInfo {
        version: LIVE_VERSION,
        segments: progress.segments,
        dropped_secs: progress.dropped_secs(),
        drain_timed_out: true,
        ..LiveInfo::default()
    }
}

/// One track, once its rate is known.
struct Lane {
    segmenter: Segmenter,
    /// Samples of this track the worker has accounted for, including the gaps
    /// it filled for chunks the queue dropped.
    position: u64,
    /// Nanoseconds of the track's own first sample, from the callback instants
    /// the chunks carry. What puts the two tracks on one timeline.
    origin: Option<f64>,
}

struct Worker {
    out: File,
    transcriber: ParakeetTranscriber,
    vad_model: Option<String>,
    lane: [Option<Lane>; 2],
    merger: Merger,
    mic_segments: u32,
    system_segments: u32,
}

impl Worker {
    fn run(
        path: &Path,
        model: &Model,
        resolved: transcribe::Resolved,
        wanted: Option<crate::audio::Sources>,
        shared: &Shared,
        rx: Receiver<Chunk>,
    ) -> LiveInfo {
        let mut info = base(model);
        match Self::transcribe(path, resolved, wanted, shared, rx, &mut info) {
            Ok(()) => {}
            Err(failure) => {
                info.failed = Some(failure.kind().into());
                let mut progress = shared.lock().expect("live progress");
                progress.state = LiveState::Failed;
                progress.kind = Some(failure.kind());
                progress.reason = Some(failure.to_string());
            }
        }
        info.dropped_secs = shared.lock().expect("live progress").dropped_secs();
        info
    }

    fn transcribe(
        path: &Path,
        resolved: transcribe::Resolved,
        wanted: Option<crate::audio::Sources>,
        shared: &Shared,
        rx: Receiver<Chunk>,
        info: &mut LiveInfo,
    ) -> Result<(), LiveFailure> {
        let transcriber = ParakeetTranscriber::create(&resolved.recognizer)?;

        // Created only once there is something to write into it, so a decline
        // leaves no empty file behind.
        let out = File::create(path).map_err(|_| LiveFailure::Io)?;
        info.path = Some(FILENAME.into());

        shared.lock().expect("live progress").state = LiveState::Running;

        let mut worker = Worker {
            out,
            transcriber,
            vad_model: resolved.vad_model(),
            lane: [None, None],
            merger: Merger::new(
                wanted.is_some_and(|s| s.wants_mic()),
                wanted.is_some_and(|s| s.wants_system()),
            ),
            mic_segments: 0,
            system_segments: 0,
        };

        loop {
            if shared.lock().expect("live progress").abandoned() {
                // The recording gave up waiting. Whatever is still queued is
                // left untranscribed, and the record says so.
                info.drain_timed_out = true;
                return Ok(());
            }
            match rx.recv_timeout(POLL) {
                Ok(chunk) => worker.handle(chunk, shared)?,
                Err(RecvTimeoutError::Timeout) => {
                    // Empty *and* asked to stop, not merely empty: between
                    // turns the queue is empty most of the time. `try_recv`
                    // consumes what it finds, so a chunk that arrived between
                    // the timeout and the check is transcribed, not dropped.
                    if shared.lock().expect("live progress").stopping() {
                        match rx.try_recv() {
                            Ok(chunk) => worker.handle(chunk, shared)?,
                            Err(_) => break,
                        }
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        worker.finish(shared)?;
        if shared.lock().expect("live progress").abandoned() {
            info.drain_timed_out = true;
        }
        info.segments = worker.mic_segments + worker.system_segments;
        info.mic_segments = worker.mic_segments;
        info.system_segments = worker.system_segments;
        info.echo_dropped = worker.merger.echo_dropped();
        Ok(())
    }

    fn handle(&mut self, chunk: Chunk, shared: &Shared) -> Result<(), LiveFailure> {
        let index = super::track_index(chunk.track);
        if self.lane[index].is_none() {
            self.lane[index] = Some(Lane {
                segmenter: Segmenter::new(chunk.track, chunk.sample_rate, self.vad_model.clone())?,
                position: 0,
                origin: None,
            });
        }
        let lane = self.lane[index].as_mut().expect("just inserted");

        if lane.origin.is_none()
            && let Some(nanos) = chunk.origin_nanos
        {
            // The chunk may not be the track's first: the queue can drop the
            // early ones while the model is still loading. The instant belongs
            // to sample `position`, so the track's own start is earlier by
            // exactly that many samples.
            let before = chunk.position as f64 / chunk.sample_rate.max(1) as f64 * 1e9;
            lane.origin = Some(nanos as f64 - before);
        }

        // A hole is silence, not a jump in the timeline, so a dropped chunk
        // moves later segments later rather than earlier.
        if chunk.position > lane.position {
            let gap = (chunk.position - lane.position) as usize;
            let silence = vec![0i16; GAP_CHUNK.min(gap)];
            let mut left = gap;
            while left > 0 {
                let n = GAP_CHUNK.min(left);
                self.absorb(index, &silence[..n], shared)?;
                left -= n;
            }
        }

        self.absorb(index, &chunk.samples, shared)
    }

    fn absorb(
        &mut self,
        index: usize,
        samples: &[i16],
        shared: &Shared,
    ) -> Result<(), LiveFailure> {
        let mut found = Vec::new();
        // Before the lane is borrowed: the shift reads the other lane.
        let shift = self.shift_of(index);
        let lane = self.lane[index].as_mut().expect("lane open");
        lane.position += samples.len() as u64;
        lane.segmenter.push(
            samples,
            &Abandon::new(&self.transcriber, shared),
            &mut found,
        );

        let track = lane.segmenter.track();
        let fed = lane.segmenter.fed_secs() + shift;
        let settled = lane.segmenter.settled_secs() + shift;
        let in_speech = lane.segmenter.in_speech();
        for segment in found {
            self.merger.push(segment.shifted(shift));
        }
        self.merger.advance(track, fed, settled, in_speech);
        self.write_ready(shared)
    }

    /// [`shift`](Self::shift) for the lane at `index`, which may not exist yet.
    fn shift_of(&self, index: usize) -> f64 {
        match self.lane[index].as_ref().map(|lane| lane.segmenter.track()) {
            Some(track) => self.shift(track),
            None => 0.0,
        }
    }

    /// The system track's own timeline, moved onto the mic track's.
    ///
    /// The same correction `transcript.json` applies, from the same source:
    /// the two streams' first callback instants. Unknown until both have
    /// delivered one, and zero until then — a segment emitted in that window
    /// is early enough that the error is a callback or two.
    fn shift(&self, track: Track) -> f64 {
        if track != Track::System {
            return 0.0;
        }
        match (self.origin(Track::Mic), self.origin(Track::System)) {
            (Some(mic), Some(system)) => (system - mic) / 1e9,
            _ => 0.0,
        }
    }

    fn origin(&self, track: Track) -> Option<f64> {
        self.lane[super::track_index(track)].as_ref()?.origin
    }

    fn finish(&mut self, shared: &Shared) -> Result<(), LiveFailure> {
        for index in 0..2 {
            let Some(lane) = self.lane[index].as_mut() else {
                continue;
            };
            let mut found = Vec::new();
            lane.segmenter
                .finish(&Abandon::new(&self.transcriber, shared), &mut found);
            let track = lane.segmenter.track();
            let shift = self.shift(track);
            for segment in found {
                self.merger.push(segment.shifted(shift));
            }
            self.merger.close(track);
        }
        // A track that was asked for but never delivered audio has no lane,
        // and would otherwise hold the other track's tail back for ever.
        self.merger.close(Track::Mic);
        self.merger.close(Track::System);
        self.write_ready(shared)
    }

    fn write_ready(&mut self, shared: &Shared) -> Result<(), LiveFailure> {
        for segment in self.merger.ready() {
            if shared.lock().expect("live progress").abandoned() {
                return Ok(());
            }
            write_line(&mut self.out, &segment)?;
            match segment.track {
                Track::Mic => self.mic_segments += 1,
                Track::System => self.system_segments += 1,
            }
            let mut progress = shared.lock().expect("live progress");
            progress.segments += 1;
            progress.last_end = Some(segment.end);
        }
        Ok(())
    }
}

/// A line of `live.jsonl`, in the order the format promises.
#[derive(serde::Serialize)]
struct Line<'a> {
    v: u32,
    track: Track,
    start: f64,
    end: f64,
    text: &'a str,
}

fn write_line(out: &mut File, segment: &Segment) -> Result<(), LiveFailure> {
    let line = Line {
        v: FORMAT_VERSION,
        track: segment.track,
        // Milliseconds. Finer than the timestamps mean, and it keeps the file
        // readable, which is half its point.
        start: (segment.start * 1e3).round() / 1e3,
        end: (segment.end * 1e3).round() / 1e3,
        text: &segment.text,
    };
    serde_json::to_writer(&mut *out, &line).expect("a segment always serialises");
    out.write_all(b"\n")
        .and_then(|_| out.flush())
        .map_err(|_| LiveFailure::Io)
}

/// The recogniser, unless the recording has given up waiting.
///
/// A decode cannot be interrupted, but it can be stopped from starting another
/// one, which is all abandoning the tail needs.
struct Abandon<'a> {
    inner: &'a dyn Transcriber,
    shared: &'a Shared,
}

impl<'a> Abandon<'a> {
    fn new(inner: &'a dyn Transcriber, shared: &'a Shared) -> Self {
        Self { inner, shared }
    }
}

impl Transcriber for Abandon<'_> {
    fn transcribe(&self, samples: &[f32]) -> Option<String> {
        if self.shared.lock().expect("live progress").abandoned() {
            return None;
        }
        self.inner.transcribe(samples)
    }
}

impl From<TranscribeError> for LiveFailure {
    fn from(err: TranscribeError) -> Self {
        match err {
            TranscribeError::Engine(_) => Self::Engine,
            // A segmenter reads no WAV of its own, so this is not a way the
            // live path fails. Mapped rather than discarded so the type stays
            // total.
            TranscribeError::Io(_) | TranscribeError::Wav(_) => Self::Io,
        }
    }
}

impl std::fmt::Display for LiveFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Engine => write!(f, "the recogniser could not be started"),
            Self::Io => write!(f, "live.jsonl could not be written"),
        }
    }
}
