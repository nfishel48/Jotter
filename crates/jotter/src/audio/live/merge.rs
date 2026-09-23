//! Putting the two tracks' segments onto one ordered timeline.
//!
//! Each track's detector hands segments over as it finishes them, which is not
//! the order a reader wants them in: a short mic segment can be decoded while
//! the system track is still inside a long one that started earlier. Writing
//! lines as they appear would make `live.jsonl` unreadable top to bottom.
//!
//! So a segment waits until it is safe to say nothing earlier can still arrive.
//! A segment is safe once the *other* track has settled past its start — the
//! other track's detector has seen that moment and is not holding a segment
//! open across it — and a mic segment waits a little longer, until the other
//! track has settled past its end, because only then has every system segment
//! it might be an echo of already arrived.
//!
//! A track that has gone quiet is treated as caught up. That is the difference
//! between a detector still deciding and a stream that has stopped delivering:
//! an idle output device yields no tap frames at all, and holding every mic
//! segment until it resumes would mean no live transcript for as long as
//! nothing is playing.

use crate::audio::transcript::{Segment, Track};

use super::echo;

/// How long a track may go without advancing before it is treated as caught up
/// with the other one.
///
/// Longer than any gap the two streams normally have between them — they are
/// fed from one queue — and shorter than a pause worth waiting out.
const IDLE_SECS: f64 = 1.0;

/// How long a segment may be held before it is released anyway.
///
/// The backstop for the case the rules above cannot settle: one track stays in
/// speech indefinitely while the other has something to say. Past this the
/// ordering guarantee is given up for that one segment, which is better than
/// the transcript stalling for the rest of the meeting.
const HOLD_CAP_SECS: f64 = 25.0;

/// How far back system segments are kept for the echo check.
///
/// A mic segment's end cannot be further behind than the hold cap, and the
/// segment itself cannot be longer than the detector allows, so anything older
/// than this can no longer overlap one that is still waiting.
const RETAIN_SECS: f64 = HOLD_CAP_SECS + 30.0;

#[derive(Clone, Copy)]
struct Lane {
    /// `false` for a track nobody is recording, which is settled for all time.
    active: bool,
    fed: f64,
    settled: f64,
    in_speech: bool,
    closed: bool,
}

impl Lane {
    fn idle() -> Lane {
        Lane {
            active: false,
            fed: 0.0,
            settled: 0.0,
            in_speech: false,
            closed: false,
        }
    }
}

/// The two tracks, waiting to be written in order.
pub(crate) struct Merger {
    lane: [Lane; 2],
    pending: Vec<Segment>,
    /// System segments already written, kept for the echo check.
    system: Vec<Segment>,
    echo_dropped: u32,
}

impl Merger {
    pub(crate) fn new(mic: bool, system: bool) -> Self {
        Self {
            lane: [
                Lane {
                    active: mic,
                    ..Lane::idle()
                },
                Lane {
                    active: system,
                    ..Lane::idle()
                },
            ],
            pending: Vec::new(),
            system: Vec::new(),
            echo_dropped: 0,
        }
    }

    pub(crate) fn push(&mut self, segment: Segment) {
        let at = self
            .pending
            .partition_point(|s| s.start.total_cmp(&segment.start).is_lt());
        self.pending.insert(at, segment);
    }

    /// Where one track has got to, on the shared timeline.
    ///
    /// `fed` is how far its audio has been seen, `settled` how far every
    /// segment starting before it has already been handed over, and
    /// `in_speech` whether its detector is inside a segment it has not
    /// finished — which is what stops `settled` moving.
    pub(crate) fn advance(&mut self, track: Track, fed: f64, settled: f64, in_speech: bool) {
        let lane = &mut self.lane[index(track)];
        lane.active = true;
        lane.fed = fed;
        lane.settled = settled;
        lane.in_speech = in_speech;
    }

    /// The track has ended, so nothing more can arrive from it.
    pub(crate) fn close(&mut self, track: Track) {
        self.lane[index(track)].closed = true;
    }

