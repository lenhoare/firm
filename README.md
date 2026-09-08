# Firm

An experimental Rust controller with one configurable provider roster and selectable roles. **Grok is the default manager**; Codex · Astra, Grok, Qwen and Muse can all be workers. It uses the existing Codex app-server and CLI logins. There is no second Codex integration, direct OpenAI API integration or API-key fallback.

## Try the demo

```sh
cargo run -- serve
```

Open **http://127.0.0.1:7433**. Enter a small objective, save it, and press **Start background work**. The simulated plan → worker → review cycle takes about ten seconds and uses no model credits. Include `[fail]` in an objective to exercise worker failure.

The **Workshop** page concentrates on the objective, work, decisions and snapshots, with the activity log retained on its right. Its overview row includes the current Codex remote command beside manager turns, worker runs and accepted tasks. Use **Team** for manager selection, visible project allowances, and each agent's enable switch, run cap and editable role description. Role edits persist and are supplied to the manager on later turns. The shared account-usage strip remains at the top of Workshop, Team and Meetings.

Demo and live state are separate. Both start paused. New demo state uses a zero manager interval; live mode uses the configured five-minute interval. Adjust allowances in the dashboard while paused and idle. Changes persist in SQLite; `firm.toml` supplies defaults for a new database. New experiments retain rolling usage counters and archive the previous experiment.

## Connect to real agents

In another terminal, using the existing Codex subscription login:

```sh
codex app-server --listen ws://127.0.0.1:4500
```

Check connectivity, account type, usage windows, and the configured model without starting inference:

```sh
cargo run -- probe
```

Stop the demo server before starting the live dashboard on the same port:

```sh
cargo run -- serve --live
```

The default workspace is [examples/playground](examples/playground/README.md), a deliberately unfinished standalone Rust exercise with acceptance tests. Paste its suggested objective into the dashboard. Enable only workers whose CLI login/configuration you have already set up. Live mode calls Codex only after Start and only when usage readings and local allowances permit. The default 50% stop threshold intentionally leaves substantial headroom; current account usage may already exceed it.

Once Firm creates the manager thread, the dashboard shows the exact command for attaching the existing Codex terminal. **Take control / pause**, wait for active manager work to finish, then chat in Codex. **Stop work** requests cancellation as well. Before resuming background work, finish the human turn. Human and controller input do not yet have an atomic ownership lock across clients.

Use `--config PATH` for a different workspace, port, state directory, verification command, or model. Paths are relative to the config file. Only localhost connections are supported in this prototype.

## v1: the parallel board (`firm board`)

Everything above describes **v0**, the sequential prototype: one manager turn before every
single unit of work. **v1** replaces that coordination model — see
[project_spec.md](project_spec.md) for the design and its influences. Milestone 1 is
implemented and runs independently of the v0 dashboard.

```sh
firm board --tasks examples/tasks.example.json --config firm.trial.toml   # run a task graph
firm board --config firm.trial.toml                                       # report on the last run
firm board --watch --config firm.trial.toml                               # follow a run live
```

The run reports progress as it happens — each dispatch, each attempt's outcome with its
duration, exit code, verdict and changed files, and each merge — so a run is legible from
the terminal that started it. `--watch` redraws a live view of the board every two seconds. It is strictly read-only —
it takes no lock and creates nothing — so it is safe to run in a second terminal while a
run is in flight, and Ctrl+C stops watching without stopping the run.

`firm.trial.toml` is a ready-made first trial: Muse only, pointed at
`workspaces/taskboard-trial` — a standalone repository (created by you, ignored by this
one) holding the task-board exercise with its three modules stubbed and six acceptance
tests failing.

Several agents work **in parallel** on a graph of tasks, each in its own **git worktree**,
so they cannot collide. A task becomes ready only once its dependencies have merged. After
an agent finishes, the **controller** — never the agent — runs `verify_command` inside that
worktree as the scorer, and only work that passes is merged into the run's integration
branch. Passing alone is not enough: the scorer runs again after merging, and work that
breaks the integration is reverted.

- The workspace must be a **clean git repository**. Your own branch is never modified; each
  run works on `firm/run-<id>` and each attempt on `firm/attempt-<id>`.
- Attempt branches are kept after the run as evidence of what each agent actually wrote,
  including for rejected attempts. Their working directories are removed.
- An agent's **native exit code** and the **controller's verdict** are recorded separately,
  so focused work is never mislabelled by an unrelated failure elsewhere in the project.
