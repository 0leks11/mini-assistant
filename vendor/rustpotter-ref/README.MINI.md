# Rustpotter reference-only adaptation

Source: https://crates.io/crates/rustpotter/3.0.2 and https://github.com/GiviMAD/rustpotter
License: Apache-2.0, retained in LICENSE. UPSTREAM_SHA256.json records the downloaded source.

Mini adaptation removes Candle/NN modules, model training/loading and legacy v2 loading.
MFCC, DTW, reference building and reference scoring are retained. This avoids an unused
neural-network dependency on the ARM32 device. We maintain this adaptation explicitly;
it is not an unmodified upstream crate or proof of acoustic accuracy. References require
owner enrollment, positive/negative checks and device CPU/RAM validation.

Mini changes also add allocation-free CBOR shape/depth/size preflight and semantic
validation before detector allocation: 1 MiB maximum, 3–8 templates, 16 finite MFCC
coefficients and 20–300 feature frames per template. The supported enrollment input
is mono 16kHz PCM16 WAV, 0.3–3 seconds per recording, at most 128 KiB each. Malformed
WAV data returns errors rather than panicking. This deliberately narrows upstream's
generic input format. Existing upstream source hashes remain in UPSTREAM_SHA256.json;
these files are a maintained adaptation, not matching copies.

`personal_reference_detector` supplies the same fixed initial scoring policy to the
device wrapper and offline evaluator: median score 0.65, average score 0.60, five
partial scores. The DTW comparator's ideal score is about 0.731, so 0.75 would make
ordinary detections unattainable. These similarity scores are not probabilities.
Reference-file thresholds cannot lower the runtime policy. Enrollment still needs
held-out positive and negative recordings and per-device acoustic/CPU/RAM checks.

Host utility (does not open a microphone or network):

```sh
keyword-enroll build keyword.rpw sample1.wav sample2.wav sample3.wav
keyword-enroll validate keyword.rpw
keyword-enroll test keyword.rpw held-out.wav
```

Build never overwrites an existing file and creates Unix output with mode 0600.
JSON output contains only counts/shape/validation status, not recognized speech.
`test` accepts up to 60 seconds, uses the runtime matcher and adds a two-second
synthetic zero tail to flush partial detections. Training files must be private;
do not commit either owner recordings or their reference artifact.

Upstream's manual filter/encoder examples depended on unvendored speech WAV files
and wrote processed audio to disk. Their five tests are replaced in this adaptation
by self-contained synthetic geometry/scale/channel, chunk-continuity, silence and
gain-bound contracts; test runs do not open audio devices or need speech assets.
