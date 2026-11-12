//! The part of the PHP-owned schema the Rust code depends on.
//!
//! Every column a Rust query reads or writes must be listed here. The
//! `schema_contract` integration test checks the list against a database
//! built from `resources/sql/quickstart.sql`, so a PHP migration which
//! renames or drops one of them fails CI instead of production.

pub struct TableContract {
    pub database: &'static str,
    pub table: &'static str,
    pub columns: &'static [&'static str],
}

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
];
