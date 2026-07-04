/*
File: path.rs

Purpose:
Virtual-path normalization shared by every backend. Turns a caller-supplied
virtual path string into a list of clean, root-relative components, resolving
`.`/`..` logically and rejecting traversal above the root.

Key functions:
- normalize(): parse a virtual path into normalized components.

Notes:
Backends never touch the real filesystem to resolve `..`; resolution is purely
lexical so the same rules hold for the in-memory backend and the browser.
*/

use crate::StorageError;

/// Splits and normalizes a virtual path into root-relative components.
///
/// Accepts both '/' and '\\' separators. Empty segments and `.` are dropped;
/// `..` pops the last component. Returns the ordered component list (empty for
/// the root itself).
///
/// # Errors
/// [`StorageError::Escape`] if `..` would pop above the root.
/// [`StorageError::InvalidPath`] if a component contains a NUL byte.
pub fn normalize(path: &str) -> Result<Vec<String>, StorageError> {
    let mut out: Vec<String> = Vec::new();
    for raw in path.split(['/', '\\']) {
        match raw {
            // Empty (leading/trailing/double separator) and current-dir markers
            // carry no path information.
            "" | "." => {}
            ".." => {
                if out.pop().is_none() {
                    return Err(StorageError::Escape(path.to_string()));
                }
            }
            comp => {
                if comp.contains('\0') {
                    return Err(StorageError::InvalidPath(path.to_string()));
                }
                out.push(comp.to_string());
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_empty_and_dot() {
        assert_eq!(normalize("a//b/./c").unwrap(), vec!["a", "b", "c"]);
        assert_eq!(normalize("/a/b/").unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn resolves_parent() {
        assert_eq!(normalize("a/b/../c").unwrap(), vec!["a", "c"]);
        assert_eq!(normalize("a/../b").unwrap(), vec!["b"]);
    }

    #[test]
    fn accepts_backslash() {
        assert_eq!(normalize("a\\b\\c").unwrap(), vec!["a", "b", "c"]);
    }

    #[test]
    fn root_is_empty() {
        assert!(normalize("").unwrap().is_empty());
        assert!(normalize("/").unwrap().is_empty());
        assert!(normalize(".").unwrap().is_empty());
    }

    #[test]
    fn escape_is_rejected() {
        assert!(matches!(normalize("../x"), Err(StorageError::Escape(_))));
        assert!(matches!(normalize("a/../../x"), Err(StorageError::Escape(_))));
    }
}
