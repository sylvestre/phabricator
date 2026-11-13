//! Static files, built into the binary and served under a content hash so
//! browsers can cache them forever (what Celerity does for PHP).

use std::sync::LazyLock;

use axum::body::Body;
use axum::extract::Path;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};

/// Where the files are served.
pub const PREFIX: &str = "/rsrc-rust";
const HASH_LENGTH: usize = 16;

struct Asset {
    name: &'static str,
    content_type: &'static str,
    bytes: &'static [u8],
    hash: String,
}

static ASSETS: LazyLock<Vec<Asset>> = LazyLock::new(|| {
    [(
        "phab.css",
        "text/css; charset=UTF-8",
        include_bytes!("../assets/phab.css").as_slice(),
    )]
    .into_iter()
    .map(|(name, content_type, bytes)| {
        let mut hash = hex::encode(Sha256::digest(bytes));
        hash.truncate(HASH_LENGTH);
        Asset {
            name,
            content_type,
            bytes,
            hash,
        }
    })
    .collect()
});

fn find(name: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.name == name)
}

/// The URI of a built-in file.
///
/// # Panics
///
/// If no file has this name: names are constants of the templates.
pub fn uri(name: &str) -> String {
    let asset = find(name).unwrap_or_else(|| panic!("no asset named {name}"));
    format!("{PREFIX}/{}/{}", asset.hash, asset.name)
}

/// Serves `/rsrc-rust/{hash}/{name}`. An outdated hash is a 404, as a
/// cached page asking for it would otherwise get the new file under the
/// old, forever cached, URI.
pub async fn serve(Path((hash, name)): Path<(String, String)>) -> Response {
    let Some(asset) = find(&name).filter(|asset| asset.hash == hash) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut response = Response::new(Body::from(asset.bytes));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(asset.content_type),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_carry_the_content_hash() {
        let uri = uri("phab.css");
        let hash = uri
            .strip_prefix("/rsrc-rust/")
            .and_then(|rest| rest.strip_suffix("/phab.css"))
            .unwrap();
        assert_eq!(hash.len(), HASH_LENGTH);
        assert!(hash.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn only_the_current_hash_is_served() {
        let asset = find("phab.css").unwrap();
        let current = serve(Path((asset.hash.clone(), "phab.css".to_owned()))).await;
        assert_eq!(current.status(), StatusCode::OK);
        assert_eq!(
            current.headers()[header::CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
        let stale = serve(Path(("0".repeat(HASH_LENGTH), "phab.css".to_owned()))).await;
        assert_eq!(stale.status(), StatusCode::NOT_FOUND);
        let missing = serve(Path((asset.hash.clone(), "nope.css".to_owned()))).await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    }
}
