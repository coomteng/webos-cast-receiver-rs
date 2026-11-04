# webos-cast-receiver-rs — DLNA MediaRenderer in Rust, static musl binary.
# SSDP needs LAN multicast (UDP 1900): run with --network host.
#
# Stage 1: build the static binary (pure-Rust TLS via rustls+ring, no cmake/OpenSSL).
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

# Stage 2: the TV app + simulator are served from the Node project (single
# source of truth); fetch them from GitHub at a pinned ref.
FROM alpine:3 AS assets
ARG APP_REPO=https://github.com/coomteng/webos-cast-receiver
ARG APP_REF=v0.2.3
RUN apk add --no-cache git && git clone --depth 1 --branch "$APP_REF" "$APP_REPO" /app-src

# Stage 3: scratch image: binary + static assets, nothing else.
FROM scratch
COPY --from=build /src/target/release/webos-cast-receiver /webos-cast-receiver
COPY --from=assets /app-src/app /app
COPY --from=assets /app-src/sim /sim
# data/ holds the persistent device UUID (phones key their device cache on it).
VOLUME ["/data"]
ENV DATA_DIR=/data APP_DIR=/app SIM_DIR=/sim PORT=8790
EXPOSE 8790
ENTRYPOINT ["/webos-cast-receiver"]
