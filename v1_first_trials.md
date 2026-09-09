# v1 first trials — 8 September 2026

Four live runs of the v1 board with real agent CLIs. Every attempt below is recorded in
`.firm/v1-demo.db`; the merged work is on each run's `firm/run-<id>` branch. No run
modified the operator's own branch.

## What was being tested

Milestone 1: the task board, the pull dispatcher, git-worktree isolation, and the
controller-run scorer, in partition mode. Then multi-provider routing.

## Runs

| Run | Workspace | Providers | Tasks | Result |
| --- | --- | --- | --- | --- |
| `4bc99e20` | taskboard-trial | muse | 3, chained | 3 merged |
| `19c35afa` | parallel-trial | muse | 4, independent | 4 merged |
| `fa0338c6` | parallel-trial | muse | 4, independent | 4 merged |
| `ef8225fa` | parallel-trial | muse + grok | 4, independent | 4 merged |

Per-attempt durations, in seconds:

- `4bc99e20` — parser 93, analytics 147, report 90. Strictly sequential by construction.
- `19c35afa` — roman 36, wrap 66, slug 81, columns 114. Three ran at once; wall clock
  2m29s against a sequential sum of 4m57s.
- `ef8225fa` — columns 37 (grok), wrap 52, roman 61, slug 900 (timed out).

All four runs were verified independently after the fact: no `todo!` stubs remained on the
integration branch and the full test suite passed.

## Established

**Isolation holds.** Every attempt changed exactly the one file its task named. Agents
never saw each other's worktrees.

**Dependencies are real.** In `4bc99e20`, analytics branched from the *merged* parser
commit, so the agent read a genuine parser rather than a stub.

**Parallelism works.** In `19c35afa` three agents started together and the fourth claimed
the first freed slot. In `ef8225fa` all four dispatched at t=0: three to muse, at its
`max_concurrent`, and one to grok.

**Per-task checks are load-bearing.** Both taskboard and parallel runs had tasks pass their
own scoped check while the whole suite was still failing on other stubs. Under a single
run-level scorer every attempt would have been rejected and both runs would have failed
outright. This is the concrete fix for the problem `second_live_trial.md` identified.

**Native exit and verdict must stay separate — now demonstrated on a real agent.** In
`ef8225fa`, muse wrote a complete and correct `slugify`, then hung without exiting. The
controller killed it at the 900s limit and recorded:

```
native_exit  = None
interruption = "Worker reached its time limit"
passed       = 1
state        = verified → merged
```

The work was committed, scored, and merged **despite the agent never exiting cleanly**,
because the controller's check decides what is kept. Under v0, where verification
overwrote the exit code, this correct implementation would have been discarded as a
failure. This is the same shape as Qwen in `first_live_trial.md`, and it is now handled.

**Routing spreads work across the roster.** Unpinned tasks take the cheapest provider that
has both allowance and a free slot, filling tier 0 and spilling to tier 1.

## Corrections made during the trials

- Routing originally picked the lowest tier unconditionally, so an unpinned task queued
  behind a busy cheap provider while a dearer one idled. A second provider was unreachable
  except by pinning. Fixed before `ef8225fa`.
- An earlier multi-provider run sent everything to muse. This was **not** a routing
  failure: a stale binary. A `cargo build` had been run from inside a git worktree, so it
  compiled the trial crate, reported success, and never rebuilt `firm`. Verify binary
  freshness before drawing conclusions from a run.

## Worth changing next

1. **Per-provider timeouts.** One global `worker_timeout_seconds` cost `ef8225fa` fifteen
   minutes of wall clock for about one minute of work. A provider that reliably hangs
   after finishing should not hold a slot for the same duration as one that needs it.
2. **Detect "done but not exited".** The work was complete minutes before the timeout.
   Watching the worktree for a quiescent tree, or scoring early, would reclaim most of
   that time.
3. **Integration worktrees accumulate.** Each run leaves `.firm/worktrees/run-<id>/`
   checked out. Useful for review, unbounded over time; needs pruning.
4. **Grok is faster than muse on these tasks** (37s vs 52–900s) but is tier 1. Once there
   is enough data, tier should be informed by observed cost and reliability, not only by
   list price.

## Agents could not compile, and it cost most of the wall clock

The forum found this on its first useful run. Grok, observing a muse attempt, reported
that `cargo` started but `rustc` failed with `Operation not permitted (os error 1)`, that
"sandbox denies `rustc` execution", and that an escalation "with `require_escalated` was
denied because approval prompts are disabled". One agent then spent minutes hand-linking
with `cc` under `/tmp/wt`.

Diagnosis, in order:

1. Muse warned all along: "Bubblewrap was not found on PATH... the built-in Bubblewrap
   will be used in the meantime." Installing `bwrap` was necessary but **not sufficient**.
2. `muse exec` running `rustc --version` succeeded, which was a misleading test: only
   `~/.cargo/bin/rustc` exists, a rustup shim, and that path is permitted.
