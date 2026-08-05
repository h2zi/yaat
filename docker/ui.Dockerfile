FROM node:22-bookworm

RUN corepack enable
RUN corepack install --global pnpm@11.18.0

WORKDIR /workspace

COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
RUN --mount=type=cache,target=/root/.local/share/pnpm/store \
    pnpm install --frozen-lockfile

COPY .prettierignore index.html tsconfig.json tsconfig.node.json vite.config.ts components.json ./
COPY src ./src

ENV VITE_YAAT_PREVIEW=1

RUN pnpm run format:check
RUN pnpm run typecheck
RUN pnpm run build
