# Second live trial

Result: **complete**. Firm is paused with Grok selected as manager and the dashboard left on the completed run.

## Trial

The disposable `examples/taskboard` crate began with three compiling stubs and six failing acceptance tests. The objective required one distinct, non-retriable assignment per worker:

1. Grok implemented `src/parser.rs`.
2. Qwen implemented `src/analytics.rs`.
3. Muse implemented `src/report.rs`.

Grok handled four manager decisions: three delegations followed by completion. Codex was deliberately not used as manager because its weekly usage bucket was at 89%, above Firm's unchanged 50% guardrail.

All provider caps are now 6. The 64-turn and 64-tool-call worker budgets remain in place. This trial used five Grok starts (four manager, one worker), one Qwen start, and one Muse start, all within their provider caps. The three old manager and worker starts still visible in the raw persisted arrays are historical; the rolling-window gate correctly ignored them.

## Outcome

- Grok's parser-focused tests passed. Its controller exit was 101 only because analytics and reporting were intentionally still stubs.
- Qwen's analytics-focused tests passed. Its controller exit was 101 only because reporting was intentionally still a stub.
- Muse's report implementation made the full controller verification pass.
- Final independent verification: `cargo test --offline` in `examples/taskboard` passed all 6 acceptance tests.
- Firm controller verification before the trial: 42 tests passed; Clippy with warnings denied passed.
- No worker requested interactive approval, no provider was retried or substituted, and no repair loop was needed.

This validates the context strategy: the manager understood that intermediate full-suite failures belonged to unassigned downstream phases, accepted focused evidence, and advanced the checklist correctly. It also exposes a useful future UI improvement: distinguish an assignment's focused result from expected whole-project failures, instead of initially displaying both early tasks as failed.

## Provenance

- Baseline snapshot: `0a9c0dce3dfca1783796c50c819c12eb848ae47bee6deea6cc8ce3885faaedef`
- Configured trial snapshot: `ebb6edda25395676f747a420ba2c570dcceb3e608620c181009118a820043fec`
- Final completed snapshot: `7eea9603c0456e6f162a6433d55387172c0818be149503c24c6c458c114e4b8f`
- Recipe: `1ab1d17f3ba28b734a074cc2039bdf2be5344a316850d17467b11d8dacadc58b`
