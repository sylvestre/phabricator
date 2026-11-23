//! `/typeahead/class/<class>/` (`PhabricatorTypeaheadModularDatasourceController`):
//! the results PHP's tokenizers and autocompletes load as they are typed
//! in, byte for byte as PHP's `AphrontAjaxResponse` sends them.
//!
//! Ported datasources: users (`PhabricatorPeopleDatasource`), projects
//! (`PhabricatorProjectDatasource`, with its `policy` and `autocomplete`
//! parameters) and reviewers (`DifferentialReviewerDatasource` and
//! `DifferentialBlockingReviewerDatasource`). The composite datasources'
//! phases (names starting with what was typed first, then the others),
//! sorting and limits are reproduced as `PhabricatorTypeaheadCompositeDatasource`
//! runs them. PHP keeps (421): other datasources, the browse dialog and
//! its selections, the debug page of requests which aren't Ajax,
//! functions (`blocking(...)`), users who are away (their availability
//! colour) or whose availability comes from calendar events, projects
//! hidden by policy among a full page, and names whose lowercase PHP's
//! `mb_strtolower()` might write differently.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::Response;
use phab_app_project::{Project, typeahead as projects};
use phab_auth::User;
use phab_core::Phid;
use serde_json::{Value, json};

use crate::error::PageError;
use crate::php_headers::{self, Options};
use crate::state::WebState;
use crate::viewer::WebUser;

/// `$limit` of the controller; it asks for one more.
const LIMIT: usize = 100;

const PEOPLE: &str = "PhabricatorPeopleDatasource";
const PROJECTS: &str = "PhabricatorProjectDatasource";
const REVIEWERS: &str = "DifferentialReviewerDatasource";
const BLOCKING: &str = "DifferentialBlockingReviewerDatasource";

/// `PHASE_PREFIX` and `PHASE_CONTENT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Prefix,
    Content,
}

impl Phase {
    fn key(self) -> &'static str {
        match self {
            Self::Prefix => "prefix",
            Self::Content => "content",
        }
    }
}

/// A datasource.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
    People,
    Projects {
        /// The `policy` parameter: no milestones.
        for_policy: bool,
        /// The `autocomplete` parameter: only projects with a hashtag.
        for_autocomplete: bool,
    },
    /// A composite datasource: the controller's runtime one, reviewers or
    /// blocking reviewers.
    Composite {
        blocking: bool,
        application: Option<&'static str>,
        children: Vec<Self>,
    },
}

impl Source {
    /// `getDatasourceApplicationClass()`.
    fn application(&self) -> Option<&'static str> {
        match self {
            Self::People => Some("PhabricatorPeopleApplication"),
            Self::Projects { .. } => Some("PhabricatorProjectApplication"),
            Self::Composite { application, .. } => *application,
        }
    }

    fn blocking() -> Self {
        Self::Composite {
            blocking: true,
            application: Some("PhabricatorDifferentialApplication"),
            children: vec![Self::people(), Self::projects()],
        }
    }

    fn people() -> Self {
        Self::People
    }

    fn projects() -> Self {
        Self::Projects {
            for_policy: false,
            for_autocomplete: false,
        }
    }

    /// The datasource of a class, with its parameters.
    fn for_class(class: &str, parameters: &serde_json::Map<String, Value>) -> Option<Self> {
        let parameter = |name: &str| parameters.get(name).is_some_and(phab_core::php::is_truthy);
        Some(match class {
            PEOPLE => Self::people(),
            PROJECTS => Self::Projects {
                for_policy: parameter("policy"),
                for_autocomplete: parameter("autocomplete"),
            },
            REVIEWERS => Self::Composite {
                blocking: false,
                application: Some("PhabricatorDifferentialApplication"),
                children: vec![Self::people(), Self::projects(), Self::blocking()],
            },
            BLOCKING => Self::blocking(),
            _ => return None,
        })
    }
}

