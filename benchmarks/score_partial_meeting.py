"""Score the meeting recordings that finished, when the run did not.

`bench meeting` writes its report only after the last pair, so a run stopped
partway leaves hours of completed transcription unscored on disk. This reads
whatever directories hold a finished transcript and produces the same summary
over those, marking it partial so the count is never mistaken for the corpus.

Deliberately a script beside the harness rather than a flag inside it: the
right fix is for `cmd_meeting` to checkpoint as it goes, and a convenience
that hides the need for it would stop that happening.
"""

from __future__ import annotations

import sys
from pathlib import Path

from jbench import meeting, normalize, paths, report
from jbench import score as scoring


def main() -> int:
    directories = sorted(
        d for d in (paths.WORK / "meeting").iterdir()
        if (d / "transcript.json").exists() and (d / "reference.json").exists()
    )
    if not directories:
        print("no completed recordings under work/meeting", file=sys.stderr)
        return 1

    normalizer = normalize.load("whisper")
    mic_result, system_result = scoring.Result(), scoring.Result()
    attributions, aec_rows = [], []

    for directory in directories:
        print(f"  {directory.name}")
        for side, result in (("mic", mic_result), ("system", system_result)):
            result.utterances.append(
                scoring.score_pair(
                    f"{directory.name}.{side}",
                    meeting.reference_text(directory, side),
                    meeting.track_text(directory, side),
                    normalizer,
                )
            )
        attributions.append(meeting.score_attribution(directory))
        aec_rows.append(meeting.aec_numbers(directory))

    combined = meeting.Attribution()
    for a in attributions:
        combined.mic_correct += a.mic_correct
        combined.mic_total += a.mic_total
        combined.system_correct += a.system_correct
        combined.system_total += a.system_total
        combined.missed += a.missed

    summary = report.build(
        "ami-two-track",
        mic_result,
        normalizer,
        {"segmentation": "vad"},
        extra={"partial": True, "stopped_after": len(directories), "full_corpus": 96},
    )
    summary["two_track"] = {
        "mic_wer": mic_result.wer,
        "system_wer": system_result.wer,
        "attribution": combined.as_dict(),
        "aec_enabled": True,
        "aec": aec_rows,
        "recordings": len(directories),
        "meetings": sorted({d.name.rsplit(".", 1)[0] for d in directories}),
    }

    json_path, md_path = report.write(summary, paths.RESULTS, "ami-two-track-partial")

    print()
    print(f"recordings         {len(directories)} of 96")
    print(f"mic track WER      {mic_result.wer:.2%}   (you)")
    print(f"system track WER   {system_result.wer:.2%}   (everyone else)")
    print(f"attribution        {combined.accuracy:.2%} over "
          f"{combined.mic_total + combined.system_total} single-speaker frames")
    print(f"  mic recall       {combined.mic_recall:.2%}")
    print(f"  system recall    {combined.system_recall:.2%}")
    print(f"  missed           {combined.missed} frames of speech went untranscribed")
    print(f"\nwritten:\n  {json_path}\n  {md_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
