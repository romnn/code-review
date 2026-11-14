# syntax=docker/dockerfile:1.7
# The ledger CLI in a small runtime image.

ARG RUST_VERSION=1.85
ARG DEBIAN_RELEASE=bookworm

FROM rust:${RUST_VERSION}-slim-${DEBIAN_RELEASE} AS build
WORKDIR /src
# Dependencies first, so source edits keep the cached layer.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo fetch --locked \
    && rm -rf src
COPY src ./src
COPY db ./db
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo test --release --locked \
    && cargo build --release --locked \
    && strip target/release/ledger

FROM debian:${DEBIAN_RELEASE}-slim AS runtime
ARG VERSION=dev
LABEL org.opencontainers.image.title="ledger" \
      org.opencontainers.image.version="${VERSION}"
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --home /var/lib/ledger ledger
COPY --from=build /src/target/release/ledger /usr/local/bin/ledger
COPY --from=build /src/db/migrations /usr/share/ledger/migrations
COPY config/settings.json /etc/ledger/settings.json
USER ledger
WORKDIR /var/lib/ledger
ENV LEDGER_CONFIG=/etc/ledger/settings.json \
    LEDGER_MIGRATIONS=/usr/share/ledger/migrations
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s CMD ["ledger", "health"]
ENTRYPOINT ["ledger"]
CMD ["serve", "--port", "8080"]
