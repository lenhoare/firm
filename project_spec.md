# Firm v1 — design

Status: milestones 1 and 2 implemented and exercised on live agents; the rest is design.
Supersedes the v0 spec previously in this file. Date: 8 September 2026.
What live runs established, including the corrections they forced, is in
`v1_first_trials.md`.

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

A check may be **scoped to one task** (`verify` on the task) rather than the whole run.
This is load-bearing in partition mode: while other modules are still stubs a whole-suite
check necessarily fails, so judging one task by it rejects perfectly good focused work —
the failure `second_live_trial.md` identified. The run-level command then runs once at the
end and is **reported, not enforced**.

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

The shared knowledge channel, and the piece taken directly from AIRA³. It is **shared
cognition**; the task graph is shared execution state, and the two are kept apart — see
*Planning: the board as a living plan*.

Append-only and typed. An entry has `id, run_id, task_id, author, type, title, body,
tags[], files[], created_at, superseded_by`. Types: `finding`, `convention`, `blocker`,
`dead_end`, `api_fact`, `decision`.

Two sources, and **agents are not one of them**. Asking a worker to write entries was tried
in design and rejected: it contradicts the "change only this file" instruction, a shared
file in a worktree would be the most merge-conflict-prone thing in the repository, and
unrewarded side-work is the first thing a cheap model drops.

- The **controller** publishes from evidence it already holds — outcome, native exit,
  check verdict, files changed, interruption. Costs nothing and cannot be skipped.
- An **observer** provider reads each finished attempt's distilled event stream and writes
  what only that agent knew: dead ends, constraints, environment facts. Configured by
  `forum_observer`, invoked through a provider's `observer_args`, budget-gated, and never
  on the critical path — the task reaches its state first.

**The hard part is that an unbounded forum poisons every prompt.** v0 already learned
this the expensive way with its 48 KiB output clips and bounded meeting transcripts. So:

- Retrieval is a **relevance slice**, not the whole log: most useful kinds first, newest
  within a kind, under a hard byte budget.
- A first attempt is not shown notes about its own task; a **retry** is, because they say
  why it was rejected, and it is also told the rejection directly.
- Observer entries **outlive their run**; controller bookkeeping does not. This is
  load-bearing rather than an optimisation: in a fully parallel run every agent starts
  before anything has been published, so a run-scoped forum is written and never read.
- Entries can be **retired** when they stop being true. Two entries saying the sandbox
  could not run `rustc` became false the moment that was fixed, and carrying them forward
  would have misled every later agent.

Informal in tone, but **typed in structure**. Leaving it conversational and unconstrained
was considered and rejected on evidence: unbounded text poisons every prompt, which is why
entries carry a kind, a byte budget, kind-ranking and retirement.

Still open: nothing detects supersession, so two entries about different implementations of
the same function can coexist — tolerable, since an agent can usually tell them apart — and
retirement is manual. Curation as a scheduled job remains the eventual answer.

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

## Planning: the board as a living plan

Two shared systems, and keeping them distinct matters:

> **Forum = shared cognition. Blackboard = shared execution state.**

The **forum** is what agents have learned — discoveries, warnings, constraints, dead ends,
approaches worth reusing. The **blackboard** is the authoritative task graph and its state.
One is informal in tone but typed in structure; the other is machine-readable and is the
only thing the dispatcher acts on.

We built both and missed the bridge between them. `create_run` is the only thing that
writes tasks: afterwards `set_state` and `claim` change a task's *status*, but nothing can
add, split, block, merge or supersede a node. **The graph is write-once.** So when an
observer discovered the toolchain would not run, that knowledge could reach a later agent's
prompt but could never change the plan. That bridge — *forum insight becomes a proposed
graph mutation* — is the missing piece.

### A mutable graph — implemented

The graph has typed mutations: `add`, `split`, `merge`, `block`, `cancel`, `supersede`,
and dependency edits. Agents and the observer **propose**; the controller **decides**. A
proposal carries a reason, and accepted mutations record who proposed them.

**A mutation is a spend primitive.** An agent that can add tasks can commit the budget, in
a project whose whole premise is controlling spend. So proposals are gated exactly like
dispatch: a cap on tasks per run, cycle checks on every accepted edge, and the existing
rolling allowances. Anything that would make the graph unsatisfiable is rejected outright.

Implemented as `Add`, `Block` and `DependOn`. Splitting a task is `Add` the children plus
`Block` the parent; no separate operation was needed. Guards: a new task must carry its own
`verify` command, dependencies must exist, the whole graph is revalidated after every change
so no mutation can introduce a cycle, finished work cannot be revised, and a run is capped
at `MAX_TASKS_PER_RUN`. Every proposal is recorded with its author and outcome — a refusal
is evidence about the proposer, and dropping it silently would hide an agent repeatedly
asking for something that will never be allowed.

