/*
File: crates/ms-launcher/src/natural_sort.rs

Purpose:
The launcher's one natural ("human") name order: digit runs compare by number, so `p2` sorts
before `p10`. Shared by the PSD import table (`psd_import_window::sort_rows_for_table`) and the
"Open image" multi-pick (`open_image`), so both lists order files the same way.

Key functions:
- natural_name_cmp(): the comparison.

Notes:
Case-insensitive (Unicode lowercase, so Latin and Cyrillic alike); names that differ only in
case fall back to the case-sensitive order, so the result is total and deterministic. Pure, every
target.
*/

/// Natural ("human") order of two names: runs of ASCII digits compare by numeric value
/// (`p2 < p10`; leading zeroes only break ties, fewer first), every other character compares
/// case-INSENSITIVELY by its Unicode lowercase (`a < B`, `p2 < P3`). Names equal under that rule
/// are ordered case-sensitively by code point (`B < b`). A name that is a prefix of the other
/// sorts first.
pub(crate) fn natural_name_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    natural_cmp_with(left, right, true).then_with(|| natural_cmp_with(left, right, false))
}

/// One pass of [`natural_name_cmp`]: digit runs by number, other characters by lowercase when
/// `fold_case`, else by code point.
fn natural_cmp_with(left: &str, right: &str, fold_case: bool) -> std::cmp::Ordering {
    let left_chars: Vec<char> = left.chars().collect();
    let right_chars: Vec<char> = right.chars().collect();
    let mut left_index = 0usize;
    let mut right_index = 0usize;

    while left_index < left_chars.len() && right_index < right_chars.len() {
        let left_char = left_chars[left_index];
        let right_char = right_chars[right_index];
        if left_char.is_ascii_digit() && right_char.is_ascii_digit() {
            let ordering =
                digit_run_cmp(&left_chars, &mut left_index, &right_chars, &mut right_index);
            if ordering != std::cmp::Ordering::Equal {
                return ordering;
            }
        } else {
            let ordering = if fold_case {
                left_char.to_lowercase().cmp(right_char.to_lowercase())
            } else {
                left_char.cmp(&right_char)
            };
            if ordering != std::cmp::Ordering::Equal {
                return ordering;
            }
            left_index += 1;
            right_index += 1;
        }
    }

    (left_chars.len() - left_index).cmp(&(right_chars.len() - right_index))
}

/// Compares the digit runs starting at `left_index` / `right_index` by numeric value, then by
/// run length (leading zeroes), advancing both indexes past their runs.
fn digit_run_cmp(
    left: &[char],
    left_index: &mut usize,
    right: &[char],
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

/// Index of the first non-`0` character in `chars[start..end]` (`end` for an all-zero run).
fn first_significant_digit(chars: &[char], start: usize, end: usize) -> usize {
    let mut index = start;
    while index < end && chars[index] == '0' {
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
    fn letters_compare_case_insensitively_and_prefixes_first() {
        assert_eq!(natural_name_cmp("a.png", "B.png"), Ordering::Less);
        assert_eq!(natural_name_cmp("p2.png", "P3.png"), Ordering::Less);
        assert_eq!(natural_name_cmp("P3.png", "p10.png"), Ordering::Less);
        assert_eq!(natural_name_cmp("Глава2", "глава10"), Ordering::Less, "Cyrillic folds too");
        assert_eq!(natural_name_cmp("page", "page2"), Ordering::Less);
    }

    #[test]
    fn case_only_differences_keep_a_total_order() {
        assert_eq!(natural_name_cmp("B.png", "b.png"), Ordering::Less, "code point order breaks the tie");
        assert_eq!(natural_name_cmp("b.png", "B.png"), Ordering::Greater);
        assert_eq!(natural_name_cmp("b.png", "b.png"), Ordering::Equal);
    }
}