/// A `PhabricatorTypeaheadResult`.
#[derive(Debug, Clone, Default, PartialEq)]
struct Item {
    name: String,
    uri: Option<String>,
    phid: String,
    priority_string: Option<String>,
    display_name: Option<String>,
    display_type: Option<Value>,
    image_uri: Option<String>,
    priority_type: Option<&'static str>,
    icon: Option<Value>,
    /// The icon of the PHID type, used when `icon` is empty.
    default_icon: Option<&'static str>,
    closed: Option<&'static str>,
    color: Option<String>,
    token_type: Option<&'static str>,
    autocomplete: Option<String>,
    phase: Option<Phase>,
}

impl Item {
    /// `getSortKey()`.
    fn sort_key(&self) -> String {
        format!("B{}", lowercase(&self.name))
    }

    /// `getWireFormat()`: trailing `null`s are dropped.
    fn wire_format(&self) -> Value {
        let text = |value: &Option<String>| value.as_ref().map_or(Value::Null, |text| json!(text));
        let icon = match &self.icon {
            Some(icon) if phab_core::php::is_truthy(icon) => icon.clone(),
            _ => self.default_icon.map_or(Value::Null, |icon| json!(icon)),
        };
        let mut data = vec![
            json!(self.name),
            text(&self.uri),
            json!(self.phid),
            text(&self.priority_string),
            text(&self.display_name),
            self.display_type.clone().unwrap_or(Value::Null),
            text(&self.image_uri),
            self.priority_type.map_or(Value::Null, |kind| json!(kind)),
            icon,
            self.closed.map_or(Value::Null, |closed| json!(closed)),
            Value::Null,
            text(&self.color),
            self.token_type.map_or(Value::Null, |kind| json!(kind)),
            Value::Null,
            text(&self.autocomplete),
            self.phase.map_or(Value::Null, |phase| json!(phase.key())),
            Value::Null,
        ];
        while data.last() == Some(&Value::Null) {
            data.pop();
        }
        Value::Array(data)
    }
}

/// `phutil_utf8_strtolower()` of text Rust lowercases as PHP does: the
/// pages refuse others first (see [`check_case`]).
fn lowercase(text: &str) -> String {
    text.to_ascii_lowercase()
}

/// Whether `mb_strtolower()` and ASCII lowercasing agree on `text`.
fn check_case(text: &str) -> Result<(), PageError> {
    if text
        .chars()
        .any(|character| !character.is_ascii() && character.is_uppercase())
    {
        return Err(PageError::Fallback(
            "a typeahead name with uppercase beyond ASCII",
        ));
    }
    Ok(())
}

/// `PhabricatorTypeaheadDatasource::tokenizeString()`.
fn tokenize(text: &str) -> Vec<String> {
    let text = lowercase(text);
    let text = text.trim_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\r' | '\0' | '\x0B'));
    let mut tokens: Vec<String> = Vec::new();
    // `\s` of PCRE without UCP is ASCII whitespace.
    let separator = |c: char| {
        matches!(
            c,
            ' ' | '\t' | '\n' | '\x0B' | '\x0C' | '\r' | '[' | ']' | '(' | ')' | '-'
        )
    };
    for token in text.split(separator) {
        if !token.is_empty() && !tokens.iter().any(|known| known == token) {
            tokens.push(token.to_owned());
        }
    }
    tokens
}

/// `getNameTokensForQuery()`: the five longest tokens, longest first.
fn longest_tokens(tokens: Vec<String>) -> Vec<String> {
    let mut tokens = tokens;
    tokens.sort_by_key(|token| std::cmp::Reverse(token.len()));
    tokens.truncate(5);
    tokens
}

/// One request's datasources and what they share.
struct Run<'a, S: WebState> {
    state: &'a S,
    viewer: &'a WebUser,
    raw: Option<String>,
    /// The applications the viewer can use, by class.
    applications: HashMap<&'static str, bool>,
}

