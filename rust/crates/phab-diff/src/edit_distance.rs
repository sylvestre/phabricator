//! Port of `PhutilEditDistanceMatrix`, the Levenshtein edit distance PHP
//! uses for intraline changes.
//!
//! Only what the diff code uses is ported: insert, delete and replace costs,
//! the alter cost, the maximum length and smoothing. Transpositions
//! (Damerau-Levenshtein) are not.
//!
//! Costs are `f64`, like PHP's floats, and are added in the same order, so
//! ties between operations break the same way.

/// How short runs of unchanged characters between changes are merged into
/// the changes (`PhutilEditDistanceMatrix::SMOOTHING_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Smoothing {
    /// `SMOOTHING_NONE`.
    None,
    /// `SMOOTHING_INTERNAL`: only runs between two changes.
    Internal,
    /// `SMOOTHING_FULL`: also runs at the start and end.
    Full,
}

/// The costs and options of a computation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Costs {
    /// `setInsertCost()`.
    pub insert: f64,
    /// `setDeleteCost()`.
    pub delete: f64,
    /// `setReplaceCost()`.
    pub replace: f64,
    /// `setAlterCost()`: added when the operation type changes, to prefer
    /// long runs of one operation.
    pub alter: f64,
    /// `setMaximumLength()`: longer sequences (after stripping the common
    /// prefix and suffix) are deleted and inserted whole.
    pub maximum_length: Option<usize>,
    /// `setApplySmoothing()`.
    pub smoothing: Smoothing,
}

impl Default for Costs {
    fn default() -> Self {
        Self {
            insert: 1.0,
            delete: 1.0,
            replace: 1.0,
            alter: 0.0,
            maximum_length: None,
            smoothing: Smoothing::None,
        }
    }
}

/// `PhutilEditDistanceMatrix` after `setSequences()`: the sequences without
/// their common prefix and suffix.
#[derive(Debug)]
pub struct EditDistance<'a, T> {
    costs: Costs,
    x: &'a [T],
    y: &'a [T],
    prefix: usize,
    suffix: usize,
}

impl<'a, T: PartialEq> EditDistance<'a, T> {
    /// `setSequences()`.
    pub fn new(costs: Costs, x: &'a [T], y: &'a [T]) -> Self {
        let shortest = x.len().min(y.len());
        let prefix = x.iter().zip(y).take_while(|(a, b)| a == b).count();
        let suffix = x
            .iter()
            .rev()
            .zip(y.iter().rev())
            .take(shortest - prefix)
            .take_while(|(a, b)| a == b)
            .count();
        Self {
            costs,
            x: &x[prefix..x.len() - suffix],
            y: &y[prefix..y.len() - suffix],
            prefix,
            suffix,
        }
    }

    fn too_long(&self) -> bool {
        self.costs
            .maximum_length
            .is_some_and(|maximum| self.x.len() > maximum || self.y.len() > maximum)
    }

    /// `getEditDistance()`.
    #[allow(clippy::cast_precision_loss)]
    pub fn distance(&self) -> f64 {
        let (xl, yl) = (self.x.len() as f64, self.y.len() as f64);
        if self.x.is_empty() {
            return self.costs.insert * yl;
        }
        if self.y.is_empty() {
            return self.costs.delete * xl;
        }
        if self.too_long() {
            return self.costs.insert * yl + self.costs.delete * xl;
        }
        if self.x == self.y {
            return 0.0;
        }
        let (distances, _) = self.matrix();
        distances[self.x.len()][self.y.len()]
    }

    /// `getEditString()`: one character per operation, `s` (same), `i`
    /// (insert), `d` (delete) or `x` (replace).
    pub fn edit_string(&self) -> String {
        let (x, y) = (self.x, self.y);
        if x.is_empty() || y.is_empty() || x == y || self.too_long() {
            let middle = if x == y {
                "s".repeat(x.len())
            } else {
                "d".repeat(x.len()) + &"i".repeat(y.len())
            };
            return self.pad(&middle);
        }

        let (_, types) = self.matrix();
        let (mut xx, mut yy) = (x.len(), y.len());
        let mut reversed = Vec::new();
        loop {
            let operation = types[xx][yy];
            match operation {
                b's' | b'x' => {
                    xx -= 1;
                    yy -= 1;
                }
                b'i' => yy -= 1,
                _ => xx -= 1,
            }
            reversed.push(operation);
            if xx == 0 && yy == 0 {
                break;
            }
        }
        reversed.reverse();
        let edits = String::from_utf8(reversed).expect("operations are ASCII");

        match self.costs.smoothing {
            Smoothing::Full => smooth(&self.pad(&edits), true),
            Smoothing::Internal => self.pad(&smooth(&edits, false)),
            Smoothing::None => self.pad(&edits),
        }
    }

