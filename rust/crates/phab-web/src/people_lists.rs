//! The people list (`/people/`, `/people/query/<key>/`,
//! `PhabricatorPeopleListController`: the builtin queries Active and All
//! through `user.search`) and a user's "Manage" page (`/people/manage/<id>/`,
//! `PhabricatorPeopleProfileManageController`), read-only: its actions link
//! to PHP's workflows. The administrators' approval queue, saved queries
//! and search parameters stay with PHP, and so do timelines with
//! transactions whose titles aren't ported (custom fields) or comments.

use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, RawQuery, State};
use axum::http::Uri;
use phab_app_dashboard::storage::{self as dashboard, Transaction};
use phab_app_differential::storage as differential;
use phab_app_people::profile;
use phab_conduit::Method;
use phab_core::{Phid, PhidType};
use phab_policy::application;
use phab_remarkup::{Markup, Mode};
use serde_json::{Map, Value, json};

use crate::error::PageError;
use crate::harbormaster::{self, Action, Scope};
use crate::response::Page;
use crate::shell::Shell;
use crate::state::WebState;
use crate::timeline::Entry;
use crate::viewer::WebUser;
use crate::{remarkup, time};

fn current(uri: &Uri) -> &str {
    uri.path_and_query().map_or("/", |path| path.as_str())
}

/// `PhabricatorApplicationSearchEngine` shows this many results a page.
const PAGE_SIZE: usize = 100;
/// `PhabricatorPeopleSearchEngine`.
const ENGINE: &str = "PhabricatorPeopleSearchEngine";

/// A user of the list.
pub(crate) struct UserItem {
    user_name: String,
    real_name: String,
    roles: Vec<&'static str>,
    disabled: bool,
    since: String,
}

#[derive(Template)]
#[template(path = "people_list.html")]
pub(crate) struct PeopleList {
    shell: Shell,
    query: &'static str,
    title: &'static str,
    is_admin: bool,
    items: Vec<UserItem>,
    next: Option<String>,
}

/// The labels of `user.search`'s roles which the list shows
/// (`PhabricatorPeopleUserFunctionDatasource` and the list view's icons).
fn role_label(role: &str) -> Option<&'static str> {
    Some(match role {
        "admin" => "Administrator",
        "disabled" => "Disabled",
        "bot" => "Bot",
        "list" => "Mailing List",
        _ => return None,
    })
}

async fn people_list<S: WebState>(
    state: &S,
    viewer: &WebUser,
    key: &str,
    query: Option<&str>,
    uri: &Uri,
) -> Result<Page<PeopleList>, PageError> {
    let (key, title, constraints) = match key {
        "active" => ("active", "Active", json!({"isDisabled": false})),
        "all" => ("all", "All", json!({})),
        // The approval queue is for administrators.
        _ => return Err(PageError::Fallback("a saved search query")),
    };
    let after = crate::herald::cursor(query)?;
    let mut params = Map::new();
    params.insert("constraints".to_owned(), constraints);
    params.insert("limit".to_owned(), json!(PAGE_SIZE));
    if let Some(after) = after {
        params.insert("after".to_owned(), json!(after.to_string()));
    }
    let scope = Scope::new(state, viewer);
    let request = scope.request_with(params);
    let result = phab_app_people::search::UserSearch
        .execute(&request)
        .await
        .map_err(harbormaster::method_error)?;
    let text = |value: &Value| value.as_str().unwrap_or_default().to_owned();
    let items = result["data"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|user| {
            let fields = &user["fields"];
            let roles: Vec<&str> = fields["roles"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            UserItem {
                user_name: text(&fields["username"]),
                real_name: text(&fields["realName"]),
                disabled: roles.contains(&"disabled"),
                roles: roles.into_iter().filter_map(role_label).collect(),
                since: time::format_utc(fields["dateCreated"].as_i64().unwrap_or_default()),
            }
        })
        .collect();
    let next = result["cursor"]["after"].as_str().map(str::to_owned);
    Ok(Page(PeopleList {
        shell: Shell::for_user(state.config(), viewer, "People", current(uri)),
        query: key,
        title,
        is_admin: viewer.user.is_admin,
        items,
        next,
    }))
}

/// `GET /people/`: the default query, Active, unless the viewer saved or
/// reordered queries.
pub(crate) async fn list<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<PeopleList>, PageError> {
    viewer
        .require_application(&*state, application::PEOPLE)
        .await?;
    if phab_app_harbormaster::web::has_named_queries(state.database(), &viewer.user.phid, ENGINE)
        .await?
    {
        return Err(PageError::Fallback("saved search queries"));
    }
    people_list(state.as_ref(), &viewer, "active", query.as_deref(), &uri).await
}

/// `GET /people/query/{key}/`: a builtin query.
pub(crate) async fn query<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(key): Path<String>,
    RawQuery(query): RawQuery,
    uri: Uri,
) -> Result<Page<PeopleList>, PageError> {
    viewer
        .require_application(&*state, application::PEOPLE)
        .await?;
    people_list(state.as_ref(), &viewer, &key, query.as_deref(), &uri).await
}