The observer is the first proposer: it already reads every finished attempt, and now may
ask for a missing task or block one that cannot proceed, alongside its forum entries. Same
call, no extra spend.

This is worth more than getting the initial plan right, because **a bad plan is cheap to
discover and cheap to fix once the graph can change.** Execution surfaces plan defects for
free: a wrong dependency appears as a blocked task, a missing task as a failing integration
check. Investing in correctability beats investing in first-shot plan quality.

### Decomposition — implemented

`firm plan --brief BRIEF.md` makes one planning call and writes the graph as the JSON
`--tasks` accepts; a person reads it before anything runs. Validation runs each proposed
check against the workspace as it is now, and reports any that cannot execute or that
already pass.

**Decomposition quality tracks how well the test suite partitions.** Observed directly: a
five-sentence brief and a one-line "Make the crate work." produced the *identical* five-task
graph, with the same scoped checks — because the planner reads the workspace, and the
crate has one test file per module. The brief carries intent; the workspace carries
specification.

The `verify` requirement is what forces this: to give each task a check that fails now and
passes when done, the planner has to find a real seam in the test suite. So the prediction
is that decomposition degrades on a project with one monolithic suite, shared fixtures, or
no tests — not because the brief is worse, but because there is no seam to find. The
remedy is then to **add test seams first**, which is itself a task the planner can propose
once the graph is mutable. It is told so in its prompt.

Two things this needed that were not obvious:

- A planner needs its **own invocation** (`planner_args`). A CLI's plan mode produces a
  plan artifact rather than a direct answer, and a schema-constrained reply is produced
  immediately without exploring. It needs read-only tools, room to look at the workspace,
  and then a plain answer.
- **Idle detection had to be disabled** for it. That check assumes a streaming event log;
  a planning invocation emits nothing until it has finished thinking, so silence is work,
  not a hang. The first attempt killed a planner that was busy confirming its own checks
  failed.

**Planner comparison, same brief, same workspace.** Grok and Codex produced the *identical*
five-task decomposition with the same scoped checks. Grok wrote longer, more prescriptive
briefs; Codex wrote terser briefs with sharper acceptance criteria, and over-specified
slightly — it required behaviour at width zero that no test covers. Codex used 10,445
tokens in one call, 45s; Grok took a comparable 44s but reports no cost in plain-output
mode. **Planning cost is not recorded anywhere**, which makes exactly this comparison
harder than it should be; recording tokens and cost per planning call is worth doing before
choosing a default planner on economics.

### Allocation from evidence, not self-report — implemented

A bidding model in which agents declare `confidence: 0.87, capability: 0.92` was
considered and rejected: those numbers are produced by the model about itself and are
uncalibrated. The attempts ledger holds better data, and routing now uses it.

Cost still leads — that is the point of the roster. Evidence orders providers within a
tier, and demotes one below every tier when it has enough of a record and too much of it
is rejection: a cheap agent whose work is usually thrown away is not cheap, because each
rejection buys another run. An unproven provider is given the benefit of the doubt rather
than ranked last on no evidence, and only the last fortnight counts, since a provider that
was unreliable a month ago may have been fixed.

Routing is a default, never a verdict. A task may pin a provider, and `--provider` forces
one for a whole run — which is also how two providers are compared on identical work.
`firm board --stats` shows the record routing is reading, so the choice is inspectable
rather than something the system does silently.

### Deliberately deferred: ensemble decomposition

Several agents independently proposing graphs, cross-examining each other, synthesising
candidates and arbitrating between them is an appealing design, and may well be a strong
problem solver. It is deferred, not rejected, for three reasons:

- **It is the most expensive thing in the system.** Roughly `2N+2` model calls before any
  code is written, against a first goal of doing more work for less money.
- **There is no baseline.** A single manager decomposing has not been tried, so an
  ensemble would be fixing an unmeasured problem.
- **Plans have no objective score.** AIRA³'s ensemble works because candidate *solutions*
  are ranked by a metric. Arbitrating between *plans* is a judgement call, so the extra
  spend buys something we cannot verify was better.

Revisit when measurement shows single-manager decomposition is the bottleneck. If it is
built, note that **agreement between agents is weak evidence**: models share training data
and see near-identical prompts, so independent proposal of the same node is correlated
rather than corroborating.

### Rejected: advisory coordination over files

"Another agent is editing this file" as a forum message reintroduces advisory locking
between models. Isolated worktrees and a serialised merge already solve it structurally: a
collision either applies cleanly or conflicts loudly, and neither outcome depends on an
agent remembering to announce itself.

## Roster and cost tiers

The v0 provider record already carries `enabled`, `max_runs`, `description`, cooldown and
per-role argument sets. v1 adds:

- `tier` — a cost band (e.g. `0` cheap/local, `1` mid, `2` premium).
- `max_concurrent` — per-provider parallelism.
- optional class affinities, informing routing.

