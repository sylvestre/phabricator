//! `ResolveComments` and the pings of `PublicEventPings` and
//! `SecureEventPings`: the comments of an event, rendered for public
//! revisions, counted for secure ones.

use std::sync::LazyLock;

use pcre2::bytes::Regex;
use phab_auth::User;
use phab_core::Phid;
use phab_remarkup::Mode;
use serde_json::{Value, json};

use super::events::TransactionList;
use super::recipients::recipient;
use super::store::{CommentRow, RevisionRow, Store, TransactionRow};
use super::{Fail, Result, production_uri};

/// `ResolveComments::findPingedUsers()`'s pattern.
static USERNAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"@([A-Za-z0-9\-_\.]{0,63}[a-zA-Z0-9\-_])").expect("the username pattern compiles")
});

/// The comment attached to a transaction (`getComment()`), which PHP
/// calls `getContent()` on.
async fn transaction_comment(
    store: &Store<'_>,
    transaction: &TransactionRow,
) -> Result<CommentRow> {
    let phid = transaction
        .comment_phid
        .as_ref()
        .ok_or(Fail::Throw("a comment transaction without comment"))?;
    store
        .comment(phid, false)
        .await?
        .ok_or(Fail::Throw("a comment transaction whose comment is gone"))
}

/// `findPingedUsers()`: the enabled users named after `@`, in order, as
/// many times as they are named.
async fn pinged_users(store: &mut Store<'_>, text: &str) -> Result<Vec<User>> {
    let mut names = Vec::new();
    for captures in USERNAME.captures_iter(text.as_bytes()) {
        let captures = captures.map_err(|error| Fail::Internal(error.to_string()))?;
        if let Some(name) = captures.get(1) {
            names.push(String::from_utf8_lossy(name.as_bytes()).into_owned());
        }
    }
    let mut users = Vec::new();
    for name in names {
        if let Some(user) = store.user_by_name(&name).await? {
            users.push(user);
        }
    }
    Ok(users)
}

/// `PHP`'s keyed array of pinged users: insertion order, later values
/// replacing earlier ones at their first place.
struct Keyed<T> {
    entries: Vec<(Phid, T)>,
}

impl<T> Default for Keyed<T> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}

impl<T> Keyed<T> {
    fn entry(&mut self, key: &Phid, make: impl FnOnce() -> T) -> &mut T {
        let found = self
            .entries
            .iter()
            .position(|(existing, _)| existing == key);
        let index = found.unwrap_or_else(|| {
            self.entries.push((key.clone(), make()));
            self.entries.len() - 1
        });
        &mut self.entries[index].1
    }
}

/// `SecureEventPings`.
#[derive(Default)]
pub(crate) struct SecurePings {
    users: Keyed<User>,
}

impl SecurePings {
    /// `intoBodies()`.
    pub(crate) async fn into_bodies(
        self,
        store: &mut Store<'_>,
        actor_email: &str,
        link: &str,
    ) -> Result<Vec<Value>> {
        let mut bodies = Vec::new();
        for (_, user) in self.users.entries {
            if let Some(recipient) = recipient(store, &user, actor_email).await? {
                bodies.push(json!({"recipient": recipient, "transactionLink": link}));
            }
        }
        Ok(bodies)
    }
}

/// `PublicPing`.
struct PublicPing {
    user: User,
    main: Value,
    inlines: Vec<Value>,
}

/// `PublicEventPings`.
#[derive(Default)]
pub(crate) struct PublicPings {
    users: Keyed<PublicPing>,
}

impl PublicPings {
    fn ping(&mut self, user: &User) -> &mut PublicPing {
        self.users.entry(&user.phid, || PublicPing {
            user: user.clone(),
            main: Value::Null,
            inlines: Vec::new(),
        })
    }

    /// `intoBodies()`: `EmailRevisionCommentPinged` dictionaries.
    pub(crate) async fn into_bodies(
        self,
        store: &mut Store<'_>,
        actor_email: &str,
        link: &str,
    ) -> Result<Vec<Value>> {
        let mut bodies = Vec::new();
        for (_, ping) in self.users.entries {
            if let Some(recipient) = recipient(store, &ping.user, actor_email).await? {
                bodies.push(json!({
                    "recipient": recipient,
                    "transactionLink": link,
                    "pingedMainCommentMessage": ping.main,
                    "pingedInlineComments": ping.inlines,
                }));
            }
        }
        Ok(bodies)
    }
}