- An agent that changes no files is reported as such rather than treated as success.
- At most five agents run at once (`MAX_CONCURRENT`), and each provider has its own
  `max_concurrent`. Providers carry a `tier`: `0` is cheapest. An unpinned task takes the
  cheapest provider that has both allowance and a free slot, so work fills the cheap tier
  and **spills to the next** rather than queueing behind a busy provider. A task may still
  pin itself to one provider.
- A task is retried once, then failed; anything depending on it is blocked, not stalled.
- Each task may carry its own `verify` command. This matters: while other modules are
  still stubs the whole suite necessarily fails, so judging one task by it would reject
  perfectly good focused work — the exact problem the second v0 live trial reported. The
  run-level `verify_command` is then used once at the end, and reported rather than
  enforced. The agent is told the exact command it will be judged by.
- Workers run with their **structured event streams** on (`--json` for Muse,
  `--output-format streaming-json` for Grok). Output is read incrementally, so the board
  records what each agent is doing while it works — visible in `--watch` — and
  `idle_timeout_seconds` releases an agent that has gone quiet after producing events.
  That is the "finished the work but never exited" case, which cost one early run fifteen
  minutes of wall clock for about one minute of work.
- Timeouts are per provider where it matters: a provider may set its own
  `worker_timeout_seconds` and `idle_timeout_seconds`, falling back to the run's
  allowances. Grok reliably finishes in well under a minute; muse ranges from 30s to
  several minutes and has hung outright, so one global limit was too crude for both.
- Rolling allowances are enforced before anything external happens, and counted from a
  durable ledger that spans runs: `worker_runs` overall, `max_runs` per provider, plus a
  per-provider cooldown after a rate-limited response. When an allowance runs out the
  remaining tasks are **held** — left open for later, not failed.

### The forum

Agents working in parallel share what they learn. Two sources, deliberately:

- The **controller** publishes from evidence it already holds — outcome, agent exit code,
  check verdict, files changed — which costs nothing and no agent can skip.
- An **observer** (`forum_observer`, default `grok`, empty disables) reads each finished
  attempt's event stream and writes up what only that agent knew: dead ends, constraints,
  environment facts. It runs on a provider's `observer_args`, is budget-gated like any
  model call, and never blocks work — the task reaches its state first.

Agents asked to self-report were rejected: it contradicts "change only this file", a shared
file in a worktree would be the most conflict-prone thing in the repo, and unrewarded
side-work is the first thing a cheap model drops.

A bounded, relevance-ordered slice is injected into each agent's prompt — dead ends first,
never notes about its own task. Entries are untrusted agent text, so they are rendered
attributed and quoted, framed explicitly as observations rather than instructions. Read
them with `firm board --forum`.

Tasks are authored as JSON for now. Automatic decomposition by the manager, the shared
forum, tier-aware routing, pluggable human/agent scorers and compete mode are the following
milestones.

## What works, and what is still experimental

- Durable dispatch reservations, global and per-provider rolling allowances, manager spacing, usage freshness checks, independent provider cooldowns, bounded process output, and cancellation of worker process groups.
- Codex structured planning/review responses choose an explicit worker ID. The default per-assignment budget is 64 turns/model steps and 64 tool calls where the CLI supports that limit. Qwen has CLI turn/tool/time limits, Muse a model-step limit, and Grok a turn limit. Every worker has Firm's external wall-clock timeout. No tool-call count limit is claimed for Codex, Muse or Grok. These are separate from the six-run provider caps and overall rolling allowances.
- Independent configured verification after a worker run, limited to 60 seconds. The initial command is `cargo test --offline`. Manager review has read-only permissions; test execution is the controller's responsibility.
- Current tasks, decisions, discoveries, recent events, retained worker output, Codex usage readings, and archived experiment snapshots in the dashboard. Output is capped at 48 KiB per stream and current activity at 300 events. Archives preserve these bounded snapshots.
- Restart always pauses. Uncertain dispatches become blocked and are never automatically retried. For an unconfirmed Codex turn, inspect it in Codex and use **Check interrupted turn** to confirm it is idle. Inspect worker processes and files after an abrupt machine/controller crash; normal shutdown kills worker process groups, but crash recovery does not claim to reattach or identify every surviving descendant.

Workers run serially in the chosen workspace. There is no concurrent worker execution, automatic worktree isolation or merge step, PTY adapter, or automatic reconnection or repair retries. Start with a disposable project. The dashboard is for trusted local use and is not a remotely authenticated service.