#[derive(Template)]
#[template(path = "people_manage.html")]
pub(crate) struct ManagePage {
    shell: Shell,
    user_id: i64,
    user_name: String,
    real_name: String,
    title: String,
    icon: Option<&'static str>,
    roles: Vec<&'static str>,
    since: Option<String>,
    blurb: Option<Markup>,
    timeline: Vec<Entry>,
    actions: Vec<Action>,
}

impl ManagePage {
    fn blurb_html(&self) -> &str {
        self.blurb.as_ref().map_or("", |blurb| {
            std::str::from_utf8(blurb.as_bytes()).unwrap_or("")
        })
    }
}

/// The titles of the user transactions (`PhabricatorUserTransactionType`).
fn title(transaction: &Transaction, author: &str) -> Result<String, PageError> {
    let new = phab_core::php::is_truthy(&transaction.new_value);
    let quoted = |value: &Value| match value {
        Value::String(text) => Ok(format!("\"{}\"", crate::diff_html::escape(text))),
        _ => Err(PageError::Fallback("an unusual user transaction")),
    };
    Ok(match transaction.transaction_type.as_str() {
        "user.approve" if new => format!("{author} approved this user."),
        "user.approve" => format!("{author} rejected this user."),
        "user.disable" if new => format!("{author} disabled this user."),
        "user.disable" => format!("{author} enabled this user."),
        "user.admin" if new => format!("{author} empowered this user as an administrator."),
        "user.admin" => format!("{author} defrocked this user."),
        "notify" => format!("{author} sent this user a test notification."),
        "user.rename" => format!(
            "{author} renamed this user from {} to {}.",
            quoted(&transaction.old_value)?,
            quoted(&transaction.new_value)?
        ),
        _ => return Err(PageError::Fallback("a user transaction not ported")),
    })
}

/// The timeline of the user: the transactions the viewer may see.
async fn timeline<S: WebState>(
    state: &S,
    viewer: &WebUser,
    user: &Phid,
) -> Result<Vec<Entry>, PageError> {
    let database = state.database();
    let transactions =
        dashboard::object_transactions(database, ("user", "user_transaction"), user).await?;
    let subjects: Vec<_> = transactions.iter().map(Transaction::subject).collect();
    let visible = differential::visible_subjects(
        database,
        state.config(),
        &viewer.user.viewer(),
        &subjects,
        state.now(),
    )
    .await
    .map_err(harbormaster::internal)?;
    let shown: Vec<&Transaction> = transactions
        .iter()
        .zip(visible)
        .filter_map(|(transaction, visible)| visible.then_some(transaction))
        .collect();
    // The timeline pages its transactions, and comments are not ported.
    if shown.len() > 100 {
        return Err(PageError::Fallback("a long user timeline"));
    }
    if shown
        .iter()
        .any(|transaction| transaction.comment_phid.is_some())
    {
        return Err(PageError::Fallback("a comment in a user timeline"));
    }
    let authors: Vec<Phid> = shown
        .iter()
        .map(|transaction| transaction.author_phid.clone())
        .collect();
    if authors
        .iter()
        .any(|author| author.phid_type() != PhidType::USER)
    {
        return Err(PageError::Fallback("a transaction by an application"));
    }
    let scope = Scope::new(state, viewer);
    let handles = harbormaster::load_handles(state, &scope, &authors).await?;
    shown
        .into_iter()
        .map(|transaction| {
            let author = harbormaster::handle(&handles, &transaction.author_phid).short_link_html();
            Ok(Entry {
                title_html: title(transaction, &author)?,
                date: time::format_utc(transaction.date_created),
                comment_html: None,
            })
        })
        .collect()
}

