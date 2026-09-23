//! Deciding that a mic segment is the system track heard again.
//!
//! On speakers, the far end leaks back into the microphone, and the recogniser
//! dutifully transcribes it twice. The offline pass answers that with echo
//! cancellation, which needs the whole recording; live transcription does not
//! have that, so it answers the cheap version of the question instead: a mic
//! segment that overlaps a system segment and mostly says the same words is the
//! same speech heard twice, and the mic copy is the one worth dropping — the
//! system track heard it directly, so its text is the better one.
//!
//! # What this does not catch
//!
//! - **Bleed the recogniser heard differently.** Quiet bleed comes back garbled
//!   rather than as the same words, and a low match is kept on purpose: the
//!   alternative is dropping the user's own speech because a few words happened
//!   to coincide with what was playing.
//! - **Double-talk.** When the user is speaking over the far end, the mic
//!   segment is mostly their words, so it is kept. The bleed underneath it
//!   stays in the text; only the final transcript, after echo cancellation,
//!   gets that out.
//! - **Bleed later than [`SLACK_SECS`].** The comparison allows a small delay,
//!   not an arbitrary one. A speaker path with a long delay can miss it.
//! - **A missing system track.** Nothing to compare against means nothing is
//!   dropped. Live transcription of the mic alone cannot know what was playing.
//! - **The reverse direction.** The user's own voice leaking into the system
//!   track — a monitor mix, a speakerphone — is not looked for at all.

use crate::audio::transcript::Segment;

/// How far apart in time two segments may be and still be the same speech.
///
/// The two tracks are aligned onto one timeline, but the recogniser's cuts are
/// not: each track's detector ends its segment at its own silence, and a
/// speaker path adds a little delay of its own. A fifth of a second covers
/// both without reaching across to the next turn.
const SLACK_SECS: f64 = 0.2;

/// How much of the mic segment must overlap system speech at all.
///
/// Below this the mic segment is mostly something the system track never had,
/// so it cannot be a copy of it.
const MIN_COVERAGE: f64 = 0.5;

/// How much of the mic segment's words must be the system track's words, in
/// order.
///
/// High, deliberately. A lower bar would catch more quiet bleed and, with it,
/// the user's own short replies that happen to share a word with what was
/// playing. Missing some bleed is a duplicated line; a false drop deletes what
/// the user said.
const MIN_MATCH: f64 = 0.7;

/// Whether `mic` is the system track's audio heard again through the
/// microphone, given the system segments it might be a copy of.
///
/// Time first, because it is cheap and decisive: no overlap means no echo,
/// whatever the words. Then the words, and only the words inside the overlap —
/// a mic segment that runs on past the system segment is judged on the part
/// that could actually be bleed.
pub fn is_echo<'a>(mic: &Segment, system: impl IntoIterator<Item = &'a Segment>) -> bool {
    let heard = tokens(&mic.text);
    if heard.is_empty() {
        return false;
    }

    let system: Vec<&Segment> = system.into_iter().collect();

    let span = mic.end - mic.start;
    if span <= 0.0 {
        return false;
    }

    let mut windows: Vec<(f64, f64)> = system
        .iter()
        .filter(|s| s.start < mic.end + SLACK_SECS && mic.start - SLACK_SECS < s.end)
        .map(|s| (s.start - SLACK_SECS, s.end + SLACK_SECS))
        .collect();
    if windows.is_empty() {
        return false;
    }

    // Union rather than a sum: overlapping system segments must not be counted
    // twice, or a busy stretch would look like it covered everything.
    windows.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut covered = 0.0;
    let mut end = f64::NEG_INFINITY;
    for (start, stop) in windows {
        let start = start.max(mic.start);
        let stop = stop.min(mic.end);
        if stop <= start {
            continue;
        }
        covered += stop - start.max(end);
        end = end.max(stop);
    }
    if covered / span < MIN_COVERAGE {
        return false;
    }

    let played: Vec<&str> = system
        .iter()
        .filter(|s| s.start < mic.end + SLACK_SECS && mic.start - SLACK_SECS < s.end)
        .flat_map(|s| tokens(&s.text))
        .collect();

    lcs_ratio(&heard, &played) >= MIN_MATCH
}

/// Lower-cased words, with the punctuation the recogniser adds stripped.
///
/// Apostrophes stay: "don't" and "dont" are different words to the model, and
/// splitting them would manufacture a match out of "don" and "t".
fn tokens(text: &str) -> Vec<&str> {
    text.split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|w| !w.is_empty())
        .collect()
}

