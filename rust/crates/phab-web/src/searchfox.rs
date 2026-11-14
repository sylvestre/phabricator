//! Links to Searchfox, which shows Mozilla's code instead of Diffusion.
//!
//! `PHAB_SEARCHFOX_TREES` maps repositories, by short name or callsign, to
//! Searchfox trees: `mozilla-central=mozilla-central,comm-central=comm-central`.
//! Repositories it doesn't name get no links.

use std::collections::HashMap;
use std::sync::OnceLock;

use phab_app_differential::repository::Repository;

const ORIGIN: &str = "https://searchfox.org";

/// The trees of `PHAB_SEARCHFOX_TREES`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Trees(HashMap<String, String>);

impl Trees {
    /// Parses `repository=tree` pairs separated by commas; malformed pairs
    /// are skipped.
    pub fn parse(spec: &str) -> Self {
        Self(
            spec.split(',')
                .filter_map(|pair| {
                    let (repository, tree) = pair.split_once('=')?;
                    let (repository, tree) = (repository.trim(), tree.trim());
                    let valid = !repository.is_empty()
                        && !tree.is_empty()
                        && tree
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte));
                    valid.then(|| (repository.to_owned(), tree.to_owned()))
                })
                .collect(),
        )
    }

    /// From the environment, read once.
    pub fn from_env() -> &'static Self {
        static TREES: OnceLock<Trees> = OnceLock::new();
        TREES.get_or_init(|| {
            std::env::var("PHAB_SEARCHFOX_TREES")
                .map(|spec| Self::parse(&spec))
                .unwrap_or_default()
        })
    }

    /// The tree of `repository`, by short name first, then callsign.
    pub fn tree_for(&self, repository: &Repository) -> Option<Tree> {
        [repository.slug.as_deref(), repository.callsign.as_deref()]
            .into_iter()
            .flatten()
            .find_map(|key| self.0.get(key))
            .map(|tree| Tree(tree.clone()))
    }
}

/// A Searchfox tree, like `mozilla-central`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tree(String);

impl Tree {
    /// The tree's root.
    pub fn uri(&self) -> String {
        format!("{ORIGIN}/{}/source/", self.0)
    }

    /// A file of the tree, as it is now (not at the diff's base).
    pub fn file_uri(&self, path: &str) -> String {
        let path: Vec<String> = path
            .trim_start_matches('/')
            .split('/')
            .map(|part| form_urlencoded::byte_serialize(part.as_bytes()).collect())
            .collect();
        format!("{}{}", self.uri(), path.join("/").replace('+', "%20"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trees_parse_and_links_are_encoded() {
        let trees =
            Trees::parse(" mozilla-central = mozilla-central ,bad,=x,COMM=comm-central,evil=a/b");
        assert_eq!(trees.0.len(), 2);
        let tree = Tree("mozilla-central".to_owned());
        assert_eq!(tree.uri(), "https://searchfox.org/mozilla-central/source/");
        assert_eq!(
            tree.file_uri("/dom/base/Element.cpp"),
            "https://searchfox.org/mozilla-central/source/dom/base/Element.cpp"
        );
        assert_eq!(
            tree.file_uri("a b/c#d?.js"),
            "https://searchfox.org/mozilla-central/source/a%20b/c%23d%3F.js"
        );
    }
}
