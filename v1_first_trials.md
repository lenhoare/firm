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

## Not yet tested

Merge conflicts between concurrent attempts on the same files; the retry path on a real
agent; compete mode; the forum.
