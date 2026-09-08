# Handoff: Firm — "Green CRT" workshop theme

## Overview

Firm is a local web app for controlling CLI-based AI coding agents. The existing
dashboard is functional but reads like an enterprise admin console: card grids,
uniform weight, telemetry competing with the actual work. This handoff replaces
its look and its information hierarchy with a single-phosphor CRT terminal
treatment, in which **the experiment brief is the hero** and all telemetry
(provider quotas, allowances, activity log) is demoted to a narrow fixed gutter.

The functional surface does not change. No features are added or removed. What
changes is the theme and the layout hierarchy.

## About the design files

The files in this bundle are **design references authored in HTML** — a
prototype showing intended look, spacing and hierarchy. They are **not
production code to copy**. The task is to recreate this design inside Firm's
existing frontend using its established components, state and patterns. Where
this document and the HTML disagree, this document wins (the HTML is a static
mock with hard-coded content).

- `green-crt-reference.html` — the design, standalone. Open in a browser.
- `theme.css` — the tokens and the CRT overlay, ready to drop in as-is. This
  is the only file intended to be used literally.

## Fidelity

**High-fidelity.** Colours, type, and spacing are final. Recreate faithfully
using the codebase's existing component structure. The one thing deliberately
unresolved is responsive behaviour below ~900px (see *Responsive*).

---

## Screen: Workshop (the main dashboard)

**Purpose.** The operator sets an experiment, watches three workers execute it
in a fixed order, and takes or releases control. They need to read the brief and
the queue at a glance, and check quotas only occasionally.

**Layout.** A single full-height shell, `display:grid`,
`grid-template-columns: 190px 1fr`. No page-level max-width — the design fills
the viewport. No cards, no rounded panels, no drop shadows anywhere inside the
app. Separation is done with 1px hairlines in `--crt-rule` only.

### Left column — telemetry gutter (fixed 190px)

`border-right: 1px solid var(--crt-rule)`, padding `20px 16px`,
`display:flex; flex-direction:column; gap:20px`.

1. **Wordmark.** `FIRM`, 15px, `letter-spacing:.2em`, `--crt-ink-bright`.
2. **CAPACITY.** Section label in the meta style (10.5px, `.16em`, uppercase,
   `--crt-ink-dim`), 6px below it one row per provider:
   `name` (`--crt-ink`) · two-digit percentage (`--crt-ink-bright`, or
   `--crt-ink-dim` when zero) · a 12-glyph bar of `▌` characters, filled
   glyphs in `--crt-ink` and unfilled in `--crt-ink-faint`. Names are padded to
   equal width so the columns align — the monospace grid is the whole point,
   do not let a longer provider name break it. Rows in the mock:
   `codex 66`, `grok 07`, `qwen 00`, `muse 00`.
3. **ALLOWANCE.** Same label style, then `manager turns 0/4`, `worker runs 0/3`,
   `tasks 1/3`. Labels `--crt-ink`, values `--crt-ink-bright`.
4. **LAST.** The three most recent activity entries, `HH:MM` + short phrase,
   the whole line in `--crt-ink-dim`. This is a teaser, not the log — the full
   log lives behind a click (see *Interactions*).
5. Spacer (`flex:1`), then the workspace path at 10px `--crt-ink-dim`,
   wrapped over two lines: `/home/len/dev/firm` / `examples/taskboard`.

Quota reset times and "checked at" timestamps are **removed from the surface**
and moved into a title/tooltip on each capacity row. They were noise.

### Right column — the work (padding `26px 30px 30px`)

1. **Status strip.** One flex row, 10.5px, `.18em`, `--crt-ink-dim`:
   a state chip (`● COMPLETE` in `--crt-ink`, dot inherits colour), the
   qualifier sentence (`paused on startup — resume when ready`), `flex:1`
   spacer, then nav `workshop / team / meetings` right-aligned. Nav is text
   only: current section `--crt-ink-bright`, others `--crt-ink-dim`, no
   underline, no pill, no active background. 22px below.
2. **Hero line.** `Set an experiment.` — 26px, `line-height:1.35`,
   `--crt-ink-bright`, `max-width:34ch`. When an experiment is set this becomes
   its one-line title; the placeholder above is the empty state.
3. **Brief.** 13px, `line-height:1.8`, `--crt-ink`, `max-width:70ch`. The
   measure cap is load-bearing: the current console runs prose the full width
   of the window and it is unreadable. Long briefs truncate to roughly 6 lines
   with an expander rather than scrolling the page.
4. **Queue.** One strip per worker, `gap:10px` between them, each
   `display:flex; gap:14px; padding:10px 14px`, 12.5px:
   two-digit index (`--crt-ink-dim`), worker name in a fixed 60px cell
   (`--crt-ink-bright`), the target file + a short description (`flex:1`),
   then the state word, right-aligned.
   - queued: `border:1px solid var(--crt-rule)`, all text `--crt-ink-dim`
   - done: `border:1px solid var(--crt-rule)`, text `--crt-ink`, state
     `--crt-ink-bright`
   - running: `border:1px solid var(--crt-rule-strong)`,
     `background: var(--crt-wash)`, state `--crt-ink-bright`
   - failed: same as running but the border and state word take the failure
     colour (not in the mock — use `#ffb08a`, the only non-green ink permitted)

   The running row is the only lit element on the screen. Do not add a spinner,
   a progress bar, or a pulse; the wash plus the brighter border is the signal.
