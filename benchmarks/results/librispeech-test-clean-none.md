# librispeech-test-clean

**WER 1.39%**  (95% CI 0.00%–5.08%)
5 utterances · 72 reference words

## Where the errors are

| | count | share of errors |
| --- | ---: | ---: |
| substitutions | 1 | 100.0% |
| deletions | 0 | 0.0% |
| insertions | 0 | 0.0% |

**Substitutions dominate — 100.0% of all errors.** That points at the acoustic model: the words are being heard, and heard wrong.

## Cost

- 0.01 h of audio in 0.1 min
- real-time factor **0.123** (8× faster than real time)

## What produced this

- jotter 0.1.13 (transcribe v1) at `ad19125439f6`
- model `parakeet-tdt-0.6b-v2-int8` via sherpa-onnx, 4 threads
- segmentation **none**
- normaliser whisper: transformers 4.53.2, english.json sha256:6607f948be98 (1739 entries)
- host macOS-26.3.1-arm64-arm-64bit-Mach-O

## Worst utterances

Ranked by absolute errors, not rate: a one-word utterance scored 100% tells you nothing, a thirty-word one scored 40% tells you a lot.

**1089-134686-0004** — 1 errors (9%)

- ref: number 10 fresh nelly is waiting on you good night husband
- hyp: number 10 fresh nellie is waiting on you good night husband

**1089-134686-0000** — 0 errors (0%)

- ref: he hoped there would be stew for dinner turnips and carrots and bruised potatoes and fat mutton pieces to be ladled out in thick peppered flour fattened sauce
- hyp: he hoped there would be stew for dinner turnips and carrots and bruised potatoes and fat mutton pieces to be ladled out in thick peppered flour fattened sauce

**1089-134686-0001** — 0 errors (0%)

- ref: stuff it into you his belly counseled him
- hyp: stuff it into you his belly counseled him

**1089-134686-0002** — 0 errors (0%)

- ref: after early nightfall the yellow lamps would light up here and there the squalid quarter of the brothels
- hyp: after early nightfall the yellow lamps would light up here and there the squalid quarter of the brothels

**1089-134686-0003** — 0 errors (0%)

- ref: hello bertie any good in your mind
- hyp: hello bertie any good in your mind
