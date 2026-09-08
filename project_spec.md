# Firm v1 — design

Status: design agreed, not implemented. Supersedes the v0 spec previously in this file.
Date: 8 September 2026.

v0 is the working prototype in this repository. It is treated as **ingredients**, not a
base: it solved the hard, unglamorous problem of driving agent CLIs reliably. v1 keeps
that and replaces the coordination model entirely.

## Purpose

Two goals, in order:

1. **Do much more useful work while spending far less on expensive models.** Cheap CLI
   agents carry the volume. Premium models (Codex, later Claude) are used only where
   they change the outcome.
2. **Exploit that several agents outperform one.** Not merely division of labour —
   parallel attempts, shared knowledge, and selection.

Constraint throughout: one Linux workstation, a personal ChatGPT subscription, and
whatever free or discounted provider credits are available.

## What v0 got wrong

Established by reading the implementation, not assumed:

- **Strictly sequential.** `schedule()` guards on a single `core.active` flag; exactly
  one manager turn *or* one worker run is ever in flight.
- **The manager sits in the per-task critical path.** One planning/review turn per unit
  of work is both the slowest and the most expensive possible topology. It is the main
  reason Codex capacity drains.
- **Only one task is real.** The scheduler acts solely on `tasks.last()`. The task list
  is a history, not a queue.
- **Verification destroys evidence.** `verify_command`'s exit code overwrites the
  worker's native exit code (`app.rs:645-647`), so focused, genuinely good work reads as
  `failed` when unrelated parts of the project are still stubs. Both live trials hit this.
- **No run identity.** Snapshots are a flat list with parent pointers; reconstructing
  "what happened in run 12" means walking the chain by hand.

## Influences

