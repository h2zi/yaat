/** IPC types mirror the serde camelCase contracts in `yaat-contracts`. */
export type Platform = "codex" | "claude_code" | "claude_desktop";
export type ProviderKind =
  "official_subscription" | "official_api" | "third_party";
export type ActivationMode = "managed_launch" | "global_credential";
export type SecretKind = "none" | "api_key" | "bearer_token";
export type ProfileStatus = "ready" | "needs_login";
export type CliStatus = "ready" | "version_unknown" | "invalid" | "missing";
export type ReasoningEffort =
  "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra";

export interface HeaderEntry {
  name: string;
  value: string;
}

export interface CodexCatalogModel {
  id: string;
  displayName: string;
  description: string;
  contextWindow: number;
  supportedReasoningEfforts: ReasoningEffort[];
  defaultReasoningEffort: ReasoningEffort;
  supportsImageInput: boolean;
  supportsImageOriginal: boolean;
  supportsParallelToolCalls: boolean;
  supportsReasoningSummaries: boolean;
  supportsSearchTool: boolean;
  supportsVerbosity: boolean;
}

export type ProviderPlatformConfig =
  | {
      platform: "codex";
      defaultModel: string | null;
      catalog: CodexCatalogModel[];
    }
  | {
      platform: "claude_code";
      defaultModel: string | null;
      sonnet: string | null;
      opus: string | null;
      haiku: string | null;
      fable: string | null;
      subagent: string | null;
    }
  | { platform: "claude_desktop"; models: string[] };

export interface ProviderProfile {
  id: string;
  platform: Platform;
  kind: ProviderKind;
  name: string;
  accountLabel: string | null;
  baseUrl: string | null;
  model: string | null;
  customHeaders: HeaderEntry[];
  userAgent: string | null;
  platformConfig: ProviderPlatformConfig;
  secretKind: SecretKind;
  hasSecret: boolean;
  profileHome: string | null;
  status: ProfileStatus;
  createdAt: number;
  updatedAt: number;
}

export interface PlatformBinding {
  platform: Platform;
  globalProfileId: string | null;
  lastManagedProfileId: string | null;
}

export interface PlatformState {
  platform: Platform;
  cliStatus: CliStatus;
  cliPath: string | null;
  cliVersion: string | null;
  cliError: string | null;
  configRoot: string;
  binding: PlatformBinding;
}

export interface AppSettings {
  language: string;
  theme: string;
  timezone: string;
  defaultActivationMode: ActivationMode;
  codexPath: string | null;
  claudePath: string | null;
  claudeDesktopPath: string | null;
  codexHome: string | null;
  claudeConfigDir: string | null;
  unifyCodexHistory: boolean;
  unifyClaudeCodeHistory: boolean;
  unifyClaudeDesktopCodeHistory: boolean;
  claudeDesktopHistoryTarget: string | null;
  usageRefreshIntervalSeconds: 0 | 5 | 10 | 30 | 60;
}

export type HistoryScope = "codex" | "claude_code" | "claude_desktop_code";
export type HistorySyncState =
  | "idle"
  | "queued"
  | "scanning"
  | "normalizing"
  | "completed"
  | "failed"
  | "cancelled";

export interface HistorySyncStatus {
  scope: HistoryScope;
  state: HistorySyncState;
  processedFiles: number;
  lastCompletedAt: number | null;
  errorSummary: string | null;
}

export interface HistoryGroup {
  id: string;
  label: string;
  rootKind: string;
  isCurrent: boolean;
  sessionCount: number;
}

export interface HistoryPreviewRequest {
  scope: HistoryScope;
  targetGroupId: string | null;
}

export interface HistoryPreview {
  scope: HistoryScope;
  groups: HistoryGroup[];
  targetGroupId: string | null;
  filesScanned: number;
  pendingCopies: number;
  metadataUpdates: number;
  identicalFiles: number;
  conflicts: number;
  invalidFiles: number;
}

export interface HistoryApplyRequest {
  scope: HistoryScope;
  targetGroupId: string | null;
}

export interface HistoryApplyResult {
  scope: HistoryScope;
  copied: number;
  metadataUpdated: number;
  identicalFiles: number;
  conflicts: number;
  invalidFiles: number;
}

export type OperationPhase = "discovering" | "processing" | "saving";

export interface OperationProgress {
  phase: OperationPhase;
  processed: number;
  total: number | null;
}

export interface OperationResult {
  message: string;
  warning: string | null;
}

export interface BootstrapResponse {
  profiles: ProviderProfile[];
  platforms: PlatformState[];
  settings: AppSettings;
  historySync: HistorySyncStatus[];
}

