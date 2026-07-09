/*
File: crates/ms-i18n/src/interpolate.rs

Purpose:
Runtime substitution of NAMED `{placeholder}` markers in a catalog template.
Used by the `tf!` and `tp!` macros (which return `String`); the plain `t!`
lookup never interpolates and stays allocation-free.

Key functions:
- interpolate(template, args) -> String

Notes:
- Named placeholders only: `{err}`, `{n}`, ... . Positional/`{}` markers are not
  supported.
- Never panics. An unknown placeholder name is emitted verbatim (`{name}` stays
  in the output) and an unterminated `{` is copied through literally, so a
  malformed template degrades gracefully instead of failing.
- `{{` / `}}` are NOT treated as escapes; a literal brace in a catalog value is
  emitted as-is unless it forms a `{name}` that matches a provided argument.
*/

use core::fmt::Display;
use core::fmt::Write as _;

/// Replaces `{name}` markers in `template` with the matching argument's `Display`
/// value and returns the result.
///
/// `args` is a slice of `(name, value)` pairs; the first pair whose name equals
/// the placeholder wins. A placeholder with no matching argument is left in the
/// output unchanged (`{name}`), and an unterminated `{` is copied literally.
/// Never panics and never returns an error.
#[must_use]
pub fn interpolate(template: &str, args: &[(&str, &dyn Display)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;

    loop {
        match rest.find('{') {
            None => {
                // No further placeholders: copy the tail and stop.
                out.push_str(rest);
                break;
            }
            Some(open) => {
                out.push_str(&rest[..open]);
                let after = &rest[open + 1..];
                match after.find('}') {
                    None => {
                        // Unterminated '{': emit the rest verbatim, including the brace.
                        out.push('{');
                        out.push_str(after);
                        break;
                    }
                    Some(close) => {
                        let name = &after[..close];
                        // Writing a Display value into a String is infallible (String's
                        // fmt::Write never errors). An unmatched name, or the impossible
                        // write error, keeps the placeholder verbatim so no data is
                        // silently dropped and nothing panics.
                        let substituted = args
                            .iter()
                            .find(|(key, _)| *key == name)
                            .is_some_and(|(_, value)| write!(out, "{value}").is_ok());
                        if !substituted {
                            out.push('{');
                            out.push_str(name);
                            out.push('}');
                        }
                        rest = &after[close + 1..];
                    }
                }
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes_named_placeholder() {
        let out = interpolate("Save failed: {err}", &[("err", &"disk full")]);
        assert_eq!(out, "Save failed: disk full");
    }

    #[test]
    fn missing_argument_is_left_verbatim_and_does_not_panic() {
        let out = interpolate("Save failed: {err}", &[]);
        assert_eq!(out, "Save failed: {err}");
    }

    #[test]
    fn unterminated_brace_is_copied_literally() {
        let out = interpolate("broken {err", &[("err", &1)]);
        assert_eq!(out, "broken {err");
    }

    #[test]
    fn numeric_value_is_formatted() {
        let out = interpolate("{n} characters", &[("n", &42_i64)]);
        assert_eq!(out, "42 characters");
    }
}
