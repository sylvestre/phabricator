//! `/feed/`: recent activity (`PhabricatorFeedListController`), its
//! builtin queries (`/feed/query/<key>/`), one story (`/feed/<key>/`,
//! `PhabricatorFeedDetailController`), and the story lists other pages
//! embed (the profile's recent activity).

use std::collections::HashMap;
use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, RawQuery, State};
use axum::http::Uri;
use phab_app_differential::title::{Part, Title};
use phab_app_feed::listing::{self, Cursor, Listing, Query};
use phab_app_feed::story::{StoryError, VisibleStory};
use phab_core::Phid;
use phab_policy::application;

use crate::error::PageError;
use crate::harbormaster::{self, Handle};
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::system::catalog;
use crate::time;
use crate::viewer::WebUser;

/// `PhabricatorFeedSearchEngine` shows this many stories a page.
const PAGE_SIZE: usize = 100;

/// A story as the templates show it.
pub(crate) struct StoryItem {
    /// The title, as HTML.
    pub(crate) title_html: String,
    /// The body (`getBodyForFeed()` of its transactions), as HTML.
    pub(crate) body_html: Option<String>,
    /// When it happened.
    pub(crate) date: String,
}

/// `htmlspecialchars()`.
fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#039;"),
            other => escaped.push(other),
        }
    }
    escaped
}

fn handle_html(phid: &Phid, handles: &Handles) -> String {
    handles
        .objects
        .get(phid)
        .map(Handle::link_html)
        .unwrap_or_default()
}

/// The handles of the titles of a listing, as the viewer sees them.
#[derive(Debug, Default)]
pub(crate) struct Handles {
    objects: HashMap<Phid, Handle>,
    /// `getFullName()` of the users which policies name, "username (Real
    /// Name)", which user handles don't keep.
    user_full_names: HashMap<Phid, String>,
}

impl From<HashMap<Phid, Handle>> for Handles {
    fn from(objects: HashMap<Phid, Handle>) -> Self {
        Self {
            objects,
            user_full_names: HashMap::new(),
        }
    }
}

/// A title as HTML: handles link to their objects, restricted ones don't.
pub(crate) fn title_html(title: &Title, handles: &Handles) -> String {
    let mut html = String::new();
    for part in &title.0 {
        match part {
            Part::Text(text) => html.push_str(&escape(text)),
            Part::Handle(phid) => html.push_str(&handle_html(phid, handles)),
            Part::HandleList(list) => {
                let links: Vec<String> =
                    list.iter().map(|phid| handle_html(phid, handles)).collect();
                html.push_str(&links.join(", "));
            }
            Part::Value(value) => {
                html.push_str("<span class=\"title-value\">");
                html.push_str(&escape(value));
                html.push_str("</span>");
            }
            Part::Capability(capability) => {
                html.push_str("<span class=\"title-value\">");
                html.push_str(&escape(capability_label(capability).unwrap_or_default()));
                html.push_str("</span>");
            }
            Part::Policy {
                policy,
                transaction,
                mode,
            } => {
                html.push_str("<span class=\"title-value\">");
                html.push_str(&policy_html(policy, transaction, mode, handles));
                html.push_str("</span>");
            }
        }
    }
    html
}

/// `PhabricatorApplication::getCapabilityLabel()`: the label of an
/// application capability, `None` for a capability which doesn't exist
/// (which PHP renders as nothing).
fn capability_label(capability: &str) -> Option<&'static str> {
    match capability {
        "view" => Some("Can Use Application"),
        "edit" => Some("Can Configure Application"),
        _ => catalog::applications()
            .iter()
            .flat_map(|application| &application.capabilities)
            .find(|custom| custom.key == capability)
            .and_then(|custom| custom.label.as_deref()),
    }
}

/// Whether [`policy_html()`] renders `policy`: a global policy, or a user,
/// project or custom policy. Other object policies are masked, which isn't
/// ported.
fn is_rendered_policy(policy: &str) -> bool {
    matches!(policy, "public" | "users" | "admin" | "no-one")
        || matches!(
            policy.get(..10),
            Some("PHID-USER-" | "PHID-PROJ-" | "PHID-PLCY-")
        )
}

