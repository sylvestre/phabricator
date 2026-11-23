//! `conduit.query` (`ConduitQueryConduitAPIMethod`): the description,
//! parameters and return type of every method the viewer can call, from
//! the method catalog `scripts/generate-catalogs.php` writes
//! (`crates/phab-web/catalog/conduit.json`), which lists PHP's methods,
//! ported or not.
//!
//! The catalog must be regenerated when PHP's methods change. Left to PHP:
//! viewers who can't see the Conduit application (the query fails).

use std::sync::OnceLock;

use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_policy::ApplicationPolicies;
use phab_policy::loader::{self, NoExtendedObjects};
use serde_json::{Map, Value};

/// A method of the catalog, with what `conduit.query` reads of it.
struct CatalogMethod {
    name: String,
    application: Option<String>,
    description: Value,
    params: Vec<(String, Value)>,
    return_type: Value,
}

/// Every method, in `PhutilClassMapQuery` order (`getSortOrder()`).
fn catalog() -> &'static [CatalogMethod] {
    static METHODS: OnceLock<Vec<CatalogMethod>> = OnceLock::new();
    METHODS.get_or_init(|| {
        let Ok(Value::Array(methods)) =
            serde_json::from_str::<Value>(include_str!("../../../phab-web/catalog/conduit.json"))
        else {
            panic!("conduit.json is a list of methods");
        };
        methods
            .iter()
            .map(|method| CatalogMethod {
                name: method["name"].as_str().unwrap_or_default().to_owned(),
                application: method["application"].as_str().map(str::to_owned),
                description: method["description"].clone(),
                params: method["params"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|param| {
                        (
                            param["name"].as_str().unwrap_or_default().to_owned(),
                            param["type"].clone(),
                        )
                    })
                    .collect(),
                return_type: method["return_type"].clone(),
            })
            .collect()
    })
}

/// `conduit.query`.
pub struct ConduitQuery;

impl ConduitQuery {
    async fn run(request: &ApiRequest<'_>) -> Result<Value, MethodError> {
        let context = request.context;
        let config = &context.config;
        let policies = ApplicationPolicies::from_config(config);
        // `filterMethods()`: methods of uninstalled applications go; then
        // `willFilterPage()` drops those of applications the viewer can't
        // see, whose policies are also the methods' own.
        let methods: Vec<&CatalogMethod> = catalog()
            .iter()
            .filter(|method| {
                method
                    .application
                    .as_deref()
                    .is_none_or(|class| policies.is_installed(class))
            })
            .collect();
        let mut classes: Vec<&str> = methods
            .iter()
            .filter_map(|method| method.application.as_deref())
            .chain([crate::APPLICATION.class])
            .collect();
        classes.sort_unstable();
        classes.dedup();
        let subjects: Vec<_> = classes
            .iter()
            .map(|class| policies.subject(class, config.allow_public()))
            .collect();
        let subject_refs: Vec<_> = subjects.iter().collect();
        let facts = loader::load_facts(
            &context.database,
            config,
            &request.viewer,
            &subject_refs,
            &NoExtendedObjects,
            request.now,
        )
        .await?;
        let mut visible = Vec::new();
        for class in &classes {
            if phab_policy::filter::can_see_application(&request.viewer, class, &facts)? {
                visible.push(*class);
            }
        }
        if !visible.contains(&crate::APPLICATION.class) {
            return Err(MethodError::Unsupported(
                "viewers who can't see the Conduit application",
            ));
        }
        let mut map = Map::new();
        for method in methods {
            if method
                .application
                .as_deref()
                .is_some_and(|class| !visible.contains(&class))
            {
                continue;
            }
            let params = if method.params.is_empty() {
                Value::Array(Vec::new())
            } else {
                Value::Object(method.params.iter().cloned().collect())
            };
            let mut entry = Map::new();
            entry.insert("description".to_owned(), method.description.clone());
            entry.insert("params".to_owned(), params);
            entry.insert("return".to_owned(), method.return_type.clone());
            map.insert(method.name.clone(), Value::Object(entry));
        }
        Ok(Value::Object(map))
    }
}

impl Method for ConduitQuery {
    fn name(&self) -> &'static str {
        "conduit.query"
    }

    fn params(&self) -> &'static [&'static str] {
        &[]
    }

    fn application(&self) -> Option<Application> {
        Some(crate::APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(Self::run(request))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalog_loads() {
        let query = catalog()
            .iter()
            .find(|method| method.name == "conduit.query")
            .expect("conduit.query is in the catalog");
        assert_eq!(query.params.len(), 0);
        assert_eq!(query.return_type, "dict<dict>");
    }
}