**AIRA²** ([arXiv 2603.26499](https://arxiv.org/html/2603.26499v2), FAIR / UCL / Oxford):
8 ReAct agents, a shared central artifact database as the only communication channel,
steady-state evolutionary search with temperature-scaled rank-based parent selection.
81.5% percentile rank on MLE-bench-30 at 24h. Named bottlenecks: compute throughput,
generalization gap, and static single-turn operators. **It has no forum.**

**AIRA³** (Meta, June 2026): finished 8th of roughly 4,000 teams for a Kaggle gold
fine-tuning a 30B Nemotron model. Many long-running agents in **separate environments**,
coordinating **asynchronously through a shared forum and a shared filesystem**, with **no
central manager in the coordination path** — an agent builds on another's experiment
without waiting for a supervisor. Run on GPT-5.5 and Claude 4.8.

### What transfers, and what does not

Transfers: removing the manager from the loop; isolated environments as the enabler of
safe parallelism; a shared filesystem plus a forum as the coordination substrate; a
trusted objective scorer as the backbone.

Does not transfer cleanly: AIRA³'s domain is **embarrassingly parallel** — candidate
solutions are independent and only the best is kept. General software work is
**interdependent**; two agents editing overlapping files produce merge pain, not a
portfolio. v1 therefore supports two modes over one mechanism, and ships the second first.

## Modes

- **Partition (v1).** Agents claim *disjoint* tasks from a dependency graph and their
  work is integrated. The everyday mode for building software.
- **Compete (planned, not in v1).** N agents attempt the *same* task in isolation; the
  scorer ranks the attempts; the best is merged and the rest discarded. This is the pure
  AIRA³ pattern and is expected to pay off wherever a sharp objective measure exists
  (performance work, algorithmic problems, anything with a metric). **The v1 data model
  must not foreclose it:** an attempt is already a first-class record keyed by task, so
  compete mode is "allow N concurrent attempts per task and add a selection step", not a
  redesign.

## Core model

- **Run** — one objective, start to finish. Has a `run_id`. Everything below belongs to a
  run.
- **Task** — a node in a dependency graph. `id, run_id, title, brief, acceptance[],
  files_hint[], depends_on[], class, tier_hint, state, created_by`.
  States: `open → claimed → running → submitted → verified | rejected → merged`, plus
  `blocked` and `failed`.
  Class is one of `design | implement | test | review | docs | integrate`.
- **Attempt** — one agent execution of one task in its own worktree. `id, task_id,
  provider, branch, base_commit, started_at, finished_at, native_exit, score,
  files_changed, output, forum_entries[], snapshot_ids[]`.
  Native exit and score are **separate recorded facts** and neither overwrites the other.
- **Forum entry** — a durable shared finding (below).
- **Roster member** — a provider, with a cost tier (below).

## Execution model

A pull-based dispatcher, not a state machine.

1. Select tasks whose dependencies are satisfied and whose state is `open`.
2. Admit while under **global concurrency 5**, per-provider concurrency, per-provider
   rolling-window caps, cooldowns, and tier budgets.
3. Route the task to a provider by `class × tier × remaining budget` (below).
4. Create a git worktree on a fresh branch from the current integration commit.
5. Compose the prompt: objective, task brief, acceptance criteria, and a
   relevance-filtered slice of the forum.
6. Spawn the agent CLI (v0's `worker.rs` machinery, unchanged in spirit).
7. On exit, the **controller** runs the scorer inside that worktree.
8. Record the attempt, then decide: merge, retry, escalate a tier, or block.

**Isolation is git worktrees.** The target workspace is a git repository; this is a
requirement, not a preference. Branch per attempt, from a known base commit. Agents
cannot clobber one another, and a bad attempt is discarded by deleting a branch.

**Integration is serialised.** One merge at a time onto the integration branch, and the
scorer runs again after merging. A conflict does not go back to the same worker; it
creates an `integrate` task, which is one of the few things worth a higher tier.

## The scorer

The backbone. Everything — merge, retry, escalation, ranking in compete mode — keys off it.

- **The controller runs it, never the agent.** An agent's claim of success is evidence,
  not a result.
- It returns a structured verdict: `{ passed, score, detail, artifacts }`. A numeric
  score is optional in partition mode and required for compete mode.
- v1 requires a scorer to be configured for a run.

Three interchangeable implementations, behind one interface:

- **Command scorer (default).** A configured command run in the attempt's worktree —
  v0's `verify_command`, promoted from a pass/fail gate to a first-class scored result.
- **Human scorer.** The attempt parks in a review state and waits for Len's verdict in
  the web app. For work that resists automatic measurement.
- **Agent scorer.** A roster member invoked read-only with a review prompt, returning the
  same structured verdict. **It must never be the provider that produced the attempt.**

The human and agent scorers exist so that projects which are hard to make objective are
still usable. They are a planned option, not the default: automatic measurement is what
makes parallel selection worth anything.

## The forum

The shared knowledge channel, and the piece taken directly from AIRA³.

Append-only and typed. An entry has `id, run_id, task_id, author, type, title, body,
tags[], files[], created_at, superseded_by`. Types: `finding`, `convention`, `blocker`,
`dead_end`, `api_fact`, `decision`.

Every agent reads a slice before starting and is expected to write after finishing —
especially dead ends, which are the entries that most reliably save another agent's run.

**The hard part is that an unbounded forum poisons every prompt.** v0 already learned
this the expensive way with its 48 KiB output clips and bounded meeting transcripts. So:

- Retrieval is a **relevance slice**, not the whole log: matched on the task's files and
  tags, plus recent and high-value entries, under a hard byte budget.
- Entries are deduplicated and superseded rather than accumulated.
- Curation (merging duplicates, marking entries stale) is itself a cheap, scheduled job.

This is an open design problem and should be expected to need tuning, not solved once.

## The manager

Demoted from per-task supervisor to **event-triggered arbiter**, with its own budget.
It is invoked on:

- run start — decompose the objective into the initial task graph;
- a task failing N times — decide whether to re-scope, escalate a tier, or block;
- an integration conflict the workers could not resolve;
- the board emptying or all remaining tasks blocking;
- run end — final review against the objective.

Nothing else. Removing the manager from the inner loop is the single largest cost saving
in v1.

## Roster and cost tiers

The v0 provider record already carries `enabled`, `max_runs`, `description`, cooldown and
per-role argument sets. v1 adds:

- `tier` — a cost band (e.g. `0` cheap/local, `1` mid, `2` premium).
- `max_concurrent` — per-provider parallelism.
- optional class affinities, informing routing.

**Routing policy:** prefer the lowest tier plausibly capable of the task class; escalate
a tier only on repeated failure or on classes explicitly reserved for premium
(decomposition, integration, final review). Each tier has its own budget per rolling
window, so premium spend is capped by construction rather than by good intentions.

**Claude** is a genuine roster member, added last, purely through configuration — a
`[[providers]]` block with its CLI invocation and `tier = 2`. Nothing in the design
should require code changes to admit it.

## Budgets and safety

Retained from v0 largely unchanged, because they work:

- Rolling-window allowances for manager turns and worker runs; per-provider run caps;
  per-provider cooldown on rate-limit-looking output; wall-clock timeouts.
- Codex account-usage gate with a headroom threshold and a staleness check; dispatch is
  held when telemetry is unavailable or stale.
- Loopback-only binding, an explicit control header on mutating requests, and
  **paused by default** — no agent runs on launch.
- Workers hold broad authority inside their worktree and this is not an OS sandbox. Use
  disposable or version-controlled workspaces. Worktree isolation improves on v0 here but
  does not make it safe to point at anything precious.

## Persistence and analysis

v0's `snapshots.rs` is kept close to as-is: content-addressed immutable artifacts, linked
recipes (config plus all prompts plus version and source hashes), parent chains, forking
into candidate recipes, and recorded evaluations. It is the strongest part of v0.

What changes: everything is grouped under a **`run_id`**, and attempts carry their score.
A run then exports as JSONL — one record per attempt, with task, provider, tier, prompt,
forum slice, native exit, score, files changed, and outcome — for offline analysis in
LangChain or similar. The question this is meant to answer is "which provider, prompt,
tier and limit combinations actually produced accepted work", across dozens of runs.

## What is kept from v0

`worker.rs` — CLI spawning, stdin and prompt-file transport, argument templating,
timeouts, bounded output, rate-limit detection, process-group cleanup. `config.rs` — the
roster model. `usage.rs` and `codex.rs` — provider telemetry and app-server RPC.
`snapshots.rs` — the archive. The safety posture above. Meetings, as-is.

## What is removed

The sequential `schedule()` loop and the `stage` string machine; `tasks.last()` as the
only actionable task; the one-decision-per-manager-turn protocol; and the entire v0
Workshop page.

## Web app

- **Board** — the task graph, live attempts, and what is blocking anything that is idle.
- **Run** — the timeline of one run: decisions, attempts, scores, merges.
- **Forum** — browse and search shared knowledge; edit and retire entries.
- **Roster** — providers, tiers, budgets, concurrency, live availability.
- **Lab** — the archive: runs, snapshots, recipes, forks, evaluations, export.

Team and Meetings survive from v0 broadly unchanged. The Green CRT theme is the visual
language.

## Milestones

1. Task board, pull dispatcher, worktree isolation, command scorer, one provider,
   partition mode.
2. The forum, with relevance slicing and a byte budget.
3. Tiered roster, routing, and the event-triggered manager.
4. Run archive keyed by `run_id`, plus JSONL export and the Lab surface.
5. Pluggable scorers: human and agent.
6. Compete mode.
7. Claude added to the roster by configuration.

## Deferred

Multiple machines; containerised isolation; sophisticated budget prediction; automatic
forum summarisation by a premium model; mobile and VR surfaces.
