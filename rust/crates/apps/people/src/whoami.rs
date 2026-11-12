//! `UserWhoAmIConduitAPIMethod`.

use phab_auth::User;
use phab_conduit::{ApiRequest, Application, Method, MethodError, MethodFuture};
use phab_core::Config;
use phab_db::{Database, DbError, rows};
use serde_json::{Value, json};

use crate::{APPLICATION, profile_image};

/// `user.whoami`.
pub struct UserWhoAmI;

/// The `user_email` row with `isPrimary` set.
pub struct PrimaryEmail {
    /// The email address.
    pub address: String,
    /// Whether the address is verified.
    pub is_verified: bool,
}

/// The primary email of `user`, if any.
pub async fn primary_email(
    database: &Database,
    user: &User,
) -> Result<Option<PrimaryEmail>, DbError> {
    let sql = format!(
        "SELECT address, isVerified FROM {} WHERE userPHID = ? AND isPrimary = 1 LIMIT 1",
        database.table("user", "user_email")
    );
    let row = phab_db::sqlx::query(&sql)
        .bind(user.phid.as_str())
        .fetch_optional(database.pool())
        .await?;
    row.map(|row| {
        Ok(PrimaryEmail {
            address: rows::text(&row, "address")?,
            is_verified: rows::flag(&row, "isVerified")?,
        })
    })
    .transpose()
}

/// `UserConduitAPIMethod::buildUserInformationDictionary()` with the email
/// and without availability.
pub fn user_information(
    config: &Config,
    user: &User,
    image: &str,
    email: Option<&PrimaryEmail>,
) -> Result<Value, phab_core::config::ConfigError> {
    let mut roles = Vec::new();
    if user.is_disabled {
        roles.push("disabled");
    }
    if user.is_system_agent {
        roles.push("agent");
    }
    if user.is_mailing_list {
        roles.push("list");
    }
    if user.is_admin {
        roles.push("admin");
    }
    let verified_email = email
        .filter(|email| email.is_verified)
        .map(|email| email.address.as_str());
    roles.push(if verified_email.is_some() {
        "verified"
    } else {
        "unverified"
    });
    if user.is_approved {
        roles.push("approved");
    }
    if user.is_activated(config) {
        roles.push("activated");
    }

    Ok(json!({
        "phid": user.phid,
        "userName": user.user_name,
        "realName": user.real_name,
        "image": image,
        "uri": config.uri(&format!("/p/{}/", user.user_name))?,
        "roles": roles,
        "primaryEmail": verified_email,
    }))
}

impl Method for UserWhoAmI {
    fn name(&self) -> &'static str {
        "user.whoami"
    }

    fn params(&self) -> &'static [&'static str] {
        &[]
    }

    fn application(&self) -> Option<Application> {
        Some(APPLICATION)
    }

    fn execute<'a>(&'a self, request: &'a ApiRequest<'a>) -> MethodFuture<'a> {
        Box::pin(async move {
            let Some(user) = request.user.as_ref() else {
                return Err(MethodError::Unsupported("user.whoami without a user"));
            };
            let context = request.context;
            let Some(image) =
                profile_image::cached_uri(&context.database, &context.config, user).await?
            else {
                return Err(MethodError::Unsupported(
                    "profile image cache needs a rebuild",
                ));
            };
            let email = primary_email(&context.database, user).await?;
            Ok(user_information(
                &context.config,
                user,
                &image,
                email.as_ref(),
            )?)
        })
    }
}