    fn pad(&self, edits: &str) -> String {
        format!(
            "{}{edits}{}",
            "s".repeat(self.prefix),
            "s".repeat(self.suffix)
        )
    }

    /// `computeMatrix()`, without transpositions: the distances and the
    /// operation chosen at each cell.
    #[allow(clippy::cast_precision_loss)]
    fn matrix(&self) -> (Vec<Vec<f64>>, Vec<Vec<u8>>) {
        let (x, y) = (self.x, self.y);
        let costs = self.costs;
        let mut m = vec![vec![0.0; y.len() + 1]; x.len() + 1];
        let mut t = vec![vec![b's'; y.len() + 1]; x.len() + 1];
        for (xx, row) in m.iter_mut().enumerate() {
            row[0] = xx as f64 * costs.delete;
        }
        for (yy, cell) in m[0].iter_mut().enumerate() {
            *cell = yy as f64 * costs.insert;
        }
        for row in &mut t {
            row[0] = b'd';
        }
        t[0].fill(b'i');
        t[0][0] = b's';

        for xx in 1..=x.len() {
            for yy in 1..=y.len() {
                let (mut rep_cost, rep_type) = if x[xx - 1] == y[yy - 1] {
                    (m[xx - 1][yy - 1] + 0.0, b's')
                } else {
                    (m[xx - 1][yy - 1] + costs.replace, b'x')
                };
                let mut del_cost = m[xx - 1][yy] + costs.delete;
                let mut ins_cost = m[xx][yy - 1] + costs.insert;
                if costs.alter != 0.0 {
                    if t[xx - 1][yy] != b'd' {
                        del_cost += costs.alter;
                    }
                    if t[xx][yy - 1] != b'i' {
                        ins_cost += costs.alter;
                    }
                    if t[xx - 1][yy - 1] != rep_type {
                        rep_cost += costs.alter;
                    }
                }
                let (cost, operation) = if rep_cost <= del_cost && rep_cost <= ins_cost {
                    (rep_cost, rep_type)
                } else if ins_cost <= del_cost {
                    (ins_cost, b'i')
                } else {
                    (del_cost, b'd')
                };
                m[xx][yy] = cost;
                t[xx][yy] = operation;
            }
        }
        (m, t)
    }
}

/// `applySmoothing()`: turns runs of up to three `s` between changes into
/// `x`, repeating the `preg_replace()` passes until nothing changes.
fn smooth(edits: &str, full: bool) -> String {
    let mut current = edits.as_bytes().to_vec();
    loop {
        let original = current.clone();
        for run in [3, 2, 1] {
            current = replace_runs(&current, run, full);
        }
        if current == original {
            break;
        }
    }
    String::from_utf8(current).expect("operations are ASCII")
}

fn is_change(byte: u8) -> bool {
    matches!(byte, b'x' | b'd' | b'i')
}

