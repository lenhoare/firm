//! Contract-focused edge-case tests for `slug`.
//!
//! Contract under test (from the assignment; matches the `src/lib.rs` doc comment):
//!
//! * ASCII letters `a`-`z` / `A`-`Z` are kept and lowercased.
//! * ASCII digits `0`-`9` are kept verbatim.
//! * Every other character -- all non-ASCII, whitespace, control characters and
//!   punctuation alike -- is a separator, and a run of one or more separators
//!   collapses into a single `-`.
//! * Boundary hyphens are trimmed.
//! * Empty input and separator-only input yield `""`.
//!
//! Classification happens per `char` on the *original* input: the contract implies
//! no Unicode case folding, no normalization and no grapheme clustering. Several
//! tests below pin that down deliberately, because the obvious shortcuts
//! (`str::to_lowercase()` first, `char::is_alphanumeric()`, `char::is_numeric()`)
//! each break it in a different way.
//!
//! Non-ASCII characters are written as `\u{..}` escapes so the expected output is
//! never ambiguous about which code point (or normalization form) is in play.

use firm_playground::slug;

fn assert_slug(input: &str, expected: &str) {
    assert_eq!(slug(input), expected, "slug({input:?})");
}

/// Table-driven helper: every case asserts an explicit expected output.
fn assert_all(cases: &[(&str, &str)]) {
    for &(input, expected) in cases {
        assert_slug(input, expected);
    }
}

#[test]
fn contract_examples_from_the_assignment() {
    assert_all(&[
        ("A1__B2", "a1-b2"),
        // "café au lait" in NFC (precomposed é = U+00E9): é is one separator.
        ("caf\u{e9} au lait", "caf-au-lait"),
        // "a界🙂b": the whole non-ASCII run collapses to a single hyphen.
        ("a\u{754c}\u{1f642}b", "a-b"),
        // "界🙂é": separator-only, so nothing survives trimming.
        ("\u{754c}\u{1f642}\u{e9}", ""),
    ]);
}

#[test]
fn ascii_digits_are_preserved_verbatim() {
    assert_all(&[
        ("0", "0"),
        ("007", "007"), // no numeric parsing, so leading zeros survive
        ("1234567890", "1234567890"),
        ("a0b", "a0b"),
        ("1a", "1a"),
        ("a1", "a1"),
        ("abc123", "abc123"),
        ("A1B2C3", "a1b2c3"),
    ]);
}

#[test]
fn digits_survive_at_boundaries_and_between_separators() {
    assert_all(&[
        ("a1", "a1"),
        ("1a", "1a"),
        ("-1", "1"),
        ("1-", "1"),
        (" 42 ", "42"),
        ("__42__", "42"),
        ("3.14", "3-14"),
        ("0.5", "0-5"),
        ("1,000,000", "1-000-000"),
        ("HTTP 200 OK", "http-200-ok"),
        ("Rust 2024 Edition", "rust-2024-edition"),
        // A non-ASCII character between two digits is still just a separator.
        ("3\u{754c}4", "3-4"),
    ]);
}

#[test]
fn ascii_letters_are_lowercased() {
    assert_all(&[
        ("ABC", "abc"),
        ("aBcDeF", "abcdef"),
        ("HELLO WORLD", "hello-world"),
        ("Hello, World!", "hello-world"),
        ("MiXeD CaSe", "mixed-case"),
        ("already-a-slug", "already-a-slug"),
        ("a1-b2", "a1-b2"),
    ]);
}

#[test]
fn separator_runs_collapse_to_one_hyphen() {
    assert_all(&[
        ("a--b", "a-b"),
        ("a_-_b", "a-b"),
        ("a - b", "a-b"),
        ("a!@#$%^&*()b", "a-b"),
        ("a__b", "a-b"),
        // A hyphen adjacent to another separator does not become two hyphens.
        ("a-\u{754c}-b", "a-b"),
        ("a\u{754c}-b", "a-b"),
    ]);

    // A long run collapses just as a short one does.
    let long_run = format!("a{}b", "_".repeat(64));
    assert_slug(&long_run, "a-b");
}

