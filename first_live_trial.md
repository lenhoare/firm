# First live trial — 6 September 2026

Outcome: all three workers were exercised once; the slug formatter remains unimplemented. The final Astra review was prevented by the approved quota guardrail, not retried or bypassed.

## Actual run

- Astra: three manager turns, correctly assigning Grok → Qwen → Muse and carrying failures forward without declaring success.
- Grok: announced an implementation approach but left `src/lib.rs` byte-for-byte unchanged. The original four tests still failed. The captured text does not establish why implementation stopped.
- Qwen: created `examples/playground/tests/edge_cases.rs` with 16 substantive tests, then exhausted its eight-turn limit without a final report. This was useful partial work despite the unsuccessful CLI outcome.
- Muse: inspected the implementation and tests, updated `README.md`, and created `REVIEW.md`. It independently reproduced the failures and explicitly distinguished intended behaviour from actual behaviour. Documentation quality has not been fully audited.
- Verification: the four original unit tests fail; running the integration target explicitly also produces 16 failures. Plain `cargo test --offline` stops on the failed unit-test target, so it does not exercise those integration tests.

## Guardrails and final state

The trial allowed at most four manager turns and three worker runs, with one run per worker and the existing five-minute manager spacing. Three manager turns and three worker runs were actually reserved; no worker was retried.

Before the fourth manager turn, Codex reported 83% usage in a quota window, exceeding the temporarily approved 80% stop threshold. Firm held dispatch. The trial was then paused, the normal 50% threshold and original provider caps restored, and all counters retained. The live dashboard is paused at `ready_review`; final manager acceptance is still pending. No additional experiment should be started automatically.

## What the snapshots established

- Baseline: `6f685fb2a8382010c6696242eb1f85839e8e929eecb168631d8c73bd73d9c7c2`.
- Qwen result: `f82a4de20ab6861f28cf6f80ab76c75fb09dd8396b633ee87f7e438ad6f2eca5` contains the new edge-case test file; its Qwen input snapshot did not.
- Muse result: `22bbaa6248962c6dc12b6cc53126799b45520f0986cd8f2f951faef2bf97ae5e` retains the identical test-file hash and adds the review documentation.
- `src/lib.rs` has the same hash throughout: `01f06c251f8b8b6046f46d36710967793bd6fec452c37403815c9410cb370637`.

This corrects an early monitoring observation: no test file was visible partway through Qwen's run, but it was created before that run ended. Failed exit status and absent final prose are not evidence of absent deliverables.

## Next changes worth considering (not implemented)

1. Diagnose why Grok stopped before writing; inspect its tool/permission evidence before changing approval settings or spending another run.
2. Retain native worker exit status separately from controller verification status. Currently a failing verification overwrites it, making a successful review/documentation task appear indistinguishable from a failed CLI process.
3. Supply compact before/after file evidence to the manager. The archive already proves Qwen's work, but the manager's supplied text did not establish that provenance.
4. Expose bounded live CLI progress, and distinguish useful partial deliverables from completed assignments.
5. Tune per-provider budgets and output scope from these observations; do not simply increase every limit. Qwen's thorough tests and Muse's long review may exceed what this tiny task needs.

The explicitly live supervisor is `tests/live-first-trial.mjs`; it requires a fresh idle live experiment and restores settings on normal/error exit. It is not a CI test and must not be rerun without another approved live trial.

## Follow-up: Grok's failure diagnosed

The retained Grok session `01a07772-46f6-7ba1-b24d-38deb7f983c0` establishes a permission failure, not an inability to write the implementation:

- `events.jsonl` lines 161–162 record a `search_replace` permission request immediately resolved as `cancelled` (`wait_ms: 0`). Earlier read operations were allowed.
- `updates.jsonl` contains the proposed `src/lib.rs` replacement: a character loop with ASCII classification, lowercasing, and a pending separator. It was prepared but never applied or validated.
- The failed edit reports “User cancelled the execution for tool `search_replace`”. The terminal update ends with `stop_reason: cancelled`, `cancellationCategory: PermissionCancelled`, and three model calls. This message does not establish that Len clicked Cancel; the unattended permission handling cancelled it immediately.
- Firm invoked Grok with `--permission-mode dontAsk`. The installed Grok permission guide says this permits pre-approved tools and built-in read-only operations, not arbitrary edits. Firm's plain headless adapter has no bidirectional approval bridge. There was no pending plan approval that Astra could subsequently accept.

Correction applied after Len's review: Grok's worker invocation now uses `bypassPermissions` for unattended edits, shell commands and tools, while explicitly denying the inherited Telegram MCP tools. Qwen's worker invocation now uses `yolo`. Manager and meeting invocations remain read-only. This is intentionally broad worker authority and not an OS-level sandbox; use disposable or otherwise trusted workspaces. No new Grok run was launched while applying the correction.

The per-assignment ceiling was raised from 8 to 64 turns/model steps and from 20 to 64 tool calls where supported. The separate run cap remains 3 per provider, and the live database was updated so the new limits apply to its next dispatch rather than only to newly created databases.

Also observed: Grok auto-loaded a Telegram MCP server from its installed local configuration. This trial's log shows only filesystem read/edit tool attempts, not messaging. The new deny rule prevents its tools from being approved, but does not claim to prevent the CLI from launching the locally configured server.
