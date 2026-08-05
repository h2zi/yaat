FROM rust:1.97.1-bookworm

RUN apt-get update \
    && apt-get install --yes --no-install-recommends gcc-mingw-w64-x86-64-posix \
    && rustup target add x86_64-pc-windows-gnu \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*

ENV CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc-posix

WORKDIR /workspace
COPY . .

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/workspace/target \
    cargo check --workspace --all-targets --target x86_64-pc-windows-gnu --locked
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/workspace/target \
    cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu --locked -- -D warnings