/// `ResolveComments::resolveSecureComments()`: the number of comments.
pub(crate) async fn secure_comments(
    store: &mut Store<'_>,
    transactions: &TransactionList,
    pings: &mut SecurePings,
) -> Result<i64> {
    let mut count = 0;
    if let Some(transaction) = transactions.attempt_with_type("core:comment")? {
        let comment = transaction_comment(store, transaction).await?;
        count += 1;
        for user in pinged_users(store, &comment.content).await? {
            let phid = user.phid.clone();
            let entry = pings.users.entry(&phid, || user.clone());
            *entry = user;
        }
    }
    for transaction in transactions.all_with_type("differential:inline") {
        count += 1;
        let comment = transaction_comment(store, transaction).await?;
        for user in pinged_users(store, &comment.content).await? {
            let phid = user.phid.clone();
            let entry = pings.users.entry(&phid, || user.clone());
            *entry = user;
        }
    }
    Ok(count)
}

/// What `resolvePublicComments()` gives.
pub(crate) struct PublicComments {
    /// `mainCommentMessage`, `null` without a comment.
    pub(crate) main: Value,
    /// `inlineComments`.
    pub(crate) inlines: Vec<Value>,
}

/// `ResolveComments::renderCommentMarkup()`: an `EmailCommentMessage`.
fn render(config: &phab_core::Config, text: &str) -> Result<Value> {
    let engine_config = phab_remarkup::Config {
        uri_base: Some(production_uri(config, "/")?),
        uri_full: true,
        ..phab_app_differential::markup::engine_config(config)
    };
    let engine = phab_remarkup::phabricator::engine(engine_config);
    let mut outputs = Vec::new();
    for mode in [Mode::Text, Mode::Html] {
        let prepared = engine.prepare(text.as_bytes(), mode);
        // The engine's viewer is a logged-out user: what it may see of the
        // referenced objects is not ported.
        if !prepared.object_references().is_empty()
            || !prepared.mentioned_usernames().is_empty()
            || !prepared.handle_phids().is_empty()
            || !prepared.repository_identifiers().is_empty()
        {
            return Err(Fail::Unsupported("a comment referencing objects"));
        }
        let rendered = engine.finish(prepared, phab_remarkup::Resolved::default());
        let output = String::from_utf8(rendered.output.into_bytes())
            .map_err(|_| Fail::Unsupported("a comment rendering to invalid UTF-8"))?;
        outputs.push(output);
    }
    let html = outputs.pop().unwrap_or_default();
    let text = outputs.pop().unwrap_or_default();
    // Code blocks are highlighted by Pygments when it is enabled.
    if config.truthy("pygments.enabled") && html.contains("remarkup-code") {
        return Err(Fail::Unsupported("a code block highlighted by Pygments"));
    }
    if config
        .value("syntax-highlighter.engine")
        .is_some_and(|engine| engine != "PhutilDefaultSyntaxHighlighterEngine")
    {
        return Err(Fail::Unsupported("a custom syntax highlighter"));
    }
    Ok(json!({"asText": text, "asHtml": html}))
}

/// PHP's `rtrim()` with its default characters.
fn rtrim(text: &str) -> &str {
    text.trim_end_matches([' ', '\t', '\n', '\r', '\0', '\x0B'])
}

/// The `EmailCodeContext` of an inline comment on code.
fn code_context(changeset: &phab_diff::Changeset, comment: &CommentRow) -> Value {
    // From getPatch() in DifferentialInlineCommentMailView.
    let mut line_length = comment.line_length;
    let context = i64::from(line_length < 2);
    if line_length > 6 {
        line_length = 6;
    }
    let side = if comment.is_new_file {
        phab_diff::parser::Side::New
    } else {
        phab_diff::parser::Side::Old
    };
    let patch = phab_diff::parser::make_context_diff(
        &changeset.hunks,
        side,
        comment.line_number,
        line_length,
        context,
    );
    let mut lines = phab_diff::lines::split_lines(&patch);
    // The "@@ -x,y +u,v @@" line.
    lines.remove(0);
    let start = (comment.line_number - context).max(0);
    let (mut old_line, mut new_line) = (start, start);
    let mut diff = Vec::new();
    for line in lines {
        let (kind, number) = match line.as_bytes().first() {
            Some(b'+') => {
                new_line += 1;
                ("added", new_line - 1)
            }
            Some(b'-') => {
                old_line += 1;
                ("removed", old_line - 1)
            }
            _ => {
                old_line += 1;
                new_line += 1;
                ("no-change", new_line - 1)
            }
        };
        let raw = line.get(1..).unwrap_or_default();
        diff.push(json!({
            "lineNumber": number,
            "type": kind,
            "rawContent": rtrim(raw),
        }));
    }
    json!({"diff": diff})
}

