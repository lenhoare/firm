# v1 second trial — handmetrics, 9 September 2026

The first run of Firm on a project it did not author. Every attempt is recorded in
`.firm/v1-live.db`; the merged work is on `firm/run-a2637cda` in `workspaces/handmetrics`.

## What was being tested

Whether Firm can take a written brief for real work, decompose it, and produce something
correct — with a scorer that has teeth. Every trial before this used a project I had built
myself, with tests I had written, so the checks could only confirm what I already knew.

## The work

`handmetrics`: measure the visual style of a handwritten word image — slant, stroke
thickness, ink coverage, baseline drift. It is deliberately not a handwriting *generator*,
which is the wider goal. Measurement comes first, because you cannot generate to a
parameter you cannot measure, and you cannot evaluate a generator without asking whether
the handwriting it produced has the slant that was requested.

Sample: 2,000 crops drawn with seed 20260909 from a 330,961-row public dataset, kept in the
workspace repository. Its defects were kept deliberately — 50 rows labelled `UNREADABLE` or
blank, 5 rows naming an image that is not present — so the loader had real work to do.

## The oracle, and why this project

A generated-handwriting brief has no objective check: "does it look like handwriting" is a
matter of opinion, and a scorer that cannot decide is decorative. Measurement can be
checked, using ground truth the checker manufactures itself:

- **Synthetic renders.** A word set in DejaVuSans is upright by construction, so its slant
  is 0. Sheared by a known angle, it is exactly that angle.
- **Real images under a known transform.** There is no ground-truth slant for a real crop,
  but shear one by 12° and the estimate must move by 12°.

Neither expectation can be satisfied by an agent writing an agreeable test.

## Result

14 tasks planned by grok from `briefs/handmetrics.md`. **12 merged, 1 failed, 1 blocked**,
in about 25 minutes of wall clock across two sittings.

| | |
| --- | --- |
| merged | load, ink, slant, coverage, baseline, and all six test tasks |
| failed | `implement-thickness` — qwen stalled twice |
| blocked | `implement-report` — depends on thickness |

Finished attempts: muse 8 verified (mean 97s), grok 1 verified (218s), qwen 2 verified
(12m01s, 10m46s) and 2 killed while silent.

## The planner separated tests from implementations by itself

Nothing in the brief asked for it. Grok split every component into a `test-*` task and an
`implement-*` task with **disjoint file scopes**: `implement-slant` may touch only
`handmetrics/slant.py`, so the agent satisfying a test is structurally incapable of editing
it. This is the class-affinity idea from the spec, arrived at without being asked.

The mechanism that makes it work is neat. Test tasks verify with `pytest --collect-only`
and set `must_fail` to the real run. Because the briefs specify importing the
implementation *inside* each test function, collection succeeds while the test itself fails
until the code exists. A weak check plus proof-of-failure compose into a real guard.

## The external anchor caught a silent sign inversion

The brief's rule survived into all fourteen task briefs — grok instantiated it per metric
rather than paraphrasing it, naming the font path, the shear angles and the tolerances.

Before dispatch I added one anchor to `test-slant`: render the word in DejaVuSans-Oblique,
a genuine italic face, and assert the measured slant is positive. The reasoning was that
every other expectation in the file was a delta or a zero, and **zero has no sign**, so a
consistently inverted convention would satisfy all of them.

It fired immediately. The merged test's shear helper built the PIL matrix as `(1, -k, ...)`
under a docstring reading *"Positive angle shifts lower rows right (defined here as leaning
right)"* — which is itself the error, since shifting the lower rows right leans a glyph
left. Measured against its upright twin:

| | change in centroid slope | direction |
| --- | --- | --- |
| `_shear_pil(+20)`, asserted to read `+20` | +0.367 | leans left |
| oblique font, asserted to read `> 0` | −0.189 | leans right |

Contradictory: no implementation could satisfy both. `implement-slant` duly failed with
**5 passed, 1 failed** — `assert -10.978... > 1.0`. That magnitude is right; muse's
estimator measured the italic face correctly and only its sign was inverted, inherited from
the test.

Without the anchor the file would have passed. Five mutually consistent checks would have
merged a metric reporting backhand as italic, and nobody would have known until the
generator produced mirror-image output.

The trap is real rather than one agent being careless: writing a reference estimator to
validate the fix, I reached for the same inverted formula myself and had to be corrected by
the same anchor.

## And then missed a larger defect entirely

After the run, the merged slant metric was measured over 200 real images:

```
185 of 200 return exactly 0.00 degrees
the 15 that don't:  -15.45  -10.70  +11.95  -12.05  +10.85 ...
```

