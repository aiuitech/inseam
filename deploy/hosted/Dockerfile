# syntax=docker/dockerfile:1

FROM node:24-bookworm-slim AS web
WORKDIR /src/apps/web
RUN corepack enable
COPY apps/web/package.json apps/web/pnpm-lock.yaml apps/web/pnpm-workspace.yaml ./
RUN pnpm install --frozen-lockfile
COPY apps/web ./
RUN pnpm build

FROM rust:1.97-bookworm AS rust
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY xtask ./xtask
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --locked --release --package inseam-cli \
    && cp /src/target/release/inseam /tmp/inseam

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
COPY --from=rust /tmp/inseam /usr/local/bin/inseam
COPY --from=web /src/apps/web/dist /opt/inseam/web
COPY deploy/hosted/composition.toml /etc/inseam/composition.toml

RUN useradd --create-home --uid 10001 inseam \
    && mkdir --parents /var/lib/inseam /srv/inseam/hosts \
    && chown --recursive inseam:inseam /var/lib/inseam /srv/inseam

USER inseam
VOLUME ["/var/lib/inseam"]
EXPOSE 7337
HEALTHCHECK --interval=30s --timeout=3s --retries=3 \
    CMD curl --fail --silent http://127.0.0.1:7337/api/v1/health > /dev/null || exit 1

ENTRYPOINT ["inseam", "--data-dir", "/var/lib/inseam", "--composition", "/etc/inseam/composition.toml"]
CMD ["serve", "--bind", "0.0.0.0:7337", "--web-dir", "/opt/inseam/web"]
