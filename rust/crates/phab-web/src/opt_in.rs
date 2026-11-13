//! The per-user switch between the two interfaces: nginx sends the ported
//! pages to Rust only when the `phab_ui=rust` cookie is set
//! (`rust/nginx/rust-web.conf`). `/rust-ui/on` and `/rust-ui/off` set and
//! clear it, then go back to the page the user came from.

use axum::extract::RawQuery;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

/// The cookie nginx routes on. nginx variables can't hold a `-`, hence
/// the underscore.
pub const COOKIE: &str = "phab_ui";
/// The cookie value which selects the Rust interface.
pub const RUST: &str = "rust";
const MAX_AGE: u32 = 365 * 24 * 60 * 60;

/// Where `next` points if it is a path on this site; anything else, such
/// as `//evil.example` or `https://evil.example`, goes home.
fn local_path(next: Option<&str>) -> &str {
    match next {
        Some(path)
            if path.starts_with('/')
                && !path.starts_with("//")
                && !path.starts_with("/\\")
                && !path.chars().any(char::is_control) =>
        {
            path
        }
        _ => "/",
    }
}

fn next_from_query(query: Option<&str>) -> String {
    let next = query.and_then(|query| {
        form_urlencoded::parse(query.as_bytes())
            .find(|(key, _)| key == "next")
            .map(|(_, value)| value.into_owned())
    });
    local_path(next.as_deref()).to_owned()
}

fn switch(query: Option<&str>, cookie: &str) -> Response {
    let next = next_from_query(query);
    let mut response = StatusCode::SEE_OTHER.into_response();
    let headers = response.headers_mut();
    if let Ok(location) = HeaderValue::from_str(&next) {
        headers.insert(header::LOCATION, location);
    } else {
        headers.insert(header::LOCATION, HeaderValue::from_static("/"));
    }
    if let Ok(cookie) = HeaderValue::from_str(cookie) {
        headers.insert(header::SET_COOKIE, cookie);
    }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// `GET /rust-ui/on?next=/D123`: use the new interface from now on. A
/// link and not a form, as the old interface has no form to post it.
pub async fn on(RawQuery(query): RawQuery) -> Response {
    switch(
        query.as_deref(),
        &format!("{COOKIE}={RUST}; Path=/; Max-Age={MAX_AGE}; SameSite=Lax; HttpOnly"),
    )
}

/// `GET /rust-ui/off?next=/D123`: go back to the old interface.
pub async fn off(RawQuery(query): RawQuery) -> Response {
    switch(
        query.as_deref(),
        &format!("{COOKIE}=; Path=/; Max-Age=0; SameSite=Lax; HttpOnly"),
    )
}

/// The link which opens `current_uri` in the old interface.
pub fn off_uri(current_uri: &str) -> String {
    let next: String = form_urlencoded::byte_serialize(current_uri.as_bytes()).collect();
    format!("/rust-ui/off?next={next}")
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case(Some("next=%2FD123"), "/D123")]
    #[case(Some("next=%2Fp%2Falice%2F%3Fx%3D1"), "/p/alice/?x=1")]
    #[case(Some("next=%2F%2Fevil.example"), "/")]
    #[case(Some("next=https%3A%2F%2Fevil.example"), "/")]
    #[case(Some("next=%2F%5Cevil.example"), "/")]
    #[case(Some("next=%2Fa%0D%0ASet-Cookie%3Ax"), "/")]
    #[case(Some("other=1"), "/")]
    #[case(None, "/")]
    fn next_stays_on_the_site(#[case] query: Option<&str>, #[case] expected: &str) {
        assert_eq!(next_from_query(query), expected);
    }

    #[tokio::test]
    async fn switching_sets_the_cookie_and_redirects() {
        let response = on(RawQuery(Some("next=%2FD1".to_owned()))).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()[header::LOCATION], "/D1");
        assert!(
            response.headers()[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .starts_with("phab_ui=rust;")
        );
        let response = off(RawQuery(None)).await;
        assert_eq!(response.headers()[header::LOCATION], "/");
        assert!(
            response.headers()[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .contains("Max-Age=0")
        );
    }

    #[test]
    fn off_links_reopen_the_page() {
        assert_eq!(off_uri("/D1?id=2"), "/rust-ui/off?next=%2FD1%3Fid%3D2");
    }
}