Bimodal — either exactly zero or a large angle — which is not what handwriting looks like.
The objective is "which shear makes the ink narrowest horizontally", scored at
−20/−10/0/+10/+20 relative to its value at 0:

```
the test's one image     0.810  0.921  1.000  0.856  0.750   peak at 0
TRAIN_161046.jpg         0.842  0.860  1.000  0.880  0.813   peak at 0
synthetic HILL           0.436  0.644  1.000  0.600  0.391   peak at 0
```

On a tall synthetic render with long vertical stems that genuinely finds the slant. On a
real crop — 300px wide, 50px tall, cursive, few long stems — the narrowest extent is always
at zero shear, and `_center_out` resolves the flat objective to upright by design. The
docstring says as much: *"so a flat objective resolves to upright"*. On real data the
objective is always flat.

**Every test still passed, because a horizontal-extent minimiser is exactly
shear-equivariant.** Shear an image by +20 and its extent-minimising angle moves by +20. So
it satisfies the upright case (it always returns ~0), both synthetic shear recoveries, the
real-image delta and the empty mask.

Including the oblique anchor. DejaVu Oblique is the roman face sheared, so asking whether
it reads positive is another equivariance question wearing a different hat. The anchor
fixed the sign — worth having — but could never establish that the metric measures slant on
real handwriting.

**The lesson: every anchor tested response to a transform, and only one real image was ever
measured untransformed.** The brief asked for both legs and got them; what it did not ask
was that the real leg use more than a single image. Grok's task brief said "the first
usable existing image", and that image returns 0.00 — the test passes only because it
asserts a delta.

The fix is a distribution-level check on real data: the measured slant must not be
identically zero across a corpus. That is materially harder to anchor objectively than
anything else here, and is the open problem this trial leaves behind.

## Three defects in Firm, found by running it

**`--resume` deleted the worktrees resumption needs.** `salvage_abandoned` removed every
`attempt-*` directory unconditionally — *"the branch carries the work"*, true when nothing
resumed — and `resume_run` calls it before driving. So `resumable()` always failed its
`Path::exists()` check and every resume silently became a fresh start. Grok's interrupted
`implement-thickness` attempt had 548 events and a written `thickness.py`; its directory was
gone by the time the task was redispatched. The unit test missed this because it drives
`Engine::attach` directly and never takes the CLI path. Fixed: salvage now keeps worktrees
named by an interrupted attempt, and commits their work as before.

**`resumable()` did not check the provider.** A session belongs to the CLI that opened it,
so continuing one with a different provider would pass a session id its CLI never created.
Latent only because of the bug above. Fixed by matching on provider.

**An operator stop was recorded as a rejection.** An agent killed mid-task having written
nothing took the "changed no files" branch and was marked `rejected` — a verdict it never
earned, which both spent one of the task's two tries and put a mark on the provider's
record that routing reads as incompetence. This is what pushed `implement-slant` to
`failed` and forced a manual reopen. Fixed: cancellation now yields `interrupted`, and an
interrupted attempt no longer counts against the task's tries.

## qwen

Two verified, two killed while silent. Both failures had the same signature: a couple of
file reads, the tool results arriving, then nothing at all for the full idle timeout.
Raising the limit from 120s to 300s did not help, because the stall is not bounded. Its two
successes took 12m01s and 10m46s against muse's 97-second average. Being unable to tell a
thinking agent from a dead one is worse for scheduling than a clean failure. Being removed
from the roster.

## Operator overrides used

Both mid-run, both deliberate, both recorded here because a trial that hides them is not
evidence:

1. Committed a two-line sign fix to the merged `tests/test_slant.py` on the integration
   branch, so `implement-slant` had a coherent target.
2. Reopened `implement-slant` in the board (`state='open'`, `attempts=0`) after the
   misclassified rejection failed it.

After the fix, qwen implemented slant in 12m01s and it merged. The merged metric measures
upright DejaVuSans at `+0.00` and the oblique face at `+11.05`.

## Established

- Firm plans, dispatches and integrates real work from a written brief, unattended.
- A brief's rules survive the planner into per-task briefs, concretely rather than as
  slogans — the single most load-bearing assumption in the design.
- The scorer has teeth when the brief gives it an external fact to stand on.
- Run-level resume is worth having: the run was stopped mid-flight, a test and a config
  were changed, and it continued with nine merged tasks intact.

## Not yet tested

- Merge conflicts between concurrent attempts. Still never seen with real agents, and no
  `integrate` task is created when one occurs.
- Session-level resume through the CLI path. The bug above means it has never actually run;
  the new tests cover the salvage interaction, but not a live agent continuing a session.
- Whether the distribution-level check that would have caught the slant defect can be
  anchored objectively at all.
