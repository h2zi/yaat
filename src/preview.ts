import type {
  ActivateProviderRequest,
  AppSettings,
  BootstrapResponse,
  CreateProviderRequest,
  ImportCurrentRequest,
  HistoryApplyRequest,
  HistoryApplyResult,
  HistoryPreview,
  HistoryPreviewRequest,
  OperationResult,
  ProviderProfile,
  UpdateProgress,
  UpdateProviderRequest,
  UsageQueryRequest,
  UsageReport,
} from "@/types";

const now = Math.floor(Date.now() / 1_000);

let state: BootstrapResponse = {
  profiles: [
    {
      id: "codex-personal",
      platform: "codex",
      kind: "official_subscription",
      name: "Personal",
      accountLabel: "haozi@example.com",
      baseUrl: null,
      model: null,
      secretKind: "none",
      hasSecret: false,
      profileHome: "/tmp/preview/codex-personal",
      status: "ready",
      createdAt: now - 86_400 * 80,
      updatedAt: now - 1_800,
    },
    {
      id: "codex-work",
      platform: "codex",
      kind: "official_subscription",
      name: "Work account",
      accountLabel: "engineering@acme.dev",
      baseUrl: null,
      model: null,
      secretKind: "none",
      hasSecret: false,
      profileHome: "/tmp/preview/codex-work",
      status: "ready",
      createdAt: now - 86_400 * 20,
      updatedAt: now - 7_200,
    },
    {
      id: "codex-openrouter",
      platform: "codex",
      kind: "third_party",
      name: "OpenRouter",
      accountLabel: "Team API",
      baseUrl: "https://openrouter.ai/api/v1",
      model: "openai/gpt-5.1-codex",
      secretKind: "api_key",
      hasSecret: true,
      profileHome: "/tmp/preview/codex-openrouter",
      status: "ready",
      createdAt: now - 86_400 * 12,
      updatedAt: now - 300,
    },
    {
      id: "claude-personal",
      platform: "claude_code",
      kind: "official_subscription",
      name: "Claude Max",
      accountLabel: "haozi@example.com",
      baseUrl: null,
      model: null,
      secretKind: "none",
      hasSecret: false,
      profileHome: "/tmp/preview/claude-personal",
      status: "ready",
      createdAt: now - 86_400 * 40,
      updatedAt: now - 500,
    },
    {
      id: "desktop-personal",
      platform: "claude_desktop",
      kind: "official_subscription",
      name: "Desktop Personal",
      accountLabel: "isolated official account",
      baseUrl: null,
      model: null,
      secretKind: "none",
      hasSecret: false,
      profileHome: "/tmp/preview/desktop-personal",
      status: "ready",
      createdAt: now - 86_400 * 8,
      updatedAt: now - 240,
    },
    {
      id: "desktop-gateway",
      platform: "claude_desktop",
      kind: "third_party",
      name: "Native gateway",
      accountLabel: "Anthropic Messages",
      baseUrl: "https://gateway.example.com",
      model: "claude-sonnet-5",
      secretKind: "api_key",
      hasSecret: true,
      profileHome: "/tmp/preview/desktop-gateway",
      status: "ready",
      createdAt: now - 86_400 * 2,
      updatedAt: now - 120,
    },
  ],
  platforms: [
    {
      platform: "codex",
      cliFound: true,
      cliPath: "/usr/local/bin/codex",
      cliVersion: "codex-cli 0.146.0",
      configRoot: "~/.codex",
      binding: {
        platform: "codex",
        globalProfileId: "codex-work",
        lastManagedProfileId: "codex-personal",
      },
    },
    {
      platform: "claude_code",
      cliFound: true,
      cliPath: "/usr/local/bin/claude",
      cliVersion: "Claude Code 2.1.220",
      configRoot: "~/.claude",
      binding: {
        platform: "claude_code",
        globalProfileId: null,
        lastManagedProfileId: "claude-personal",
      },
    },
    {
      platform: "claude_desktop",
      cliFound: true,
      cliPath: "/Applications/Claude.app/Contents/MacOS/Claude",
      cliVersion: "1.24012.9",
      configRoot: "~/Library/Application Support/Claude",
      binding: {
        platform: "claude_desktop",
        globalProfileId: null,
        lastManagedProfileId: "desktop-personal",
      },
    },
  ],
  settings: {
    language: "zh",
    theme: "auto",
    timezone: "Asia/Taipei",
    defaultActivationMode: "managed_launch",
    codexPath: null,
    claudePath: null,
    claudeDesktopPath: null,
    codexHome: null,
    claudeConfigDir: null,
    unifyCodexHistory: false,
    unifyClaudeCodeHistory: false,
    unifyClaudeDesktopCodeHistory: false,
    claudeDesktopHistoryTarget: null,
  },
};

