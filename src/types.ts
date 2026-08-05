/** IPC types mirror the serde camelCase contracts in `yaat-contracts`. */
export type Platform = "codex" | "claude_code" | "claude_desktop";
export type ProviderKind =
  "official_subscription" | "official_api" | "third_party";
export type ActivationMode = "managed_launch" | "global_credential";
export type SecretKind = "none" | "api_key" | "bearer_token";
export type ProfileStatus = "ready" | "needs_login";

export interface ProviderProfile {
  id: string;
  platform: Platform;
  kind: ProviderKind;
  name: string;
  accountLabel: string | null;
  baseUrl: string | null;
  model: string | null;
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
  cliFound: boolean;
  cliPath: string | null;
  cliVersion: string | null;
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
}

export type HistoryScope = "codex" | "claude_code" | "claude_desktop_code";

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

export interface ImportCurrentRequest {
  platform: Platform;
  name: string;
  accountLabel: string | null;
}

export interface UsageQueryRequest {
  platform: Platform;
  startDate: string;
  endDate: string;
  timezone: string;
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
  totals: TokenBreakdown;
  requestCount: number;
  buckets: UsageBucket[];
  diagnostics: UsageDiagnostics;
}

export const tokenInput = (tokens: TokenBreakdown) =>
  tokens.uncachedInput + tokens.cacheRead + tokens.cacheWrite;

export const tokenTotal = (tokens: TokenBreakdown) =>
  tokenInput(tokens) + tokens.output;
