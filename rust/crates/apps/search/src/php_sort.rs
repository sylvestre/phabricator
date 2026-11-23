//! PHP 8's `ksort()` with `SORT_REGULAR`, for arrays keyed by strings.
//!
//! PHP compares two keys numerically when both are numeric strings (or
//! integers, which keys like `"123"` become) and byte by byte otherwise,
//! falling back to the insertion order for ties. That order is not
//! transitive (`" 9 " < " 10"` numerically, `" 10" < " 5a" < " 9 "` as
//! bytes), so the result depends on the algorithm: this is `zend_sort()`,
//! the hybrid insertion sort and quicksort of `Zend/zend_sort.c`, step by
//! step.

use std::cmp::Ordering;

/// White space `_is_numeric_string_ex()` skips before and after a number.
fn is_numeric_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | b'\x0b' | b'\x0c')
}

/// The digits from `index`, and where they stop.
fn digits(bytes: &[u8], mut index: usize) -> (usize, usize) {
    let start = index;
    while bytes.get(index).is_some_and(u8::is_ascii_digit) {
        index += 1;
    }
    (index - start, index)
}

/// `is_numeric_string()` without errors allowed: the value of a string
/// which is a whole number, with optional white space around it.
fn numeric_value(text: &str) -> Option<f64> {
    let bytes = text.as_bytes();
    let mut index = 0;
    while bytes.get(index).copied().is_some_and(is_numeric_space) {
        index += 1;
    }
    let start = index;
    if matches!(bytes.get(index), Some(b'-' | b'+')) {
        index += 1;
    }
    let (integer, after) = digits(bytes, index);
    index = after;
    if integer > 0 {
        if bytes.get(index) == Some(&b'.') {
            index = digits(bytes, index + 1).1;
        }
    } else if bytes.get(index) == Some(&b'.')
        && bytes.get(index + 1).is_some_and(u8::is_ascii_digit)
    {
        index = digits(bytes, index + 1).1;
    } else {
        return None;
    }
    if matches!(bytes.get(index), Some(b'e' | b'E')) {
        let mut exponent = index + 1;
        if matches!(bytes.get(exponent), Some(b'-' | b'+')) {
            exponent += 1;
        }
        let (count, after) = digits(bytes, exponent);
        if count > 0 {
            index = after;
        }
    }
    let end = index;
    while bytes.get(index).copied().is_some_and(is_numeric_space) {
        index += 1;
    }
    if index != bytes.len() {
        return None;
    }
    text[start..end].parse().ok()
}

/// A key with what the comparison needs, and its insertion index
/// (`Z_EXTRA`).
struct Key {
    text: String,
    number: Option<f64>,
    order: usize,
}

/// `php_array_key_compare()`: `php_array_key_compare_unstable_i()`, then
/// `stable_sort_fallback()`.
fn compare(first: &Key, second: &Key) -> i32 {
    let unstable = match (first.number, second.number) {
        // `zendi_smart_strcmp()` or `compare_longs_to_string()` of two
        // numbers; two integer keys compare the same way.
        (Some(one), Some(two)) => match one.partial_cmp(&two) {
            Some(Ordering::Less) => -1,
            Some(Ordering::Greater) => 1,
            _ => 0,
        },
        // `zend_binary_strcmp()`, with an integer key as its digits.
        _ => match first.text.as_bytes().cmp(second.text.as_bytes()) {
            Ordering::Less => -1,
            Ordering::Equal => 0,
            Ordering::Greater => 1,
        },
    };
    if unstable != 0 {
        return unstable;
    }
    match first.order.cmp(&second.order) {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }
}

/// The array `zend_sort()` works on, with its `cmp` and `swp`.
struct Sorter {
    keys: Vec<Key>,
}

impl Sorter {
    fn cmp(&self, a: usize, b: usize) -> i32 {
        compare(&self.keys[a], &self.keys[b])
    }

    fn swap(&mut self, a: usize, b: usize) {
        self.keys.swap(a, b);
    }

    fn sort_2(&mut self, a: usize, b: usize) {
        if self.cmp(a, b) > 0 {
            self.swap(a, b);
        }
    }

    fn sort_3(&mut self, a: usize, b: usize, c: usize) {
        if self.cmp(a, b) <= 0 {
            if self.cmp(b, c) <= 0 {
                return;
            }
            self.swap(b, c);
            if self.cmp(a, b) > 0 {
                self.swap(a, b);
            }
            return;
        }
        if self.cmp(c, b) <= 0 {
            self.swap(a, c);
            return;
        }
        self.swap(a, b);
        if self.cmp(b, c) > 0 {
            self.swap(b, c);
        }
    }

    fn sort_4(&mut self, a: usize, b: usize, c: usize, d: usize) {
        self.sort_3(a, b, c);
        if self.cmp(c, d) > 0 {
            self.swap(c, d);
            if self.cmp(b, c) > 0 {
                self.swap(b, c);
                if self.cmp(a, b) > 0 {
                    self.swap(a, b);
                }
            }
        }
    }

