# hars: a static musl binary in a scratch image.
#
#   docker build -t hars .
#   docker run --rm -p 8123:8123 -v hars-config:/config hars
#
# For LAN integrations (LIFX broadcast, mDNS) run with --network host.

FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev ca-certificates tzdata
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked && cp target/release/hars /hars

FROM scratch
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=build /usr/share/zoneinfo /usr/share/zoneinfo
COPY --from=build /hars /hars
ENV HARS_BIND=0.0.0.0:8123 \
    SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
# Persistent state (config, .storage, recorder DB) lives here.
VOLUME /config
EXPOSE 8123
# hars handles SIGTERM itself: it stops HTTP, drains, and exits 0.
STOPSIGNAL SIGTERM
ENTRYPOINT ["/hars"]