impl<S: WebState> Run<'_, S> {
    /// `getPrefixQuery()`.
    fn prefix_query(&self) -> String {
        lowercase(self.raw.as_deref().unwrap_or(""))
    }

    /// `loadResultsForPhase()` of `source`: `limit` is the phase's,
    /// `self_limit` what `setLimit()` gave the source, `prefix` the
    /// `prefixString` of a composite.
    async fn load(
        &self,
        source: &Source,
        phase: Phase,
        limit: usize,
        self_limit: usize,
        prefix: Option<&str>,
    ) -> Result<Vec<Item>, PageError> {
        let Source::Composite { children, .. } = source else {
            return match source {
                Source::People => self.people(phase, limit).await,
                Source::Projects {
                    for_policy,
                    for_autocomplete,
                } => {
                    self.projects(phase, limit, *for_policy, *for_autocomplete)
                        .await
                }
                Source::Composite { .. } => unreachable!("handled above"),
            };
        };
        let mut results = Vec::new();
        for child in children {
            if let Some(application) = child.application()
                && !self.applications.get(application).copied().unwrap_or(false)
            {
                continue;
            }
            // Each child is a fresh clone: its own prefix is only set in
            // the prefix phase.
            let child_prefix = (phase == Phase::Prefix).then(|| self.prefix_query());
            let total = (limit * 3).div_ceil(2) + 1;
            let found =
                Box::pin(self.load(child, phase, limit, total, child_prefix.as_deref())).await?;
            let found = did_load(child, found);
            results.extend(filter_phase(phase, found, prefix));
        }
        // `msort()` is stable.
        results.sort_by_cached_key(Item::sort_key);
        results.truncate(self_limit);
        Ok(results)
    }

    /// `PhabricatorPeopleDatasource::loadResults()`.
    async fn people(&self, phase: Phase, limit: usize) -> Result<Vec<Item>, PageError> {
        let database = self.state.database();
        let limit = u32::try_from(limit).unwrap_or(u32::MAX);
        let users = match phase {
            Phase::Prefix => {
                phab_auth::user::search_for_datasource(
                    database,
                    Some(&self.prefix_query()),
                    &[],
                    limit,
                )
                .await?
            }
            Phase::Content => {
                let tokens = tokenize(self.raw.as_deref().unwrap_or(""));
                phab_auth::user::search_for_datasource(database, None, &tokens, limit).await?
            }
        };
        let phids: Vec<Phid> = users.iter().map(|user| user.phid.clone()).collect();
        match phab_auth::user::any_away(database, &phids, self.state.now()).await? {
            Some(false) => {}
            Some(true) => return Err(PageError::Fallback("a user who is away")),
            None => return Err(PageError::Fallback("availability from calendar events")),
        }
        users.iter().map(user_item).collect()
    }

    /// `PhabricatorProjectDatasource::loadResults()`.
    async fn projects(
        &self,
        phase: Phase,
        limit: usize,
        for_policy: bool,
        for_autocomplete: bool,
    ) -> Result<Vec<Item>, PageError> {
        let state = self.state;
        let database = state.database();
        let config = state.config();
        let raw = self.raw.as_deref().unwrap_or("");
        let limit_u32 = u32::try_from(limit).unwrap_or(u32::MAX);
        let found = match phase {
            Phase::Prefix => {
                projects::search(
                    database,
                    Some(&self.prefix_query()),
                    &[],
                    for_policy,
                    limit_u32,
                )
                .await?
            }
            Phase::Content => {
                let tokens = longest_tokens(tokenize(raw.trim_start_matches('#')));
                projects::search(database, None, &tokens, for_policy, limit_u32).await?
            }
        };
        let loaded = found.len();
        let (visible, graph) = phab_app_project::policy::visible_projects(
            database,
            config,
            &self.viewer.user.viewer(),
            found,
            state.now(),
        )
        .await
        .map_err(|error| PageError::Internal(Box::new(error)))?;
        // PHP's query loads more rows to fill a page the policy filter
        // thinned.
        if loaded == limit && visible.len() < loaded {
            return Err(PageError::Fallback("projects hidden among a full page"));
        }
        let phids: Vec<Phid> = visible.iter().map(|project| project.phid.clone()).collect();
        let slugs = projects::load_slugs(database, &phids).await?;
        let images = self.project_images(&visible).await?;
        let icons = projects::Icons::load(config);
        let mut items = Vec::new();
        for project in &visible {
            let project_slugs = slugs.get(&project.phid).map_or(&[][..], Vec::as_slice);
            let slug = project
                .primary_slug
                .clone()
                .or_else(|| project_slugs.iter().find(|slug| !slug.is_empty()).cloned());
            if for_autocomplete && slug.is_none() {
                continue;
            }
            let parent = graph.parent(project);
            let mut strings = vec![project.name.clone()];
            if project.is_milestone() {
                let parent = parent.ok_or(PageError::Fallback("a milestone without parent"))?;
                strings.push(parent.name.clone());
            }
            strings.extend(project_slugs.iter().cloned());
            let item = Item {
                name: strings.join("\n"),
                uri: Some(format!("/project/view/{}/", project.id)),
                phid: project.phid.as_str().to_owned(),
                display_name: Some(project.display_name(parent)),
                display_type: icons.display_name(project),
                image_uri: images.get(&project.phid).cloned(),
                priority_type: Some("proj"),
                icon: icons.display_icon(project),
                default_icon: Some("fa-briefcase bluegrey"),
                closed: project.is_archived().then_some("Archived"),
                color: Some(project.color.clone()),
                autocomplete: slug.map(|slug| format!("#{slug}")),
                ..Item::default()
            };
            check_case(&item.name)?;
            items.push(item);
        }
        Ok(items)
    }

    /// `needImages()`: a visible custom image, or the icon's builtin one.
    async fn project_images(
        &self,
        visible: &[Project],
    ) -> Result<HashMap<Phid, String>, PageError> {
        let state = self.state;
        let database = state.database();
        let config = state.config();
        let phids: Vec<Phid> = visible.iter().map(|project| project.phid.clone()).collect();
        let custom = projects::load_profile_images(database, &phids).await?;
        let mut images = HashMap::new();
        if !custom.is_empty() {
            let files: Vec<Phid> = custom.values().cloned().collect();
            let candidates = phab_app_files::load_by_phids(database, &files).await?;
            let scope = crate::harbormaster::Scope::new(state, self.viewer);
            let files = phab_app_files::search::visible_files(&scope.request(), candidates)
                .await
                .map_err(crate::harbormaster::method_error)?;
            for (project, image) in &custom {
                if let Some(file) = files.iter().find(|file| file.phid == *image) {
                    images.insert(project.clone(), file.best_uri(config));
                }
            }
        }
        let icons = projects::Icons::load(config);
        let mut keys = HashMap::new();
        for project in visible
            .iter()
            .filter(|project| !images.contains_key(&project.phid))
        {
            let name = icons.builtin_image(project).ok_or(PageError::Fallback(
                "a project icon image which is not a string",
            ))?;
            keys.insert(project.phid.clone(), projects::builtin_file_key(&name)?);
        }
        let wanted: Vec<String> = keys.values().cloned().collect();
        let builtins = phab_app_files::load_builtins(database, &wanted).await?;
        for (project, key) in keys {
            // PHP writes a builtin file which doesn't exist yet.
            let file = builtins
                .get(&key)
                .ok_or(PageError::Fallback("a builtin image PHP would create"))?;
            images.insert(project, file.best_uri(config));
        }
        Ok(images)
    }
}

