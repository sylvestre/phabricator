# Proxies the current location to the Rust service. Included by the
# locations of *.conf; not a .conf itself, so site.conf doesn't include it
# at the server level.
#
# Resolve the service per request: with a literal host name, nginx refuses
# to start (taking PHP down with it) whenever the name doesn't resolve.
# 127.0.0.11 is Docker's resolver; use the platform's one elsewhere.
resolver 127.0.0.11 valid=10s ipv6=off;
set $phab_rust http://phab-rust:8080;
proxy_pass $phab_rust;
proxy_set_header Host $host;
proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
proxy_set_header X-Forwarded-Proto $scheme;
proxy_connect_timeout 2s;
