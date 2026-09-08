# Independent review: `slug` (Muse, 2026-09-06)

Scope: independently inspect `src/lib.rs`, the original unit tests, and
`tests/edge_cases.rs` if present; update only `README.md` and this file;
no implementation or test repairs. Observations below are from this
session's own file reads and test runs. Claims about prior workers are
repeated from the assignment brief, not independently verified.

## Files inspected

- `src/lib.rs` (8-line implementation + 4 unit tests in `mod tests`)
- `tests/edge_cases.rs` — **present** (16 tests, ~328 lines, contract-focused)
- `Cargo.toml` — package `firm-playground`, edition 2024, no dependencies
- `README.md` — pre-existing assignment text (preserved and extended this session)

No other source or test files exist in the workspace (workspace root is not
a git repository, so no git history was available to audit prior workers).

## 1. Implementation status

`src/lib.rs` is still the placeholder:

```rust
pub fn slug(label: &str) -> String {
    // The worker's assignment is to replace this placeholder.
    label.to_owned()
}
```

Concrete discrepancies vs the contract (lowercase ASCII, preserve ASCII
digits, collapse all other runs incl. non-ASCII to one `-`, trim boundary
hyphens, empty for empty/separator-only):

- No lowercasing (`"Hello World"` returned unchanged).
- No separator collapsing or trimming (`"  One__TWO ... 3! "` returned unchanged).
- Non-ASCII passed through instead of acting as a separator (`"café au lait"` returned unchanged).
- Separator-only input not mapped to empty (`"--- !!!"` returned unchanged).

In short: the function is the identity function; every non-trivial contract
rule is unimplemented. This matches the controller report quoted in the
brief ("returning their unchanged inputs").

## 2. Original tests (`src/lib.rs`, `mod tests`)

All four original tests are present and unchanged:

| Test | Assertion |
|---|---|
| `basic_label` | `slug("Hello World") == "hello-world"` |
| `repeated_separators` | `slug("  One__TWO ... 3! ") == "one-two-3"` |
| `empty_and_punctuation` | `slug("") == ""` and `slug("--- !!!") == ""` |
| `non_ascii_is_a_separator` | `slug("café au lait") == "caf-au-lait"` |

Coverage of the original set alone: basic lowercasing/separation, run
collapsing, trim, empty/punctuation-only, one non-ASCII case. It does not
pin down digit preservation at boundaries, control characters, non-ASCII
digits/letters, case-fold lookalikes, combining marks/NFC-vs-NFD, separator
runs of mixed scripts, invariants, or idempotence — all of which are covered
by `tests/edge_cases.rs` (see §3).

## 3. Edge-case tests (`tests/edge_cases.rs`)

**Not absent.** The file exists and contains 16 tests. None were created or
modified this session. Inventory:

1. `contract_examples_from_the_assignment` — `A1__B2`, NFC café, `a界🙂b`, separator-only non-ASCII
2. `ascii_digits_are_preserved_verbatim` — `0`, `007`, full digit run, mixed alphanumerics
3. `digits_survive_at_boundaries_and_between_separators` — `-1`, `1-`, ` 42 `, `3.14`, `1,000,000`, `HTTP 200 OK`, non-ASCII between digits
4. `ascii_letters_are_lowercased`
5. `separator_runs_collapse_to_one_hyphen` — incl. `a-界-b` and a 64-char run
6. `whitespace_collapses_and_is_trimmed` — `\t\n\r`, vertical tab, form feed, NBSP, ideographic space, ZWSP
7. `boundary_separators_are_trimmed` — incl. non-ASCII/ZWSP boundaries
8. `unicode_between_ascii_characters_is_exactly_one_hyphen` — CJK, emoji, é, Greek/Cyrillic runs, ZWJ family emoji
9. `non_ascii_only_input_is_empty` — incl. fullwidth letters/digits, ZWJ sequence
10. `empty_and_separator_only_input_is_empty` — incl. NUL, DEL, NBSP, ZWSP
11. `control_characters_are_separators`
12. `non_ascii_digits_and_letters_are_separators_not_payload` — guards against `is_alphanumeric`/`is_numeric` shortcuts (fullwidth digits, Arabic-Indic digits, `²`, `½`, Ohm sign, fullwidth `Ａ`)
13. `non_ascii_case_foldable_lookalikes_are_separators` — guards against lowercase-before-classify (Kelvin `K`, `İ`, long `ſ`, micro sign, `ß`)
14. `combining_marks_are_separators_and_are_not_normalized_away` — pins NFC (`caf-au-lait`) vs NFD (`cafe-au-lait`) difference
15. `output_obeys_slug_shape_invariants` — output charset `[a-z0-9-]`, no `--`, no boundary `-`, over 15 probes
16. `idempotent_over_representative_inputs` — first-pass value plus fixed-point check over 18 cases

Coverage gaps / notes:

- No missing-file gap: the brief allowed that `tests/edge_cases.rs` might be absent; it is present and thorough.
- The suite deliberately documents contract subtleties (per-`char` classification, no folding/normalization) rather than assuming NFC/NFD equivalence — a strength, not a gap.
- Residual non-functional gaps (minor): no large-input/performance test beyond one 64-char run; no doc-tests; no fuzz/property test beyond the fixed probe lists. Functional contract coverage is otherwise comprehensive relative to the stated rules.
- Qwen's "test changes and coverage" (per brief) could not be verified: there is no git history in this workspace, and the provenance of `tests/edge_cases.rs` is unknown from file inspection alone. Its content is consistent with a careful contract reading, but authorship is not established here.