/// The curtain's actions (`buildCurtain()`), which link to PHP.
fn actions(viewer: &WebUser, user: &phab_auth::User, can_disable: bool) -> Vec<Action> {
    let id = user.id;
    let is_self = user.phid == viewer.user.phid;
    let is_admin = viewer.user.is_admin;
    let can_admin = is_admin && !is_self;
    // Users can edit themselves; administrators edit bots and lists.
    let can_edit = is_self || (is_admin && (user.is_system_agent || user.is_mailing_list));
    let mut actions = vec![
        Action {
            name: "Edit Profile",
            icon: "fa-pencil",
            uri: format!("/people/editprofile/{id}/"),
            enabled: can_edit,
        },
        Action {
            name: "Edit Profile Picture",
            icon: "fa-picture-o",
            uri: format!("/people/picture/{id}/"),
            enabled: can_edit,
        },
        Action {
            name: "Edit Settings",
            icon: "fa-wrench",
            uri: format!("/settings/user/{}/", user.user_name),
            enabled: can_edit,
        },
        if user.is_admin {
            Action {
                name: "Remove Administrator",
                icon: "fa-arrow-circle-o-down",
                uri: format!("/people/empower/{id}/"),
                enabled: can_admin,
            }
        } else {
            Action {
                name: "Make Administrator",
                icon: "fa-arrow-circle-o-up",
                uri: format!("/people/empower/{id}/"),
                enabled: can_admin,
            }
        },
        Action {
            name: "Change Username",
            icon: "fa-tag",
            uri: format!("/people/rename/{id}/"),
            enabled: is_admin,
        },
        Action {
            name: "Send Welcome Email",
            icon: "fa-envelope",
            uri: format!("/people/welcome/{id}/"),
            enabled: is_admin,
        },
    ];
    if !user.is_approved {
        actions.push(Action {
            name: "Approve User",
            icon: "fa-thumbs-up",
            uri: format!("/people/approve/{id}/via/profile/"),
            enabled: is_admin,
        });
    }
    actions.push(if user.is_disabled {
        Action {
            name: "Enable User",
            icon: "fa-check-circle-o",
            uri: format!("/people/disable/{id}/"),
            enabled: can_disable && !is_self,
        }
    } else {
        Action {
            name: "Disable User",
            icon: "fa-ban",
            uri: format!("/people/disable/{id}/"),
            enabled: can_disable && !is_self,
        }
    });
    actions.push(Action {
        name: "Delete User",
        icon: "fa-times",
        uri: format!("/people/delete/{id}/"),
        enabled: can_admin,
    });
    actions
}

/// `GET /people/manage/{id}/`.
pub(crate) async fn manage<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<i64>,
    uri: Uri,
) -> Result<Page<ManagePage>, PageError> {
    viewer
        .require_application(&*state, application::PEOPLE)
        .await?;
    let state = state.as_ref();
    let database = state.database();
    let user = phab_auth::user::load_user_by_id(database, id)
        .await?
        .ok_or(PageError::NotFound)?;
    let as_viewer = viewer.user.viewer();
    let user_profile = profile::load_profile(database, &user.phid).await?;
    let blurb = if user_profile.blurb.is_empty() {
        None
    } else {
        Some(remarkup::render(state, &as_viewer, &user_profile.blurb, Mode::Html).await?)
    };
    let since = profile::date_created(database, &user.phid)
        .await?
        .map(time::format_utc);
    let timeline = timeline(state, &viewer, &user.phid).await?;
    let scope = Scope::new(state, &viewer);
    let can_disable = phab_app_people::can_disable_users(&scope.request())
        .await
        .map_err(harbormaster::method_error)?;
    let actions = actions(&viewer, &user, can_disable);
    Ok(Page(ManagePage {
        shell: Shell::for_user(
            state.config(),
            &viewer,
            &format!("Manage User · {}", user.user_name),
            current(&uri),
        ),
        user_id: user.id,
        title: user_profile.display_title().to_owned(),
        icon: user_profile.icon_class(),
        roles: profile::roles(&user),
        since,
        blurb,
        timeline,
        actions,
        user_name: user.user_name,
        real_name: user.real_name,
    }))
}
