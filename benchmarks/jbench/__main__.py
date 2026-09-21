"""`bench` — the command line.

    bench fetch    --corpus librispeech-test-clean   download and pin a corpus
    bench prepare  --corpus librispeech-test-clean   corpus -> manifests
    bench run      --corpus librispeech-test-clean   manifests -> hypotheses
    bench score    --corpus librispeech-test-clean   hypotheses -> a result
    bench compare  a.json b.json                     is the difference real?
    bench verify   --corpus librispeech-test-clean   fast path == production?
    bench smoke                                      does any of this work?

`run` implies `prepare`, and `score` implies `run`, so the everyday command is
just `bench score --corpus librispeech-test-clean`. The stages exist separately
because they fail differently and cost differently: preparing is minutes of
disk, running is hours of CPU, and scoring is seconds — and a scoring change
should never mean paying for the hours again.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path

from . import manifest, normalize, paths, report
from . import score as scoring
from .corpora import REGISTRY, get


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="bench", description=__doc__.split("\n")[0])
    sub = parser.add_subparsers(dest="command", required=True)

    def corpus_arg(p, required=True):
        p.add_argument(
            "--corpus",
            required=required,
            choices=sorted(REGISTRY),
            help="which corpus to work on",
        )

    p = sub.add_parser("list", help="show the known corpora and their licences")

    p = sub.add_parser("fetch", help="download a corpus, or the normaliser")
    corpus_arg(p, required=False)
    p.add_argument(
        "--normalizer",
        action="store_true",
        help="fetch Whisper's spelling map, without which scores are not comparable",
    )

    p = sub.add_parser("prepare", help="turn a downloaded corpus into manifests")
    corpus_arg(p)
    p.add_argument("--root", type=Path, help="use an already-extracted corpus here")
    p.add_argument("--limit", type=int, help="only the first N items")
    p.add_argument("--meetings", help="AMI only: comma-separated meeting ids")

    p = sub.add_parser("run", help="transcribe a prepared corpus")
    corpus_arg(p)
    p.add_argument("--segmentation", choices=["vad", "none"], default="vad")
    p.add_argument("--model", help="model id, from `jotter models list`")
    p.add_argument("--limit", type=int)
    p.add_argument("--force", action="store_true", help="re-run even if hypotheses exist")

    p = sub.add_parser("score", help="score a run and write a report")
    corpus_arg(p)
    p.add_argument("--segmentation", choices=["vad", "none"], default="vad")
    p.add_argument("--model")
    p.add_argument("--limit", type=int)
    p.add_argument("--force", action="store_true")
    p.add_argument("--normalizer", choices=["whisper", "basic"], default="whisper")
    p.add_argument("--tag", help="suffix for the result filename")

    p = sub.add_parser("compare", help="is the difference between two results real?")
    p.add_argument("baseline", type=Path)
    p.add_argument("candidate", type=Path)

    p = sub.add_parser("verify", help="check the fast path agrees with production")
    corpus_arg(p)
    p.add_argument("--limit", type=int, default=10)

    p = sub.add_parser("meeting", help="two-track meeting accuracy, from AMI")
    p.add_argument("--root", type=Path, help="an already-downloaded AMI corpus")
    p.add_argument("--limit", type=int, help="only the first N (meeting, speaker) pairs")
    p.add_argument("--bleed-db", type=float, default=None,
                   help="how far the speaker bleed sits below the near voice")
    p.add_argument("--rir", type=Path, help="a measured impulse response, instead of a synthetic room")
    p.add_argument("--no-aec", action="store_true", help="skip the echo pass, to measure what it buys")
    p.add_argument("--normalizer", choices=["whisper", "basic"], default="whisper")
    p.add_argument("--tag", help="suffix for the result filename")

    p = sub.add_parser("aec", help="echo cancellation across a swept echo return loss")
    p.add_argument("--corpus", default="librispeech-test-clean", choices=sorted(REGISTRY),
                   help="where the near and far speech comes from")
    p.add_argument("--items", type=int, default=4, help="how many scenarios (4 clips each)")
    p.add_argument("--sweep", help="comma-separated ERL values in dB")
    p.add_argument("--rir", type=Path)
    p.add_argument("--tag")

    p = sub.add_parser("smoke", help="run the whole harness on generated audio")

    args = parser.parse_args(argv)
    return globals()[f"cmd_{args.command}"](args)


def cmd_list(args) -> int:
    for name in sorted(REGISTRY):
        c = REGISTRY[name]
        flag = "" if c.commercial_ok else "  [non-commercial]"
        print(f"{name}{flag}\n    {c.licence} · {c.url}\n    {c.notes}\n")
    return 0


def cmd_fetch(args) -> int:
    from . import fetch

    if args.normalizer:
        fetch.fetch_normalizer(normalize.SPELLING_PATH, normalize.LOCK_PATH)
    if args.corpus:
        get(args.corpus).prepare(paths.DATA)
    if not args.normalizer and not args.corpus:
        print("nothing to do — pass --corpus and/or --normalizer", file=sys.stderr)
        return 2
    return 0


def cmd_prepare(args) -> int:
    corpus = get(args.corpus)
    if args.meetings:
        _override_ami_meetings(args.meetings)

    root = args.root or corpus.prepare(paths.DATA)
    items = corpus.items(root)
    if args.limit:
        items = _take(items, args.limit)

    items_path, refs_path = _manifest_paths(args.corpus)
    items = _decoded(items, args.corpus)
    count = manifest.write(items, items_path, refs_path)
    if count == 0:
        print(f"{args.corpus}: no items found under {root}", file=sys.stderr)
        return 1

    print(f"{args.corpus}: {count} items\n  {items_path}\n  {refs_path}")
    if not corpus.commercial_ok:
        print(f"  note: {corpus.licence} — measure with it, do not redistribute it")
    return 0


def cmd_run(args) -> int:
    items_path, _ = _manifest_paths(args.corpus)
    if not items_path.exists():
        rc = cmd_prepare(_as_namespace(corpus=args.corpus, root=None, limit=args.limit, meetings=None))
        if rc != 0:
            return rc

    out = _hypotheses_path(args.corpus, args.segmentation)
    if out.exists() and not args.force:
        print(f"{out} exists — pass --force to re-run")
        return 0

    command = [
        str(paths.cargo_binary()),
        "--manifest", str(items_path),
        "--out", str(out),
        "--segmentation", args.segmentation,
        "--progress",
    ]
    if args.model:
        command += ["--model", args.model]
    if args.limit:
        command += ["--limit", str(args.limit)]

    out.parent.mkdir(parents=True, exist_ok=True)
    print(" ".join(command))
    return subprocess.run(command).returncode


def cmd_score(args) -> int:
    rc = cmd_run(args)
    if rc != 0:
        return rc

    _, refs_path = _manifest_paths(args.corpus)
    hyps_path = _hypotheses_path(args.corpus, args.segmentation)

    references = manifest.read_references(refs_path)
    bench_provenance, hypotheses = manifest.read_hypotheses(hyps_path)
    normalizer = normalize.load(args.normalizer)

    missing = set(references) - set(hypotheses)
    if missing and not args.limit:
        # Loud, not fatal: a run killed partway is still worth scoring, but
        # scoring 80% of a corpus and calling it the corpus is not.
        print(
            f"warning: {len(missing)} of {len(references)} items have no hypothesis "
            "— this is a partial run",
            file=sys.stderr,
        )

    result = scoring.Result()
    for item_id, hypothesis in hypotheses.items():
        reference = references.get(item_id)
        if reference is None:
            print(f"warning: {item_id} has a hypothesis but no reference", file=sys.stderr)
            continue
        result.utterances.append(
            scoring.score_pair(
                item_id,
                reference["reference"],
                hypothesis.get("text", ""),
                normalizer,
                audio_secs=hypothesis.get("audio_secs", 0.0),
                elapsed_secs=hypothesis.get("elapsed_secs", 0.0),
            )
        )

    if not result.utterances:
        print("nothing was scored", file=sys.stderr)
        return 1

    summary = report.build(
        args.corpus,
        result,
        normalizer,
        bench_provenance,
        extra={"partial": bool(missing), "limit": args.limit},
    )
    stem = f"{args.corpus}-{args.segmentation}" + (f"-{args.tag}" if args.tag else "")
    json_path, md_path = report.write(summary, paths.RESULTS, stem)

    print()
    print(report.to_markdown(summary))
    print(f"\nwritten:\n  {json_path}\n  {md_path}")
    return 0


def cmd_compare(args) -> int:
    """Two result files, one question: did the change actually help?"""
    baseline = _result_from_json(args.baseline)
    candidate = _result_from_json(args.candidate)
    verdict = scoring.paired_bootstrap(baseline, candidate)

    print(f"baseline   {verdict['baseline_wer']:.2%}  {args.baseline}")
    print(f"candidate  {verdict['candidate_wer']:.2%}  {args.candidate}")
    print(f"delta      {verdict['delta_wer']:+.2%}  over {verdict['paired_utterances']} paired items")
    print(f"p          {verdict['p_value']:.3f}  ({verdict['resamples']} paired resamples)")
    print()
    if verdict["p_value"] < 0.05:
        direction = "better" if verdict["delta_wer"] < 0 else "worse"
        print(f"The candidate is {direction}, and the difference survives resampling.")
    else:
        print("The difference does not survive resampling. Treat it as noise.")
    return 0


def cmd_verify(args) -> int:
    """Does `jotter-bench` produce what `jotter transcribe` produces?

    The whole case for the fast path is that it runs the same code as the real
    pass. This checks that claim instead of asserting it: the same clips are
    put through both, and the text must match.
    """
    from .verify import run_verification

    return run_verification(args.corpus, args.limit)


def cmd_smoke(args) -> int:
    """Exercise every stage on generated audio, with no corpus and no network.

    Deliberately says nothing about accuracy — synthetic audio has no words in
    it. It answers a different question: does the manifest reach the binary,
    does the binary write parseable output, does the scorer read it.
    """
    from .smoke import run_smoke

    return run_smoke()


def cmd_meeting(args) -> int:
    """Synthesise Jotter recordings out of AMI, run the real pipeline, score it."""
    from . import meeting, report
    from .corpora import ami

    root = args.root or (paths.DATA / "ami")
    if not (Path(root) / "annotations" / "words").exists():
        print(
            f"{root} does not hold an AMI corpus — run `bench fetch --corpus ami-ihm-test`",
            file=sys.stderr,
        )
        return 1

    into = paths.WORK / "meeting"
    kwargs = {"limit": args.limit, "rir": args.rir}
    if args.bleed_db is not None:
        kwargs["bleed_db"] = args.bleed_db

    directories = meeting.synthesise(Path(root), into, **kwargs)
    if not directories:
        print("nothing was synthesised", file=sys.stderr)
        return 1

    normalizer = normalize.load(args.normalizer)
    mic_result, system_result = scoring.Result(), scoring.Result()
    attributions, aec_rows = [], []

    for directory in directories:
        print(f"  {directory.name}")
        meeting.transcribe(directory, with_aec=not args.no_aec)

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
        if not args.no_aec:
            aec_rows.append(meeting.aec_numbers(directory))

    combined = meeting.Attribution()
    for a in attributions:
        combined.mic_correct += a.mic_correct
        combined.mic_total += a.mic_total
        combined.system_correct += a.system_correct
        combined.system_total += a.system_total
        combined.missed += a.missed

    summary = report.build("ami-two-track", mic_result, normalizer, {"segmentation": "vad"})
    summary["two_track"] = {
        "mic_wer": mic_result.wer,
        "system_wer": system_result.wer,
        "attribution": combined.as_dict(),
        "aec_enabled": not args.no_aec,
        "aec": aec_rows,
        "recordings": len(directories),
        "meetings": sorted({d.name.rsplit(".", 1)[0] for d in directories}),
    }

    stem = "ami-two-track" + ("-no-aec" if args.no_aec else "") + (f"-{args.tag}" if args.tag else "")
    json_path, md_path = report.write(summary, paths.RESULTS, stem)

    print()
    print(f"mic track WER      {mic_result.wer:.2%}   (you)")
    print(f"system track WER   {system_result.wer:.2%}   (everyone else)")
    print(f"attribution        {combined.accuracy:.2%} over "
          f"{combined.mic_total + combined.system_total} single-speaker frames")
    print(f"  mic recall       {combined.mic_recall:.2%}")
    print(f"  system recall    {combined.system_recall:.2%}")
    print(f"  missed           {combined.missed} frames of speech went untranscribed")
    print(f"\nwritten:\n  {json_path}\n  {md_path}")
    return 0


def cmd_aec(args) -> int:
    """Sweep echo return loss, and report both the dB and the word error rate."""
    from . import aec, report
    from .corpora import get as get_corpus

    _, refs_path = _manifest_paths(args.corpus)
    items_path, _ = _manifest_paths(args.corpus)
    if not items_path.exists():
        print(f"run `bench prepare --corpus {args.corpus}` first", file=sys.stderr)
        return 1

    references = manifest.read_references(refs_path)
    clips = []
    with items_path.open() as handle:
        for line in handle:
            if not line.strip():
                continue
            row = json.loads(line)
            reference = references.get(row["id"])
            if reference:
                clips.append((row["id"], Path(row["audio"]), reference["reference"]))
            if len(clips) >= args.items * 4:
                break

    if len(clips) < 4:
        print(f"need at least 4 clips, found {len(clips)}", file=sys.stderr)
        return 1

    sweep = (
        tuple(float(v) for v in args.sweep.split(","))
        if args.sweep
        else aec.DEFAULT_SWEEP
    )
    conditions = aec.build(clips, paths.WORK / "aec", sweep=sweep, rir=args.rir)

    rows = []
    for condition in conditions:
        print(f"  {condition.directory.name}")
        rows.append(aec.measure(condition))

    table = aec.summarise(rows)
    summary = {
        "corpus": "aec-sweep",
        "sweep_db": list(sweep),
        "rows": rows,
        "run": report.provenance({"source_corpus": args.corpus, "items": len(conditions)}),
    }
    stem = "aec-sweep" + (f"-{args.tag}" if args.tag else "")
    paths.RESULTS.mkdir(parents=True, exist_ok=True)
    (paths.RESULTS / f"{stem}.json").write_text(json.dumps(summary, indent=2) + "\n")
    (paths.RESULTS / f"{stem}.md").write_text(f"# Echo cancellation sweep\n\n{table}\n")

    print()
    print(table)
    print(f"\nwritten: {paths.RESULTS / stem}.json/.md")
    return 0


def _decoded(items, corpus: str):
    """Point each item at audio `jotter-bench` can actually read.

    Lazy, so the decode is interleaved with the walk rather than done as a
    separate pass over thousands of files — `manifest.write` streams, and a
    prepare that printed nothing for ten minutes would look hung.

    The decoded clips live under `work/`, which is gitignored as regenerable;
    they are, at the cost of re-decoding. `data/` is deliberately left holding
    exactly what was downloaded and checksum-pinned.
    """
    from .audio import ensure_wav

    cache = paths.WORK / corpus / "audio"
    for item in items:
        source = Path(item.audio)
        item.audio = ensure_wav(source, cache / f"{item.id}.wav")
        yield item


def _manifest_paths(corpus: str) -> tuple[Path, Path]:
    directory = paths.WORK / corpus
    return directory / "items.jsonl", directory / "references.jsonl"


def _hypotheses_path(corpus: str, segmentation: str) -> Path:
    return paths.WORK / corpus / f"hypotheses-{segmentation}.jsonl"


def _result_from_json(path: Path) -> scoring.Result:
    """Rebuild enough of a Result from a report to run a paired test.

    The report stores only the worst utterances in full, so a comparison needs
    the per-utterance counts — which is why `score` also writes them out
    alongside. Falls back to a clear error rather than a silently wrong test.
    """
    data = json.loads(Path(path).read_text())
    per_item = data.get("per_utterance")
    if per_item is None:
        raise SystemExit(
            f"{path} has no per-utterance counts, so a paired test is not possible. "
            "It was written by an older version of `bench score`; re-score the run."
        )
    result = scoring.Result()
    for row in per_item:
        result.utterances.append(
            scoring.Utterance(
                id=row["id"],
                reference="",
                hypothesis="",
                counts=scoring.Counts(**row["counts"]),
            )
        )
    return result


def _take(iterable, n):
    for index, value in enumerate(iterable):
        if index >= n:
            return
        yield value


def _override_ami_meetings(value: str) -> None:
    from .corpora import ami

    chosen = [m.strip() for m in value.split(",") if m.strip()]
    ami.meetings = lambda: chosen  # type: ignore[assignment]


def _as_namespace(**kwargs):
    return argparse.Namespace(**kwargs)


if __name__ == "__main__":
    raise SystemExit(main())