The terminal attachment and real planning/worker/review cycle still need a small live acceptance trial. Protocol tests use a fake app-server; demo success is not evidence of model quality or savings.

## Account usage header

Both Workshop and Meetings show the same **Account usage** strip above the page content (above the local run counters in Workshop). Percentages mean **used**, not remaining. Each card labels its provider-reported window explicitly. Codex reads the existing app-server's `account/rateLimits/read` metadata every 30 seconds, independently of active jobs; demo mode also permits this read-only telemetry connection without starting real model turns. The browser refreshes cached readings every five seconds. If app-server was unavailable on startup, start it and restart Firm to connect.

Codex shows only explicitly reported 300-minute windows. Multiple Codex pools remain separate; weekly, monthly and unknown-duration Codex windows are never relabelled as five-hour usage. Codex reset times use your browser's timezone. Old readings and passed reset times are marked stale, not silently reset to zero. The existing scheduling gates still check all relevant Codex windows, including weekly limits: this display does not change allowances or dispatch policy.

All configured providers appear, including disabled ones. While Firm is idle, the CLI readings refresh every ten minutes without making a model call. Grok runs `/usage show` through a short-lived PTY and displays its reported **Weekly limit (SuperGrok)** percentage and reset text. Qwen runs `qwen /usage`; Firm adds its prompt and output tokens, divides by the 40,000-token weekly allowance, and displays the resulting percentage. Muse runs `/usage` through a short-lived PTY and displays its reported **Total** token count raw, without turning it into a percentage. No percentage is estimated from Firm's local run counters.

Codex's field meanings follow the [official app-server rate-limit documentation](https://learn.chatgpt.com/docs/app-server#6-rate-limits-chatgpt). CLI observations were checked against the installed commands/source; they are not claims about every version or subscription plan.

## Manager selection

Use the **Manager** dropdown while paused and idle to choose Grok (the default), Codex · Astra, Qwen, or Muse. The selection persists through restarts and new objectives. The dropdown and worker cards are two role views of the same provider roster; providers are not configured twice. The selection controls workshop planning and review, not the meeting speaking order. There is no automatic failover and selecting a manager never starts work or resets counters.

A replacement receives the objective, current phases, five recent decision excerpts, worker roster, and (for review) the latest assignment and up to 32 KiB of its output. The common handoff packet is capped at 128 KiB. This transfers controller context, not private CLI sessions; returning to Codex resumes its retained thread with updated evidence. Decisions and immutable input/response snapshots record the actual manager.

All managers share the manager-turn cap and spacing. A manager turn also consumes the selected provider's run cap, shared with that provider's worker/meeting runs, and respects its cooldown. It does not consume the global worker-role allowance. CLI providers do not depend on Codex telemetry and their remote credit balance remains unknown; Codex additionally keeps its subscription usage checks. Live mode can start paused without a working Codex connection so CLI managers remain usable; restart to reconnect Codex.

After a failed planning/review invocation, selecting a different manager requeues only that step, still paused until **Start**. Uncertain Codex turns require reconciliation before switching. Completed work and deliberate blocked decisions are not retried by switching.

Each non-Codex manager needs explicit read-only `manager_args` in its TOML provider block; worker arguments are never reused implicitly. Codex keeps the existing manager transport while its worker role uses the installed CLI. The shipped configurations request planning-only output with implementation tools restricted. Managers must return a validated JSON decision and an explicit worker ID; malformed/nonzero responses stop the workflow and are retained for inspection. These are trusted CLI settings, not OS-level isolation. Fake-CLI tests verify routing and failure handling; real model compliance and quality still need a small live acceptance trial.

## Worker roster

Use **Worker providers** in the dashboard to enable/disable each provider and set its local run cap while paused and idle. Codex · Astra uses the existing installed Codex CLI, configured model and subscription login; changing Astra from manager to worker changes its role and instructions, not its identity. Astra is described to the manager as the first-choice designer for non-trivial architecture, constraints and decomposition, while remaining available for difficult implementation. These settings persist separately in demo/live SQLite state and override that provider's initial `enabled` and `max_runs` values in TOML. They survive new objectives and restarts without resetting counters. Removing a provider from TOML removes it from the roster; its historical counters remain. Reuse IDs only for the same provider.

The selected manager receives the current roster, descriptions, available local runs and cooldown status on every turn. It selects `assignment.provider`; unknown or unavailable selections are rejected. Disabling a queued task's provider holds that task until it is enabled again (or you stop the experiment). No silent failover. A rate-limit-looking response cools down only its own provider; manager review may explicitly choose another. With no eligible workers, initial planning waits without consuming a manager turn. Demo chooses the first eligible provider in config order and simulates it without launching the CLI.