/// A user's result (`PhabricatorPeopleDatasource`).
fn user_item(user: &User) -> Result<Item, PageError> {
    let closed = if user.is_disabled {
        Some("Disabled")
    } else if user.is_system_agent {
        Some("Bot")
    } else if user.is_mailing_list {
        Some("Mailing List")
    } else {
        None
    };
    let name = if user.real_name.is_empty() {
        user.user_name.clone()
    } else {
        format!("{} ({})", user.user_name, user.real_name)
    };
    check_case(&name)?;
    Ok(Item {
        name,
        uri: Some(format!("/p/{}/", user.user_name)),
        phid: user.phid.as_str().to_owned(),
        priority_string: Some(user.user_name.clone()),
        priority_type: Some("user"),
        icon: user.is_mailing_list.then(|| json!("fa-envelope-o")),
        default_icon: Some("fa-user bluegrey"),
        closed,
        autocomplete: Some(format!("@{}", user.user_name)),
        ..Item::default()
    })
}

/// `didLoadResults()` of a datasource.
fn did_load(source: &Source, items: Vec<Item>) -> Vec<Item> {
    let Source::Composite { blocking: true, .. } = source else {
        return items;
    };
    items
        .into_iter()
        .map(|item| {
            let display = item
                .display_name
                .clone()
                .unwrap_or_else(|| item.name.clone());
            Item {
                color: Some("red".to_owned()),
                token_type: Some("function"),
                icon: Some(json!("fa-asterisk")),
                phid: format!("blocking({})", item.phid),
                display_name: Some(format!("Blocking: {display}")),
                name: format!("{} blocking", item.name),
                ..item
            }
        })
        .collect()
}

