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

# Stage 2: scratch image: binary + the TV app / simulator from this repo, nothing else.
FROM scratch
COPY --from=build /src/target/release/webos-cast-receiver /webos-cast-receiver
COPY app /app
COPY sim /sim
# data/ holds the persistent device UUID (phones key their device cache on it).
VOLUME ["/data"]
ENV DATA_DIR=/data APP_DIR=/app SIM_DIR=/sim PORT=8790
EXPOSE 8790
ENTRYPOINT ["/webos-cast-receiver"]
