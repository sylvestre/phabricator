# Sends a page of the Rust interface to the service, for users who opted in
# with the phab_ui=rust cookie; everyone else gets PHP. Included by the page
# locations of rust-web.conf.
#
# 418: the user didn't opt in or asked for PHP. 421: the page needs
# something Rust doesn't do yet (a logged-out user, a pending MFA prompt).
# 502/504: the Rust service is down. Pages are read-only, so replaying them
# against PHP is safe.
if ($cookie_phab_ui != "rust") {
  return 418;
}
if ($http_x_phab_skip_rust) {
  return 418;
}
include /app/nginx/rust/proxy.inc;
proxy_intercept_errors on;
error_page 418 421 502 504 = @php;
