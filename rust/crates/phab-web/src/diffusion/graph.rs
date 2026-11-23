//! The commit graph next to lists of commits (`PHUIDiffGraphView`): for
//! each commit, a line of columns (`o` the commit, `|` a branch passing
//! by, `^` a commit nothing follows, `x` and `X` the end of a branch) and
//! the columns it joins and splits to, which the page's script draws as
//! PHP's `diffusion-commit-graph` behavior does.

use serde_json::{Value, json};

/// A row of the graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Row {
    pub(crate) line: String,
    pub(crate) split: Vec<usize>,
    pub(crate) join: Vec<usize>,
}

impl Row {
    /// The row as the script reads it.
    pub(crate) fn json(&self) -> String {
        json!({"line": self.line, "split": self.split, "join": self.join}).to_string()
    }
}

/// `DiffusionCommitGraphView::newGraphView()` with `setFilterParents()`:
/// parents outside the list are dropped.
pub(crate) fn filter_parents(parents: &mut [(String, Vec<String>)]) {
    let known: std::collections::HashSet<String> =
        parents.iter().map(|(hash, _)| hash.clone()).collect();
    for (_, list) in parents.iter_mut() {
        list.retain(|parent| known.contains(parent));
    }
}

/// The `parents` map of `diffusion.historyquery`, in its order.
pub(crate) fn parents_of(result: &Value) -> Vec<(String, Vec<String>)> {
    result
        .get("parents")
        .and_then(Value::as_object)
        .map(|parents| {
            parents
                .iter()
                .map(|(hash, list)| {
                    let list = list
                        .as_array()
                        .map(|list| {
                            list.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect()
                        })
                        .unwrap_or_default();
                    (hash.clone(), list)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `renderRawGraph()`: the rows of the commits, newest first, and the
/// width of the graph.
#[allow(clippy::too_many_lines)]
pub(crate) fn render(
    parents: &[(String, Vec<String>)],
    is_head: bool,
    is_tail: bool,
) -> (Vec<Row>, usize) {
    let mut graph: Vec<Row> = Vec::new();
    // The next commit of each column; `None` once a column ended.
    let mut threads: Vec<Option<String>> = Vec::new();
    let mut count = 0;
    for (cursor, parent_list) in parents {
        let mut joins = Vec::new();
        let mut splits = Vec::new();
        let mut line = String::new();
        let mut found = false;
        let mut pos = threads.len();
        for (n, thread) in threads.iter_mut().enumerate() {
            match thread {
                None => line.push(' '),
                Some(next) if next.is_empty() => line.push(' '),
                Some(next) if next == cursor => {
                    if found {
                        line.push(' ');
                        joins.push(n);
                        *thread = None;
                    } else {
                        line.push('o');
                        found = true;
                        pos = n;
                    }
                }
                Some(_) => line.push('|'),
            }
        }
        if !found {
            if is_head {
                line.push('^');
            } else {
                line.push('o');
                for row in &mut graph {
                    while row.line.len() <= count {
                        row.line.push('|');
                    }
                }
            }
        }
        let first = parent_list.first().cloned();
        if pos == threads.len() {
            threads.push(first);
        } else {
            threads[pos] = first;
        }
        count = count.max(pos + 1);
        for parent in parent_list.iter().skip(1) {
            if let Some(index) = threads
                .iter()
                .position(|thread| thread.as_deref() == Some(parent.as_str()))
            {
                splits.push(index);
                continue;
            }
            let free = (0..count)
                .find(|n| {
                    threads
                        .get(*n)
                        .is_none_or(|thread| thread.as_deref().is_none_or(str::is_empty))
                })
                .unwrap_or(count);
            if free >= threads.len() {
                threads.resize(free + 1, None);
            }
            threads[free] = Some(parent.clone());
            splits.push(free);
            count = count.max(free + 1);
        }
        graph.push(Row {
            line,
            split: splits,
            join: joins,
        });
    }

    if is_tail && !graph.is_empty() {
        let mut terminated = std::collections::HashSet::new();
        for key in (0..graph.len()).rev() {
            let mut line: Vec<u8> = graph[key].line.clone().into_bytes();
            for (index, character) in line.iter_mut().enumerate() {
                match *character {
                    b'o' => {
                        if !terminated.insert(index) {
                            continue;
                        }
                        if graph
                            .get(key + 1)
                            .is_some_and(|next| next.join.contains(&index))
                        {
                            continue;
                        }
                        *character = b'x';
                    }
                    b' ' => {
                        terminated.remove(&index);
                    }
                    _ => {
                        terminated.insert(index);
                    }
                }
            }
            graph[key].line = String::from_utf8(line).unwrap_or_default();
        }
        if let Some(last) = graph.last_mut() {
            last.line = last.line.replace('^', "X");
        }
    }
    (graph, count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parents(list: &[(&str, &[&str])]) -> Vec<(String, Vec<String>)> {
        list.iter()
            .map(|(hash, parents)| {
                (
                    (*hash).to_owned(),
                    parents.iter().map(|parent| (*parent).to_owned()).collect(),
                )
            })
            .collect()
    }

    fn lines(rows: &[Row]) -> Vec<&str> {
        rows.iter().map(|row| row.line.as_str()).collect()
    }

    fn picture(list: &[(&str, &[&str])]) -> Vec<String> {
        let (rows, _) = render(&parents(list), true, true);
        rows.into_iter().map(|row| row.line).collect()
    }

    // PHUIDiffGraphViewTestCase.
    #[test]
    fn terminates_tails_like_php() {
        assert_eq!(
            picture(&[
                ("A", &["B"]),
                ("B", &["C", "D", "E"]),
                ("E", &[]),
                ("D", &[]),
                ("C", &["F", "G"]),
                ("G", &[]),
                ("F", &[]),
            ]),
            ["^", "o", "||x", "|x ", "o  ", "|x ", "x  "]
        );
    }

    #[test]
    fn draws_reverse_trees_like_php() {
        assert_eq!(
            picture(&[
                ("A", &["B"]),
                ("C", &["B"]),
                ("B", &["D"]),
                ("E", &["D"]),
                ("F", &["D"]),
                ("D", &["G"]),
                ("G", &[]),
            ]),
            ["^", "|^", "o ", "| ^", "| |^", "o   ", "x   "]
        );
    }

    #[test]
    fn terminates_joins_like_php() {
        assert_eq!(
            picture(&[("A", &["D"]), ("B", &["C"]), ("C", &["D"]), ("D", &[])]),
            ["^", "|^", "|o", "x "]
        );
    }

    #[test]
    fn joins_three_ways_like_php() {
        assert_eq!(
            picture(&[
                ("A", &["D", "C", "B"]),
                ("B", &["D"]),
                ("C", &["B", "E", "F"]),
                ("D", &[]),
                ("E", &[]),
                ("F", &[]),
            ]),
            ["^", "||o", "|o|", "x| ||", " | x|", " |  x"]
        );
        let (rows, count) = render(&parents(&[("A", &["B"]), ("B", &[])]), true, true);
        assert_eq!(rows[0].json(), r#"{"line":"^","split":[],"join":[]}"#);
        assert_eq!(count, 1);
    }

    #[test]
    fn draws_pages_after_the_first_like_php() {
        let (rows, _) = render(&parents(&[("A", &["B"]), ("B", &[])]), false, false);
        assert_eq!(lines(&rows), ["o", "o"]);
        let (rows, _) = render(&parents(&[("A", &[])]), true, true);
        assert_eq!(lines(&rows), ["X"]);
    }

    #[test]
    fn filters_parents() {
        let mut list = parents(&[("A", &["B", "Z"]), ("B", &[])]);
        filter_parents(&mut list);
        assert_eq!(list[0].1, ["B"]);
    }
}