/// One `preg_replace()` of `/([xdi])(s{n})([xdi])/` (with `^` and `\z` as
/// alternatives in full mode) by `$1` + `x` × n + `$3`: matches don't
/// overlap and the search resumes after the consumed delimiter.
fn replace_runs(edits: &[u8], run: usize, full: bool) -> Vec<u8> {
    let match_at = |start: usize| -> Option<usize> {
        let mut prefixes = Vec::with_capacity(2);
        if full && start == 0 {
            prefixes.push(0);
        }
        if edits.get(start).copied().is_some_and(is_change) {
            prefixes.push(1);
        }
        prefixes.into_iter().find_map(|prefix| {
            let first = start + prefix;
            let after = first + run;
            if after > edits.len() || edits[first..after].iter().any(|&byte| byte != b's') {
                return None;
            }
            if edits.get(after).copied().is_some_and(is_change) {
                Some(after + 1)
            } else if full && after == edits.len() {
                Some(after)
            } else {
                None
            }
        })
    };

    let mut result = Vec::with_capacity(edits.len());
    let mut position = 0;
    while position < edits.len() {
        if let Some(end) = match_at(position) {
            // The delimiters are changes; only the run itself is `s`.
            result.extend(
                edits[position..end]
                    .iter()
                    .map(|&byte| if byte == b's' { b'x' } else { byte }),
            );
            position = end;
        } else {
            result.push(edits[position]);
            position += 1;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn chars(text: &str) -> Vec<u8> {
        text.bytes().collect()
    }

    // From PhutilEditDistanceMatrixTestCase::testEditDistance().
    #[rstest]
    #[case(0.0, "", "", 1.0, 1.0, 1.0)]
    #[case(27.0, "", "xxx", 9.0, 1.0, 1.0)]
    #[case(27.0, "xxx", "", 1.0, 9.0, 1.0)]
    #[case(0.0, "x", "x", 1.0, 1.0, 1.0)]
    #[case(0.0, "xyzabc", "xyzabc", 1.0, 1.0, 1.0)]
    #[case(1.0, "cat", "car", 1.0, 1.0, 1.0)]
    #[case(1.0, "cat", "cut", 1.0, 1.0, 1.0)]
    #[case(1.0, "cat", "bat", 1.0, 1.0, 1.0)]
    #[case(2.0, "cat", "bar", 1.0, 1.0, 1.0)]
    #[case(3.0, "cat", "dog", 1.0, 1.0, 1.0)]
    #[case(1.0, "cat", "cats", 1.0, 1.0, 1.0)]
    #[case(1.0, "cat", "cast", 1.0, 1.0, 1.0)]
    #[case(1.0, "cat", "xcat", 1.0, 1.0, 1.0)]
    #[case(1.0, "cat", "cxat", 1.0, 1.0, 1.0)]
    #[case(1.0, "cat", "ca", 1.0, 1.0, 1.0)]
    #[case(1.0, "cat", "ct", 1.0, 1.0, 1.0)]
    #[case(1.0, "cat", "at", 1.0, 1.0, 1.0)]
    #[case(1.0, "xxxxx", "xxxxy", 1.0, 1.0, 1.0)]
    #[case(1.0, "xxxxx", "xxyxx", 1.0, 1.0, 1.0)]
    #[case(1.0, "yxxxx", "xxxxx", 1.0, 1.0, 1.0)]
    #[case(27.0, "x", "xxxx", 9.0, 1.0, 1.0)]
    #[case(27.0, "xxxx", "x", 1.0, 9.0, 1.0)]
    #[case(27.0, "xxxxx", "xyyyx", 5.0, 5.0, 9.0)]
    #[case(
        13.0,
        "The quick brown fox jumped over the lazy dog!",
        "The brown fox pumped over those lazy dogs!~~~",
        1.0,
        1.0,
        1.0
    )]
    fn distances(
        #[case] expected: f64,
        #[case] x: &str,
        #[case] y: &str,
        #[case] insert: f64,
        #[case] delete: f64,
        #[case] replace: f64,
    ) {
        let costs = Costs {
            insert,
            delete,
            replace,
            ..Costs::default()
        };
        let (x, y) = (chars(x), chars(y));
        let distance = EditDistance::new(costs, &x, &y).distance();
        assert!((distance - expected).abs() < f64::EPSILON, "{distance}");
    }

    // From PhutilEditDistanceMatrixTestCase::testEditString().
    #[rstest]
    #[case("", "", "", 0.0)]
    #[case("iii", "", "aaa", 0.0)]
    #[case("ddd", "aaa", "", 0.0)]
    #[case("sss", "aaa", "aaa", 0.0)]
    #[case("xssisssisx", "Qz(ab)zQ", "Rz((ab))zR", 0.001)]
    #[case("xxs", "cat", "rot", 0.0)]
    #[case(
        "xxsssxxsssixsxxxsssdssssssssxssxsixsissssddddsdxxsssss",
        "Cozy lummox gives smart squid who asks for job pen.",
        "Lazy hammock takes mart squad that basks in pen.",
        0.0
    )]
    #[case("xxss", "land", "alnd", 0.0)]
    fn edit_strings(#[case] expected: &str, #[case] x: &str, #[case] y: &str, #[case] alter: f64) {
        let costs = Costs {
            alter,
            ..Costs::default()
        };
        let (x, y) = (chars(x), chars(y));
        assert_eq!(EditDistance::new(costs, &x, &y).edit_string(), expected);
    }

    // From PhutilEditDistanceMatrixTestCase::testEditMatrixMaximumLength().
    #[rstest]
    #[case("XaaY", "QaaR", "ddddiiii")]
    #[case("ffXaaYgg", "ffQaaRgg", "ssddddiiiiss")]
    fn maximum_length(#[case] x: &str, #[case] y: &str, #[case] expected: &str) {
        let costs = Costs {
            insert: 3.0,
            delete: 7.0,
            maximum_length: Some(1),
            ..Costs::default()
        };
        let (x, y) = (chars(x), chars(y));
        let matrix = EditDistance::new(costs, &x, &y);
        assert!((matrix.distance() - 40.0).abs() < f64::EPSILON);
        assert_eq!(matrix.edit_string(), expected);
    }

    #[rstest]
    #[case("xsx", false, "xxx")]
    #[case("xssssx", false, "xssssx")]
    #[case("xsssx", false, "xxxxx")]
    #[case("xsxsx", false, "xxxxx")]
    #[case("ssx", false, "ssx")]
    #[case("ssx", true, "xxx")]
    #[case("xss", true, "xxx")]
    #[case("ssss", true, "ssss")]
    fn smoothing(#[case] edits: &str, #[case] full: bool, #[case] expected: &str) {
        assert_eq!(smooth(edits, full), expected);
    }
}
