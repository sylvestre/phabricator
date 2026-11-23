//! `/mail/detail/<id>/` (`PhabricatorMetaMTAMailViewController`): a mail
//! for its sender and recipients, with its message, headers, delivery and
//! metadata. Mail to projects or lists (whose members PHP expands to decide
//! who may see it), encryption reasons and Herald's routing rules stay
//! with PHP.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::Uri;
use phab_app_metamta::mail::{self, Mail};
use phab_core::Phid;
use serde_json::Value;

use super::{Body, PageBuilder, Property, Section, SystemPage, no_query};
use crate::diff_html::escape;
use crate::error::PageError;
use crate::harbormaster::{self, Handle, Scope};
use crate::response::Page;
use crate::state::WebState;
use crate::time;
use crate::viewer::WebUser;

/// `PhabricatorMetaMTAApplication`.
pub(crate) const APPLICATION: &str = "PhabricatorMetaMTAApplication";

fn unsupported(error: phab_app_metamta::Error) -> PageError {
    match error {
        phab_app_metamta::Error::Db(error) => PageError::Internal(Box::new(error)),
        phab_app_metamta::Error::Unsupported(_) => PageError::Fallback("a mail Rust doesn't read"),
    }
}

/// A list parameter of PHIDs.
fn phids(mail: &Mail, key: &str) -> Result<Vec<Phid>, PageError> {
    Ok(mail
        .strings_param(key)
        .map_err(unsupported)?
        .into_iter()
        .map(Phid::new)
        .collect())
}

/// `PhabricatorMailOutboundStatus::getStatusName()`.
fn status_name(status: &str) -> &'static str {
    match status {
        "queued" => "Queued",
        "fail" => "Delivery Failed",
        "sent" => "Sent",
        "void" => "Voided",
        _ => "Unknown",
    }
}

/// `PhabricatorMetaMTAActor::getReasonName()` and `getReasonDescription()`.
fn reason(code: &str) -> (String, String) {
    let (name, description) = match code {
        "none" => ("None", "No special rules affected this mail."),
        "disabled" => (
            "Disabled Recipient",
            "This user is disabled; disabled users do not receive mail.",
        ),
        "bot" => (
            "Bot Recipient",
            "This user is a bot; bot accounts do not receive mail.",
        ),
        "noaddress" => (
            "No Address",
            "Unable to load an email address for this PHID.",
        ),
        "exernaltype" => (
            "External Recipient",
            "Only external accounts of type \"email\" are deliverable; this account has a \
             different type.",
        ),
        "unmailable" => (
            "Not Mailable",
            "This PHID type does not correspond to a mailable object.",
        ),
        "response" => (
            "Similar Reply",
            "This message is a response to another email message, and this recipient received \
             the original email message, so we are not sending them this substantially similar \
             message (for example, the sender used \"Reply All\" instead of \"Reply\" in \
             response to mail from this server).",
        ),
        "self" => (
            "Self Mail",
            "This recipient is the user whose actions caused delivery of this message, but they \
             have set preferences so they do not receive mail about their own actions (Settings \
             > Email Preferences > Self Actions).",
        ),
        "maildisabled" => (
            "Mail Disabled",
            "This recipient has disabled all email notifications (Settings > Email Preferences \
             > Email Notifications).",
        ),
        "mailtags" => (
            "Mail Tags",
            "This mail has tags which control which users receive it, and this recipient has \
             not elected to receive mail with any of the tags on this message (Settings > Email \
             Preferences).",
        ),
        "unloadable" => ("Bad Recipient", "Unable to load user record for this PHID."),
        "force" => (
            "Forced Mail",
            "Delivery of this mail is forced and ignores deliver preferences. Mail which uses \
             forced delivery is usually related to account management or authentication. For \
             example, password reset email ignores mail preferences.",
        ),
        "force-herald" => (
            "Forced by Herald",
            "This recipient was added by a \"Send me an Email\" rule in Herald, which overrides \
             some delivery settings.",
        ),
        "route-as-notification" => (
            "Route as Notification",
            "This message was downgraded to a notification by outbound mail rules in Herald.",
        ),
        "route-as-mail" => (
            "Route as Mail",
            "This message was upgraded to email by outbound mail rules in Herald.",
        ),
        "unverified" => (
            "Address Not Verified",
            "This recipient does not have a verified primary email address.",
        ),
        "muted" => (
            "Muted",
            "This recipient has muted notifications for this object.",
        ),
        other => {
            return (
                format!("Unknown (\"{other}\")"),
                format!("Unknown Reason (\"{other}\")"),
            );
        }
    };
    (name.to_owned(), description.to_owned())
}

/// `isDeliveryReason()`.
fn is_delivery_reason(code: &str) -> bool {
    matches!(code, "none" | "force" | "force-herald" | "route-as-mail")
}

/// `renderHandleList()`.
fn handle_list(handles: &std::collections::HashMap<Phid, Handle>, phids: &[Phid]) -> String {
    phids
        .iter()
        .map(|phid| harbormaster::handle(handles, phid).link_html())
        .collect::<Vec<_>>()
        .join(", ")
}

