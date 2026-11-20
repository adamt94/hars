# hars: a static musl binary in a scratch image.
#
#   docker build -t hars .
#   docker run --rm -p 8123:8123 -v hars-config:/config hars
#
# For LAN integrations (LIFX broadcast, mDNS) run with --network host.

# The official frontend, pinned to the release HA 2026.6.1 ships. It is served
# from disk. Source maps are dropped. The ES5 build stays: the index loads it
# for any browser older than its `isModern` cut-off (Safari < 26, iOS < 18.5,
# Firefox < 140). Its map tiles are pointed at hars's OpenStreetMap relay:
# CARTO's tiles now carry an "API KEY REQUIRED" overlay (docs/adr/0010).
FROM alpine:3 AS frontend
ARG FRONTEND_VERSION=20260527.4
ARG FRONTEND_SHA256=a85e6dbb007962a157bcd6b5c1ce98ed8872f966732773e4b6008444d1b10e18
ARG FRONTEND_URL=https://files.pythonhosted.org/packages/b0/2d/af298b8e695296360b91bf5d732618bd6fc87b9e61b3db84fa22bbd029ec/home_assistant_frontend-${FRONTEND_VERSION}-py3-none-any.whl
RUN apk add --no-cache curl unzip && \
    curl -fsSL "$FRONTEND_URL" -o /frontend.whl && \
    echo "$FRONTEND_SHA256  /frontend.whl" | sha256sum -c - && \
    unzip -q /frontend.whl 'hass_frontend/*' -d /out && \
    unzip -q -j /frontend.whl '*.dist-info/licenses/LICENSE.md' -d /out/licence && \
    rm -rf /out/hass_frontend/__init__.py /out/hass_frontend/py.typed && \
    find /out/hass_frontend -name '*.map' -o -name '*.map.br' -o -name '*.map.gz' | xargs rm -f
COPY scripts/patch_frontend_map.py /patch_frontend_map.py
RUN apk add --no-cache python3 && python3 /patch_frontend_map.py /out/hass_frontend

FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev ca-certificates tzdata
WORKDIR /src
COPY . .
# Build with the image's own toolchain: rust-toolchain.toml says `stable`,
# which would make rustup download a second copy inside the build.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    export RUSTUP_TOOLCHAIN="$(rustup default | cut -d' ' -f1)" && \
    cargo build --release --locked && cp target/release/hars /hars

FROM scratch
# No CA bundle: the outbound client has Mozilla's roots compiled in.
COPY --from=build /usr/share/zoneinfo /usr/share/zoneinfo
COPY --from=frontend /out/hass_frontend /frontend
COPY --from=build /hars /hars
# The licences the image must carry: hars's own, its notices, its crates'
# (scripts/gen-third-party.py) and the frontend's.
COPY --from=build /src/LICENSE /src/NOTICE /src/THIRD-PARTY-LICENSES.txt /licenses/
COPY --from=frontend /out/licence/LICENSE.md /licenses/home-assistant-frontend-LICENSE.md
ENV HARS_BIND=0.0.0.0:8123 \
    HARS_FRONTEND=/frontend \
    HARS_CONFIG=/config
# Persistent state (config, .storage, recorder DB) lives here.
VOLUME /config
EXPOSE 8123
# hars handles SIGTERM itself: it stops HTTP, drains, and exits 0.
STOPSIGNAL SIGTERM
ENTRYPOINT ["/hars"]