/// `PhabricatorPolicyRef::newTransactionLink()`: a global policy's name, a
/// link to a user or project (with a hint), or a link to the dialog
/// describing a custom policy.
fn policy_html(policy: &str, transaction: &Phid, mode: &str, handles: &Handles) -> String {
    let link = |name: &str, uri: Option<&str>| match uri {
        Some(uri) => format!("<a href=\"{}\">{}</a>", escape(uri), escape(name)),
        None => escape(name),
    };
    match policy {
        "public" => escape("Public (No Login Required)"),
        "users" => escape("All Users"),
        "admin" => escape("Administrators"),
        "no-one" => escape("No One"),
        _ if policy.starts_with("PHID-PLCY-") => link(
            "Custom Policy",
            Some(&format!("/transactions/{mode}/{}/", transaction.as_str())),
        ),
        _ => {
            let phid = Phid::new(policy);
            let handle = handles.objects.get(&phid);
            let uri = handle.and_then(|handle| handle.uri.as_deref());
            if policy.starts_with("PHID-PROJ-") {
                let name = handle.map_or("", |handle| handle.name.as_str());
                format!("{} (Project)", link(name, uri))
            } else {
                // A user who doesn't exist keeps the handle's name.
                let name = handles
                    .user_full_names
                    .get(&phid)
                    .map(String::as_str)
                    .or(handle.map(|handle| handle.full_name.as_str()));
                link(name.unwrap_or(""), uri)
            }
        }
    }
}

/// The bodies of a story's transactions which have one: the summary of a
/// commit (`PhabricatorAuditTransaction::getBodyForFeed()`, with
/// `renderSummary()`). Comments aren't shown.
fn body_html(story: &VisibleStory) -> Option<String> {
    let bodies: Vec<String> = story
        .transactions
        .iter()
        .filter(|(transaction, _)| transaction.transaction_type == "audit:commit")
        .filter_map(|(transaction, _)| {
            let summary = phab_core::php::string_cast(transaction.new_value.get("summary")?);
            let summary = phab_core::text::truncate(&summary, None, Some(128));
            phab_core::php::is_truthy_str(&summary)
                .then(|| escape(&summary).replace('\n', "<br />"))
        })
        .collect();
    (!bodies.is_empty()).then(|| bodies.join(""))
}

pub(crate) fn page_error(error: StoryError) -> PageError {
    match error {
        StoryError::Database(error) => error.into(),
        other => PageError::Internal(Box::new(other)),
    }
}

/// Leaves out the stories whose titles name objects whose handles aren't
/// ported, and loads the handles of the others.
pub(crate) async fn resolve<S: WebState>(
    state: &S,
    viewer: &WebUser,
    listing: &mut Listing,
) -> Result<Handles, PageError> {
    // `PhabricatorApplicationQuery` loads no application of a class which
    // doesn't exist, which hides the story.
    listing.entries.retain(|entry| {
        let object = entry.story.object.as_str();
        object
            .strip_prefix("PHID-APPS-")
            .is_none_or(|class| catalog::application(class).is_some())
    });
    listing.retain_handles(harbormaster::is_supported);
    listing.retain_titles(|title| {
        title.0.iter().all(|part| match part {
            Part::Policy { policy, .. } => is_rendered_policy(policy),
            Part::Capability(capability) => capability_label(capability).is_some(),
            _ => true,
        })
    });
    let scope = harbormaster::Scope::new(state, viewer);
    let mut handles: Handles = harbormaster::load_handles(state, &scope, &listing.handle_phids())
        .await?
        .into();
    let mut users: Vec<Phid> = listing
        .entries
        .iter()
        .flat_map(|entry| &entry.title.0)
        .filter_map(|part| match part {
            Part::Policy { policy, .. } if policy.starts_with("PHID-USER-") => {
                Some(Phid::new(policy.as_str()))
            }
            _ => None,
        })
        .collect();
    users.sort();
    users.dedup();
    if !users.is_empty() {
        for user in phab_auth::user::load_users_by_phid(state.database(), &users).await? {
            let full_name = if user.real_name.is_empty() {
                user.user_name
            } else {
                format!("{} ({})", user.user_name, user.real_name)
            };
            handles.user_full_names.insert(user.phid, full_name);
        }
    }
    Ok(handles)
}

/// The stories of a listing, ready for a template.
pub(crate) fn items(listing: &Listing, handles: &Handles) -> Vec<StoryItem> {
    listing
        .entries
        .iter()
        .map(|entry| StoryItem {
            title_html: title_html(&entry.title, handles),
            body_html: body_html(&entry.story),
            date: time::format_utc(i64::try_from(entry.story.row.epoch()).unwrap_or(i64::MAX)),
        })
        .collect()
}

/// Loads a page of the feed for `viewer`, with the handles its titles
/// name.
pub(crate) async fn load<S: WebState>(
    state: &S,
    viewer: &WebUser,
    query: Query<'_>,
) -> Result<(Listing, Handles), PageError> {
    let mut listing = listing::list(
        state.database(),
        state.config(),
        &viewer.user.viewer(),
        query,
        state.now(),
    )
    .await
    .map_err(page_error)?;
    let handles = resolve(state, viewer, &mut listing).await?;
    Ok((listing, handles))
}

