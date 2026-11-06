# wtfps, serving the HTTP API by default (see docker-compose.yml).
#
# Only official images (rust, debian). Cargo's downloads and its target/ folder are kept in
# BuildKit cache mounts between builds, so a source edit recompiles just wtfps itself (about
# 25 s) instead of all ~240 dependency crates (3 min). The cache lives on the building
# machine; `docker builder prune` clears it.

FROM rust:1-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
# the saved query data in .sqlx/ replaces a database at build time
ENV SQLX_OFFLINE=true
# target/ is a cache mount, not part of the image, so copy the binary out in the same step
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
    && cp target/release/wtfps /usr/local/bin/wtfps

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/bin/wtfps /usr/local/bin/wtfps
EXPOSE 8090
ENTRYPOINT ["wtfps"]
CMD ["serve", "--listen", "0.0.0.0:8090"]
