# syntax=docker/dockerfile:1

FROM node:22-bookworm AS node

RUN corepack enable
RUN corepack install --global pnpm@11.18.0

FROM rust:1.97.1-bookworm

ARG CLAUDE_VERSION=2.1.220

COPY --from=node /usr/local/ /usr/local/
RUN rustup component add clippy rustfmt

ENV PNPM_HOME=/pnpm
ENV PATH="${PNPM_HOME}/bin:${PATH}"

RUN corepack install --global pnpm@11.18.0

RUN pnpm add --global --allow-build=@anthropic-ai/claude-code \
    "@anthropic-ai/claude-code@${CLAUDE_VERSION}" \
    && claude --version

WORKDIR /workspace
COPY . .

ENV CLAUDE_BIN=/pnpm/bin/claude
ENV RUSTUP_TOOLCHAIN=1.97.1

RUN cargo fmt --manifest-path tools/claude-interop/Cargo.toml --all -- --check
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/workspace/tools/claude-interop/target \
    cargo clippy --manifest-path tools/claude-interop/Cargo.toml \
        --bin yaat-claude-interop --release --locked -- -D warnings
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/workspace/tools/claude-interop/target \
    cargo build --manifest-path tools/claude-interop/Cargo.toml --release --locked \
    && cp tools/claude-interop/target/release/yaat-claude-interop /usr/local/bin/

CMD ["/usr/local/bin/yaat-claude-interop"]