/// `GET /mail/detail/<id>/`.
#[allow(clippy::too_many_lines)]
pub(crate) async fn detail<S: WebState>(
    State(state): State<Arc<S>>,
    viewer: WebUser,
    Path(id): Path<String>,
    uri: Uri,
) -> Result<Page<SystemPage>, PageError> {
    viewer.require_application(&*state, APPLICATION).await?;
    no_query(&uri)?;
    let state = state.as_ref();
    let id = super::parse_id(&id)?;
    let mail = mail::load(state.database(), id)
        .await
        .map_err(unsupported)?
        .ok_or(PageError::NotFound)?;

    // The policy is "no one", but for the sender and the recipients
    // (`getExpandedRecipientPHIDs()`).
    let to = phids(&mail, "to")?;
    let cc = phids(&mail, "cc")?;
    let mut recipients: Vec<Phid> = to.iter().chain(&cc).cloned().collect();
    if let Some(actor) = &mail.actor_phid {
        recipients.push(Phid::new(actor.clone()));
    }
    if !recipients.contains(&viewer.user.phid) {
        if recipients
            .iter()
            .any(|phid| phid.phid_type() != phab_core::PhidType::USER)
        {
            return Err(PageError::Fallback("mail to projects or lists"));
        }
        return Err(PageError::NotFound);
    }
    if !phids(&mail, "mustEncryptReasons")?.is_empty() {
        return Err(PageError::Fallback("the encryption reasons of a mail"));
    }

    let sensitive = mail
        .param("sensitive")
        .is_none_or(phab_core::php::is_truthy);
    let title = if sensitive {
        "Content Redacted".to_owned()
    } else {
        mail.string_param("subject")
            .map_err(unsupported)?
            .unwrap_or_default()
            .to_owned()
    };

    let from = mail
        .string_param("from")
        .map_err(unsupported)?
        .map(Phid::new);
    let files: Vec<Phid> = match mail.param("attachments") {
        Some(Value::Array(attachments)) => attachments
            .iter()
            .filter_map(|attachment| attachment.get("filePHID")?.as_str())
            .map(Phid::new)
            .collect(),
        _ => Vec::new(),
    };
    let related = mail.related_phid.as_deref().map(Phid::new);
    let actor = mail.actor_phid.as_deref().map(Phid::new);
    let mut wanted: Vec<Phid> = to.iter().chain(&cc).chain(&files).cloned().collect();
    wanted.extend(from.iter().cloned());
    wanted.extend(related.iter().cloned());
    wanted.extend(actor.iter().cloned());
    let handles = harbormaster::load_handles(state, &Scope::new(state, &viewer), &wanted).await?;

    // Message.
    let mut message = vec![
        Property::html(
            "From",
            from.as_ref().map_or_else(
                || "Sent by Phabricator".to_owned(),
                |from| harbormaster::handle(&handles, from).link_html(),
            ),
        ),
        Property::html(
            "To",
            if to.is_empty() {
                "None".to_owned()
            } else {
                handle_list(&handles, &to)
            },
        ),
        Property::html(
            "Cc",
            if cc.is_empty() {
                "None".to_owned()
            } else {
                handle_list(&handles, &cc)
            },
        ),
        Property::text("Sent", &time::format_utc(mail_date(state, &mail).await?)),
    ];
    let body = if sensitive {
        "<em>The content of this mail is sensitive and it can not be viewed from the web \
         UI.</em>"
            .to_owned()
    } else {
        format!(
            "<pre class=\"remarkup-wrap\">{}</pre>",
            escape(
                mail.string_param("body")
                    .map_err(unsupported)?
                    .unwrap_or("")
            )
        )
    };
    message.push(Property::html("Message", body));
    if !files.is_empty() {
        message.push(Property::html(
            "Attached Files",
            handle_list(&handles, &files),
        ));
    }

    // Headers, sorted by name.
    let mut headers: Vec<(String, String)> = match mail.param("headers.sent") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(list)) => list
            .iter()
            .map(|header| {
                let pair = header.as_array().filter(|pair| pair.len() == 2)?;
                Some((
                    phab_core::php::string_cast(&pair[0]).into_owned(),
                    phab_core::php::string_cast(&pair[1]).into_owned(),
                ))
            })
            .collect::<Option<_>>()
            .ok_or(PageError::Fallback("unusual mail headers"))?,
        Some(_) => return Err(PageError::Fallback("unusual mail headers")),
    };
    headers.sort_by(|left, right| left.0.cmp(&right.0));
    let headers: Vec<Property> = headers
        .iter()
        .map(|(name, value)| Property::text(name.clone(), value))
        .collect();

    // Delivery.
    let mut delivery = Vec::new();
    let queued = mail.status == "queued";
    match mail.param("actors.sent") {
        None | Some(Value::Null) => delivery.push(Property::html(
            "Delivery",
            format!(
                "<em>{}</em>",
                if queued {
                    "This message has not been delivered yet, so delivery information is not \
                     available."
                } else {
                    "This is an older message that predates recording delivery information, \
                     so none is available."
                }
            ),
        )),
        Some(Value::Object(actors)) => match actors.get(viewer.user.phid.as_str()) {
            None => delivery.push(Property::html(
                "Delivery",
                "<em>This message was not delivered to you.</em>".to_owned(),
            )),
            Some(actor) => {
                let deliverable = actor
                    .get("deliverable")
                    .is_some_and(phab_core::php::is_truthy);
                delivery.push(Property::text(
                    "Delivery",
                    if deliverable { "Delivered" } else { "Voided" },
                ));
                let mut codes: Vec<String> = actor
                    .get("reasons")
                    .and_then(Value::as_array)
                    .map(|reasons| {
                        reasons
                            .iter()
                            .map(|code| phab_core::php::string_cast(code).into_owned())
                            .collect()
                    })
                    .unwrap_or_default();
                if codes.is_empty() {
                    codes.push("none".to_owned());
                }
                let items: Vec<String> = codes
                    .iter()
                    .map(|code| {
                        let (name, description) = reason(code);
                        let icon = if is_delivery_reason(code) {
                            "fa-check remarkup-icon-green"
                        } else {
                            "fa-times remarkup-icon-red"
                        };
                        format!(
                            "<li><span class=\"fa {icon}\" aria-hidden=\"true\"></span> \
                             <strong>{}</strong> {}</li>",
                            escape(&name),
                            escape(&description)
                        )
                    })
                    .collect();
                delivery.push(Property::html(
                    "Reasons",
                    format!(
                        "<ul class=\"hm-actions\">{}</ul><p class=\"muted\"><em>Delivery reasons \
                         are listed from weakest to strongest.</em></p>",
                        items.join("")
                    ),
                ));
            }
        },
        Some(_) => return Err(PageError::Fallback("unusual mail delivery data")),
    }
    let routing = match mail.param("routingmap.sent") {
        None | Some(Value::Null) => format!(
            "<em>{}</em>",
            if queued {
                "This message has not been sent yet, so routing rules have not been computed."
            } else {
                "This is an older message which predates routing rules."
            }
        ),
        Some(Value::Object(map))
            if !map.contains_key(viewer.user.phid.as_str()) && !map.contains_key("default") =>
        {
            if mail
                .param("routing.sent")
                .is_some_and(phab_core::php::is_truthy)
            {
                return Err(PageError::Fallback("mail routing rules"));
            }
            "<em>No routing rules applied when delivering this message to you.</em>".to_owned()
        }
        Some(Value::Array(list)) if list.is_empty() => {
            if mail
                .param("routing.sent")
                .is_some_and(phab_core::php::is_truthy)
            {
                return Err(PageError::Fallback("mail routing rules"));
            }
            "<em>No routing rules applied when delivering this message to you.</em>".to_owned()
        }
        Some(_) => return Err(PageError::Fallback("mail routing rules")),
    };
    delivery.push(Property::html("Effective Rule", routing));

    // Metadata.
    let metadata = vec![
        Property::text("Message PHID", &mail.phid),
        match mail
            .message
            .as_deref()
            .filter(|details| !details.is_empty())
        {
            Some(details) => Property::text("Status Details", details),
            None => Property::html("Status Details", "<em>None</em>".to_owned()),
        },
        match &actor {
            Some(actor) => {
                Property::html("Actor", harbormaster::handle(&handles, actor).link_html())
            }
            None => Property::text("Actor", "Generated by Server"),
        },
        match &related {
            Some(related) => Property::html(
                "Related Object",
                harbormaster::handle(&handles, related).link_html(),
            ),
            None => Property::html("Related Object", "<em>None</em>".to_owned()),
        },
    ];

    let mut page = PageBuilder::new(title)
        .icon("fa-envelope")
        .tag(status_name(&mail.status))
        .section(Section::new("Message", Body::Properties(message)))
        .section(Section::new("Headers", Body::Properties(headers)))
        .section(Section::new("Delivery", Body::Properties(delivery)))
        .section(Section::new("Metadata", Body::Properties(metadata)));
    if mail.flag("mustEncrypt") {
        page = page.tag("Must Encrypt");
    }
    if let Some(related) = &related {
        let handle = harbormaster::handle(&handles, related);
        if let Some(uri) = &handle.uri {
            page = page.side(super::Link::new("View Object", uri.clone()).icon("fa-chevron-right"));
        }
    }
    Ok(page.build(state, &viewer, &uri))
}

/// `dateCreated` of the mail.
async fn mail_date<S: WebState>(state: &S, mail: &Mail) -> Result<i64, PageError> {
    let database = state.database();
    let sql = format!(
        "SELECT dateCreated FROM {} WHERE id = ?",
        database.table("metamta", "metamta_mail")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(mail.id)
        .fetch_one(database.pool())
        .await
        .map_err(phab_db::DbError::from)?;
    Ok(phab_db::rows::int(&row, "dateCreated")?)
}
