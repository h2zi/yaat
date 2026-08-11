import { Channel, invoke } from "@tauri-apps/api/core";

import { previewApi } from "@/preview";
import type {
  ActivateProviderRequest,
  AppSettings,
  BootstrapResponse,
  CreateProviderRequest,
  ModelFetchRequest,
  ModelFetchResponse,
  HistoryApplyRequest,
  HistoryApplyResult,
  HistoryPreview,
  HistoryPreviewRequest,
  HistorySyncStatus,
  OperationProgress,
  OperationResult,
  Platform,
  ProviderCredentialResponse,
  ProviderImportCommitRequest,
  ProviderImportPreview,
  ProviderImportPreviewRequest,
  ProviderImportResult,
  ProviderProfile,
  ReleaseUpdate,
  UpdateProgress,
  UpdateProviderRequest,
  UsageQueryRequest,
  UsageReport,
} from "@/types";

const preview = import.meta.env.VITE_YAAT_PREVIEW === "1";

const commands = {
  bootstrap: "bootstrap",
  checkUpdate: "app_update_check",
  installUpdate: "app_update_install",
  cancelUpdate: "app_update_cancel",
  createProvider: "provider_create",
  updateProvider: "provider_update",
  getProviderCredential: "provider_credential_get",
  fetchProviderModels: "provider_models_fetch",
  deleteProvider: "provider_delete",
  activateProvider: "provider_activate",
  deactivateGlobal: "provider_global_deactivate",
  login: "provider_login",
  capture: "provider_capture",
  previewProviderImport: "provider_import_preview",
  commitProviderImport: "provider_import_commit",
  launch: "profile_launch",
  queryUsage: "usage_query",
  rescanUsage: "usage_rescan",
  cancelUsage: "usage_cancel",
  updateSettings: "settings_update",
  previewHistory: "history_preview",
  applyHistory: "history_apply",
  cancelHistory: "history_cancel",
  historySyncStatus: "history_sync_status",
} as const;

type CommandName = (typeof commands)[keyof typeof commands];

export class ApiRequestError extends Error {
  constructor(
    message: string,
    readonly code: string,
  ) {
    super(message);
    this.name = "ApiRequestError";
  }
}

function ipcError(error: unknown): Error {
  if (typeof error === "object" && error !== null && "message" in error) {
    return new ApiRequestError(
      String(error.message),
      "code" in error ? String(error.code) : "ipc",
    );
  }
  return new Error(
    typeof error === "string" ? error : "YAAT IPC request failed",
  );
}

async function request<T>(command: CommandName, payload?: unknown): Promise<T> {
  try {
    return await invoke<T>(
      command,
      payload === undefined ? undefined : { request: payload },
    );
  } catch (error) {
    throw ipcError(error);
  }
}

async function requestWithProgress<T>(
  command: CommandName,
  payload: unknown,
  onProgress: (progress: OperationProgress) => void,
): Promise<T> {
  const channel = new Channel<OperationProgress>();
  channel.onmessage = onProgress;
  try {
    return await invoke<T>(command, {
      request: payload,
      onProgress: channel,
    });
  } catch (error) {
    throw ipcError(error);
  }
}

