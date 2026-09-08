# Firm quick instructions

Firm coordinates a manager and several CLI agents inside one project workspace. Grok is the default manager; Codex · Astra, Grok, Qwen and Muse are all members of the same provider roster.

## Starting and stopping

Try the interface without spending model credits:

```sh
cargo run -- serve
```

For real agents, start the Codex app-server in one terminal:

```sh
codex app-server --listen ws://127.0.0.1:4500
```

Then start Firm in another:

```sh
cargo run -- serve --live
```

Open <http://127.0.0.1:7433>. Stop either terminal with `Ctrl+C`. Firm always starts paused and does not call an agent until you press **Start background work**. Use a disposable workspace while experimenting; change `workspace` in `firm.toml` before starting Firm.

## Workshop

1. Enter a small, verifiable objective and save it.
2. Press **Start background work**. The manager creates phases and assignments, a selected worker carries them out, and the manager reviews the evidence.
3. Watch **The work**, **Ideas & review decisions**, and the **Activity** log.

**Take control / pause** prevents further automatic dispatch after current activity settles. **Stop work** also requests cancellation of an active run. The Codex command in the overview attaches your terminal to Firm's retained Codex conversation when one exists.

Snapshots preserve versioned configuration, instructions, context and results. **Capture baseline** records the current state; a fork is an editable candidate recipe, not an activated configuration. Evaluations let you record whether a candidate appears better or worse.

## Team and settings

Settings can be changed only while the workshop is paused and no agent is active. They persist in the local SQLite state.

| Setting | Meaning |
| --- | --- |
| **Manager** | Agent responsible for planning and reviewing. Changing it keeps the existing objective, work and counters. There is no automatic failover. |
| **Manager turns** | Maximum planning/review calls within the rolling window. |
| **Worker runs** | Maximum worker calls across all providers within the rolling window. Meeting participants also use these shared allowances. |
| **Window (seconds)** | Length of the rolling period used by the two limits above. `18000` is five hours. |
| **Manager interval** | Minimum spacing between manager calls. Use `0` for quick demos; use a delay for live work. |
| **Manager timeout** | Maximum wall-clock duration of one manager call. |
| **Worker timeout** | Maximum wall-clock duration of one worker call. |
| **Usage stop threshold** | Codex account-usage percentage at which Firm stops dispatching Codex, leaving the chosen headroom. |
| **Usage freshness** | Maximum acceptable age of Codex usage telemetry before a Codex dispatch is held. |
| **Worker turns per run** | Turn/model-step budget passed to CLIs that support such a limit. |
| **Worker tool calls** | Tool-call budget passed to CLIs that support it; it cannot be enforced universally. |
| **Provider cooldown** | How long one provider is held after a response that looks like a rate-limit failure. Other providers remain independent. |

Each agent also has:

- **Enabled** — whether the manager may select it. Disabling an already assigned provider holds that task; it does not silently substitute another agent.
- **Role, strengths and preferred work** — editable guidance shown to the manager when delegating.
- **Run cap for this provider** — total local calls by that provider, shared across its manager, worker and meeting roles. This is separate from the overall Worker runs allowance.

The account strip reports remote usage where the installed tools expose it: Codex's five-hour percentage, Grok's weekly percentage, Qwen's `(prompt + output) / 40,000` weekly percentage, and Muse's raw total tokens. CLI readings refresh while Firm is idle. These readings are different from Firm's local project counters.

## Meetings

Create a meeting, optionally add pinned context, and ask a question. By default the agents answer in the order **Grok → Muse → Qwen → Codex · Astra**; each sees the earlier replies and Codex provides the final synthesis. Meetings are discussion-only and do not add tasks to the workshop.

Pause the workshop before starting a round. Meetings consume the same provider caps and rolling allowances as normal work. **Stop round** interrupts the current participant and skips the rest of that round.

## Configuration and recovery

`firm.toml` sets the workspace, commands, initial limits, meeting order and provider CLI arguments. Dashboard values override its defaults once a state database exists. Add another `[[providers]]` block and restart Firm to expand the roster.

Firm stores demo and live histories separately under `.firm`. A restart pauses work. If a crash leaves a Codex turn uncertain, inspect it first and use **Check interrupted turn**; Firm will not retry it blindly.
