import { useEffect, useState } from "react";
import {
  ChevronDown,
  FolderCog,
  History,
  Languages,
  LoaderCircle,
  MoonStar,
  Save,
  ScanSearch,
  ShieldAlert,
  X,
} from "lucide-react";

import { ApiRequestError, api } from "@/api";
import { Button } from "@/components/ui/button";
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
import { Separator } from "@/components/ui/separator";
import { Switch } from "@/components/ui/switch";
import { translate, type Language, type TranslationKey } from "@/i18n";
import { optional } from "@/lib/utils";
import type {
  ActivationMode,
  AppSettings,
  HistoryApplyResult,
  HistoryPreview,
  OperationProgress,
  HistoryScope,
} from "@/types";

interface SettingsDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  settings: AppSettings;
  language: Language;
  busy: boolean;
  onPreviewTheme: (theme: string) => void;
  onSave: (settings: AppSettings, close?: boolean) => Promise<boolean>;
}

const fallbackTimezones = [
  "UTC",
  "America/Los_Angeles",
  "America/Denver",
  "America/Chicago",
  "America/New_York",
  "America/Toronto",
  "America/Sao_Paulo",
  "Europe/London",
  "Europe/Paris",
  "Europe/Berlin",
  "Europe/Moscow",
  "Africa/Cairo",
  "Africa/Johannesburg",
  "Asia/Dubai",
  "Asia/Kolkata",
  "Asia/Bangkok",
  "Asia/Singapore",
  "Asia/Hong_Kong",
  "Asia/Shanghai",
  "Asia/Taipei",
  "Asia/Tokyo",
  "Asia/Seoul",
  "Australia/Perth",
  "Australia/Sydney",
  "Pacific/Auckland",
];

const intlWithTimezones = Intl as typeof Intl & {
  supportedValuesOf?: (key: "timeZone") => string[];
};

const timezones = Array.from(
  new Set([
    "UTC",
    Intl.DateTimeFormat().resolvedOptions().timeZone,
    ...(intlWithTimezones.supportedValuesOf?.("timeZone") ?? fallbackTimezones),
  ]),
).filter(Boolean);

