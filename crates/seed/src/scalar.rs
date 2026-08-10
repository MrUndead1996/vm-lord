//! Two readers, two sets of rules.
//!
//! The document is read as YAML, and `/etc/default/keyboard` inside it is later
//! read by shell scripts with `source`. A value safe for one is not thereby
//! safe for the other, so neither escaping stands in for the other.

/// Prints a value as a single-quoted YAML scalar.
///
/// Single quotes rather than double: inside them YAML has no escape sequences
/// whatsoever, so a value cannot mean anything but itself, and the only
/// character worth handling is the quote that ends the scalar.
pub(crate) fn yaml(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Escapes a value for a double-quoted shell assignment.
pub(crate) fn shell(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(character, '\\' | '"' | '`' | '$') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::{shell, yaml};

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

    #[test]
    fn a_plain_layout_passes_through_the_shell_untouched() {
        assert_eq!(shell("us"), "us");
    }

    /// `/etc/default/keyboard` is read with `source`, so a value is code until
    /// it is escaped: `$(...)` would run, and a quote would end the assignment.
    #[test]
    fn a_shell_value_cannot_run_a_command_or_end_its_assignment() {
        assert_eq!(shell("us$(id)"), "us\\$(id)");
        assert_eq!(shell("us\"; reboot #"), "us\\\"; reboot #");
        assert_eq!(shell("us`id`"), "us\\`id\\`");
        assert_eq!(shell("us\\"), "us\\\\");
    }
}