/// `filterPhaseResults()`: the results of `phase`, given the composite's
/// prefix.
fn filter_phase(phase: Phase, items: Vec<Item>, prefix: Option<&str>) -> Vec<Item> {
    items
        .into_iter()
        .filter_map(|mut item| {
            let result_phase = match prefix {
                Some(prefix) if !prefix.is_empty() && lowercase(&item.name).starts_with(prefix) => {
                    Phase::Prefix
                }
                _ => Phase::Content,
            };
            (result_phase == phase).then(|| {
                item.phase = Some(result_phase);
                item
            })
        })
        .collect()
}

/// The request's parameters, as `AphrontRequest` merges them: the form
/// body wins over the query string.
fn parameters(query: Option<&str>, body: &[u8]) -> HashMap<String, String> {
    let mut parameters: HashMap<String, String> =
        form_urlencoded::parse(query.unwrap_or("").as_bytes())
            .into_owned()
            .collect();
    parameters.extend(form_urlencoded::parse(body).into_owned());
    parameters
}

/// `GET` or `POST /typeahead/class/<class>/`.
pub(crate) async fn class<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(class): Path<String>,
    RawQuery(query): RawQuery,
    request: HeaderMap,
    body: Bytes,
) -> Result<Response, PageError> {
    let parameters = parameters(query.as_deref(), &body);
    let present = |name: &str| parameters.contains_key(name);
    if !present("__ajax__") || present("__quicksand__") {
        return Err(PageError::Fallback("the typeahead debug page"));
    }
    if parameters
        .get("select")
        .is_some_and(|select| !select.is_empty())
    {
        return Err(PageError::Fallback("a typeahead selection"));
    }
    viewer
        .require_application(&*state, "PhabricatorTypeaheadApplication")
        .await?;
    let source_parameters = match parameters.get("parameters").filter(|text| !text.is_empty()) {
        None => serde_json::Map::new(),
        Some(text) => match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(map)) => map,
            Ok(Value::Array(list)) if list.is_empty() => serde_json::Map::new(),
            _ => return Err(PageError::Fallback("unusual typeahead parameters")),
        },
    };
    let source = Source::for_class(&class, &source_parameters)
        .ok_or(PageError::Fallback("a datasource not ported"))?;
    let query = parameters.get("q").cloned();
    // `nonempty($request->getStr('raw'), $query)`.
    let raw = parameters
        .get("raw")
        .filter(|raw| !raw.is_empty() && *raw != "0")
        .cloned()
        .or(query);
    if raw.as_deref().is_some_and(|raw| raw.contains('(')) {
        return Err(PageError::Fallback("a typeahead function"));
    }
    if let Some(raw) = &raw {
        check_case(raw)?;
    }

    let classes = [
        "PhabricatorPeopleApplication",
        "PhabricatorProjectApplication",
        "PhabricatorDifferentialApplication",
    ];
    let visible = viewer.can_see_applications(&*state, &classes).await?;
    let run = Run {
        state: state.as_ref(),
        viewer: &viewer,
        raw,
        applications: classes.into_iter().zip(visible).collect(),
    };

    // `PhabricatorTypeaheadRuntimeCompositeDatasource`: one source, with
    // the limit of the controller. Its prefix stays set once the prefix
    // phase ran.
    let runtime = Source::Composite {
        blocking: false,
        application: None,
        children: vec![source],
    };
    let limit = LIMIT + 1;
    let mut phases = Vec::new();
    let has_raw = run.raw.as_deref().is_some_and(|raw| !raw.is_empty());
    if has_raw {
        phases.push(Phase::Prefix);
    }
    phases.push(Phase::Content);
    let prefix = has_raw.then(|| run.prefix_query());
    let mut results: Vec<Item> = Vec::new();
    for phase in phases {
        let phase_limit = limit - results.len();
        results.extend(
            run.load(&runtime, phase, phase_limit, limit, prefix.as_deref())
                .await?,
        );
        if results.len() >= limit {
            break;
        }
    }

    let payload: Vec<Value> = results.iter().map(Item::wire_format).collect();
    let body = format!(
        "for (;;);{}",
        phab_conduit::json::encode(&json!({"error": null, "payload": payload}))
    );
    Ok(ajax_response(state.as_ref(), &request, body))
}