**Routing policy:** an unpinned task takes the cheapest provider that has both allowance
and a free slot, so work fills the cheap tier and **spills to the next** rather than
queueing behind a busy provider. Escalate a tier on repeated failure or for classes
reserved for premium (decomposition, integration, final review).

Providers also carry their own `worker_timeout_seconds` and `idle_timeout_seconds`, and a
separate `observer_args`. One global limit proved too crude: agents differ by an order of
magnitude, and an observer given planning arguments behaves like an agent with tools and
never replies. Each tier has its own budget per rolling
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

1. **Done.** Task board, pull dispatcher, worktree isolation, command scorer,
   partition mode. See `v1_first_trials.md`.
2. **Done.** The forum, with relevance slicing, a byte budget, cross-run carry-over and
   retirement.
3. **Done.** Tiered roster and routing, single-manager decomposition with validated
   checks, the mutable graph, and the forum-to-graph bridge. Cost is now recorded per run
   as share of a rolling window. Next: routing from the attempts ledger.
4. Run archive keyed by `run_id`, plus JSONL export and the Lab surface.
5. Pluggable scorers: human and agent.
6. Compete mode.
7. Claude added to the roster by configuration.

## Judging work the team also specified

Where a project has no test seam, the planner creates one — which means agents write the
tests that later agents are judged by. The trial that established this had a human-written
acceptance suite above the agent-written seams, so the risk was bounded. On a real project
it may not be, and two structural guards apply rather than trusting the arrangement:

- **Declared file scope.** A task names the files it may modify; an attempt that changes
  anything else is rejected. The obvious way to pass a test you cannot satisfy is to edit
  the test, and `files_changed` was already recorded and never acted upon.
- **Proof that a test tests something.** A test-writing task names a command that must
  still fail once the test exists. A test that passes against unimplemented code asserts
  nothing.

Both are model-free and cost nothing per run. A third measure — routing test-writing to a
different provider than implementation, as class affinity in the roster — reduces
correlated error but is weaker than it appears, for the same reason ensemble agreement is:
models share priors. It is worth doing and is mostly configuration.

Neither removes the underlying limitation: when agent-written tests sit at the top of the
chain, nothing establishes that they encode what was actually wanted. The human checkpoint
at `firm plan` is the answer, and seam tasks are a distinct early layer that is worth
reading.

Compliance was the practical obstacle. Asked for `files` as a rule in prose, the planner
omitted it entirely; stated as a requirement, with the field in the example, it emitted
scope and proof-of-failure for every task. Optional-looking fields get dropped — the same
lesson as asking workers to self-report.

## Stopping a long run

Chunking was never the problem: each task is its own attempt, worktree and merge, so work
lands incrementally and everything merged is a commit on the run's branch. Resuming was.
Every run minted a fresh run id and branch, so a stopped run could not be continued — the
work survived, but only by starting again and redoing it.

Pausing mid-task destroys nothing. An agent killed part-way has what it had already written
committed to its attempt branch before the interruption is recorded, and attempt branches
outlive their worktrees. The work is not reused automatically — a half-finished attempt is
a poor starting point, and the task starts again cleanly — but `--resume` lists it so it can
be inspected or cherry-picked.

`--resume` continues a run on its own branch. Reconciliation on open returns any task left
`running` to the queue and closes its attempt as interrupted, which also keeps the ledger
honest: an attempt that never finished is not evidence about a provider. v0 had this
property explicitly on restart and v1 had dropped it.

Agent CLIs do keep their own resumable sessions from headless runs — qwen has
`--continue`, `--resume` and even `--session-id`, muse has `resume`, and grok writes a
session directory per working directory. Firm deliberately does not use them. Each attempt
gets a fresh worktree and a fresh session, with the previous rejection passed in the
prompt, because resuming would carry forward the reasoning that was rejected, point at a
worktree that has since been deleted, and make attempts conditional on each other — which
would spoil the ledger as a record and foreclose compete mode.

## Learned from running it

Recorded here because each contradicted an assumption in this document:

- **Structured event streams, not a PTY.** Both shipped CLIs emit machine-readable event
  streams (`--json`, `--output-format streaming-json`) carrying reasoning summaries, tool
  calls and status. That is strictly better than scraping a rendered screen, and it does
  not change agent behaviour by making a CLI believe it is interactive. A PTY is held in
  reserve for a CLI that genuinely blocks on input we cannot answer another way.
- **Agents finish without exiting.** Reading output incrementally gives live activity and
  an idle timeout, so a quiet agent's work is committed and scored instead of holding a
  slot for the full limit.
- **The workspace must be its own repository root.** A workspace nested in a larger repo
  would silently isolate the enclosing project and hand agents everything in it.
- **Derived state accumulates.** Integration worktrees cost about 48 MiB per run;
  `firm board --prune` removes them, and the branches retain all the work.

## Deferred

Multiple machines; containerised isolation; sophisticated budget prediction; automatic
forum summarisation by a premium model; mobile and VR surfaces.
