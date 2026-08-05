FROM mcr.microsoft.com/playwright:v1.62.1-noble

RUN corepack enable
RUN corepack install --global pnpm@11.18.0

WORKDIR /workspace

COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
RUN --mount=type=cache,target=/root/.local/share/pnpm/store \
    pnpm install --frozen-lockfile

COPY index.html tsconfig.json tsconfig.node.json vite.config.ts components.json ./
COPY src ./src
COPY scripts/visual-test.mjs ./scripts/visual-test.mjs

ENV VITE_YAAT_PREVIEW=1

RUN pnpm run build

CMD ["node", "scripts/visual-test.mjs", "/output"]