export function SettingsDialog({
  open,
  onOpenChange,
  settings,
  language,
  busy,
  onPreviewTheme,
  onSave,
}: SettingsDialogProps) {
  const t = (key: TranslationKey) => translate(language, key);
  const [draft, setDraft] = useState(settings);
  const [advanced, setAdvanced] = useState(false);
  const [historyBusy, setHistoryBusy] = useState<HistoryScope | null>(null);
  const [historyProgress, setHistoryProgress] =
    useState<OperationProgress | null>(null);
  const [historyCancelling, setHistoryCancelling] = useState(false);
  const [historyError, setHistoryError] = useState<string | null>(null);
  const [codexHistory, setCodexHistory] = useState<HistoryApplyResult | null>(
    null,
  );
  const [claudeCodeHistory, setClaudeCodeHistory] =
    useState<HistoryApplyResult | null>(null);
  const [claudeHistory, setClaudeHistory] = useState<HistoryPreview | null>(
    null,
  );
  const [claudeHistoryResult, setClaudeHistoryResult] =
    useState<HistoryApplyResult | null>(null);
  const [claudeTarget, setClaudeTarget] = useState<string>("");

  useEffect(() => {
    if (open) {
      setDraft(settings);
      setAdvanced(false);
      setHistoryBusy(null);
      setHistoryProgress(null);
      setHistoryCancelling(false);
      setHistoryError(null);
      setCodexHistory(null);
      setClaudeCodeHistory(null);
      setClaudeHistory(null);
      setClaudeHistoryResult(null);
      setClaudeTarget(settings.claudeDesktopHistoryTarget ?? "");
    }
  }, [open]);

  const save = async () => {
    await onSave({
      ...draft,
      codexPath: optional(draft.codexPath ?? ""),
      codexHome: optional(draft.codexHome ?? ""),
      claudePath: optional(draft.claudePath ?? ""),
      claudeDesktopPath: optional(draft.claudeDesktopPath ?? ""),
      claudeConfigDir: optional(draft.claudeConfigDir ?? ""),
    });
  };

  const setHistoryEnabled = async (scope: HistoryScope, enabled: boolean) => {
    if (
      scope === "claude_desktop_code" &&
      enabled &&
      !draft.claudeDesktopHistoryTarget
    ) {
      await scanClaudeGroups();
      return;
    }

    const previous = draft;
    const next = {
      ...draft,
      unifyCodexHistory: scope === "codex" ? enabled : draft.unifyCodexHistory,
      unifyClaudeCodeHistory:
        scope === "claude_code" ? enabled : draft.unifyClaudeCodeHistory,
      unifyClaudeDesktopCodeHistory:
        scope === "claude_desktop_code"
          ? enabled
          : draft.unifyClaudeDesktopCodeHistory,
    };
    setDraft(next);
    const persisted = {
      ...settings,
      unifyCodexHistory: next.unifyCodexHistory,
      unifyClaudeCodeHistory: next.unifyClaudeCodeHistory,
      unifyClaudeDesktopCodeHistory: next.unifyClaudeDesktopCodeHistory,
      claudeDesktopHistoryTarget: next.claudeDesktopHistoryTarget,
    };
    if (!(await onSave(persisted, false))) {
      setDraft(previous);
      return;
    }
    if (enabled) {
      await applyHistory(
        scope,
        scope === "claude_desktop_code"
          ? next.claudeDesktopHistoryTarget
          : null,
      );
    }
  };

  const scanClaudeGroups = async () => {
    setHistoryBusy("claude_desktop_code");
    setHistoryProgress({ phase: "discovering", processed: 0, total: null });
    setHistoryCancelling(false);
    setHistoryError(null);
    try {
      const preview = await api.previewHistory(
        {
          scope: "claude_desktop_code",
          targetGroupId: null,
        },
        setHistoryProgress,
      );
      setClaudeHistory(preview);
      setClaudeHistoryResult(null);
      const current = preview.groups.find((group) => group.isCurrent);
      const target = draft.claudeDesktopHistoryTarget ?? current?.id ?? "";
      setClaudeTarget(target);
      setDraft((currentDraft) => ({
        ...currentDraft,
        claudeDesktopHistoryTarget: target || null,
      }));
    } catch (error) {
      if (!(error instanceof ApiRequestError && error.code === "cancelled")) {
        setHistoryError(error instanceof Error ? error.message : String(error));
      }
    } finally {
      setHistoryBusy(null);
      setHistoryCancelling(false);
    }
  };

  const applyHistory = async (
    scope: HistoryScope,
    targetGroupId: string | null,
  ) => {
    setHistoryBusy(scope);
    setHistoryProgress({ phase: "discovering", processed: 0, total: null });
    setHistoryCancelling(false);
    setHistoryError(null);
    try {
      const result = await api.applyHistory(
        { scope, targetGroupId },
        setHistoryProgress,
      );
      if (scope === "codex") setCodexHistory(result);
      else if (scope === "claude_code") setClaudeCodeHistory(result);
      else setClaudeHistoryResult(result);
    } catch (error) {
      if (!(error instanceof ApiRequestError && error.code === "cancelled")) {
        setHistoryError(error instanceof Error ? error.message : String(error));
      }
    } finally {
      setHistoryBusy(null);
      setHistoryCancelling(false);
    }
  };

  const cancelHistory = async () => {
    setHistoryCancelling(true);
    await api.cancelHistory();
  };

  const progressStatus = () => {
    if (!historyBusy || !historyProgress) return null;
    const phase =
      historyProgress.phase === "discovering"
        ? "progressDiscovering"
        : historyProgress.phase === "processing"
          ? "progressProcessing"
          : "progressSaving";
    return (
      <div className="absolute bottom-20 right-6 z-20 flex w-[min(22rem,calc(100%_-_3rem))] items-center gap-3 rounded-xl border border-border bg-popover p-3 text-xs text-muted-foreground shadow-menu">
        <LoaderCircle className="size-3.5 animate-spin text-primary" />
        <span>
          {t(phase)}
          {historyProgress.processed > 0
            ? ` · ${historyProgress.processed}${historyProgress.total === null ? "" : ` / ${historyProgress.total}`}`
            : ""}
        </span>
        <Button
          className="ml-auto disabled:opacity-100"
          size="sm"
          variant="ghost"
          disabled={historyCancelling}
          onClick={() => void cancelHistory()}
        >
          {historyCancelling ? (
            <LoaderCircle className="animate-spin" />
          ) : (
            <X />
          )}
          {t("cancel")}
        </Button>
      </div>
    );
  };

  const report = (result: HistoryApplyResult) => (
    <div className="flex flex-wrap gap-2 text-xs text-muted-foreground">
      <span className="rounded-md bg-primary/8 px-2 py-1 text-primary">
        {result.copied} {t("historyCopied")}
      </span>
      {result.metadataUpdated > 0 ? (
        <span className="rounded-md bg-muted px-2 py-1">
          {result.metadataUpdated} {t("historyMetadata")}
        </span>
      ) : null}
      {result.conflicts > 0 ? (
        <span className="rounded-md bg-amber-500/10 px-2 py-1 text-amber-700 dark:text-amber-300">
          {result.conflicts} {t("historyConflicts")}
        </span>
      ) : null}
      {result.invalidFiles > 0 ? (
        <span className="rounded-md bg-destructive/8 px-2 py-1 text-destructive">
          {result.invalidFiles} {t("historyInvalid")}
        </span>
      ) : null}
    </div>
  );

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!next) onPreviewTheme(settings.theme);
        onOpenChange(next);
      }}
    >
      <DialogContent className="max-w-2xl">
        <DialogHeader>
          <DialogTitle>{t("settingsTitle")}</DialogTitle>
        </DialogHeader>

        <div className="grid gap-5">
          <section className="grid gap-4">
            <div className="flex items-center gap-2 text-sm font-semibold">
              <MoonStar className="size-4 text-primary" />
              {t("appearance")}
            </div>
            <div className="grid gap-4 sm:grid-cols-2">
              <div className="grid gap-2">
                <Label htmlFor="settings-language">
                  <span className="inline-flex items-center gap-1.5">
                    <Languages className="size-3.5 text-muted-foreground" />
                    {t("language")}
                  </span>
                </Label>
                <Select
                  value={draft.language}
                  onValueChange={(value) =>
                    setDraft({ ...draft, language: value })
                  }
                >
                  <SelectTrigger id="settings-language" className="w-full">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="system">{t("system")}</SelectItem>
                    <SelectItem value="zh">简体中文</SelectItem>
                    <SelectItem value="en">English</SelectItem>
                  </SelectContent>
                </Select>
              </div>
              <div className="grid gap-2">
                <Label htmlFor="settings-theme">{t("theme")}</Label>
                <Select
                  value={draft.theme}
                  onValueChange={(value) => {
                    setDraft({ ...draft, theme: value });
                    onPreviewTheme(value);
                  }}
                >
                  <SelectTrigger id="settings-theme" className="w-full">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="auto">{t("system")}</SelectItem>
                    <SelectItem value="light">{t("light")}</SelectItem>
                    <SelectItem value="dark">{t("dark")}</SelectItem>
                  </SelectContent>
                </Select>
              </div>
              <div className="grid gap-2">
                <Label htmlFor="settings-timezone">{t("timezone")}</Label>
                <Select
                  value={draft.timezone}
                  onValueChange={(value) =>
                    setDraft({ ...draft, timezone: value })
                  }
                >
                  <SelectTrigger id="settings-timezone" className="w-full">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent className="min-w-[var(--radix-select-trigger-width)]">
                    {(timezones.includes(draft.timezone)
                      ? timezones
                      : [draft.timezone, ...timezones]
                    ).map((timezone) => (
                      <SelectItem key={timezone} value={timezone}>
                        {timezone}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </div>
              <div className="grid gap-2">
                <Label htmlFor="settings-mode">{t("defaultMode")}</Label>
                <Select
                  value={draft.defaultActivationMode}
                  onValueChange={(value) =>
                    setDraft({
                      ...draft,
                      defaultActivationMode: value as ActivationMode,
                    })
                  }
                >
                  <SelectTrigger id="settings-mode" className="w-full">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="managed_launch">
                      {t("managedLaunch")}
                    </SelectItem>
                    <SelectItem value="global_credential">
                      {t("globalCredential")}
                    </SelectItem>
                  </SelectContent>
                </Select>
              </div>
            </div>
          </section>

          <Separator />

          <section className="grid gap-3">
            <div className="flex items-center gap-2 text-sm font-semibold">
              <History className="size-4 text-primary" />
              {t("sessionHistory")}
            </div>
            <p className="text-sm leading-relaxed text-muted-foreground">
              {t("sessionHistoryDescription")}
            </p>
            <div className="grid gap-3 rounded-xl border border-border p-4">
              <div className="flex items-start gap-3">
                <div className="min-w-0 flex-1">
                  <p className="text-sm font-medium">
                    {t("codexUnifiedHistory")}
                  </p>
                  <p className="mt-1 text-xs leading-relaxed text-muted-foreground">
                    {t("codexUnifiedHistoryDescription")}
                  </p>
                </div>
                <Switch
                  className="disabled:opacity-100"
                  checked={draft.unifyCodexHistory}
                  disabled={busy || historyBusy === "codex"}
                  aria-label={t("codexUnifiedHistory")}
                  onCheckedChange={(checked) =>
                    void setHistoryEnabled("codex", checked)
                  }
                />
              </div>
              {codexHistory ? (
                <div className="border-t border-border pt-3">
                  {report(codexHistory)}
                </div>
              ) : null}
            </div>

            <div className="grid gap-3 rounded-xl border border-border p-4">
              <div className="flex items-start gap-3">
                <div className="min-w-0 flex-1">
                  <p className="text-sm font-medium">
                    {t("claudeCodeUnifiedHistory")}
                  </p>
                  <p className="mt-1 text-xs leading-relaxed text-muted-foreground">
                    {t("claudeCodeUnifiedHistoryDescription")}
                  </p>
                </div>
                <Switch
                  className="disabled:opacity-100"
                  checked={draft.unifyClaudeCodeHistory}
                  disabled={busy || historyBusy === "claude_code"}
                  aria-label={t("claudeCodeUnifiedHistory")}
                  onCheckedChange={(checked) =>
                    void setHistoryEnabled("claude_code", checked)
                  }
                />
              </div>
              {claudeCodeHistory ? (
                <div className="border-t border-border pt-3">
                  {report(claudeCodeHistory)}
                </div>
              ) : null}
            </div>

            <div className="grid gap-3 rounded-xl border border-border p-4">
              <div className="flex items-start gap-3">
                <div className="min-w-0 flex-1">
                  <p className="text-sm font-medium">
                    {t("claudeDesktopUnifiedHistory")}
                  </p>
                  <p className="mt-1 text-xs leading-relaxed text-muted-foreground">
                    {t("claudeDesktopUnifiedHistoryDescription")}
                  </p>
                </div>
                <div className="flex shrink-0 items-center gap-2">
                  <Switch
                    className="disabled:opacity-100"
                    checked={draft.unifyClaudeDesktopCodeHistory}
                    disabled={busy || historyBusy === "claude_desktop_code"}
                    aria-label={t("claudeDesktopUnifiedHistory")}
                    onCheckedChange={(checked) =>
                      void setHistoryEnabled("claude_desktop_code", checked)
                    }
                  />
                  <Button
                    className={
                      historyBusy === "claude_desktop_code" &&
                      !draft.unifyClaudeDesktopCodeHistory
                        ? "disabled:opacity-100"
                        : undefined
                    }
                    size="sm"
                    variant="outline"
                    disabled={
                      historyBusy === "claude_desktop_code" ||
                      draft.unifyClaudeDesktopCodeHistory
                    }
                    onClick={() => void scanClaudeGroups()}
                  >
                    {historyBusy === "claude_desktop_code" &&
                    !draft.unifyClaudeDesktopCodeHistory ? (
                      <LoaderCircle className="animate-spin" />
                    ) : (
                      <ScanSearch />
                    )}
                    {t("historyScanAccounts")}
                  </Button>
                </div>
              </div>
              {claudeHistory ? (
                <div className="grid gap-3 border-t border-border pt-3">
                  {claudeHistory.groups.length > 0 ? (
                    <div className="grid gap-2">
                      <Label htmlFor="claude-history-target">
                        {t("historyTarget")}
                      </Label>
                      <Select
                        value={claudeTarget}
                        disabled={
                          historyBusy === "claude_desktop_code" ||
                          draft.unifyClaudeDesktopCodeHistory
                        }
                        onValueChange={(value) => {
                          setClaudeTarget(value);
                          setDraft({
                            ...draft,
                            claudeDesktopHistoryTarget: value,
                          });
                          setClaudeHistoryResult(null);
                        }}
                      >
                        <SelectTrigger
                          id="claude-history-target"
                          className={`w-full ${historyBusy === "claude_desktop_code" ? "disabled:opacity-100" : ""}`}
                        >
                          <SelectValue placeholder={t("historyChooseTarget")} />
                        </SelectTrigger>
                        <SelectContent>
                          {claudeHistory.groups.map((group) => (
                            <SelectItem key={group.id} value={group.id}>
                              {group.label}
                              {group.isCurrent
                                ? ` · ${t("historyCurrent")}`
                                : ""}{" "}
                              · {group.sessionCount}
                            </SelectItem>
                          ))}
                        </SelectContent>
                      </Select>
                    </div>
                  ) : (
                    <p className="text-xs text-muted-foreground">
                      {t("historyNoClaudeGroups")}
                    </p>
                  )}
                  {claudeHistoryResult ? report(claudeHistoryResult) : null}
                </div>
              ) : null}
            </div>
            {historyError ? (
              <p className="rounded-lg border border-destructive/20 bg-destructive/7 px-3 py-2 text-xs leading-relaxed text-destructive">
                {historyError}
              </p>
            ) : null}
          </section>

          <Separator />

          <section className="grid gap-3">
            <button
              type="button"
              className="flex w-full items-center gap-2 rounded-lg py-1 text-left text-sm font-semibold outline-none focus-visible:ring-[3px] focus-visible:ring-ring/20"
              onClick={() => setAdvanced((value) => !value)}
            >
              <FolderCog className="size-4 text-primary" />
              {t("advancedPaths")}
              <ChevronDown
                className={`ml-auto size-4 text-muted-foreground transition-transform ${advanced ? "rotate-180" : ""}`}
              />
            </button>
            {advanced ? (
              <div className="grid gap-4 rounded-xl border border-border bg-muted/30 p-4 sm:grid-cols-2">
                {(
                  [
                    ["codexPath", "codexPath"],
                    ["codexHome", "codexHome"],
                    ["claudePath", "claudePath"],
                    ["claudeConfigDir", "claudeHome"],
                    ["claudeDesktopPath", "claudeDesktopPath"],
                  ] as const
                ).map(([field, label]) => (
                  <div className="grid gap-2" key={field}>
                    <Label htmlFor={`settings-${field}`}>{t(label)}</Label>
                    <Input
                      id={`settings-${field}`}
                      value={draft[field] ?? ""}
                      onChange={(event) =>
                        setDraft({ ...draft, [field]: event.target.value })
                      }
                      placeholder={t("optionalAutoDetect")}
                    />
                  </div>
                ))}
              </div>
            ) : null}
          </section>

          <div className="rounded-xl border border-amber-500/20 bg-amber-500/7 p-4 text-sm leading-relaxed">
            <div className="flex gap-3">
              <ShieldAlert className="mt-0.5 size-4 shrink-0 text-amber-600 dark:text-amber-400" />
              <p className="text-amber-950/80 dark:text-amber-100/80">
                {t("globalWarning")}
              </p>
            </div>
          </div>
        </div>

        {progressStatus()}

        <DialogFooter>
          <Button
            variant="ghost"
            onClick={() => {
              onPreviewTheme(settings.theme);
              onOpenChange(false);
            }}
          >
            {t("cancel")}
          </Button>
          <Button
            className="disabled:opacity-100"
            onClick={save}
            disabled={busy}
          >
            {busy ? <LoaderCircle className="animate-spin" /> : <Save />}
            {t("save")}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