CLI-provider capacity/login is not probed: for Grok, Qwen and Muse, "available" means eligible under Firm's local limits, not verified remote credit availability. Codex worker eligibility also respects the existing Codex usage gate. The overall `allowances.worker_runs` cap still covers **all** providers combined in the same rolling window. Failed dispatch attempts count. Old single-worker databases attribute their existing worker reservations/cooldown to Qwen.

To add another CLI, append a block to `firm.toml` and restart the controller (which starts paused):

```toml
[[providers]]
id = "another-agent"
name = "Another agent"
command = "/path/to/agent"
enabled = false
max_runs = 1
description = "Describe its useful strengths or limitations for Astra."
input = "stdin"
args = ["--headless"] # Replace with this CLI's actual non-interactive arguments.
```

Commands execute directly, never as shell strings. Arguments may contain `{workspace}`, `{codex_model}`, `{max_turns}`, `{max_tool_calls}`, and `{timeout_seconds}`. For CLIs that read a file, set `input = "prompt_file"` and include `{prompt_file}` in the arguments; Firm supplies a private temporary UTF-8 file and removes it after execution. Prompts are never expanded into shell code or placed directly in process arguments. Configure supported CLI limits explicitly; the controller cannot enforce an agent's internal tool-call count on its own. Commands/wrappers should stay in the foreground so process-group cancellation can reach their children.

The shipped definitions follow installed `muse exec --help`, `grok --help`, and Grok's installed permission guide. Muse keeps its sandbox and uses `--approval-mode never --approval-judge off`. Qwen's **worker** uses `--approval-mode yolo`; its manager and meeting invocations remain in plan mode with no tool calls. Grok's **worker** uses `--permission-mode bypassPermissions --no-subagents`, authorizing unattended edits, shell commands and tool calls. It explicitly denies `MCPTool(telegram__*)` so the inherited personal messaging integration is not authorized for coding tasks. These are broad worker permissions, not a filesystem sandbox: the working directory and assignment instructions do not themselves prevent access elsewhere. Grok's manager/meeting invocations remain in plan mode. The first trial's previous `dontAsk` policy cancelled the proposed edit immediately; it did not queue an approval for Astra or Len. Authentication, CLI-global settings, model choice and billing remain owned by each CLI. Adjust trusted TOML arguments deliberately for your environment. A legacy config containing only `qwen_command` still loads as a Qwen-only roster; it is not silently expanded.

## Meetings

Open **Meetings** in the header, create a discussion, and ask a question. The default round is **Grok → Muse → Qwen → Codex (Astra)**, with Codex asked to synthesise agreements, disagreements and next steps. Each participant receives the earlier replies; no meeting suggestion becomes an implementation task automatically. Replies appear as each agent finishes. Follow-up questions stay in the same saved meeting.

Background work must be paused and idle. Meetings use the **same** durable manager/worker counters, provider caps, cooldowns and Codex usage gates as implementation. A full round needs three worker runs and one manager turn; with the default three-run allowance, one full round consumes that window's worker allowance. Adjust allowances deliberately if you want more discussion. Disabled or unconfigured participants are visibly skipped; a provider also needs explicit `meeting_args`. Preflight checks the entire round before spending anything, then checks again at each dispatch. Failures are retained, not retried. Stop interrupts the active participant and prevents later replies without cancelling the workshop objective. Interrupted rounds are never resumed automatically after restart.

Firm uses a bounded shared transcript, **not** each CLI's private conversation history: pinned notes (4,000 bytes), an optional 4 KiB brief of the current objective/recent results/discoveries, the current question and preceding replies, then as many as 12 older messages that fit. The supplied packet is capped at 48 KiB; long replies are excerpted at 4 KiB with visible markers. The meeting retains up to 16 KiB of each reply, and each reply's **Context supplied** expander shows the exact input. These are byte limits, not exact token counts. Keep important decisions in pinned notes. This prototype retains at most 20 meetings, each up to 100 messages, in the existing demo/live SQLite database.

