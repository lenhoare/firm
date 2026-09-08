//! An intentionally unfinished task for the first live delegation experiment.

/// Turn a label into a lowercase ASCII slug, collapsing non-alphanumeric
/// sequences into one hyphen and trimming leading/trailing hyphens.
pub fn slug(label: &str) -> String {
    // The worker's assignment is to replace this placeholder.
    label.to_owned()
}

#[cfg(test)]
mod tests {
    use super::slug;

    #[test]
    fn basic_label() { assert_eq!(slug("Hello World"), "hello-world"); }
    #[test]
    fn repeated_separators() { assert_eq!(slug("  One__TWO ... 3! "), "one-two-3"); }
    #[test]
    fn empty_and_punctuation() { assert_eq!(slug(""), ""); assert_eq!(slug("--- !!!"), ""); }
    #[test]
    fn non_ascii_is_a_separator() { assert_eq!(slug("café au lait"), "caf-au-lait"); }
}