#[test]
fn whitespace_collapses_and_is_trimmed() {
    assert_all(&[
        ("\t\n\r hello \t\n\r", "hello"),
        ("a b", "a-b"),
        ("a\tb", "a-b"),
        ("a\nb", "a-b"),
        ("a\rb", "a-b"),
        ("a\u{b}b", "a-b"), // vertical tab
        ("a\u{c}b", "a-b"), // form feed
        ("a\tb\nc\rd\u{b}e\u{c}f", "a-b-c-d-e-f"),
        ("a  \t\n  b", "a-b"),
        // Non-ASCII spaces are separators too, not "whitespace to strip silently".
        ("a\u{a0}b", "a-b"),   // no-break space
        ("a\u{3000}b", "a-b"), // ideographic space
        ("a\u{200b}b", "a-b"), // zero-width space still yields a visible hyphen
        ("\u{a0}\u{3000}", ""),
    ]);
}

#[test]
fn boundary_separators_are_trimmed() {
    assert_all(&[
        ("---a---", "a"),
        ("_a_", "a"),
        ("!a!", "a"),
        (" a ", "a"),
        ("-_-a-_-", "a"),
        ("\u{754c}a\u{754c}", "a"),
        ("\u{1f642}a\u{1f642}", "a"),
        ("\u{200b}a\u{200b}", "a"),
    ]);
}

#[test]
fn unicode_between_ascii_characters_is_exactly_one_hyphen() {
    assert_all(&[
        ("a\u{754c}b", "a-b"),      // CJK ideograph
        ("a\u{1f642}b", "a-b"),     // emoji
        ("a\u{e9}b", "a-b"),        // é, precomposed
        ("a\u{3a3}\u{3b1}b", "a-b"), // Greek run -> one hyphen, not two
        ("a\u{432}\u{44b}b", "a-b"), // Cyrillic run -> one hyphen
        // Family emoji is three code points joined by ZWJ; still one hyphen.
        ("a\u{1f468}\u{200d}\u{1f469}b", "a-b"),
    ]);
}

#[test]
fn non_ascii_only_input_is_empty() {
    assert_all(&[
        ("\u{754c}", ""),
        ("\u{1f642}", ""),
        ("\u{e9}\u{e8}\u{ea}", ""),
        ("\u{754c}\u{1f642}", ""),
        ("\u{3a3}\u{3b1}\u{3b2}", ""),
        ("\u{ff21}\u{ff22}\u{ff23}", ""), // fullwidth "ＡＢＣ"
        ("\u{ff11}\u{ff12}\u{ff13}", ""), // fullwidth "１２３"
        ("\u{1f468}\u{200d}\u{1f469}", ""),
    ]);
}

#[test]
fn empty_and_separator_only_input_is_empty() {
    assert_all(&[
        ("", ""),
        (" ", ""),
        ("   ", ""),
        ("\t\n\r", ""),
        ("-", ""),
        ("---", ""),
        ("_", ""),
        ("__", ""),
        ("!", ""),
        ("!!! ...", ""),
        ("-_-_-", ""),
        ("\u{0}", ""),
        ("\u{7f}", ""),
        ("\u{a0}", ""),
        ("\u{200b}", ""),
    ]);
}

#[test]
fn control_characters_are_separators() {
    assert_all(&[
        ("a\u{0}b", "a-b"),  // NUL
        ("a\u{1}b", "a-b"),  // SOH
        ("a\u{1b}b", "a-b"), // ESC
        ("a\u{7f}b", "a-b"), // DEL
        ("\u{0}\u{1}a\u{1b}\u{7f}", "a"),
    ]);
}

#[test]
fn non_ascii_digits_and_letters_are_separators_not_payload() {
    // Guards against `char::is_alphanumeric()` / `is_numeric()` / `is_whitespace()`,
    // which all accept code points outside ASCII.
    assert_all(&[
        ("a\u{ff11}b", "a-b"), // fullwidth digit one
        ("a\u{663}b", "a-b"),  // Arabic-Indic digit three
        ("a\u{6f3}b", "a-b"),  // extended Arabic-Indic digit three
        ("a\u{b2}b", "a-b"),   // superscript two (is_numeric() == true)
        ("a\u{bc}b", "a-b"),   // vulgar fraction one half
        ("a\u{967}b", "a-b"),  // Devanagari digit seven
        ("a\u{2126}b", "a-b"), // ohm sign (a letter, not ASCII)
        ("a\u{ff21}b", "a-b"), // fullwidth letter A
    ]);
}