    /// The segments it is now safe to write, in start order, with the mic
    /// copies of system speech already removed.
    pub(crate) fn ready(&mut self) -> Vec<Segment> {
        let mut out = Vec::new();
        while let Some(segment) = self.pending.first() {
            if !self.releasable(segment) {
                break;
            }
            let segment = self.pending.remove(0);
            if segment.track == Track::Mic && self.echoes(&segment) {
                self.echo_dropped += 1;
                continue;
            }
            if segment.track == Track::System {
                self.system.push(segment.clone());
            }
            out.push(segment);
        }
        self.prune();
        out
    }

    pub(crate) fn echo_dropped(&self) -> u32 {
        self.echo_dropped
    }

    fn releasable(&self, segment: &Segment) -> bool {
        let other = self.effective(other(segment.track));
        let needs = match segment.track {
            // A mic segment waits until every system segment it might echo has
            // arrived, which is past its end rather than its start.
            Track::Mic => segment.end,
            Track::System => segment.start,
        };
        if other >= needs {
            return true;
        }
        // The backstop. Measured from the end, because that is the moment the
        // segment became decidable at all.
        self.fed() - segment.end >= HOLD_CAP_SECS
    }

    /// How far a track can be trusted to have produced everything before.
    fn effective(&self, track: Track) -> f64 {
        let lane = self.lane[index(track)];
        if !lane.active || lane.closed {
            return f64::INFINITY;
        }
        // Quiet, and the other track has moved on without it: an idle stream,
        // not a detector still thinking. Only while it is not inside a
        // segment, which would still be holding audio from before `settled`.
        let idle = !lane.in_speech && self.lane[index(other(track))].fed - lane.fed >= IDLE_SECS;
        if idle { self.fed() } else { lane.settled }
    }

    fn fed(&self) -> f64 {
        self.lane.iter().map(|l| l.fed).fold(0.0, f64::max)
    }

    fn echoes(&self, mic: &Segment) -> bool {
        let pending = self.pending.iter().filter(|s| s.track == Track::System);
        echo::is_echo(mic, self.system.iter().chain(pending))
    }

    fn prune(&mut self) {
        let horizon = self.fed() - RETAIN_SECS;
        self.system.retain(|s| s.end >= horizon);
    }
}

fn index(track: Track) -> usize {
    match track {
        Track::Mic => 0,
        Track::System => 1,
    }
}

