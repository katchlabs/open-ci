FROM rust:1.89-slim-bookworm AS builder

WORKDIR /src
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config cmake perl make g++ \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates

RUN cargo build --release --bin open-ci

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home open-ci

COPY --from=builder /src/target/release/open-ci /usr/local/bin/open-ci

USER open-ci
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/open-ci"]
CMD ["serve", "--config", "/etc/open-ci/open-ci.toml"]