#[test]
fn non_ascii_case_foldable_lookalikes_are_separators() {
    // Guards against lowercasing (or otherwise case-folding) the input *before*
    // classifying it: these code points fold into ASCII and would leak through.
    assert_all(&[
        ("a\u{212a}b", "a-b"), // Kelvin sign, to_lowercase() -> ASCII 'k'
        ("a\u{130}b", "a-b"),  // Latin capital I with dot above, folds to "i" + U+0307
        ("a\u{17f}b", "a-b"),  // long s, folds to ASCII 's'
        ("a\u{b5}b", "a-b"),   // micro sign
        ("stra\u{df}e", "stra-e"), // sharp s stays a separator
        ("\u{1e9e}", ""),          // capital sharp s
    ]);
}

#[test]
fn combining_marks_are_separators_and_are_not_normalized_away() {
    // Per-character classification means the same visible text can slug differently
    // depending on normalization form. That is what the contract says; these cases
    // document it rather than assume NFC/NFD equivalence.
    assert_all(&[
        ("a\u{301}b", "a-b"), // combining acute between ASCII letters
        // "café au lait" in NFD: the 'e' is ASCII and survives, the mark does not.
        ("caf\u{65}\u{301} au lait", "cafe-au-lait"),
        // The same text in NFC drops the 'e' along with the mark.
        ("caf\u{e9} au lait", "caf-au-lait"),
    ]);
}

const INVARIANT_PROBES: &[&str] = &[
    "",
    "Hello World",
    "  One__TWO ... 3! ",
    "--- !!!",
    "A1__B2",
    "caf\u{e9} au lait",
    "a\u{754c}\u{1f642}b",
    "\u{754c}\u{1f642}\u{e9}",
    "HTTP 200 OK",
    "a\u{212a}b",
    "stra\u{df}e",
    "\u{1f468}\u{200d}\u{1f469}",
    "-1",
    "a\tb\nc\rd",
    "\u{ff11}\u{ff12}\u{ff13}",
];

#[test]
fn output_obeys_slug_shape_invariants() {
    for &input in INVARIANT_PROBES {
        let out = slug(input);
        assert!(
            out.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "slug({input:?}) = {out:?} contains a character outside [a-z0-9-]"
        );
        assert!(
            !out.contains("--"),
            "slug({input:?}) = {out:?} contains a repeated hyphen"
        );
        assert!(
            !out.starts_with('-'),
            "slug({input:?}) = {out:?} starts with a hyphen"
        );
        assert!(
            !out.ends_with('-'),
            "slug({input:?}) = {out:?} ends with a hyphen"
        );
    }
}

/// (input, expected first pass). The expected first pass is also the fixed point,
/// so each row checks the transformation *and* idempotence with explicit values.
const IDEMPOTENCE_CASES: &[(&str, &str)] = &[
    ("", ""),
    ("Hello World", "hello-world"),
    ("  One__TWO ... 3! ", "one-two-3"),
    ("--- !!!", ""),
    ("A1__B2", "a1-b2"),
    ("caf\u{e9} au lait", "caf-au-lait"),
    ("a\u{754c}\u{1f642}b", "a-b"),
    ("\u{754c}\u{1f642}\u{e9}", ""),
    ("HTTP 200 OK", "http-200-ok"),
    ("already-a-slug", "already-a-slug"),
    ("-1", "1"),
    ("007", "007"),
    ("a\u{212a}b", "a-b"),
    ("stra\u{df}e", "stra-e"),
    ("\t\n\r hello \t\n\r", "hello"),
    ("a\u{1f468}\u{200d}\u{1f469}b", "a-b"),
    ("\u{ff11}\u{ff12}\u{ff13}", ""),
    ("a\u{301}b", "a-b"),
];

#[test]
fn idempotent_over_representative_inputs() {
    for &(input, first_pass) in IDEMPOTENCE_CASES {
        let once = slug(input);
        assert_eq!(once, first_pass, "slug({input:?})");

        let twice = slug(&once);
        assert_eq!(twice, once, "slug(slug({input:?})) changed the slug");

        assert_eq!(
            slug(first_pass),
            first_pass,
            "slug({first_pass:?}) should be a fixed point"
        );
    }
}