Each CLI starts a fresh discussion invocation in an empty temporary directory. The shipped `meeting_args` use Qwen's plan mode with no tool calls, Muse's disabled write/shell/web tools, and Grok's plan mode without subagents/web search. Codex gets a separate read-only app-server thread for each response, leaving the implementation thread unchanged. The shared packet preserves meeting continuity. This follows the [documented thread/turn API](https://learn.chatgpt.com/docs/app-server), not automation of the CLI's `/side` UI; importing full native forks would defeat the common context budget. Installed/global CLI configuration and external tools remain outside Firm's isolation guarantees: custom meeting commands are trusted local configuration, not an OS sandbox. Meeting instructions prohibit tools and implementation; no controller verification commands run in meetings.

Change `meeting_order` (at most eight unique IDs, ending in `codex`) and add provider `meeting_args` to expand meetings. Their prompt transport matches normal worker `input`. Meeting requests/replies are also captured in immutable snapshots, including the meeting instruction template. Snapshot imports remain archival only; they do not create active meetings, restore private sessions or reset counters. Demo simulates all replies without launching any CLI; live authentication and real discussion quality still need an acceptance trial.

## Immutable snapshots and recipes

The dashboard's **Snapshots & recipes** panel captures a manual baseline while paused, inspects artifacts, compares content hashes against a parent, exports/imports portable JSON bundles, saves candidate recipes, and records human evaluations. Captures also happen automatically immediately before each model/worker dispatch and after its result or failure. A failed input capture prevents dispatch; a failed result capture pauses scheduling.

Each manifest has schema version `1` and an ID equal to the SHA-256 hash of Firm's deterministic JSON serialisation (schema field order, sorted artifact/metadata keys). Artifact bytes live in an immutable object store, shared when their hashes match. SQLite indexes manifests, kinds, parent/recipe relationships, and artifact references. Demo and live archives are separate under `.firm/snapshots-demo/` and `.firm/snapshots-live/`.

A **recipe** records effective controller configuration (including the provider roster, enable switches and current allowances), the exact manager/worker instruction templates, and controller/dependency fingerprints. **Input**, **result**, and **manual** snapshots link a recipe to supplied requests, selected CLI/arguments/prompt transport, controller context, observable Codex history when available, Codex version readings in live mode, and workspace file contents. Worker versions are not automatically probed: custom wrappers might treat `--version` as an inference request. File permissions retain the executable flag. Workspace snapshots include uncommitted/untracked files, subject to exclusions and limits; Git HEAD is recorded when available.

Exports include the referenced ancestors, recipes, and artifact bytes, encoded as base64 in one JSON bundle. Import checks schema versions, safe artifact names, hashes, lengths, and complete acyclic lineage before publishing the graph. It is idempotent and does **not** load archived controller state, reset usage counters, overwrite files, execute commands, or activate a recipe. Paths in configuration describe the source machine; no workspace path is silently remapped.

Forking stores a new candidate with edited configuration and instructions, linked to its original snapshot. Its inherited context and results remain **baseline evidence**, not results from running the candidate. Candidates are not active profiles yet: promotion, workspace restoration, and budgeted comparison runs are a later step. Evaluation notes record Len's assessment without invoking a model.

Coverage is explicit in every manifest. Default workspace limits are 2,000 files, 1 MiB per file, and 16 MiB total. Generated directories, state directories, common credential paths, and symlinks are excluded. Recognised credential-like text is redacted and the artifact is marked; this heuristic cannot recognise every possible embedded secret, so review artifacts before sharing a bundle. Environment variables and login stores are not collected. Hidden provider state, implicit CLI context outside the workspace, unexposed worker conversation details, and live processes cannot be reproduced. A file walk is not an atomic filesystem snapshot; capture while the workspace is quiet. Existing runs from before this feature are not retrospectively reconstructed.

Bundles are limited to 64 MiB JSON / 32 MiB decoded artifacts. The UI lists the latest 200 manifests; older indexed records remain addressable by ID. Normal snapshot operations use no inference credits. Archives should be backed up with their object files and SQLite database while Firm is stopped; automatic garbage collection is not implemented.

## Development

```sh
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Tests cover scheduling, pause/stop, allowance exhaustion, restart recovery, failed workers, malformed decisions, early app-server notifications, process cleanup, and dashboard request boundaries. One protocol test binds a loopback socket.

The playground's failing tests are intentional and are not part of `cargo test` at the repository root.

Implementation references: [project spec](project_spec.md), [delegation principles](delegation.md), [prior process learnings](prior_learning.md), and the [official Codex app-server documentation](https://learn.chatgpt.com/docs/app-server). Protocol fields were checked against schemas generated by installed Codex CLI **0.153.2**. App-server is experimental; rerun the checks when upgrading.