export interface ReleaseUpdate {
  currentVersion: string;
  latestVersion: string;
}

export type UpdatePhase = "downloading" | "installing";

export interface UpdateProgress {
  phase: UpdatePhase;
  downloaded: number;
  total: number | null;
}

export interface CreateProviderRequest {
  platform: Platform;
  kind: ProviderKind;
  name: string;
  accountLabel: string | null;
  baseUrl: string | null;
  model: string | null;
  customHeaders: HeaderEntry[];
  userAgent: string | null;
  platformConfig: ProviderPlatformConfig;
  secretKind: SecretKind;
  secret: string | null;
  officialCredential: string | null;
}

export interface UpdateProviderRequest {
  id: string;
  name: string;
  accountLabel: string | null;
  baseUrl: string | null;
  model: string | null;
  customHeaders: HeaderEntry[];
  userAgent: string | null;
  platformConfig: ProviderPlatformConfig;
  secretKind: SecretKind;
  replacementSecret: string | null;
  replacementOfficialCredential: string | null;
}

export interface ProviderCredentialResponse {
  credential: string | null;
}

export interface ActivateProviderRequest {
  platform: Platform;
  profileId: string;
  mode: ActivationMode;
}

export type ProviderImportSource = "active_config" | "official_credential";
export type ProviderImportCredentialState =
  "ready" | "needs_input" | "unsupported_helper";

export interface ProviderImportPreviewRequest {
  platform: Platform;
}

export interface ProviderImportCandidate {
  candidateId: string;
  source: ProviderImportSource;
  active: boolean;
  kind: ProviderKind;
  name: string;
  accountLabel: string | null;
  baseUrl: string | null;
  model: string | null;
  customHeaders: HeaderEntry[];
  userAgent: string | null;
  platformConfig: ProviderPlatformConfig;
  secretKind: SecretKind;
  credentialState: ProviderImportCredentialState;
  alreadyImportedProviderId: string | null;
  warnings: string[];
}

export interface ProviderImportPreview {
  platform: Platform;
  sourceRevision: string;
  candidates: ProviderImportCandidate[];
  warnings: string[];
}

export interface ProviderImportSelection {
  candidateId: string;
  provider: CreateProviderRequest;
}

export interface ProviderImportCommitRequest {
  platform: Platform;
  sourceRevision: string;
  selections: ProviderImportSelection[];
}

export interface ProviderImportResult {
  profiles: ProviderProfile[];
}

export interface UsageQueryRequest {
  platform: Platform;
  startDate: string;
  endDate: string;
  timezone: string;
  model: string | null;
}

export interface TokenBreakdown {
  uncachedInput: number;
  cacheRead: number;
  cacheWrite: number;
  output: number;
  reasoningOutput: number;
}

export interface UsageBucket {
  date: string;
  tokens: TokenBreakdown;
  requestCount: number;
}

export interface UsageDiagnostics {
  filesScanned: number;
  malformedRecords: number;
  duplicateRecords: number;
  coverageStart: number | null;
  coverageEnd: number | null;
  lastScannedAt: number | null;
  isPartial: boolean;
}

export interface UsageReport {
  platform: Platform;
  startDate: string;
  endDate: string;
  timezone: string;
  selectedModel: string | null;
  availableModels: string[];
  totals: TokenBreakdown;
  cacheHitTokens: number;
  cacheHitRate: number;
  requestCount: number;
  buckets: UsageBucket[];
  diagnostics: UsageDiagnostics;
}

export interface ModelFetchRequest {
  platform: Platform;
  baseUrl: string;
  secretKind: SecretKind;
  credential: string;
  customHeaders: HeaderEntry[];
  userAgent: string | null;
}

export interface FetchedModel {
  id: string;
  ownedBy: string | null;
  directCompatible: boolean;
  warning: string | null;
}

export interface ModelFetchResponse {
  models: FetchedModel[];
}

export function emptyPlatformConfig(
  platform: Platform,
): ProviderPlatformConfig {
  if (platform === "codex") {
    return { platform, defaultModel: null, catalog: [] };
  }
  if (platform === "claude_code") {
    return {
      platform,
      defaultModel: null,
      sonnet: null,
      opus: null,
      haiku: null,
      fable: null,
      subagent: null,
    };
  }
  return { platform, models: [] };
}

export const tokenInput = (tokens: TokenBreakdown) =>
  tokens.uncachedInput + tokens.cacheRead + tokens.cacheWrite;

export const tokenTotal = (tokens: TokenBreakdown) =>
  tokenInput(tokens) + tokens.output;
