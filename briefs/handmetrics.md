# handmetrics — measuring the style of handwriting

Build a small Python library and CLI that measures the visual style of a handwritten word
image: how far it slants, how thick its strokes are, how much ink it puts on the page,
where its baseline sits and whether that baseline drifts.

The eventual goal of the wider project is a handwriting *generator* — give it text and
parameters like slant and messiness, get back an image. This project is not that. It is
the instrument that makes that possible: you cannot generate to a parameter you cannot
measure, and you cannot evaluate a generator without a way to ask whether the handwriting
it produced actually has the slant that was requested. Measurement comes first.

Work against the sample described in `DATA.md`. Read it before planning: the manifest has
real defects and the code has to cope with them.

## What to build

A package `handmetrics/`, tests in `tests/`, no other top-level directories.

- **Loading.** Join `data/labels.csv` to the files in `data/images/`, yielding usable
  records only: a label that is neither blank nor `UNREADABLE`, and an image that exists.
  Report what was discarded and why rather than failing silently. Counts must be computed
  from the data, never hardcoded.
- **Ink.** Turn an image into a boolean mask, ink against background. Everything else is
  built on this, so it belongs in its own module and must handle the sample's variety —
  these are photographed crops, not clean scans, and background brightness varies.
- **Slant.** The dominant angle of the vertical strokes, in degrees. Positive leans right,
  0 is upright.
- **Stroke thickness.** Typical pen width in pixels.
- **Ink coverage.** The fraction of the writing area that is ink, and the fraction of the
  full image that the writing occupies.
- **Baseline.** Where the writing sits, and the slope of any drift across the word.
- **A report command.** `python3 -m handmetrics report --limit N` prints the distribution
  of each metric across N sampled images: at least count, mean, standard deviation and a
  few percentiles. This is what turns the library into something useful — it is how the
  real spread of handwriting style gets known.

Each metric is independent of the others. Only ink is shared.

## How the work is judged

This is the part that matters most, and it is not negotiable.

**Every expected value in a test must come from something the test itself controls — never
from running the implementation and recording what it said.** A test whose expected number
was obtained by looking at the output proves only that the code has not changed since. It
is worthless as a check on whether the code is right, and writing one is a failure of the
task, not a shortcut through it.

There are two legitimate sources of truth here, and both are available:

**Synthetic images, where the answer is known exactly.** Render a word with PIL using
`/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf`, which is upright — its slant is 0 by
construction. Shear that render by a known angle and its slant is exactly that angle.
Scale it and the stroke thickness scales with it. Build the image whose properties you
already know, then assert the metric recovers them.

**Real sample images, under a transform the test applies.** There is no ground-truth slant
for a real handwriting crop — but you do not need one. Take a sample image, shear it by a
known angle, and require the estimate to move by that angle relative to the untransformed
estimate. Thicken the strokes by a known amount and require thickness to rise by it.
Enlarge the image and require ink coverage, which is a ratio, to stay roughly put. This
proves the metric responds correctly to real handwriting rather than only to clean fonts.

Use both. Synthetic renders pin the absolute scale; transformed real images prove it
survives contact with the data. State tolerances explicitly and keep them honest — a
tolerance wide enough to admit any answer is not a test.

A test may not import the implementation in order to compute what it expects.

## Constraints

- Python 3.12. `numpy`, `pillow` and `pytest` only. No new dependencies, no network, and
  nothing outside this repository — in particular do not go looking for the full dataset.
- No machine learning: no training, no model weights, no neural networks. These are
  measurements from geometry and pixel statistics. If a metric seems to need a model, the
  metric has been defined too ambitiously — simplify it.
- No generation. Nothing in this project produces handwriting; rendering a font is a test
  fixture, not a feature.
- No GUI, no web server, no notebooks.
- `data/` is read-only. Do not modify, move or add to it.
- Every metric returns a plain number in documented units, with the units in its
  docstring. A metric nobody can interpret is not a measurement.
- The report command must run over 200 images in under a minute on a laptop CPU.