fn other(track: Track) -> Track {
    match track {
        Track::Mic => Track::System,
        Track::System => Track::Mic,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(track: Track, start: f64, end: f64, text: &str) -> Segment {
        Segment {
            start,
            end,
            track,
            speaker: None,
            text: text.into(),
        }
    }

    /// A track caught up to `t` and not currently in speech.
    fn caught_up(merger: &mut Merger, track: Track, t: f64) {
        merger.advance(track, t, t, false);
    }

    #[test]
    fn a_mic_segment_waits_for_the_system_track_to_pass_it() {
        let mut merger = Merger::new(true, true);
        merger.push(seg(Track::Mic, 1.0, 3.0, "hello"));
        // The system track has been seen past the start but not the end, so
        // the segment might still turn out to be echo. The mic track stays
        // within the idle gap of it: the two are fed from one queue, and a
        // wider gap would mean the system stream had stopped.
        caught_up(&mut merger, Track::System, 2.0);
        caught_up(&mut merger, Track::Mic, 2.5);
        assert!(merger.ready().is_empty());

        caught_up(&mut merger, Track::System, 3.0);
        let out = merger.ready();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "hello");
    }

    #[test]
    fn a_system_segment_is_written_as_soon_as_it_cannot_be_overtaken() {
        let mut merger = Merger::new(true, true);
        merger.push(seg(Track::System, 5.0, 8.0, "from the call"));

        // The mic track has passed its start, so nothing earlier can arrive.
        caught_up(&mut merger, Track::Mic, 5.0);
        caught_up(&mut merger, Track::System, 9.0);
        let out = merger.ready();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].track, Track::System);
    }

    #[test]
    fn output_comes_out_in_start_order() {
        let mut merger = Merger::new(true, true);
        // Arriving in the wrong order, as decoding them would.
        merger.push(seg(Track::Mic, 4.0, 6.0, "second"));
        merger.push(seg(Track::System, 1.0, 3.0, "first"));

        caught_up(&mut merger, Track::Mic, 7.0);
        caught_up(&mut merger, Track::System, 7.0);
        let out = merger.ready();
        assert_eq!(
            out.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(),
            ["first", "second"]
        );
    }

    #[test]
    fn an_earlier_segment_holds_back_a_later_one() {
        let mut merger = Merger::new(true, true);
        merger.push(seg(Track::System, 1.0, 1.5, "long"));
        merger.push(seg(Track::Mic, 1.2, 1.4, "short"));

        // The mic track has not settled past the earlier segment's start, and
        // the two tracks are close enough that neither looks stalled. The
        // later segment must wait behind it.
        merger.advance(Track::Mic, 0.9, 0.9, false);
        merger.advance(Track::System, 1.5, 1.5, false);
        assert!(
            merger.ready().is_empty(),
            "the later segment must not leapfrog the earlier one"
        );

        caught_up(&mut merger, Track::Mic, 1.5);
        let out = merger.ready();
        assert_eq!(out[0].text, "long");
    }

    #[test]
    fn echo_is_dropped_once_both_segments_are_in() {
        let mut merger = Merger::new(true, true);
        merger.push(seg(Track::System, 1.0, 4.0, "Can you hear me all right?"));
        merger.push(seg(Track::Mic, 1.1, 3.9, "Can you hear me all right?"));

        caught_up(&mut merger, Track::Mic, 5.0);
        caught_up(&mut merger, Track::System, 5.0);
        let out = merger.ready();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].track, Track::System);
        assert_eq!(merger.echo_dropped(), 1);
    }

    #[test]
    fn a_single_track_never_waits() {
        let mut merger = Merger::new(true, false);
        merger.push(seg(Track::Mic, 1.0, 3.0, "only me"));
        caught_up(&mut merger, Track::Mic, 4.0);
        assert_eq!(merger.ready().len(), 1);
    }

    #[test]
    fn an_idle_track_is_treated_as_caught_up() {
        let mut merger = Merger::new(true, true);
        merger.push(seg(Track::Mic, 0.0, 0.5, "hello"));

        // The system stream delivered nothing and the mic track has moved on
        // past the idle gap. Holding the segment now would stall the
        // transcript for as long as the silence lasts.
        merger.advance(Track::System, 0.0, 0.0, false);
        caught_up(&mut merger, Track::Mic, IDLE_SECS + 0.5);
        assert_eq!(merger.ready().len(), 1);
    }

    #[test]
    fn a_track_inside_a_segment_is_not_treated_as_idle() {
        let mut merger = Merger::new(true, true);
        // A mic segment that may turn out to be echo of the system segment
        // still being detected.
        merger.push(seg(Track::Mic, 1.0, 3.0, "maybe echo"));

        // The system track's detector is holding a segment open, so it has not
        // settled past the mic segment even though the mic track has moved on.
        // In speech it must not be treated as idle, or the mic segment would
        // be released before that system segment arrives to be compared.
        merger.advance(Track::System, 4.0, 0.5, true);
        caught_up(&mut merger, Track::Mic, 4.0);
        assert!(merger.ready().is_empty());
    }

    #[test]
    fn a_segment_held_too_long_is_released_anyway() {
        let mut merger = Merger::new(true, true);
        merger.push(seg(Track::Mic, 1.0, 3.0, "held"));

        // The system track is stuck inside a segment and never settles.
        merger.advance(Track::System, 1.0, 0.0, true);
        caught_up(&mut merger, Track::Mic, 3.0 + HOLD_CAP_SECS);
        assert_eq!(
            merger.ready().len(),
            1,
            "the hold cap must release a segment the rules cannot settle"
        );
    }

    #[test]
    fn closing_a_track_releases_everything_waiting_on_it() {
        let mut merger = Merger::new(true, true);
        merger.push(seg(Track::Mic, 1.0, 3.0, "tail"));
        merger.advance(Track::System, 0.5, 0.5, false);
        assert!(merger.ready().is_empty());

        merger.close(Track::System);
        assert_eq!(merger.ready().len(), 1);
    }
}
