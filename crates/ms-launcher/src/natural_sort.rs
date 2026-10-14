/*
File: crates/ms-launcher/src/natural_sort.rs

Purpose:
The launcher's one natural ("human") name order: digit runs compare by number, so `p2` sorts
before `p10`. Shared by the PSD import table (`psd_import_window::sort_rows_for_table`) and the
"Open image" multi-pick (`open_image`), so both lists order files the same way.

Key functions:
- natural_name_cmp(): the comparison.

Notes:
Byte-based and case-sensitive by design (the PSD import order users already know). Pure, every
target.
*/

/// Natural ("human") order of two names: runs of ASCII digits compare by numeric value
/// (`p2 < p10`; leading zeroes only break ties, fewer first), every other byte compares by its
/// value, so the order is case-SENSITIVE (`B < a`). A name that is a prefix of the other sorts
/// first.
pub(crate) fn natural_name_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    let left_bytes = left.as_bytes();
    let right_bytes = right.as_bytes();
    let mut left_index = 0usize;
    let mut right_index = 0usize;

    while left_index < left_bytes.len() && right_index < right_bytes.len() {
        let left_byte = left_bytes[left_index];
        let right_byte = right_bytes[right_index];
        if left_byte.is_ascii_digit() && right_byte.is_ascii_digit() {
            let ordering =
                digit_run_cmp(left_bytes, &mut left_index, right_bytes, &mut right_index);
            if ordering != std::cmp::Ordering::Equal {
                return ordering;
            }
        } else {
            let ordering = left_byte.cmp(&right_byte);
            if ordering != std::cmp::Ordering::Equal {
                return ordering;
            }
            left_index += 1;
            right_index += 1;
        }
    }

    left_bytes.len().cmp(&right_bytes.len())
}

/// Compares the digit runs starting at `left_index` / `right_index` by numeric value, then by
/// run length (leading zeroes), advancing both indexes past their runs.
fn digit_run_cmp(
    left: &[u8],
    left_index: &mut usize,
    right: &[u8],
    right_index: &mut usize,
) -> std::cmp::Ordering {
    let left_start = *left_index;
    let right_start = *right_index;
    while *left_index < left.len() && left[*left_index].is_ascii_digit() {
        *left_index += 1;
    }
    while *right_index < right.len() && right[*right_index].is_ascii_digit() {
        *right_index += 1;
    }

    let left_significant = first_significant_digit(left, left_start, *left_index);
    let right_significant = first_significant_digit(right, right_start, *right_index);
    let left_significant_len = *left_index - left_significant;
    let right_significant_len = *right_index - right_significant;

    left_significant_len
        .cmp(&right_significant_len)
        .then_with(|| {
            left[left_significant..*left_index].cmp(&right[right_significant..*right_index])
        })
        .then_with(|| (*left_index - left_start).cmp(&(*right_index - right_start)))
}

/// Index of the first non-`0` byte in `bytes[start..end]` (`end` for an all-zero run).
fn first_significant_digit(bytes: &[u8], start: usize, end: usize) -> usize {
    let mut index = start;
    while index < end && bytes[index] == b'0' {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::natural_name_cmp;
    use std::cmp::Ordering;

    #[test]
    fn digit_runs_compare_by_number() {
        assert_eq!(natural_name_cmp("p2.png", "p10.png"), Ordering::Less);
        assert_eq!(natural_name_cmp("page 10", "page 9"), Ordering::Greater);
        assert_eq!(natural_name_cmp("ch1 p3", "ch1 p3"), Ordering::Equal);
    }

    #[test]
    fn leading_zeroes_only_break_ties() {
        assert_eq!(natural_name_cmp("p02", "p3"), Ordering::Less);
        assert_eq!(natural_name_cmp("p2", "p02"), Ordering::Less, "the shorter spelling of an equal number first");
    }

    #[test]
    fn letters_compare_case_sensitively_and_prefixes_first() {
        assert_eq!(natural_name_cmp("B.png", "a.png"), Ordering::Less, "byte order: uppercase before lowercase");
        assert_eq!(natural_name_cmp("page", "page2"), Ordering::Less);
    }
}