5. **Prompt.** Above it `border-top: 1px solid var(--crt-rule)`, 14px of
   padding. Then `> ` in `--crt-ink-bright` followed by the live command in
   `--crt-ink` (`codex --remote ws://127.0.0.1:4500`) and the blinking caret.
   This line is where control actions live (see below) — it replaces the three
   large buttons in the current header entirely.

### The CRT overlay

Applied once, to the shell. Two `pointer-events:none` layers from `theme.css`:
1px scanlines every 3px at 4.5% white, and a radial vignette darkening the
corners. Static — never animated. Content must sit at a higher `z-index` than
neither layer swallows clicks.

---

## Interactions & behaviour

- **Control actions are typed, not clicked.** Focus the prompt line (click it,
  or `/` anywhere) and the operator types `start`, `pause`, `stop`, `resume`.
  Show a one-line inline hint of valid verbs in `--crt-ink-dim` on focus.
  Keep keyboard equivalents for the same three actions if they already exist.
- **Nav** — `1` / `2` / `3` jump to workshop / team / meetings, as does
  clicking the words. Hover: `--crt-ink-dim` → `--crt-ink`, 90ms.
- **Activity log** — clicking the `LAST` block opens the full log. Prefer an
  overlay panel sliding in from the left over the gutter (140ms,
  `cubic-bezier(.2,0,0,1)`), same type styles, no new chrome. A route is
  acceptable if that matches the codebase.
- **Capacity rows** — hover reveals reset time and last-checked in the title
  attribute. No click target.
- **Queue rows** — clicking a row expands that worker's output inline beneath
  it, monospace, `--crt-ink`, indented to the description column. Collapsed by
  default.
- **State changes** — when a worker transitions, cross-fade the row's border
  and background over 160ms. No layout movement, no scroll jump.
- **Live updates** — new activity entries and quota changes swap in with no
  animation. A terminal does not animate its scrollback.
- **Loading** — no skeletons and no spinners. Unknown values render as
  `--` in `--crt-ink-dim`.
- **Errors** — a failed run sets the queue row to the failure treatment and
  writes one line under the prompt in `#ffb08a`. Nothing is thrown away or
  hidden; the log keeps the detail.

## State

Nothing new. The design reads the state Firm already has: per-provider usage
(percentage or token count, limit label, reset time, checked-at), the three
allowance counters, controller state (`running` / `paused` / `complete`), the
experiment brief text, the ordered worker queue with per-item target file and
state, the activity log, the remote command string and workspace path. Two
pieces of pure view state are added: prompt focus, and which queue rows are
expanded.

## Design tokens

All in `theme.css` — use it as the source of truth rather than transcribing
hexes. Summary:

| token | value | use |
|---|---|---|
| `--crt-bg` | `#050a06` | every surface |
| `--crt-ink` | `#7dffa0` | body text, prompt |
| `--crt-ink-bright` | `#d9ffe6` | headings, values |
| `--crt-ink-dim` | `#58a870` | labels, muted states |
| `--crt-ink-faint` | `#204a2c` | unfilled bar glyphs only, never text |
| `--crt-rule` | `rgba(125,255,160,.18)` | hairlines |
| `--crt-rule-strong` | `rgba(125,255,160,.50)` | active row border |
| `--crt-wash` | `rgba(125,255,160,.06)` | active row fill |
| failure | `#ffb08a` | the only non-green ink |

Type: Space Mono 400/700 throughout — 26 / 13 / 12.5 / 10.5 / 10px, nothing
else. Spacing: multiples of 2px, effectively 4 / 6 / 10 / 14 / 20 / 22 / 26 /
30. Radius: `0` everywhere. Shadows: none.

`--crt-ink-faint` is below text contrast by design and is reserved for the
unfilled half of a bar glyph. If it ends up on a word, that word is illegible.

## Responsive

Designed for a desktop window. Below ~900px, collapse the gutter to a single
horizontal strip above the work column (capacity as one row of four, allowance
as one row of three, `LAST` reduced to the single most recent entry). Below
~600px is out of scope — this is a workstation tool.

## Assets

None. No images, no icon set, no SVG. Every graphic element is a text
character (`▌` for bars, `●` for the state dot, `>` for the prompt) or a CSS
border/gradient. Keep it that way — the absence of iconography is a large part
of why this reads as a terminal.

## Files

- `green-crt-reference.html` — the design (option `1c` of four explorations)
- `screenshot-green-crt.png` — static capture. Note: the scanline and vignette
  overlays do not survive this capture method — open the HTML to see them.
- `theme.css` — tokens + CRT overlay, drop in as-is
- In the design project, all four explorations remain in
  `Firm Console Ideas.dc.html` for reference
