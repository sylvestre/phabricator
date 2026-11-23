//! `PhabricatorGitGraphStream` (the parents and dates of commits, read from
//! `git log` as far as needed) and the topological sort of
//! `AbstractDirectedGraph`.

use std::collections::HashMap;

use indexmap::{IndexMap, IndexSet};

use crate::git::{LineStream, WorkingCopy, ref_selector};
use crate::import::ImportError;

/// `PhabricatorGitGraphStream`.
pub struct GitGraphStream {
    lines: LineStream,
    start: Option<String>,
    parents: HashMap<String, Vec<String>>,
    dates: HashMap<String, String>,
    /// The stream ended (or failed): every commit not parsed yet is not
    /// reachable.
    exhausted: bool,
}

impl GitGraphStream {
    /// The stream of the ancestors of `start`, or of every ref.
    pub fn new(copy: &WorkingCopy, start: Option<&str>) -> Result<Self, ImportError> {
        let mut argv = vec!["log".to_owned(), "--format=%H%x01%P%x01%ct".to_owned()];
        match start {
            Some(start) => argv.push(ref_selector(start)?),
            None => argv.push("--all".to_owned()),
        }
        argv.push("--".to_owned());
        Ok(Self {
            lines: copy.stream(&argv)?,
            start: start.map(str::to_owned),
            parents: HashMap::new(),
            dates: HashMap::new(),
            exhausted: false,
        })
    }

    /// `getParents()`: without the duplicates Git may list.
    pub async fn parents(&mut self, commit: &str) -> Result<Vec<String>, ImportError> {
        self.parse_until(commit).await?;
        let mut unique: IndexSet<String> = IndexSet::new();
        for parent in &self.parents[commit] {
            unique.insert(parent.clone());
        }
        Ok(unique.into_iter().collect())
    }

    /// `getCommitDate()`: the `%ct` of the commit, as Git prints it.
    pub async fn commit_date(&mut self, commit: &str) -> Result<String, ImportError> {
        self.parse_until(commit).await?;
        Ok(self.dates[commit].clone())
    }

    async fn parse_until(&mut self, commit: &str) -> Result<(), ImportError> {
        if self.dates.contains_key(commit) {
            return Ok(());
        }
        while !self.exhausted {
            let line = match self.lines.next_line().await {
                Ok(Some(line)) => line,
                Ok(None) => {
                    self.exhausted = true;
                    break;
                }
                Err(error) => {
                    self.exhausted = true;
                    return Err(error.into());
                }
            };
            let line = crate::php::text(line)?;
            let line = crate::php::trim(&line);
            if line.is_empty() {
                self.exhausted = true;
                break;
            }
            let mut fields = line.split('\u{1}');
            let (Some(hash), Some(parents), Some(epoch)) =
                (fields.next(), fields.next(), fields.next())
            else {
                return Err(ImportError::unsupported("an unusual git log line"));
            };
            let parents = if parents.is_empty() {
                Vec::new()
            } else {
                parents.split(' ').map(str::to_owned).collect()
            };
            self.dates.insert(hash.to_owned(), epoch.to_owned());
            self.parents.insert(hash.to_owned(), parents);
            if hash == commit {
                return Ok(());
            }
        }
        Err(ImportError::Failed(match &self.start {
            Some(start) => {
                format!("Commit \"{commit}\" is not a reachable ancestor of \"{start}\".")
            }
            None => format!("Commit \"{commit}\" is not a reachable ancestor of any ref."),
        }))
    }
}

/// `AbstractDirectedGraph::getNodesInTopologicalOrder()` of a graph whose
/// every node is a key of `nodes`.
pub fn topological_order(nodes: &IndexMap<String, Vec<String>>) -> Vec<String> {
    let mut inverse: IndexMap<&str, IndexSet<&str>> = IndexMap::new();
    for (node, edges) in nodes {
        inverse.entry(node.as_str()).or_default();
        for edge in edges {
            inverse
                .entry(edge.as_str())
                .or_default()
                .insert(node.as_str());
        }
    }
    let mut ends: Vec<&str> = inverse
        .iter()
        .filter(|(_, edges)| edges.is_empty())
        .map(|(node, _)| *node)
        .collect();
    let mut sorted = Vec::new();
    while let Some(current) = ends.pop() {
        sorted.push(current.to_owned());
        for edge in nodes.get(current).into_iter().flatten() {
            if let Some(inverse_edges) = inverse.get_mut(edge.as_str()) {
                inverse_edges.shift_remove(current);
                if inverse_edges.is_empty() {
                    ends.push(edge.as_str());
                }
            }
        }
    }
    sorted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(edges: &[(&str, &[&str])]) -> IndexMap<String, Vec<String>> {
        edges
            .iter()
            .map(|(node, parents)| {
                (
                    (*node).to_owned(),
                    parents.iter().map(|parent| (*parent).to_owned()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn sorts_like_php() {
        // A merge M of B and C, both children of A: PHP pops the last end
        // node first, so M, then C (its last parent), then B, then A.
        let nodes = graph(&[("M", &["B", "C"]), ("C", &["A"]), ("B", &["A"]), ("A", &[])]);
        assert_eq!(topological_order(&nodes), ["M", "C", "B", "A"]);
        let nodes = graph(&[("X", &[]), ("Y", &[])]);
        assert_eq!(topological_order(&nodes), ["Y", "X"]);
    }
}