const providerCredentials = new Map<string, string>([
  [
    "codex-personal",
    JSON.stringify(
      {
        format: "yaat.official-credential",
        version: 1,
        platform: "codex",
        storageKind: "codex.auth-json.v1",
        accountLabel: "haozi@example.com",
        credential: {
          auth_mode: "chatgpt",
          tokens: {
            access_token: "preview-access-token",
            refresh_token: "preview-refresh-token",
          },
        },
      },
      null,
      2,
    ),
  ],
  ["codex-openrouter", "sk-preview-openrouter"],
  ["desktop-gateway", "sk-preview-anthropic"],
]);

const copy = <T>(value: T): T => structuredClone(value);
const operation = async (message: string): Promise<OperationResult> => ({
  message,
  warning: null,
});

function profileFromRequest(
  request: CreateProviderRequest | ImportCurrentRequest,
): ProviderProfile {
  const create = "kind" in request ? request : null;
  return {
    id: crypto.randomUUID(),
    platform: request.platform,
    kind: create?.kind ?? "official_subscription",
    name: request.name,
    accountLabel: request.accountLabel,
    baseUrl: create?.baseUrl ?? null,
    model: create?.model ?? null,
    secretKind: create?.secretKind ?? "none",
    hasSecret: Boolean(create?.secret),
    profileHome: "/tmp/preview/new-profile",
    status:
      create?.kind === "official_subscription" && !create.officialCredential
        ? "needs_login"
        : "ready",
    createdAt: now,
    updatedAt: now,
  };
}

function usage(request: UsageQueryRequest): UsageReport {
  const start = new Date(`${request.startDate}T00:00:00`);
  const end = new Date(`${request.endDate}T00:00:00`);
  const weights = [
    0.56, 0.84, 0.42, 1.14, 0.92, 1.3, 0.74, 1.48, 1.02, 0.66, 1.2, 0.88,
  ];
  const requestedDays = Math.max(
    1,
    Math.min(
      366,
      Math.round((end.getTime() - start.getTime()) / 86_400_000) + 1,
    ),
  );
  const buckets = Array.from({ length: requestedDays }, (_, index) => {
    const weight = weights[index % weights.length];
    const date = new Date(start);
    date.setDate(start.getDate() + index);
    return {
      date: date.toISOString().slice(0, 10),
      tokens: {
        uncachedInput: Math.round(182_000 * weight),
        cacheRead: Math.round(410_000 * weight),
        cacheWrite: Math.round(36_000 * weight),
        output: Math.round(96_000 * weight),
        reasoningOutput: Math.round(42_000 * weight),
      },
      requestCount: Math.round(13 * weight),
    };
  });
  const totals = buckets.reduce(
    (sum, bucket) => ({
      uncachedInput: sum.uncachedInput + bucket.tokens.uncachedInput,
      cacheRead: sum.cacheRead + bucket.tokens.cacheRead,
      cacheWrite: sum.cacheWrite + bucket.tokens.cacheWrite,
      output: sum.output + bucket.tokens.output,
      reasoningOutput: sum.reasoningOutput + bucket.tokens.reasoningOutput,
    }),
    {
      uncachedInput: 0,
      cacheRead: 0,
      cacheWrite: 0,
      output: 0,
      reasoningOutput: 0,
    },
  );
  return {
    ...request,
    totals,
    requestCount: buckets.reduce((sum, bucket) => sum + bucket.requestCount, 0),
    buckets,
    diagnostics: {
      filesScanned: 46,
      malformedRecords: 0,
      duplicateRecords: 12,
      coverageStart: now - 86_400 * 30,
      coverageEnd: now,
      lastScannedAt: now - 20,
      isPartial: false,
    },
  };
}