#[derive(Template)]
#[template(path = "feed.html")]
pub(crate) struct Feed {
    shell: Shell,
    heading: &'static str,
    /// The builtin queries: name, link, and whether the page shows it.
    queries: Vec<(&'static str, &'static str, bool)>,
    stories: Vec<StoryItem>,
    skipped: usize,
    older: Option<String>,
    newer: Option<String>,
}

/// The parameters of `/feed/`: `after`, `before` and `userPHIDs`.
#[derive(Debug, Default, PartialEq, Eq)]
struct Parameters {
    cursor: Cursor,
    users: Vec<Phid>,
}

fn parameters(query: Option<&str>) -> Parameters {
    let mut parameters = Parameters::default();
    for (key, value) in form_urlencoded::parse(query.unwrap_or("").as_bytes()) {
        match key.as_ref() {
            "after" => {
                if let Ok(key) = value.parse() {
                    parameters.cursor = Cursor::After(key);
                }
            }
            "before" => {
                if let Ok(key) = value.parse() {
                    parameters.cursor = Cursor::Before(key);
                }
            }
            "userPHIDs" | "userPHIDs[]" => parameters.users.extend(
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|phid| !phid.is_empty())
                    .map(Phid::new),
            ),
            _ => {}
        }
    }
    parameters
}

/// The link to another page of the same listing, at `path`.
pub(crate) fn page_link(path: &str, cursor: &str, key: u64, users: &[Phid]) -> String {
    let mut link = format!("{path}?{cursor}={key}");
    if !users.is_empty() {
        let users: Vec<&str> = users.iter().map(Phid::as_str).collect();
        link.push_str("&userPHIDs=");
        link.extend(form_urlencoded::byte_serialize(users.join(",").as_bytes()));
    }
    link
}

/// The builtin queries of `PhabricatorFeedSearchEngine` for a logged-in
/// viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Builtin {
    /// `all`: every story.
    All,
    /// `projects`: the stories about the viewer's projects.
    Projects,
}

impl Builtin {
    const ALL: [Self; 2] = [Self::All, Self::Projects];

    fn from_key(key: &str) -> Option<Self> {
        match key {
            "all" => Some(Self::All),
            "projects" => Some(Self::Projects),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::All => "All Stories",
            Self::Projects => "Tags",
        }
    }

    fn uri(self) -> &'static str {
        match self {
            Self::All => "/feed/query/all/",
            Self::Projects => "/feed/query/projects/",
        }
    }
}

/// The projects the viewer is a member of and may see
/// (`withMemberPHIDs()`), for the `projects` query.
async fn viewer_projects<S: WebState>(state: &S, viewer: &WebUser) -> Result<Vec<Phid>, PageError> {
    let projects =
        phab_app_project::storage::load_member_projects(state.database(), &viewer.user.phid)
            .await?;
    let (projects, _) = phab_app_project::policy::visible_projects(
        state.database(),
        state.config(),
        &viewer.user.viewer(),
        projects,
        state.now(),
    )
    .await
    .map_err(|error| PageError::Internal(Box::new(error)))?;
    Ok(projects.into_iter().map(|project| project.phid).collect())
}

/// A page of the feed at `path`: the stories referencing `references`
/// (all of them when empty, as `if ($phids)` is false).
async fn render<S: WebState>(
    state: &S,
    viewer: &WebUser,
    (path, builtin): (&str, Option<Builtin>),
    cursor: Cursor,
    references: &[Phid],
    uri: &Uri,
) -> Result<Page<Feed>, PageError> {
    let (listing, handles) = load(
        state,
        viewer,
        Query {
            cursor,
            limit: PAGE_SIZE,
            references: (!references.is_empty()).then_some(references),
        },
    )
    .await?;
    // Only `/feed/` carries the users of its query string to other pages.
    let users = if builtin.is_some() {
        &[][..]
    } else {
        references
    };
    let key = |entry: Option<&listing::Entry>| entry.map(|entry| entry.story.row.chronological_key);
    let older = key(listing.entries.last())
        .filter(|_| listing.has_older)
        .map(|key| page_link(path, "after", key, users));
    let newer = key(listing.entries.first())
        .filter(|_| listing.has_newer)
        .map(|key| page_link(path, "before", key, users));

    let current = uri.path_and_query().map_or("/feed/", |path| path.as_str());
    Ok(Page(Feed {
        shell: Shell::for_user(state.config(), viewer, "Feed", current),
        heading: builtin.map_or("Feed", Builtin::name),
        queries: Builtin::ALL
            .iter()
            .map(|query| (query.name(), query.uri(), builtin == Some(*query)))
            .collect(),
        stories: items(&listing, &handles),
        skipped: listing.skipped,
        older,
        newer,
    }))
}

