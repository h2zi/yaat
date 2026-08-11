import {
  lazy,
  Suspense,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import {
  AlertCircle,
  AppWindow,
  Bot,
  CheckCircle2,
  Download,
  FolderOpen,
  LoaderCircle,
  Plus,
  PowerOff,
  Settings,
  ShieldCheck,
  Sparkles,
  UsersRound,
} from "lucide-react";
import { Toaster, toast } from "sonner";

import { AccountCard } from "@/components/account-card";
import {
  ProviderDialog,
  type ProviderDialogMode,
} from "@/components/provider-dialog";
import { ProviderImportDialog } from "@/components/provider-import-dialog";
import { SettingsDialog } from "@/components/settings-dialog";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Skeleton } from "@/components/ui/skeleton";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { ApiRequestError, api } from "@/api";
import { translate, type Language, type TranslationKey } from "@/i18n";
import { cn } from "@/lib/utils";
import type {
  ActivationMode,
  AppSettings,
  BootstrapResponse,
  CreateProviderRequest,
  OperationResult,
  OperationProgress,
  Platform,
  ProviderImportCommitRequest,
  ProviderProfile,
  ReleaseUpdate,
  UpdateProgress,
  UpdateProviderRequest,
  UsageReport,
} from "@/types";

type MainTab = "accounts" | "usage";
type DialogState =
  | {
      mode: ProviderDialogMode;
      profile?: ProviderProfile | null;
    }
  | { mode: "import" }
  | null;

const UsageDashboard = lazy(() =>
  import("@/components/usage-dashboard").then((module) => ({
    default: module.UsageDashboard,
  })),
);

function detectedLanguage(): Language {
  return navigator.language.toLowerCase().startsWith("zh") ? "zh" : "en";
}

function resolvedLanguage(value: string): Language {
  if (value === "zh" || value === "en") return value;
  return detectedLanguage();
}