    #[allow(clippy::many_single_char_names)]
    fn sort_5(&mut self, a: usize, b: usize, c: usize, d: usize, e: usize) {
        self.sort_4(a, b, c, d);
        if self.cmp(d, e) > 0 {
            self.swap(d, e);
            if self.cmp(c, d) > 0 {
                self.swap(c, d);
                if self.cmp(b, c) > 0 {
                    self.swap(b, c);
                    if self.cmp(a, b) > 0 {
                        self.swap(a, b);
                    }
                }
            }
        }
    }

    /// Moves the element at `from` down to `to`, one swap at a time.
    fn rotate_down(&mut self, from: usize, to: usize) {
        let mut k = from;
        while k > to {
            self.swap(k, k - 1);
            k -= 1;
        }
    }

    /// `zend_insert_sort()`.
    fn insert_sort(&mut self, start: usize, count: usize) {
        match count {
            0 | 1 => {}
            2 => self.sort_2(start, start + 1),
            3 => self.sort_3(start, start + 1, start + 2),
            4 => self.sort_4(start, start + 1, start + 2, start + 3),
            5 => self.sort_5(start, start + 1, start + 2, start + 3, start + 4),
            _ => {
                let end = start + count;
                let sentry = start + 6;
                for i in start + 1..sentry {
                    let mut j = i - 1;
                    if self.cmp(j, i) <= 0 {
                        continue;
                    }
                    while j != start {
                        j -= 1;
                        if self.cmp(j, i) <= 0 {
                            j += 1;
                            break;
                        }
                    }
                    self.rotate_down(i, j);
                }
                for i in sentry..end {
                    let mut j = i - 1;
                    if self.cmp(j, i) <= 0 {
                        continue;
                    }
                    loop {
                        j -= 2;
                        if self.cmp(j, i) <= 0 {
                            j += 1;
                            if self.cmp(j, i) <= 0 {
                                j += 1;
                            }
                            break;
                        }
                        if j == start {
                            break;
                        }
                        if j == start + 1 {
                            j -= 1;
                            if self.cmp(i, j) > 0 {
                                j += 1;
                            }
                            break;
                        }
                    }
                    self.rotate_down(i, j);
                }
            }
        }
    }

    /// `zend_sort()`.
    fn sort(&mut self, mut start: usize, mut count: usize) {
        loop {
            if count <= 16 {
                self.insert_sort(start, count);
                return;
            }
            let end = start + count;
            let offset = count >> 1;
            let pivot = start + offset;
            if count >> 10 != 0 {
                let delta = offset >> 1;
                self.sort_5(start, start + delta, pivot, pivot + delta, end - 1);
            } else {
                self.sort_3(start, pivot, end - 1);
            }
            self.swap(start + 1, pivot);
            let pivot = start + 1;
            let mut i = pivot + 1;
            let mut j = end - 1;
            'partition: loop {
                while self.cmp(pivot, i) > 0 {
                    i += 1;
                    if i == j {
                        break 'partition;
                    }
                }
                j -= 1;
                if j == i {
                    break;
                }
                while self.cmp(j, pivot) > 0 {
                    j -= 1;
                    if j == i {
                        break 'partition;
                    }
                }
                self.swap(i, j);
                i += 1;
                if i == j {
                    break;
                }
            }
            self.swap(pivot, i - 1);
            if (i - 1) - start < end - i {
                self.sort(start, (i - start) - 1);
                start = i;
                count = end - i;
            } else {
                self.sort(i, end - i);
                count = (i - start) - 1;
            }
        }
    }
}

/// `ksort($array)` of an array whose keys are `keys`, in insertion order;
/// `array_keys()` of the result.
pub fn ksort_strings(keys: &mut Vec<String>) {
    let mut sorter = Sorter {
        keys: keys
            .drain(..)
            .enumerate()
            .map(|(order, text)| Key {
                number: numeric_value(&text),
                text,
                order,
            })
            .collect(),
    };
    let count = sorter.keys.len();
    if count > 1 {
        sorter.sort(0, count);
    }
    keys.extend(sorter.keys.into_iter().map(|key| key.text));
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case("123", Some(123.0))]
    #[case(" 12", Some(12.0))]
    #[case("12 ", Some(12.0))]
    #[case("-12", Some(-12.0))]
    #[case("1e3", Some(1000.0))]
    #[case("1.", Some(1.0))]
    #[case(".5", Some(0.5))]
    #[case("1.e", None)]
    #[case("1e", None)]
    #[case("1e+", None)]
    #[case(" . ", None)]
    #[case("0x1", None)]
    #[case("- 1", None)]
    #[case("", None)]
    #[case("   ", None)]
    #[case("abc", None)]
    fn reads_numeric_strings_like_php(#[case] text: &str, #[case] expected: Option<f64>) {
        assert_eq!(numeric_value(text), expected);
    }

    fn sorted(keys: &[&str]) -> Vec<String> {
        let mut keys: Vec<String> = keys.iter().map(|key| (*key).to_owned()).collect();
        ksort_strings(&mut keys);
        keys
    }

    #[test]
    fn compares_numbers_numerically() {
        assert_eq!(sorted(&[" 9 ", " 10", "abc"]), [" 9 ", " 10", "abc"]);
        assert_eq!(sorted(&["12 ", " 12", "100"]), ["12 ", " 12", "100"]);
    }
}