## 4. Prior worker failures (as reported in the brief; not independently observed)

- Grok "failed implementation acceptance" — reported, not observable here (no history/artifacts beyond the current placeholder).
- Qwen "exhausted its session turns without a substantive report, so its test changes and coverage are unverified" — `tests/edge_cases.rs` exists and is substantive, but whether it is Qwen's work is unverified from this workspace alone.
- Controller `cargo test --offline` exited 101 with all four original tests failing and returning unchanged inputs — **independently reproduced** (see §5).

## 5. Independently obtained test evidence (this session)

Commands run (offline, no installs, no networking):

- `cargo test --offline`
- `cargo test --offline --test edge_cases`

Results:

### `cargo test --offline` — exit 101

```text
running 4 tests
test tests::repeated_separators ... FAILED
test tests::non_ascii_is_a_separator ... FAILED
test tests::basic_label ... FAILED
test tests::empty_and_punctuation ... FAILED

failures:
    tests::basic_label               left: "Hello World"          right: "hello-world"
    tests::repeated_separators       left: "  One__TWO ... 3! "   right: "one-two-3"
    tests::empty_and_punctuation     left: "--- !!!"              right: ""
    tests::non_ascii_is_a_separator  left: "café au lait"         right: "caf-au-lait"

test result: FAILED. 0 passed; 4 failed; 0 ignored; 0 measured; 0 filtered out
error: test failed, to rerun pass `--lib`
```

(The suite stops after the lib target fails, so integration tests do not run under plain `cargo test` while the unit tests fail.)

### `cargo test --offline --test edge_cases` — exit 101

```text
running 16 tests
test ascii_digits_are_preserved_verbatim ... FAILED
test ascii_letters_are_lowercased ... FAILED
test boundary_separators_are_trimmed ... FAILED
test control_characters_are_separators ... FAILED
test contract_examples_from_the_assignment ... FAILED
test combining_marks_are_separators_and_are_not_normalized_away ... FAILED
test digits_survive_at_boundaries_and_between_separators ... FAILED
test idempotent_over_representative_inputs ... FAILED
test non_ascii_case_foldable_lookalikes_are_separators ... FAILED
test non_ascii_only_input_is_empty ... FAILED
test empty_and_separator_only_input_is_empty ... FAILED
test non_ascii_digits_and_letters_are_separators_not_payload ... FAILED
test unicode_between_ascii_characters_is_exactly_one_hyphen ... FAILED
test output_obeys_slug_shape_invariants ... FAILED
test whitespace_collapses_and_is_trimmed ... FAILED
test separator_runs_collapse_to_one_hyphen ... FAILED

test result: FAILED. 0 passed; 16 failed; 0 ignored; 0 measured; 0 filtered out
error: test failed, to rerun pass `--test edge_cases`
```

Representative failure lines (each shows identity-return, i.e. left == input):

- `slug("A1__B2")` left `"A1__B2"`, right `"a1-b2"`
- `slug("ABC")` left `"ABC"`, right `"abc"`
- `slug("---a---")` left `"---a---"`, right `"a"`
- `slug(" ")` left `" "`, right `""`
- `slug("界")` left `"界"`, right `""`
- `slug("a界b")` left `"a界b"`, right `"a-b"`
- `slug("a\0b")` left `"a\0b"`, right `"a-b"`
- `slug("a１b")` (fullwidth digit) left unchanged, right `"a-b"`
- `slug("aKb")` (Kelvin) left unchanged, right `"a-b"`
- `slug("a\u{301}b")` left unchanged, right `"a-b"`
- invariant probe: `slug("Hello World") = "Hello World"` contains characters outside `[a-z0-9-]`

Counts: 0/4 unit + 0/16 integration pass. Both failures are fully explained by the identity-function placeholder; no test logic defect was observed (all failure diffs show correct expected values vs unprocessed input).

## 6. Changed files and remaining blockers

Changed files (this session; docs only, per acceptance criteria):

- `README.md` — added contract, usage examples, limits, and observed-behavior section; original assignment text preserved.
- `REVIEW.md` — created (this file).

Unchanged, as required: `src/lib.rs`, the four original unit tests therein, `tests/edge_cases.rs`, `Cargo.toml`/`Cargo.lock`.

Remaining blockers / next steps (not done here, by instruction):

- Implementation still missing: replace `label.to_owned()` with the contract logic. Suggested pitfalls the edge-case suite guards against: do not use `char::is_alphanumeric`/`is_numeric` (accept non-ASCII), do not `to_lowercase` before classifying (Kelvin/`İ`/`ſ`/`ß` leaks), handle per-`char` without normalization.
- No verification of prior-worker claims was possible beyond reproducing the test failures (no git history; workspace is not a git repository).
- Completion criterion unmet: `cargo test --offline` does not pass (exit 101, 4/4 unit failures; 16/16 edge-case failures when run directly).