/// Share of `a`'s words appearing in `b`, in order.
///
/// Order matters: two people saying the same handful of words in a different
/// order is a coincidence, and a bag-of-words match would call it an echo.
/// The sequences are a sentence or two, so the quadratic cost is nothing.
fn lcs_ratio(a: &[&str], b: &[&str]) -> f64 {
    if a.is_empty() {
        return 0.0;
    }
    let mut prev = vec![0u32; b.len() + 1];
    let mut curr = vec![0u32; b.len() + 1];
    for &left in a {
        for (j, &right) in b.iter().enumerate() {
            curr[j + 1] = if left.eq_ignore_ascii_case(right) {
                prev[j] + 1
            } else {
                prev[j + 1].max(curr[j])
            };
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()] as f64 / a.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::transcript::Track;

    fn seg(track: Track, start: f64, end: f64, text: &str) -> Segment {
        Segment {
            start,
            end,
            track,
            speaker: None,
            text: text.into(),
        }
    }

    fn mic(start: f64, end: f64, text: &str) -> Segment {
        seg(Track::Mic, start, end, text)
    }

    fn system(start: f64, end: f64, text: &str) -> Segment {
        seg(Track::System, start, end, text)
    }

    #[test]
    fn identical_overlapping_speech_is_echo() {
        let heard = mic(1.0, 4.0, "Can you hear me all right?");
        let played = [system(1.1, 3.9, "Can you hear me all right?")];
        assert!(is_echo(&heard, played.iter()));
    }

    #[test]
    fn punctuation_and_case_do_not_save_it() {
        // The recogniser capitalises and punctuates each track on its own, so
        // a comparison that required them to agree would miss every echo.
        let heard = mic(0.0, 3.0, "hello, world.");
        let played = [system(0.0, 3.0, "Hello world!")];
        assert!(is_echo(&heard, played.iter()));
    }

    #[test]
    fn a_few_misheard_words_are_still_echo() {
        // Quiet bleed comes back with errors. A handful should not be enough
        // to keep the duplicate.
        let heard = mic(5.0, 9.0, "the meeting starts at three I think");
        let played = [system(5.1, 8.8, "The meeting starts at three, I think.")];
        assert!(is_echo(&heard, played.iter()));
    }

    #[test]
    fn no_overlap_in_time_is_never_echo() {
        // The same sentence, a turn later. Dropping this would delete a real
        // reply.
        let heard = mic(10.0, 13.0, "Can you hear me all right?");
        let played = [system(1.0, 4.0, "Can you hear me all right?")];
        assert!(!is_echo(&heard, played.iter()));
    }

    #[test]
    fn a_small_delay_is_tolerated() {
        let heard = mic(2.1, 5.0, "Can you hear me all right?");
        let played = [system(2.0, 4.9, "Can you hear me all right?")];
        assert!(is_echo(&heard, played.iter()));
    }

    #[test]
    fn double_talk_is_kept() {
        // The user talks over the far end. Most of the words are their own, so
        // this must survive even though the two overlap completely.
        let heard = mic(1.0, 5.0, "No, I already sent that yesterday afternoon");
        let played = [system(0.5, 6.0, "Could you forward me the document?")];
        assert!(!is_echo(&heard, played.iter()));
    }

    #[test]
    fn shared_words_in_a_different_order_are_kept() {
        let heard = mic(1.0, 4.0, "three at starts the meeting");
        let played = [system(1.0, 4.0, "the meeting starts at three")];
        assert!(!is_echo(&heard, played.iter()));
    }

    #[test]
    fn a_short_reply_sharing_one_word_is_kept() {
        let heard = mic(4.0, 5.0, "Yes, three.");
        let played = [system(3.5, 6.0, "Shall we say three o'clock then?")];
        assert!(!is_echo(&heard, played.iter()));
    }

    #[test]
    fn partial_overlap_is_judged_on_the_covered_part() {
        // The first half is bleed; the user carries on after. Coverage of the
        // whole segment is what saves it.
        let heard = mic(1.0, 6.0, "all right so I will send it over now");
        let played = [system(0.8, 3.0, "All right.")];
        assert!(!is_echo(&heard, played.iter()));
    }

    #[test]
    fn bleed_split_across_two_system_segments_is_still_caught() {
        // The system track's detector cut at a pause the mic's did not. The
        // words are spread over two segments; the echo is one.
        let heard = mic(1.0, 6.0, "the meeting starts at three I think");
        let played = [
            system(1.0, 3.2, "The meeting starts"),
            system(3.6, 5.8, "at three, I think."),
        ];
        assert!(is_echo(&heard, played.iter()));
    }

    #[test]
    fn overlapping_system_segments_are_not_double_counted() {
        // Two system segments covering the same moment must not add up to full
        // coverage of a mic segment they only partly overlap.
        let heard = mic(0.0, 10.0, "one two three four five six seven eight");
        let played = [
            system(0.0, 3.0, "one two three"),
            system(1.0, 4.0, "one two three"),
        ];
        assert!(!is_echo(&heard, played.iter()));
    }

    #[test]
    fn nothing_to_compare_against_is_not_echo() {
        let heard = mic(1.0, 4.0, "Can you hear me all right?");
        let played: [Segment; 0] = [];
        assert!(!is_echo(&heard, played.iter()));
    }
}