export const previewApi = {
  operation,
  bootstrap: async () => copy(state),
  checkUpdate: async () => null,
  installUpdate: async (
    _onProgress: (progress: UpdateProgress) => void,
  ): Promise<void> => {},
  cancelUpdate: async (): Promise<void> => {},
  createProvider: async (request: CreateProviderRequest) => {
    const profile = profileFromRequest(request);
    state.profiles.push(profile);
    const credential =
      request.kind === "official_subscription"
        ? request.officialCredential
        : request.secret;
    if (credential) providerCredentials.set(profile.id, credential);
    return copy(profile);
  },
  updateProvider: async (request: UpdateProviderRequest) => {
    const profile = state.profiles.find((item) => item.id === request.id);
    if (!profile) throw new Error("Profile not found");
    Object.assign(profile, {
      name: request.name,
      accountLabel: request.accountLabel,
      baseUrl: request.baseUrl,
      model: request.model,
      secretKind: request.secretKind,
      hasSecret: profile.hasSecret || Boolean(request.replacementSecret),
    });
    const credential =
      profile.kind === "official_subscription"
        ? request.replacementOfficialCredential
        : request.replacementSecret;
    if (credential) providerCredentials.set(profile.id, credential);
    if (request.replacementOfficialCredential) profile.status = "ready";
    return copy(profile);
  },
  getProviderCredential: async (id: string) => ({
    credential: providerCredentials.get(id) ?? null,
  }),
  deleteProvider: async (id: string) => {
    state.profiles = state.profiles.filter((profile) => profile.id !== id);
    providerCredentials.delete(id);
    return operation("Profile deleted");
  },
  activateProvider: async (request: ActivateProviderRequest) => {
    const platform = state.platforms.find(
      (item) => item.platform === request.platform,
    );
    if (platform) {
      platform.binding.globalProfileId = request.profileId;
    }
    return operation("Profile activated");
  },
  deactivateGlobal: async (selected: import("@/types").Platform) => {
    const platform = state.platforms.find((item) => item.platform === selected);
    if (platform) platform.binding.globalProfileId = null;
    return operation("Global management stopped");
  },
  login: async (id: string) => {
    const profile = state.profiles.find((item) => item.id === id);
    if (profile) profile.status = "ready";
    return operation("Login opened");
  },
  capture: async (id: string) => {
    const profile = state.profiles.find((item) => item.id === id);
    if (!profile) throw new Error("Profile not found");
    profile.status = "ready";
    return operation("Credentials captured");
  },
  importCurrent: async (request: ImportCurrentRequest) => {
    const profile = profileFromRequest(request);
    profile.status = "ready";
    state.profiles.push(profile);
    return operation("Current account imported");
  },
  queryUsage: async (request: UsageQueryRequest) => usage(request),
  updateSettings: async (settings: AppSettings) => {
    state.settings = copy(settings);
    return copy(settings);
  },
  previewHistory: async (
    request: HistoryPreviewRequest,
  ): Promise<HistoryPreview> => ({
    scope: request.scope,
    groups:
      request.scope !== "claude_desktop_code"
        ? [
            {
              id: "global",
              label:
                request.scope === "codex"
                  ? "Codex 全局目录"
                  : "Claude Code 全局目录",
              rootKind: "global",
              isCurrent: true,
              sessionCount: 18,
            },
            {
              id: "profile:personal",
              label: "Personal",
              rootKind: "managed",
              isCurrent: false,
              sessionCount: 12,
            },
            {
              id: "profile:work",
              label: "Work account",
              rootKind: "managed",
              isCurrent: false,
              sessionCount: 7,
            },
          ]
        : [
            {
              id: "claude:11111111-1111-4111-8111-111111111111:aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
              label: "Claude · 11111111 / aaaaaaaa",
              rootKind: "claude",
              isCurrent: false,
              sessionCount: 9,
            },
            {
              id: "claude:22222222-2222-4222-8222-222222222222:bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
              label: "Claude · 22222222 / bbbbbbbb",
              rootKind: "claude",
              isCurrent: true,
              sessionCount: 4,
            },
          ],
    targetGroupId: request.targetGroupId,
    filesScanned: request.scope === "claude_desktop_code" ? 13 : 37,
    pendingCopies:
      request.scope === "claude_desktop_code"
        ? request.targetGroupId
          ? 9
          : 0
        : 38,
    metadataUpdates: request.scope === "codex" ? 18 : 0,
    identicalFiles: 7,
    conflicts: 0,
    invalidFiles: 0,
  }),
  applyHistory: async (
    request: HistoryApplyRequest,
  ): Promise<HistoryApplyResult> => ({
    scope: request.scope,
    copied: request.scope === "codex" ? 38 : 9,
    metadataUpdated: request.scope === "codex" ? 18 : 0,
    identicalFiles: 7,
    conflicts: 0,
    invalidFiles: 0,
  }),
};