/// `getContentState()` of an inline: whether it suggests a change, and the
/// suggested text.
fn suggestion(comment: &CommentRow) -> Result<(bool, String)> {
    let Some(state) = comment.attributes.get("inline.state") else {
        return Ok((false, String::new()));
    };
    // A list has none of the keys; anything but an array is no state.
    let Value::Object(state) = state else {
        return Ok((false, String::new()));
    };
    let has = state
        .get("hasSuggestion")
        .is_some_and(phab_core::php::is_truthy);
    let text = match state.get("suggestionText") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(_) => return Err(Fail::Unsupported("an unusual suggestion text")),
    };
    Ok((has, text))
}

/// `ResolveComments::resolvePublicComments()`.
pub(crate) async fn public_comments(
    store: &mut Store<'_>,
    transactions: &TransactionList,
    revision: &RevisionRow,
    pings: &mut PublicPings,
) -> Result<PublicComments> {
    let mut main = Value::Null;
    if let Some(transaction) = transactions.attempt_with_type("core:comment")? {
        let comment = transaction_comment(store, transaction).await?;
        let message = render(store.config, &comment.content)?;
        for user in pinged_users(store, &comment.content).await? {
            pings.ping(&user).main = message.clone();
        }
        main = message;
    }

    let mut inlines = Vec::new();
    for transaction in transactions.all_with_type("differential:inline") {
        let comment = transaction_comment(store, transaction).await?;
        let changeset = match comment.changeset_id {
            Some(id) => store.changeset(id).await?,
            None => None,
        }
        .ok_or(Fail::Throw("an inline comment whose changeset is gone"))?;
        let filename = format!("/{}", changeset.display_filename());
        let link = production_uri(
            store.config,
            &format!(
                "/{}?id={}#inline-{}",
                revision.monogram(),
                changeset.diff_id,
                comment.id
            ),
        )?;
        let (kind, context) = match &comment.reply_to_phid {
            Some(parent) => {
                let other = store
                    .comment(parent, true)
                    .await?
                    .ok_or(Fail::Throw("a reply to a comment which is gone"))?;
                let date = jiff::Timestamp::from_second(other.date_created)
                    .map_err(|error| Fail::Internal(error.to_string()))?
                    .strftime("%Y-%m-%dT%H:%M:%S+00:00")
                    .to_string();
                let author = store.user(&other.author_phid).await?;
                let message = render(store.config, &other.content)?;
                (
                    "reply",
                    json!({
                        "otherAuthor": author.user_name,
                        "otherDateUtc": date,
                        "otherCommentMessage": message,
                    }),
                )
            }
            None => ("code", code_context(&changeset, &comment)),
        };
        let (has_suggestion, suggestion_text) = suggestion(&comment)?;
        let message = render(store.config, &comment.content)?;
        let inline = json!({
            "fileContext": format!("{filename}:{}", comment.line_number),
            "link": link,
            "message": message,
            "contextKind": kind,
            "context": context,
            "hasSuggestion": has_suggestion,
            "suggestionText": suggestion_text,
        });
        for user in pinged_users(store, &comment.content).await? {
            pings.ping(&user).inlines.push(inline.clone());
        }
        inlines.push(inline);
    }
    Ok(PublicComments { main, inlines })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn config() -> phab_core::Config {
        let mut config = phab_core::Config::default();
        let Value::Object(values) = json!({"phabricator.base-uri": "http://phab.test/"}) else {
            unreachable!()
        };
        config.push_layer("test", values);
        config
    }

    #[test]
    fn renders_comments_without_references() {
        assert_eq!(
            render(&config(), "Looks **good**").unwrap(),
            json!({"asText": "Looks **good**\n", "asHtml": "<p>Looks <strong>good</strong></p>"})
        );
        assert!(matches!(
            render(&config(), "Thanks @alice"),
            Err(Fail::Unsupported(_))
        ));
        assert!(matches!(
            render(&config(), "See D12"),
            Err(Fail::Unsupported(_))
        ));
    }
}