/// An `AphrontAjaxResponse`.
fn ajax_response<S: WebState>(state: &S, request: &HeaderMap, body: String) -> Response {
    let mut response = Response::new(Body::empty());
    let headers = response.headers_mut();
    php_headers::base_headers(headers, state.config(), request);
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=UTF-8"),
    );
    php_headers::cache_headers(headers, &Options::default(), state.now());
    let body = php_headers::compress_output(headers, request, body.into_bytes());
    *response.body_mut() = Body::from(body);
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_are_tokenized_like_php() {
        assert_eq!(tokenize("  Foo-bar [baz] foo "), ["foo", "bar", "baz"]);
        assert_eq!(tokenize(" - "), Vec::<String>::new());
        assert_eq!(
            longest_tokens(tokenize("a bbb cc dddd e ffff")),
            ["dddd", "ffff", "bbb", "cc", "a"]
        );
    }

    #[test]
    fn wire_formats_drop_trailing_nulls() {
        let item = Item {
            name: "alice (Alice)".to_owned(),
            uri: Some("/p/alice/".to_owned()),
            phid: "PHID-USER-alice".to_owned(),
            priority_string: Some("alice".to_owned()),
            priority_type: Some("user"),
            default_icon: Some("fa-user bluegrey"),
            autocomplete: Some("@alice".to_owned()),
            phase: Some(Phase::Prefix),
            ..Item::default()
        };
        assert_eq!(
            item.wire_format(),
            json!([
                "alice (Alice)",
                "/p/alice/",
                "PHID-USER-alice",
                "alice",
                null,
                null,
                null,
                "user",
                "fa-user bluegrey",
                null,
                null,
                null,
                null,
                null,
                "@alice",
                "prefix"
            ])
        );
    }

    #[test]
    fn phases_split_on_the_prefix() {
        let item = |name: &str| Item {
            name: name.to_owned(),
            ..Item::default()
        };
        let items = vec![item("Alice"), item("bob")];
        let prefix: Vec<String> = filter_phase(Phase::Prefix, items.clone(), Some("al"))
            .into_iter()
            .map(|item| item.name)
            .collect();
        assert_eq!(prefix, ["Alice"]);
        assert_eq!(filter_phase(Phase::Content, items, None).len(), 2);
    }
}