/// `GET /feed/`.
pub(crate) async fn show<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<Feed>, PageError> {
    viewer
        .require_application(&*state, application::FEED)
        .await?;
    let parameters = parameters(query.as_deref());
    render(
        state.as_ref(),
        &viewer,
        ("/feed/", None),
        parameters.cursor,
        &parameters.users,
        &uri,
    )
    .await
}

/// `GET /feed/query/{key}/`: the builtin queries. Saved queries go to PHP.
pub(crate) async fn query<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(key): Path<String>,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<Feed>, PageError> {
    viewer
        .require_application(&*state, application::FEED)
        .await?;
    let builtin = Builtin::from_key(&key).ok_or(PageError::Fallback("a saved feed query"))?;
    let references = match builtin {
        Builtin::All => Vec::new(),
        Builtin::Projects => viewer_projects(state.as_ref(), &viewer).await?,
    };
    // A saved query ignores the query string, but for the pager.
    let cursor = parameters(query.as_deref()).cursor;
    render(
        state.as_ref(),
        &viewer,
        (builtin.uri(), Some(builtin)),
        cursor,
        &references,
        &uri,
    )
    .await
}

#[derive(Template)]
#[template(path = "feed_story.html")]
pub(crate) struct FeedStory {
    shell: Shell,
    stories: Vec<StoryItem>,
}

/// `GET /feed/{key}/` (`PhabricatorFeedDetailController`): one story, by
/// chronological key.
pub(crate) async fn detail<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(key): Path<String>,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<FeedStory>, PageError> {
    viewer
        .require_application(&*state, application::FEED)
        .await?;
    // `?text=1` answers the story as plain text.
    let text = form_urlencoded::parse(query.as_deref().unwrap_or("").as_bytes())
        .any(|(name, value)| name == "text" && phab_core::php::is_truthy_str(&value));
    if text {
        return Err(PageError::Fallback("a feed story as text"));
    }
    // `%Ls` quotes the key: anything but digits matches no row.
    let key: u64 = key.parse().map_err(|_| PageError::NotFound)?;
    let mut listing = listing::story(
        state.database(),
        state.config(),
        &viewer.user.viewer(),
        key,
        state.now(),
    )
    .await
    .map_err(page_error)?;
    let handles = resolve(state.as_ref(), &viewer, &mut listing).await?;
    if listing.skipped > 0 {
        return Err(PageError::Fallback("a feed story Rust can't show"));
    }
    if listing.entries.is_empty() {
        return Err(PageError::NotFound);
    }
    let current = uri.path_and_query().map_or("/feed/", |path| path.as_str());
    Ok(Page(FeedStory {
        shell: Shell::for_user(state.config(), &viewer, "Story", current),
        stories: items(&listing, &handles),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_link_visible_handles_only() {
        let handles = HashMap::from([
            (
                Phid::new("PHID-USER-alice"),
                Handle {
                    name: "alice".to_owned(),
                    full_name: "alice".to_owned(),
                    uri: Some("/p/alice/".to_owned()),
                    closed: false,
                },
            ),
            (
                Phid::new("PHID-DREV-1"),
                Handle {
                    name: "Restricted Differential Revision".to_owned(),
                    full_name: "Restricted Differential Revision".to_owned(),
                    uri: None,
                    closed: false,
                },
            ),
        ]);
        let title = Title(vec![
            Part::Handle(Phid::new("PHID-USER-alice")),
            Part::Text(" retitled ".to_owned()),
            Part::Handle(Phid::new("PHID-DREV-1")),
            Part::Text(" to ".to_owned()),
            Part::Value("<b>".to_owned()),
            Part::Text(".".to_owned()),
        ]);
        assert_eq!(
            title_html(&title, &handles.into()),
            "<a href=\"/p/alice/\">alice</a> retitled \
             <span class=\"handle-restricted\">Restricted Differential Revision</span> to \
             <span class=\"title-value\">&lt;b&gt;</span>."
        );
    }

    #[test]
    fn parameters_follow_php() {
        assert_eq!(
            parameters(Some("after=12&userPHIDs=PHID-USER-a,PHID-USER-b")),
            Parameters {
                cursor: Cursor::After(12),
                users: vec![Phid::new("PHID-USER-a"), Phid::new("PHID-USER-b")],
            }
        );
        assert_eq!(parameters(Some("before=x")), Parameters::default());
        assert_eq!(
            page_link("/feed/", "before", 3, &[Phid::new("PHID-USER-a")]),
            "/feed/?before=3&userPHIDs=PHID-USER-a"
        );
    }
}