function isOperationResult(value: unknown): value is OperationResult {
  return (
    typeof value === "object" &&
    value !== null &&
    "message" in value &&
    "warning" in value
  );
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function applyTheme(theme: string) {
  const dark =
    theme === "dark" ||
    (theme === "auto" &&
      window.matchMedia("(prefers-color-scheme: dark)").matches);
  document.documentElement.classList.toggle("dark", dark);
  document.documentElement.dataset.theme = theme;
}

export default function App() {
  const [platform, setPlatform] = useState<Platform>("codex");
  const [tab, setTab] = useState<MainTab>("accounts");
  const [data, setData] = useState<BootstrapResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [busyAction, setBusyAction] = useState<string | null>(null);
  const [usageMounted, setUsageMounted] = useState(false);
  const [language, setLanguage] = useState<Language>(detectedLanguage);
  const [activationMode, setActivationMode] =
    useState<ActivationMode>("managed_launch");
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [providerDialog, setProviderDialog] = useState<DialogState>(null);
  const [pendingDelete, setPendingDelete] = useState<ProviderProfile | null>(
    null,
  );
  const [pendingSwitch, setPendingSwitch] = useState<ProviderProfile | null>(
    null,
  );
  const [pendingLaunch, setPendingLaunch] = useState<ProviderProfile | null>(
    null,
  );
  const [launchCwd, setLaunchCwd] = useState("");
  const [pendingDeactivate, setPendingDeactivate] = useState(false);
  const [usage, setUsage] = useState<UsageReport | null>(null);
  const [usageLoading, setUsageLoading] = useState(false);
  const [usageProgress, setUsageProgress] = useState<OperationProgress | null>(
    null,
  );
  const [availableUpdate, setAvailableUpdate] = useState<ReleaseUpdate | null>(
    null,
  );
  const [updateProgress, setUpdateProgress] = useState<UpdateProgress | null>(
    null,
  );
  const [updateBusy, setUpdateBusy] = useState(false);
  const [updateCancelling, setUpdateCancelling] = useState(false);
  const bootstrapped = useRef(false);
  const updateChecked = useRef(false);

  const t = useCallback(
    (key: TranslationKey) => translate(language, key),
    [language],
  );

  const refresh = useCallback(
    async (initial = false) => {
      if (initial) setLoading(true);
      try {
        const next = await api.bootstrap();
        setData(next);
        setLanguage(resolvedLanguage(next.settings.language));
        if (initial) setActivationMode(next.settings.defaultActivationMode);
        applyTheme(next.settings.theme);
      } catch (requestError) {
        toast.error(t("errorTitle"), {
          description: errorMessage(requestError),
        });
      } finally {
        if (initial) setLoading(false);
      }
    },
    [t],
  );

  useEffect(() => {
    if (bootstrapped.current) return;
    bootstrapped.current = true;
    void refresh(true);
  }, [refresh]);

  useEffect(() => {
    const media = window.matchMedia("(prefers-color-scheme: dark)");
    const update = () => {
      if (data?.settings.theme === "auto") applyTheme("auto");
    };
    media.addEventListener("change", update);
    return () => media.removeEventListener("change", update);
  }, [data?.settings.theme]);

  useEffect(() => {
    if (!data || updateChecked.current) return;
    updateChecked.current = true;
    void api
      .checkUpdate()
      .then(setAvailableUpdate)
      .catch(() => undefined);
  }, [data]);

  const platformState =
    data?.platforms.find((item) => item.platform === platform) ?? null;
  const platformExecutableReady = platformState?.cliStatus === "ready";
  const platformExecutableUnknown =
    platformState?.cliStatus === "version_unknown";
  const platformDiagnostic = platformState
    ? [
        platformState.cliVersion ||
          platformState.cliPath ||
          (platform === "claude_desktop" ? "Claude Desktop" : "CLI"),
        platformState.cliStatus === "ready" ? null : platformState.cliError,
        platformState.configRoot,
      ]
        .filter(Boolean)
        .join(" · ")
    : null;
  const platformUsage = usage?.platform === platform ? usage : null;
  const profiles = useMemo(
    () =>
      data?.profiles.filter((profile) => profile.platform === platform) ?? [],
    [data?.profiles, platform],
  );
  const perform = useCallback(
    async (
      actionId: string,
      action: () => Promise<unknown>,
      success?: string,
      after?: () => void,
    ) => {
      setBusyAction(actionId);
      try {
        const result = await action();
        if (isOperationResult(result)) {
          toast.success(
            success ?? result.message,
            success ? { description: result.message } : undefined,
          );
          if (result.warning) toast.warning(result.warning);
        } else if (success) {
          toast.success(success);
        }
        after?.();
        await refresh();
      } catch (requestError) {
        const message = errorMessage(requestError);
        toast.error(t("errorTitle"), { description: message });
      } finally {
        setBusyAction(null);
      }
    },
    [refresh, t],
  );

  const createProvider = async (request: CreateProviderRequest) => {
    await perform(
      "provider_save",
      () => api.createProvider(request),
      t("saved"),
      () => setProviderDialog(null),
    );
  };
  const updateProvider = async (request: UpdateProviderRequest) => {
    await perform(
      "provider_save",
      () => api.updateProvider(request),
      t("saved"),
      () => setProviderDialog(null),
    );
  };
  const commitProviderImport = async (request: ProviderImportCommitRequest) => {
    await perform(
      "provider_import",
      () => api.commitProviderImport(request),
      t("imported"),
      () => setProviderDialog(null),
    );
  };
  const loadProviderCredential = useCallback(async (profileId: string) => {
    const response = await api.getProviderCredential(profileId);
    return response.credential;
  }, []);

  const executeSwitch = async (profile: ProviderProfile) => {
    await perform(
      "switch",
      () =>
        api.activateProvider({
          platform: profile.platform,
          profileId: profile.id,
          mode: "global_credential",
        }),
      t("switched"),
      () => setPendingSwitch(null),
    );
  };

  const requestSwitch = (profile: ProviderProfile) => {
    setPendingSwitch(profile);
  };

  const requestLaunch = (profile: ProviderProfile) => {
    if (profile.platform === "claude_desktop") {
      void perform(
        `launch:${profile.id}`,
        () => api.launch(profile.platform, profile.id, null),
        t("launched"),
      );
      return;
    }
    setLaunchCwd("");
    setPendingLaunch(profile);
  };

  const executeLaunch = async () => {
    if (!pendingLaunch) return;
    const profile = pendingLaunch;
    await perform(
      "launch",
      () => api.launch(profile.platform, profile.id, launchCwd.trim()),
      t("launched"),
      () => setPendingLaunch(null),
    );
  };

  const deleteProvider = async () => {
    if (!pendingDelete) return;
    const profile = pendingDelete;
    await perform(
      "delete",
      () => api.deleteProvider(profile.id),
      undefined,
      () => setPendingDelete(null),
    );
  };

  const queryUsage = useCallback(
    async (
      startDate: string,
      endDate: string,
      rescan = false,
      model: string | null = null,
    ) => {
      const showProgress = rescan || platformUsage === null;
      if (showProgress) {
        setUsageLoading(true);
        setUsageProgress({ phase: "discovering", processed: 0, total: null });
      }
      try {
        if (rescan) await api.rescanUsage(platform, setUsageProgress);
        const report = await api.queryUsage(
          {
            platform,
            startDate,
            endDate,
            timezone: data?.settings.timezone ?? "UTC",
            model,
          },
          setUsageProgress,
        );
        setUsage(report);
      } catch (requestError) {
        if (
          !(requestError instanceof ApiRequestError) ||
          requestError.code !== "cancelled"
        ) {
          const message = errorMessage(requestError);
          toast.error(t("errorTitle"), { description: message });
        }
      } finally {
        if (showProgress) setUsageLoading(false);
      }
    },
    [data?.settings.timezone, platform, platformUsage, t],
  );

  const cancelUsage = useCallback(async () => {
    await api.cancelUsage();
  }, []);

  const saveSettings = async (settings: AppSettings, close = true) => {
    setBusyAction("settings");
    try {
      const saved = await api.updateSettings(settings);
      if (close) {
        setLanguage(resolvedLanguage(saved.language));
        setActivationMode(saved.defaultActivationMode);
        applyTheme(saved.theme);
        setSettingsOpen(false);
        toast.success(t("saved"));
        await refresh();
      } else {
        setData((current) =>
          current ? { ...current, settings: saved } : current,
        );
      }
      return true;
    } catch (requestError) {
      const message = errorMessage(requestError);
      toast.error(t("errorTitle"), { description: message });
      return false;
    } finally {
      setBusyAction(null);
    }
  };

  const installUpdate = async () => {
    setUpdateBusy(true);
    setUpdateCancelling(false);
    setUpdateProgress({ phase: "downloading", downloaded: 0, total: null });
    try {
      await api.installUpdate(setUpdateProgress);
      setAvailableUpdate(null);
    } catch (requestError) {
      if (
        requestError instanceof ApiRequestError &&
        requestError.code === "cancelled"
      ) {
        setAvailableUpdate(null);
      } else {
        toast.error(t("errorTitle"), {
          description: errorMessage(requestError),
        });
        try {
          setAvailableUpdate(await api.checkUpdate());
        } catch {
          setAvailableUpdate(null);
        }
      }
    } finally {
      setUpdateBusy(false);
      setUpdateCancelling(false);
      setUpdateProgress(null);
    }
  };

  const cancelUpdate = async () => {
    setUpdateCancelling(true);
    try {
      await api.cancelUpdate();
    } catch (requestError) {
      setUpdateCancelling(false);
      toast.error(t("errorTitle"), {
        description: errorMessage(requestError),
      });
    }
  };

  const updatePercent =
    updateProgress?.phase === "downloading" && updateProgress.total
      ? Math.min(
          100,
          Math.round((updateProgress.downloaded / updateProgress.total) * 100),
        )
      : null;

  return (
    <TooltipProvider delayDuration={350}>
      <Tabs
        value={tab}
        onValueChange={(value) => {
          const next = value as MainTab;
          if (next === "usage") setUsageMounted(true);
          setTab(next);
        }}
        className="min-h-screen"
      >
        <header className="sticky top-0 z-40 border-b border-border/80 bg-background/88 backdrop-blur-xl">
          <div className="mx-auto flex h-16 max-w-[1360px] items-center gap-3 px-5 lg:px-8">
            <div className="flex items-center">
              <Select
                value={platform}
                onValueChange={(value) => {
                  const next = value as Platform;
                  if (next === "claude_desktop") setTab("accounts");
                  setPlatform(next);
                }}
              >
                <SelectTrigger className="h-9 min-w-[152px] border-transparent bg-muted/75 pl-2.5 shadow-none hover:border-transparent hover:bg-muted">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent align="start" className="min-w-[190px]">
                  <SelectItem value="codex">
                    <span className="flex items-center gap-2.5">
                      <span className="grid size-6 place-items-center rounded-md bg-indigo-500/10 text-indigo-600 dark:text-indigo-400">
                        <Bot className="size-3.5" />
                      </span>
                      <span>Codex</span>
                    </span>
                  </SelectItem>
                  <SelectItem value="claude_code">
                    <span className="flex items-center gap-2.5">
                      <span className="grid size-6 place-items-center rounded-md bg-orange-500/10 text-orange-700 dark:text-orange-400">
                        <Sparkles className="size-3.5" />
                      </span>
                      <span>Claude Code</span>
                    </span>
                  </SelectItem>
                  <SelectItem value="claude_desktop">
                    <span className="flex items-center gap-2.5">
                      <span className="grid size-6 place-items-center rounded-md bg-violet-500/10 text-violet-700 dark:text-violet-400">
                        <AppWindow className="size-3.5" />
                      </span>
                      <span>Claude Desktop</span>
                    </span>
                  </SelectItem>
                </SelectContent>
              </Select>
            </div>

            <TabsList className="ml-2">
              <TabsTrigger value="accounts">{t("accounts")}</TabsTrigger>
              {platform !== "claude_desktop" ? (
                <TabsTrigger value="usage">{t("usage")}</TabsTrigger>
              ) : null}
            </TabsList>

            <div className="ml-auto flex items-center gap-2">
              <Tooltip>
                <TooltipTrigger asChild>
                  <Button
                    variant="ghost"
                    size="icon"
                    aria-label={t("settings")}
                    onClick={() => setSettingsOpen(true)}
                  >
                    <Settings />
                  </Button>
                </TooltipTrigger>
                <TooltipContent>{t("settings")}</TooltipContent>
              </Tooltip>
            </div>
          </div>
        </header>

        <TabsContent value="accounts">
          <main className="mx-auto w-full max-w-[1240px] px-6 pb-10 pt-7 lg:px-8">
            <div className="flex flex-col justify-between gap-5 md:flex-row md:items-start">
              <div>
                <div className="flex flex-wrap items-center gap-2">
                  <h1 className="text-2xl font-semibold tracking-[-0.035em]">
                    {t("accountsTitle")}
                  </h1>
                  {platformState ? (
                    <Badge
                      variant={platformExecutableReady ? "success" : "warning"}
                      title={platformState.cliError ?? undefined}
                    >
                      <span
                        className={cn(
                          "size-1.5 rounded-full",
                          platformExecutableReady
                            ? "bg-emerald-500"
                            : platformExecutableUnknown
                              ? "bg-amber-500"
                              : "bg-destructive",
                        )}
                      />
                      {t(
                        platform === "claude_desktop"
                          ? platformExecutableReady
                            ? "appReady"
                            : platformExecutableUnknown
                              ? "appVersionUnknown"
                              : platformState.cliStatus === "invalid"
                                ? "appInvalid"
                                : "appMissing"
                          : platformExecutableReady
                            ? "cliReady"
                            : platformExecutableUnknown
                              ? "cliVersionUnknown"
                              : platformState.cliStatus === "invalid"
                                ? "cliInvalid"
                                : "cliMissing",
                      )}
                    </Badge>
                  ) : null}
                </div>
                {platformState ? (
                  <p
                    className="mt-2 truncate text-xs text-muted-foreground/75"
                    title={platformDiagnostic ?? undefined}
                  >
                    {platformDiagnostic}
                  </p>
                ) : null}
              </div>

              <div className="flex flex-wrap items-center gap-2">
                <div className="mr-1 flex h-9 items-center rounded-lg border border-border bg-card p-1 shadow-xs">
                  {(["managed_launch", "global_credential"] as const).map(
                    (mode) => (
                      <button
                        key={mode}
                        type="button"
                        onClick={() => setActivationMode(mode)}
                        className={cn(
                          "h-7 rounded-md px-2.5 text-xs font-medium transition-colors",
                          activationMode === mode
                            ? "bg-muted text-foreground"
                            : "text-muted-foreground hover:text-foreground",
                        )}
                      >
                        {t(
                          mode === "managed_launch"
                            ? "managedLaunch"
                            : "globalCredential",
                        )}
                      </button>
                    ),
                  )}
                </div>
                {platformState?.binding.globalProfileId ? (
                  <Button
                    variant="outline"
                    onClick={() => setPendingDeactivate(true)}
                  >
                    <PowerOff />
                    {t("stopGlobal")}
                  </Button>
                ) : null}
                <Button
                  variant="secondary"
                  onClick={() => setProviderDialog({ mode: "import" })}
                >
                  <Download />
                  {t("importCurrent")}
                </Button>
                <Button onClick={() => setProviderDialog({ mode: "create" })}>
                  <Plus />
                  {t("addAccount")}
                </Button>
              </div>
            </div>

            {loading ? (
              <div className="app-grid mt-7 gap-4">
                {[0, 1, 2].map((item) => (
                  <Skeleton key={item} className="h-[216px] rounded-xl" />
                ))}
              </div>
            ) : profiles.length === 0 ? (
              <Card className="subtle-grid mt-7 overflow-hidden border-dashed">
                <CardContent className="grid min-h-72 place-items-center p-8 text-center">
                  <div>
                    <div className="mx-auto grid size-12 place-items-center rounded-2xl border border-primary/15 bg-primary/9 text-primary">
                      <UsersRound className="size-5" />
                    </div>
                    <h2 className="mt-4 text-base font-semibold">
                      {t("emptyTitle")}
                    </h2>
                    <Button
                      className="mt-5"
                      onClick={() => setProviderDialog({ mode: "create" })}
                    >
                      <Plus />
                      {t("addAccount")}
                    </Button>
                  </div>
                </CardContent>
              </Card>
            ) : (
              <div className="app-grid mt-7 gap-4">
                {profiles.map((profile) => {
                  const selectedMode: ActivationMode = activationMode;
                  const cardBusy =
                    busyAction === `login:${profile.id}` ||
                    busyAction === `capture:${profile.id}` ||
                    busyAction === `launch:${profile.id}`;
                  const active =
                    selectedMode === "global_credential"
                      ? platformState?.binding.globalProfileId === profile.id
                      : platformState?.binding.lastManagedProfileId ===
                        profile.id;
                  return (
                    <AccountCard
                      key={profile.id}
                      profile={profile}
                      active={active}
                      activeMode={selectedMode}
                      busy={cardBusy}
                      busyAction={
                        busyAction === `login:${profile.id}`
                          ? "login"
                          : busyAction === `capture:${profile.id}`
                            ? "capture"
                            : busyAction === `launch:${profile.id}`
                              ? "launch"
                              : null
                      }
                      text={t}
                      onSwitch={() => requestSwitch(profile)}
                      onLaunch={() => requestLaunch(profile)}
                      onLogin={() =>
                        void perform(`login:${profile.id}`, () =>
                          api.login(profile.id),
                        )
                      }
                      onCapture={() =>
                        void perform(
                          `capture:${profile.id}`,
                          () => api.capture(profile.id),
                          t("saved"),
                        )
                      }
                      onEdit={() =>
                        setProviderDialog({ mode: "edit", profile })
                      }
                      onDelete={() => setPendingDelete(profile)}
                    />
                  );
                })}
              </div>
            )}
          </main>
        </TabsContent>

        {platform !== "claude_desktop" ? (
          <TabsContent
            value="usage"
            forceMount={usageMounted ? true : undefined}
            className="data-[state=inactive]:hidden"
          >
            <Suspense
              fallback={
                <div className="grid min-h-[560px] place-items-center text-sm text-muted-foreground">
                  <div className="flex items-center gap-2">
                    <LoaderCircle className="size-4 animate-spin text-primary" />
                    {t("loading")}
                  </div>
                </div>
              }
            >
              <UsageDashboard
                platform={platform}
                timezone={data?.settings.timezone ?? "UTC"}
                language={language}
                text={t}
                loading={usageLoading}
                progress={usageProgress}
                report={platformUsage}
                active={tab === "usage"}
                refreshIntervalSeconds={
                  data?.settings.usageRefreshIntervalSeconds ?? 0
                }
                onQuery={queryUsage}
                onRefreshIntervalChange={async (seconds) => {
                  if (!data) return;
                  await saveSettings(
                    {
                      ...data.settings,
                      usageRefreshIntervalSeconds: seconds,
                    },
                    false,
                  );
                }}
                onCancel={cancelUsage}
              />
            </Suspense>
          </TabsContent>
        ) : null}
      </Tabs>

      {data ? (
        <SettingsDialog
          open={settingsOpen}
          onOpenChange={setSettingsOpen}
          settings={data.settings}
          historySync={data.historySync}
          language={language}
          busy={busyAction === "settings"}
          onPreviewTheme={applyTheme}
          onSave={saveSettings}
        />
      ) : null}

      <AlertDialog
        open={availableUpdate !== null}
        onOpenChange={(open) => {
          if (!open && !updateBusy) setAvailableUpdate(null);
        }}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <div className="mb-1 grid size-10 place-items-center rounded-xl bg-primary/10 text-primary">
              {updateBusy ? (
                <LoaderCircle className="size-[18px] animate-spin" />
              ) : (
                <Download className="size-[18px]" />
              )}
            </div>
            <AlertDialogTitle>
              {t("updateAvailable")} {availableUpdate?.latestVersion}
            </AlertDialogTitle>
            <AlertDialogDescription>
              {t("updateDescription")} {t("updateCurrent")}{" "}
              {availableUpdate?.currentVersion}
            </AlertDialogDescription>
          </AlertDialogHeader>

          {updateBusy && updateProgress ? (
            <div className="grid gap-2 rounded-xl border border-border bg-muted/35 p-4">
              <div className="flex items-center justify-between gap-3 text-sm">
                <span className="font-medium">
                  {t(
                    updateProgress.phase === "installing"
                      ? "updateInstalling"
                      : "updateDownloading",
                  )}
                </span>
                {updatePercent !== null ? (
                  <span className="tabular-nums text-muted-foreground">
                    {updatePercent}%
                  </span>
                ) : null}
              </div>
              <div className="h-1.5 overflow-hidden rounded-full bg-muted">
                <div
                  className={cn(
                    "h-full rounded-full bg-primary transition-[width] duration-200",
                    updatePercent === null && "w-1/3 animate-pulse",
                  )}
                  style={
                    updatePercent === null
                      ? undefined
                      : { width: `${updatePercent}%` }
                  }
                />
              </div>
            </div>
          ) : null}

          <AlertDialogFooter>
            {updateBusy ? (
              updateProgress?.phase === "downloading" ? (
                <Button
                  variant="ghost"
                  disabled={updateCancelling}
                  onClick={() => void cancelUpdate()}
                >
                  {updateCancelling ? (
                    <LoaderCircle className="animate-spin" />
                  ) : null}
                  {t(updateCancelling ? "cancelling" : "cancel")}
                </Button>
              ) : null
            ) : (
              <>
                <Button
                  variant="ghost"
                  onClick={() => setAvailableUpdate(null)}
                >
                  {t("updateLater")}
                </Button>
                <Button onClick={() => void installUpdate()}>
                  <Download />
                  {t("updateInstall")}
                </Button>
              </>
            )}
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>

      <ProviderDialog
        open={providerDialog !== null && providerDialog.mode !== "import"}
        onOpenChange={(open) => {
          if (!open) setProviderDialog(null);
        }}
        mode={providerDialog?.mode === "edit" ? "edit" : "create"}
        platform={platform}
        profile={
          providerDialog && providerDialog.mode !== "import"
            ? providerDialog.profile
            : null
        }
        language={language}
        busy={
          busyAction === "provider_save" || busyAction === "provider_import"
        }
        onLoadCredential={loadProviderCredential}
        onCreate={createProvider}
        onUpdate={updateProvider}
      />

      <ProviderImportDialog
        open={providerDialog?.mode === "import"}
        onOpenChange={(open) => {
          if (!open) setProviderDialog(null);
        }}
        platform={platform}
        language={language}
        busy={busyAction === "provider_import"}
        onPreview={api.previewProviderImport}
        onCommit={commitProviderImport}
      />

      <AlertDialog
        open={pendingDelete !== null}
        onOpenChange={(open) => {
          if (!open) setPendingDelete(null);
        }}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>{t("deleteTitle")}</AlertDialogTitle>
            <AlertDialogDescription>
              {t("deleteDescription")}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel asChild>
              <Button variant="ghost">{t("cancel")}</Button>
            </AlertDialogCancel>
            <AlertDialogAction asChild>
              <Button
                className="disabled:opacity-100"
                variant="danger"
                disabled={busyAction === "delete"}
                onClick={() => void deleteProvider()}
              >
                {busyAction === "delete" ? (
                  <LoaderCircle className="animate-spin" />
                ) : (
                  <AlertCircle />
                )}
                {t("continueDelete")}
              </Button>
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>

      <AlertDialog
        open={pendingSwitch !== null}
        onOpenChange={(open) => {
          if (!open) setPendingSwitch(null);
        }}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <div className="mb-1 grid size-10 place-items-center rounded-xl bg-amber-500/10 text-amber-600 dark:text-amber-400">
              <ShieldCheck className="size-[18px]" />
            </div>
            <AlertDialogTitle>
              {t("globalCredential")} · {pendingSwitch?.name}
            </AlertDialogTitle>
            <AlertDialogDescription>
              {t("globalWarning")}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel asChild>
              <Button variant="ghost">{t("cancel")}</Button>
            </AlertDialogCancel>
            <AlertDialogAction asChild>
              <Button
                className="disabled:opacity-100"
                disabled={busyAction === "switch"}
                onClick={() => {
                  if (pendingSwitch) void executeSwitch(pendingSwitch);
                }}
              >
                {busyAction === "switch" ? (
                  <LoaderCircle className="animate-spin" />
                ) : (
                  <CheckCircle2 />
                )}
                {t("switch")}
              </Button>
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>

      <Dialog
        open={pendingLaunch !== null}
        onOpenChange={(open) => {
          if (!open) setPendingLaunch(null);
        }}
      >
        <DialogContent className="max-w-md">
          <DialogHeader>
            <div className="mb-1 grid size-10 place-items-center rounded-xl bg-primary/10 text-primary">
              <FolderOpen className="size-[18px]" />
            </div>
            <DialogTitle>
              {t("launchProjectTitle")} · {pendingLaunch?.name}
            </DialogTitle>
          </DialogHeader>
          <div className="grid gap-2 py-1">
            <Label htmlFor="launch-cwd">{t("projectDirectory")}</Label>
            <Input
              id="launch-cwd"
              autoFocus
              value={launchCwd}
              onChange={(event) => setLaunchCwd(event.target.value)}
              placeholder={t("projectPathPlaceholder")}
            />
          </div>
          <DialogFooter>
            <Button variant="ghost" onClick={() => setPendingLaunch(null)}>
              {t("cancel")}
            </Button>
            <Button
              className={cn(busyAction === "launch" && "disabled:opacity-100")}
              disabled={busyAction === "launch" || !launchCwd.trim()}
              onClick={() => void executeLaunch()}
            >
              {busyAction === "launch" ? (
                <LoaderCircle className="animate-spin" />
              ) : (
                <FolderOpen />
              )}
              {t("launch")}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <AlertDialog open={pendingDeactivate} onOpenChange={setPendingDeactivate}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <div className="mb-1 grid size-10 place-items-center rounded-xl bg-amber-500/10 text-amber-600 dark:text-amber-400">
              <PowerOff className="size-[18px]" />
            </div>
            <AlertDialogTitle>{t("stopGlobalTitle")}</AlertDialogTitle>
            <AlertDialogDescription>
              {t("stopGlobalDescription")}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel asChild>
              <Button variant="ghost">{t("cancel")}</Button>
            </AlertDialogCancel>
            <AlertDialogAction asChild>
              <Button
                className="disabled:opacity-100"
                disabled={busyAction === "deactivate"}
                onClick={() =>
                  void perform(
                    "deactivate",
                    () => api.deactivateGlobal(platform),
                    undefined,
                    () => setPendingDeactivate(false),
                  )
                }
              >
                {busyAction === "deactivate" ? (
                  <LoaderCircle className="animate-spin" />
                ) : (
                  <PowerOff />
                )}
                {t("stopGlobal")}
              </Button>
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>

      <Toaster
        position="bottom-right"
        richColors
        closeButton
        theme={
          document.documentElement.classList.contains("dark") ? "dark" : "light"
        }
      />
    </TooltipProvider>
  );
}
