//! How a value is printed into the YAML document.
//!
//! Only YAML's own rules live here. A file carried inside the document has a
//! second reader with rules of its own -- `/etc/default/keyboard` is read with
//! `source` -- and that escaping belongs with the form the profile names it
//! in, in `vmlord_core::KeyboardForm`.

/// Prints a value as a single-quoted YAML scalar.
///
/// Single quotes rather than double: inside them YAML has no escape sequences
/// whatsoever, so a value cannot mean anything but itself, and the only
/// character worth handling is the quote that ends the scalar.
pub(crate) fn yaml(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::yaml;

    #[test]
    fn a_plain_value_is_quoted() {
        assert_eq!(yaml("en_US.UTF-8"), "'en_US.UTF-8'");
    }

    /// A single-quoted YAML scalar has no escape sequences at all, so the one
    /// character that can end it is the only one that needs handling.
    #[test]
    fn an_apostrophe_is_doubled_rather_than_escaped() {
        assert_eq!(yaml("don't"), "'don''t'");
    }

    /// Every character that would otherwise close the scalar or introduce
    /// structure stays inside it.
    #[test]
    fn nothing_breaks_out_of_a_quoted_scalar() {
        for value in [
            "a: b",
            "- item",
            "#comment",
            "\"quoted\"",
            "{}",
            "[]",
            "*anchor",
        ] {
            let quoted = yaml(value);
            assert!(quoted.starts_with('\''), "{quoted:?}");
            assert!(quoted.ends_with('\''), "{quoted:?}");
            assert_eq!(quoted.matches('\'').count(), 2, "{quoted:?}");
        }
    }
}
