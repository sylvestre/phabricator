# Build from the rust/ directory: docker build -t mozilla/phabricator-rust rust
FROM rust:1.91-slim-trixie AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p phab-server

FROM debian:trixie-slim AS production
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 10001 --create-home app
COPY --from=build /src/target/release/phab-server /usr/local/bin/phab-server
USER app
ENV PHAB_LISTEN=0.0.0.0:8080 \
    PHAB_CONFIG_DIR=/app/phabricator/conf/local
EXPOSE 8080
CMD ["phab-server"]
