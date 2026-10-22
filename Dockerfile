# dev: Rust toolchain + the prebuilt DuckDB library. Used by the `dev` service.
FROM rust:1-slim-bookworm AS dev
# Must match the duckdb crate version in Cargo.toml (1.10506.x = DuckDB 1.5.6).
ARG DUCKDB_VERSION=1.5.6
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl unzip \
    && rm -rf /var/lib/apt/lists/* \
    && curl -fsSL -o /tmp/libduckdb.zip \
       "https://github.com/duckdb/duckdb/releases/download/v${DUCKDB_VERSION}/libduckdb-linux-amd64.zip" \
    && unzip -q /tmp/libduckdb.zip -d /tmp/libduckdb \
    && cp /tmp/libduckdb/libduckdb.so /usr/local/lib/ \
    && cp /tmp/libduckdb/duckdb.h /usr/local/include/ \
    && rm -rf /tmp/libduckdb /tmp/libduckdb.zip \
    && ldconfig \
    && rustup component add clippy rustfmt
ENV DUCKDB_LIB_DIR=/usr/local/lib \
    DUCKDB_INCLUDE_DIR=/usr/local/include \
    LD_LIBRARY_PATH=/usr/local/lib
WORKDIR /app

# build: release binary. Dependencies get their own layer, so changing our code
# doesn't rebuild them.
FROM dev AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs && cargo build --release --locked && rm -rf src
COPY src ./src
COPY static ./static
COPY rules.toml ./
RUN touch src/main.rs && cargo build --release --locked

# app: just the binary and libduckdb.
FROM debian:bookworm-slim AS app
COPY --from=dev /usr/local/lib/libduckdb.so /usr/local/lib/
RUN ldconfig
WORKDIR /app
COPY --from=build /src/target/release/finance /usr/local/bin/finance
COPY rules.toml ./

ENTRYPOINT ["finance"]
