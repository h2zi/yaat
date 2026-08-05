FROM node:22-bookworm AS node

RUN corepack enable
RUN corepack install --global pnpm@11.18.0

FROM rust:1.97.1-bookworm

COPY --from=node /usr/local/ /usr/local/
RUN corepack enable pnpm
RUN rustup component add clippy rustfmt

RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    apt-get -o Acquire::Retries=5 update \
    && apt-get -o Acquire::Retries=5 install --yes --no-install-recommends \
        file \
        libayatana-appindicator3-dev \
        librsvg2-dev \
        libssl-dev \
        libwebkit2gtk-4.1-dev \
        libxdo-dev

WORKDIR /workspace
COPY . .

RUN --mount=type=cache,target=/root/.local/share/pnpm/store \
    pnpm install --frozen-lockfile
RUN pnpm run format:check
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/workspace/target \
    cargo check --workspace --all-targets --locked
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/workspace/target \
    cargo clippy --workspace --all-targets --locked -- -D warnings
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/workspace/target \
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/workspace/target \
    cargo test --workspace --locked --no-fail-fast
RUN --mount=type=cache,target=/root/.local/share/pnpm/store \
    --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/workspace/target \
    pnpm run tauri build --debug --no-bundle
