//! The part of the PHP-owned schema the Rust code depends on.
//!
//! Every column a Rust query reads or writes must be listed here. The
//! `schema_contract` integration test checks the list against a database
//! built from `resources/sql/quickstart.sql`, so a PHP migration which
//! renames or drops one of them fails CI instead of production.

/// Columns of one table that the Rust code relies on.
pub struct TableContract {
    /// Database name without the namespace prefix, e.g. `user`.
    pub database: &'static str,
    /// Table name.
    pub table: &'static str,
    /// Columns that must exist.
    pub columns: &'static [&'static str],
}

/// Every table and column the Rust code reads or writes.
pub const CONTRACT: &[TableContract] = &[
    TableContract {
        database: "config",
        table: "config_entry",
        columns: &["namespace", "configKey", "value", "isDeleted"],
    },
    TableContract {
        database: "user",
        table: "edge",
        columns: &["src", "type", "dst", "dateCreated", "seq"],
    },
    TableContract {
        database: "project",
        table: "edge",
        columns: &["src", "type", "dst", "dateCreated", "seq"],
    },
    TableContract {
        database: "differential",
        table: "edge",
        columns: &["src", "type", "dst", "dateCreated", "seq"],
    },
    TableContract {
        database: "repository",
        table: "edge",
        columns: &["src", "type", "dst", "dateCreated", "seq"],
    },
    TableContract {
        database: "project",
        table: "project",
        columns: &["phid"],
    },
    TableContract {
        database: "differential",
        table: "differential_reviewer",
        columns: &["revisionPHID", "reviewerPHID"],
    },
    TableContract {
        database: "policy",
        table: "policy",
        columns: &["phid", "rules", "defaultAction"],
    },
    TableContract {
        database: "spaces",
        table: "spaces_namespace",
        columns: &["phid", "viewPolicy", "editPolicy", "isDefaultNamespace"],
    },
    TableContract {
        database: "user",
        table: "user",
        columns: &[
            "id",
            "phid",
            "userName",
            "realName",
            "profileImagePHID",
            "isSystemAgent",
            "isDisabled",
            "isAdmin",
            "isEmailVerified",
            "isApproved",
            "isMailingList",
            "accountSecret",
        ],
    },
    TableContract {
        database: "user",
        table: "phabricator_session",
        columns: &[
            "id",
            "phid",
            "userPHID",
            "type",
            "sessionKey",
            "sessionStart",
            "sessionExpires",
            "highSecurityUntil",
            "isPartial",
        ],
    },
    TableContract {
        database: "auth",
        table: "auth_hmackey",
        columns: &["keyName", "keyValue"],
    },
    TableContract {
        database: "conduit",
        table: "conduit_token",
        columns: &[
            "id",
            "objectPHID",
            "tokenType",
            "token",
            "expires",
            "dateModified",
        ],
    },
    TableContract {
        database: "conduit",
        table: "conduit_methodcalllog",
        columns: &[
            "method",
            "error",
            "duration",
            "dateCreated",
            "dateModified",
            "callerPHID",
        ],
    },
    TableContract {
        database: "user",
        table: "user_email",
        columns: &["userPHID", "address", "isVerified", "isPrimary"],
    },
    TableContract {
        database: "user",
        table: "user_cache",
        columns: &[
            "userPHID",
            "cacheIndex",
            "cacheKey",
            "cacheData",
            "cacheType",
        ],
    },
    TableContract {
        database: "differential",
        table: "differential_revision",
        columns: &[
            "phid",
            "authorPHID",
            "viewPolicy",
            "editPolicy",
            "repositoryPHID",
        ],
    },
    TableContract {
        database: "differential",
        table: "differential_reviewer",
        columns: &["revisionPHID", "reviewerPHID", "reviewerStatus"],
    },
    TableContract {
        database: "differential",
        table: "differential_transaction",
        columns: &[
            "phid",
            "authorPHID",
            "objectPHID",
            "viewPolicy",
            "editPolicy",
            "commentPHID",
            "transactionType",
            "oldValue",
            "newValue",
            "metadata",
        ],
    },
    TableContract {
        database: "differential",
        table: "differential_transaction_comment",
        columns: &["phid", "authorPHID", "viewPolicy", "editPolicy", "content"],
    },
    TableContract {
        database: "repository",
        table: "repository",
        columns: &[
            "phid",
            "viewPolicy",
            "editPolicy",
            "pushPolicy",
            "spacePHID",
        ],
    },
    TableContract {
        database: "feed",
        table: "feed_storydata",
        columns: &[
            "id",
            "phid",
            "chronologicalKey",
            "storyType",
            "storyData",
            "authorPHID",
        ],
    },
    TableContract {
        database: "feed",
        table: "feed_storyreference",
        columns: &["chronologicalKey"],
    },
    TableContract {
        database: "auth",
        table: "auth_providerconfig",
        columns: &["phid", "providerClass"],
    },
    TableContract {
        database: "user",
        table: "user_externalaccount",
        columns: &["id", "phid", "userPHID", "providerConfigPHID"],
    },
    TableContract {
        database: "user",
        table: "user_externalaccountidentifier",
        columns: &[
            "id",
            "externalAccountPHID",
            "providerConfigPHID",
            "identifierHash",
            "identifierRaw",
        ],
    },
];