3. A real build failed on the toolchain binary itself:
   `could not execute process /home/len/.rustup/toolchains/.../bin/rustc ... (never
   executed) Caused by: Operation not permitted`.
4. `bwrap --ro-bind / / -- .../bin/rustc --version` works, so bubblewrap is fine. Muse's
   sandbox policy simply does not bind `~/.rustup`, and exposes no allow-list — `muse
   sandbox` is Windows-only and `settings.json` has no sandbox keys.

Fix: `--disable-sandbox` on muse's **worker** invocation only. The manager, meeting and
observer invocations keep the sandbox, since they are read-only and never build. This is
consistent with the existing posture — grok workers use `bypassPermissions`, qwen `yolo` —
and the same caveat applies: worker authority is deliberately broad and is not an OS
sandbox. The git worktree bounds the blast radius to a disposable branch; use a disposable
workspace.

Effect, same four tasks, same providers:

| task | before | after |
| --- | --- | --- |
| wrap (muse) | 5m13s, then 3m01s | 31s |
| slug (muse) | 5m02s, then 1m41s | 43s |
| roman (muse) | 1m12s | 1m14s |
| whole run | 1m42s–5m50s | 1m14s |

All four agents ran the acceptance tests themselves, with zero sandbox denials. The
controller still runs its own check; the difference is that agents are no longer working
blind.

This is also the clearest argument so far for the forum. The blocker was invisible in every
diff, invisible to the controller — whose check passed every time — and only surfaced
because an observer read what the agent actually said.

## The retry path, verified on a real agent

Every live attempt until now had passed first time, so retry was exercised only against a
fake CLI. Forced with a check that can never pass (`sh -c 'exit 1'`), pinned to grok:

```
[    0s] → unsatisfiable dispatched to grok
[   19s] ✗ unsatisfiable grok rejected in 20s · exit 0 · check failed · NOTES.md
[   19s] → unsatisfiable dispatched to grok
[ 1m50s] ✗ unsatisfiable grok rejected in 1m31s · exit 0 · check failed · NOTES.md
[ 1m50s] ✗ unsatisfiable failed — 2 attempts exhausted
```

Reading back the prompts grok actually received, from its own session history:

- The **first** attempt had no retry notice, and a forum slice carrying
  `[dead_end] Do not change align's public signature... (by grok, earlier run)` — the first
  time cross-run knowledge reached a live agent.
- The **retry** had both: "A previous attempt at this task was rejected. Do not simply
  repeat it. What happened: this check always fails", and the controller's blocker about
  its own task, which `include_own` exists to unlock.

The injection framing survived into both prompts intact.

Separately, muse solved a *fair* ambiguity first time — a brief saying "the sum of its
values" where the tests pin bytes rather than chars — by reading the tests instead of the
brief. Checks matter more than prose.

## The messy trial — a project that does not partition

The prediction in `project_spec.md` was that decomposition quality tracks how well the test
suite partitions, and that where no seam exists the planner should make creating one the
first task. `workspaces/messy-trial` was built to test it: three interdependent modules
(parse feeds summarise feeds render) and a single `tests/acceptance.rs` in which every test
exercises the whole pipeline. There is no per-module seam to find.

From the brief "Make the acceptance tests pass", the planner produced seven tasks:

```
test-parse   test-report   test-render        (parallel, no dependencies)
        |            |            |
impl-parse   impl-summarise  impl-render      (each after its own test task)
        \            |            /
              pass-acceptance                 (after all three)
```

It built the seams first. Unprompted, it also gave the seam tasks `--no-run` checks, which
verify the test target compiles without requiring the implementation to exist — a sound
check for "add a test file", and a distinction nothing in the prompt asked for.

The run merged all seven: three test targets in parallel, three implementations in
parallel, then integration. Verified independently: no stubs remain and eleven tests pass
across four targets.

### It also found a real bug

The first attempt failed. `pass-acceptance` had nothing to do — the three implementations
had already satisfied its check — so the agent correctly changed nothing, and the
controller rejected it for changing nothing. **A task whose dependencies have already
satisfied it was impossible to complete.**

This is the same principle failing for the third time. Verification once overwrote the
native exit code; an agent that hung after finishing was nearly discarded; and here a diff
was again used as a proxy for a verdict. The check decides.

The fix is narrower than "run the check anyway", because three existing tests then broke —
correctly. They use a weak run-level sentinel that passes vacuously when an agent does
nothing, and one of them had exited non-zero. Merging on that would hide real failures. So
"nothing needed doing" is accepted only when the task has **its own scoped check** and the
agent **exited cleanly**: only a check written for this task can establish that this task's
goal is already met, and a catch-all check passing says little about any one task.

Re-run after the fix: seven merged, `pass-acceptance` recorded as "Nothing needed changing;
the check already passes", whole-project check passed.

## Not yet tested

Merge conflicts between concurrent attempts on the same files; compete mode; whether
observer entries are ever read by an agent that would otherwise have gone wrong; and
whether agent-written test seams are strong enough to be worth judging work by — in the
messy trial the same team wrote both the tests and the code that had to pass them.
