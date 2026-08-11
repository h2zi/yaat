# syntax=docker/dockerfile:1

FROM node:22-bookworm AS node

RUN corepack enable
RUN corepack install --global pnpm@11.18.0

FROM rust:1.97.1-bookworm

ARG CODEX_VERSION=0.147.0

COPY --from=node /usr/local/ /usr/local/
RUN rustup component add clippy rustfmt

ENV PNPM_HOME=/pnpm
ENV PATH="${PNPM_HOME}/bin:${PATH}"

RUN corepack install --global pnpm@11.18.0

RUN pnpm add --global "@openai/codex@${CODEX_VERSION}" \
    && codex --version

WORKDIR /workspace
COPY . .

ENV CODEX_BIN=/pnpm/bin/codex
ENV RUSTUP_TOOLCHAIN=1.97.1

RUN cargo fmt --manifest-path tools/codex-interop/Cargo.toml --all -- --check
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/workspace/tools/codex-interop/target \
    cargo clippy --manifest-path tools/codex-interop/Cargo.toml \
        --bin yaat-codex-interop --release --locked -- -D warnings
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/workspace/tools/codex-interop/target \
    cargo build --manifest-path tools/codex-interop/Cargo.toml --release --locked \
    && cp tools/codex-interop/target/release/yaat-codex-interop /usr/local/bin/

CMD ["/usr/local/bin/yaat-codex-interop"]