export const api = {
  bootstrap: (): Promise<BootstrapResponse> =>
    preview ? previewApi.bootstrap() : request(commands.bootstrap),

  checkUpdate: (): Promise<ReleaseUpdate | null> =>
    preview ? previewApi.checkUpdate() : request(commands.checkUpdate),

  installUpdate: (
    onProgress: (progress: UpdateProgress) => void,
  ): Promise<void> => {
    if (preview) return previewApi.installUpdate(onProgress);
    const channel = new Channel<UpdateProgress>();
    channel.onmessage = onProgress;
    return invoke<void>(commands.installUpdate, { onProgress: channel }).catch(
      (error) => {
        throw ipcError(error);
      },
    );
  },

  cancelUpdate: (): Promise<void> =>
    preview ? previewApi.cancelUpdate() : request(commands.cancelUpdate),

  createProvider: (value: CreateProviderRequest): Promise<ProviderProfile> =>
    preview
      ? previewApi.createProvider(value)
      : request(commands.createProvider, value),

  updateProvider: (value: UpdateProviderRequest): Promise<ProviderProfile> =>
    preview
      ? previewApi.updateProvider(value)
      : request(commands.updateProvider, value),

  getProviderCredential: (id: string): Promise<ProviderCredentialResponse> =>
    preview
      ? previewApi.getProviderCredential(id)
      : request(commands.getProviderCredential, { id }),

  fetchProviderModels: (
    value: ModelFetchRequest,
  ): Promise<ModelFetchResponse> =>
    preview
      ? previewApi.fetchProviderModels(value)
      : request(commands.fetchProviderModels, value),

  deleteProvider: (id: string): Promise<OperationResult> =>
    preview
      ? previewApi.deleteProvider(id)
      : request(commands.deleteProvider, { id }),

  activateProvider: (
    value: ActivateProviderRequest,
  ): Promise<OperationResult> =>
    preview
      ? previewApi.activateProvider(value)
      : request(commands.activateProvider, value),

  deactivateGlobal: (platform: Platform): Promise<OperationResult> =>
    preview
      ? previewApi.deactivateGlobal(platform)
      : request(commands.deactivateGlobal, { platform }),

  login: (profileId: string): Promise<OperationResult> =>
    preview
      ? previewApi.login(profileId)
      : request(commands.login, { profileId, console: false }),

  capture: (profileId: string): Promise<OperationResult> =>
    preview
      ? previewApi.capture(profileId)
      : request(commands.capture, { profileId }),

  previewProviderImport: (
    value: ProviderImportPreviewRequest,
  ): Promise<ProviderImportPreview> =>
    preview
      ? previewApi.previewProviderImport(value)
      : request(commands.previewProviderImport, value),

  commitProviderImport: (
    value: ProviderImportCommitRequest,
  ): Promise<ProviderImportResult> =>
    preview
      ? previewApi.commitProviderImport(value)
      : request(commands.commitProviderImport, value),

  launch: (
    platform: Platform,
    profileId: string,
    cwd: string | null,
  ): Promise<OperationResult> =>
    preview
      ? previewApi.operation("Managed Codex launched")
      : request(commands.launch, { platform, profileId, cwd }),

  queryUsage: (
    value: UsageQueryRequest,
    onProgress: (progress: OperationProgress) => void,
  ): Promise<UsageReport> =>
    preview
      ? previewApi.queryUsage(value)
      : requestWithProgress(commands.queryUsage, value, onProgress),

  rescanUsage: (
    platform: Platform,
    onProgress: (progress: OperationProgress) => void,
  ): Promise<OperationResult> =>
    preview
      ? previewApi.operation("Local usage rescanned")
      : requestWithProgress(commands.rescanUsage, { platform }, onProgress),

  cancelUsage: (): Promise<void> =>
    preview ? Promise.resolve() : request(commands.cancelUsage),

  updateSettings: (value: AppSettings): Promise<AppSettings> =>
    preview
      ? previewApi.updateSettings(value)
      : request(commands.updateSettings, value),

  previewHistory: (
    value: HistoryPreviewRequest,
    onProgress: (progress: OperationProgress) => void,
  ): Promise<HistoryPreview> =>
    preview
      ? previewApi.previewHistory(value)
      : requestWithProgress(commands.previewHistory, value, onProgress),

  applyHistory: (
    value: HistoryApplyRequest,
    onProgress: (progress: OperationProgress) => void,
  ): Promise<HistoryApplyResult> =>
    preview
      ? previewApi.applyHistory(value)
      : requestWithProgress(commands.applyHistory, value, onProgress),

  cancelHistory: (): Promise<void> =>
    preview ? Promise.resolve() : request(commands.cancelHistory),

  historySyncStatus: (): Promise<HistorySyncStatus[]> =>
    preview
      ? previewApi.historySyncStatus()
      : request(commands.historySyncStatus),
};
