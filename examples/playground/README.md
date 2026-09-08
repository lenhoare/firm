# First live assignment

This tiny standalone Rust crate is deliberately unfinished. Its failing tests are the task, not a failure in Firm itself.

Suggested objective:

> Implement `slug` in `src/lib.rs`. Lowercase ASCII letters, preserve ASCII digits, collapse runs of other characters into a single hyphen, and trim leading/trailing hyphens. Empty or punctuation-only input returns an empty string. Keep the existing tests unchanged, add no dependencies, and report any useful edge cases. Completion requires `cargo test --offline` to pass.

Firm's default live workspace points here so the first worker assignment is separate from the controller's source. Qwen works directly in this folder; automatic branching and integration are future work.

## `slug` contract (intended behavior)

Intended function signature (`src/lib.rs`):

```rust
pub fn slug(label: &str) -> String;
```

Rules:

1. ASCII letters `A`–`Z` are lowercased to `a`–`z`; `a`–`z` pass through.
2. ASCII digits `0`–`9` are preserved verbatim (no numeric parsing; `"007"` stays `"007"`).
3. Every other character — whitespace, punctuation, control characters, and **all** non-ASCII (including non-ASCII letters/digits) — is a separator. A run of one or more separators collapses into a single `-`.
4. Leading/trailing hyphens are trimmed.
5. Empty input or separator-only input returns `""`.

Classification is per `char` on the original input: no Unicode case folding, no normalization, no grapheme clustering.

## Usage examples (intended outputs)

```rust
use firm_playground::slug;

assert_eq!(slug("Hello World"), "hello-world");
assert_eq!(slug("  One__TWO ... 3! "), "one-two-3");
assert_eq!(slug(""), "");
assert_eq!(slug("--- !!!"), "");
assert_eq!(slug("café au lait"), "caf-au-lait"); // é (U+00E9) is one separator
assert_eq!(slug("A1__B2"), "a1-b2");
assert_eq!(slug("HTTP 200 OK"), "http-200-ok");
assert_eq!(slug("-1"), "1");
assert_eq!(slug("3.14"), "3-14");
assert_eq!(slug("already-a-slug"), "already-a-slug");
```

## Limits and edge cases

- ASCII-only output: a correct output matches `[a-z0-9-]*`, never contains `--`, and never starts/ends with `-`.
- Non-ASCII letters and digits are **not** kept: fullwidth `Ａ` (U+FF21), fullwidth `１` (U+FF11), Arabic-Indic digits, superscript `²` (U+00B2), Ohm sign `Ω` (U+2126) are all separators.
- Characters that Unicode-lowercase to ASCII still count as separators because classification happens before any case mapping: Kelvin sign `K` (U+212A), `İ` (U+0130), long `ſ` (U+017F), `ß` (U+00DF) never leak ASCII into the output.
- Combining marks are separators and no normalization is applied, so NFC vs NFD of the same visible text can differ: `"caf\u{e9} au lait"` (NFC) intends `"caf-au-lait"`, while `"cafe\u{301} au lait"` (NFD) intends `"cafe-au-lait"`.
- Control characters (`\0`, `\x1b`, `\x7f`), NBSP (U+00A0), ideographic space (U+3000), and zero-width space (U+200B) are separators; even an invisible ZWJ sequence (e.g. family emoji) between ASCII letters yields a visible `-`.
- Correct outputs are idempotent fixed points: `slug(slug(x)) == slug(x)`.

## Currently observed behavior (2026-09-06, independent review)

> The formatter does **not** currently work.

- `src/lib.rs` still contains the placeholder `label.to_owned()` (returns input unchanged).
- `cargo test --offline`: 0 passed, 4 failed (all original unit tests), exit 101.
- `cargo test --offline --test edge_cases`: 0 passed, 16 failed, exit 101.
- So none of the usage examples above produce their intended outputs yet; e.g. observed `slug("Hello World") == "Hello World"`, observed `slug("--- !!!") == "--- !!!"`. See `REVIEW.md` for full evidence.
