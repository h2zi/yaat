import { useCallback, useEffect, useMemo, useState } from "react";
import {
  AlertTriangle,
  CheckCircle2,
  LoaderCircle,
  Pencil,
  RefreshCw,
  Server,
  ShieldCheck,
} from "lucide-react";

import { ProviderDialog } from "@/components/provider-dialog";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import type { Language } from "@/i18n";
import type {
  CreateProviderRequest,
  Platform,
  ProviderImportCommitRequest,
  ProviderImportPreview,
  ProviderImportPreviewRequest,
  ProviderProfile,
} from "@/types";

interface ProviderImportDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  platform: Platform;
  language: Language;
  busy: boolean;
  onPreview: (
    request: ProviderImportPreviewRequest,
  ) => Promise<ProviderImportPreview>;
  onCommit: (request: ProviderImportCommitRequest) => Promise<void>;
}

export function ProviderImportDialog({
  open,
  onOpenChange,
  platform,
  language,
  busy,
  onPreview,
  onCommit,
}: ProviderImportDialogProps) {
  const text = language === "zh" ? zh : en;
  const [preview, setPreview] = useState<ProviderImportPreview | null>(null);
  const [drafts, setDrafts] = useState<Record<string, CreateProviderRequest>>(
    {},
  );
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [editing, setEditing] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const scan = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const result = await onPreview({ platform });
      const nextDrafts = Object.fromEntries(
        result.candidates.map((candidate) => [
          candidate.candidateId,
          candidateRequest(platform, candidate),
        ]),
      );
      const nextSelected = new Set(
        result.candidates
          .filter((candidate) => !candidate.alreadyImportedProviderId)
          .map((candidate) => candidate.candidateId),
      );
      setPreview(result);
      setDrafts(nextDrafts);
      setSelected(nextSelected);
    } catch (scanError) {
      setPreview(null);
      setDrafts({});
      setSelected(new Set());
      setError(
        scanError instanceof Error ? scanError.message : String(scanError),
      );
    } finally {
      setLoading(false);
    }
  }, [onPreview, platform]);

  useEffect(() => {
    if (!open) {
      setPreview(null);
      setDrafts({});
      setSelected(new Set());
      setEditing(null);
      setError(null);
      return;
    }
    setEditing(null);
    void scan();
  }, [open, scan]);

  const editingCandidate = preview?.candidates.find(
    (candidate) => candidate.candidateId === editing,
  );
  const editingDraft = editing ? drafts[editing] : undefined;
  const editingProfile = useMemo(
    () =>
      editingCandidate && editingDraft
        ? draftProfile(
            editingCandidate.candidateId,
            editingDraft,
            editingCandidate.credentialState === "ready",
          )
        : null,
    [editingCandidate, editingDraft],
  );

  const toggle = (candidateId: string) => {
    setSelected((current) => {
      const next = new Set(current);
      if (next.has(candidateId)) next.delete(candidateId);
      else next.add(candidateId);
      return next;
    });
  };

  const commit = async () => {
    if (!preview || selected.size === 0) {
      setError(text.selectOne);
      return;
    }
    const selections = preview.candidates
      .filter((candidate) => selected.has(candidate.candidateId))
      .map((candidate) => ({
        candidateId: candidate.candidateId,
        provider: drafts[candidate.candidateId],
      }));
    const incomplete = preview.candidates.find(
      (candidate) =>
        selected.has(candidate.candidateId) &&
        candidate.kind !== "official_subscription" &&
        candidate.credentialState !== "ready" &&
        !drafts[candidate.candidateId]?.secret,
    );
    if (incomplete) {
      setError(`${incomplete.name}: ${text.credentialRequired}`);
      return;
    }
    setError(null);
    await onCommit({
      platform,
      sourceRevision: preview.sourceRevision,
      selections,
    });
  };

  return (
    <>
      <Dialog open={open} onOpenChange={onOpenChange}>
        <DialogContent className="max-h-[90vh] max-w-2xl overflow-y-auto">
          <DialogHeader>
            <DialogTitle>{text.title}</DialogTitle>
          </DialogHeader>

          <div className="grid gap-4">
            <div className="flex items-center justify-between gap-3 rounded-xl border border-border bg-muted/30 px-4 py-3">
              <div>
                <p className="text-sm font-medium">{platformName(platform)}</p>
                <p className="text-xs text-muted-foreground">{text.subtitle}</p>
              </div>
              <Button
                type="button"
                size="sm"
                variant="outline"
                disabled={loading || busy}
                onClick={() => void scan()}
              >
                {loading ? (
                  <LoaderCircle className="animate-spin" />
                ) : (
                  <RefreshCw />
                )}
                {text.rescan}
              </Button>
            </div>

            {loading ? (
              <div className="flex min-h-44 items-center justify-center gap-2 text-sm text-muted-foreground">
                <LoaderCircle className="size-4 animate-spin" />
                {text.scanning}
              </div>
            ) : null}

            {!loading && preview?.candidates.length === 0 ? (
              <div className="rounded-xl border border-dashed border-border p-8 text-center text-sm text-muted-foreground">
                {text.noneFound}
              </div>
            ) : null}

            {!loading
              ? preview?.candidates.map((candidate) => {
                  const disabled = Boolean(candidate.alreadyImportedProviderId);
                  const checked = selected.has(candidate.candidateId);
                  const draft = drafts[candidate.candidateId];
                  return (
                    <div
                      key={candidate.candidateId}
                      className="grid gap-3 rounded-xl border border-border bg-background p-4 shadow-sm"
                    >
                      <div className="flex items-start gap-3">
                        <input
                          className="mt-1 size-4 accent-primary"
                          type="checkbox"
                          aria-label={`${text.select} ${candidate.name}`}
                          checked={checked && !disabled}
                          disabled={disabled}
                          onChange={() => toggle(candidate.candidateId)}
                        />
                        <div className="min-w-0 flex-1">
                          <div className="flex flex-wrap items-center gap-2">
                            {candidate.source === "active_config" ? (
                              <Server className="size-4 text-sky-600" />
                            ) : (
                              <ShieldCheck className="size-4 text-emerald-600" />
                            )}
                            <p className="font-medium">
                              {draft?.name ?? candidate.name}
                            </p>
                            {candidate.active ? (
                              <Badge>{text.active}</Badge>
                            ) : (
                              <Badge variant="secondary">{text.residual}</Badge>
                            )}
                            {disabled ? (
                              <Badge variant="success">
                                {text.alreadyImported}
                              </Badge>
                            ) : null}
                          </div>
                          <p className="mt-1 text-xs text-muted-foreground">
                            {kindLabel(candidate.kind, text)}
                            {draft?.accountLabel
                              ? ` · ${draft.accountLabel}`
                              : ""}
                          </p>
                          {draft?.baseUrl ? (
                            <p className="mt-2 truncate font-mono text-xs text-muted-foreground">
                              {draft.baseUrl}
                            </p>
                          ) : null}
                          {draft?.model ? (
                            <p className="mt-1 truncate text-xs text-muted-foreground">
                              {text.model}: {draft.model}
                            </p>
                          ) : null}
                        </div>
                        <Button
                          type="button"
                          size="sm"
                          variant="outline"
                          disabled={disabled}
                          onClick={() => setEditing(candidate.candidateId)}
                        >
                          <Pencil />
                          {text.review}
                        </Button>
                      </div>

                      {candidate.credentialState !== "ready" ? (
                        <div className="flex gap-2 rounded-lg bg-amber-500/10 px-3 py-2 text-xs text-amber-800 dark:text-amber-200">
                          <AlertTriangle className="mt-0.5 size-3.5 shrink-0" />
                          {candidate.credentialState === "unsupported_helper"
                            ? text.helperUnsupported
                            : text.credentialRequired}
                        </div>
                      ) : (
                        <div className="flex items-center gap-2 text-xs text-emerald-700 dark:text-emerald-300">
                          <CheckCircle2 className="size-3.5" />
                          {text.credentialReady}
                        </div>
                      )}

                      {candidate.warnings.map((warning) => (
                        <div
                          key={warning}
                          className="flex gap-2 text-xs text-amber-700 dark:text-amber-300"
                        >
                          <AlertTriangle className="mt-0.5 size-3.5 shrink-0" />
                          {warning}
                        </div>
                      ))}
                    </div>
                  );
                })
              : null}

            {preview?.warnings.map((warning) => (
              <div
                key={warning}
                className="flex gap-2 rounded-lg bg-amber-500/10 px-3 py-2 text-sm text-amber-800 dark:text-amber-200"
              >
                <AlertTriangle className="mt-0.5 size-4 shrink-0" />
                {warning}
              </div>
            ))}

            {error ? (
              <p className="rounded-lg bg-destructive/8 px-3 py-2 text-sm text-destructive">
                {error}
              </p>
            ) : null}
          </div>

          <DialogFooter>
            <Button
              type="button"
              variant="ghost"
              onClick={() => onOpenChange(false)}
            >
              {text.cancel}
            </Button>
            <Button
              type="button"
              disabled={busy || loading || selected.size === 0}
              onClick={() => void commit()}
            >
              {busy ? (
                <LoaderCircle className="animate-spin" />
              ) : (
                <ShieldCheck />
              )}
              {text.importSelected.replace("{count}", String(selected.size))}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {editingCandidate && editingDraft && editingProfile ? (
        <ProviderDialog
          open
          onOpenChange={(nextOpen) => {
            if (!nextOpen) setEditing(null);
          }}
          mode="create"
          platform={platform}
          profile={editingProfile}
          language={language}
          busy={false}
          credentialAvailable={
            editingCandidate.credentialState === "ready" ||
            Boolean(editingDraft.secret)
          }
          lockKind
          titleOverride={text.reviewTitle}
          submitLabel={text.applyReview}
          onLoadCredential={async () => null}
          onCreate={async (provider) => {
            setDrafts((current) => ({
              ...current,
              [editingCandidate.candidateId]: provider,
            }));
            setEditing(null);
          }}
          onUpdate={async () => undefined}
        />
      ) : null}
    </>
  );
}

function candidateRequest(
  platform: Platform,
  candidate: ProviderImportPreview["candidates"][number],
): CreateProviderRequest {
  return {
    platform,
    kind: candidate.kind,
    name: candidate.name,
    accountLabel: candidate.accountLabel,
    baseUrl: candidate.baseUrl,
    model: candidate.model,
    customHeaders: candidate.customHeaders,
    userAgent: candidate.userAgent,
    platformConfig: candidate.platformConfig,
    secretKind: candidate.secretKind,
    secret: null,
    officialCredential: null,
  };
}

function draftProfile(
  id: string,
  draft: CreateProviderRequest,
  credentialReady: boolean,
): ProviderProfile {
  return {
    id,
    platform: draft.platform,
    kind: draft.kind,
    name: draft.name,
    accountLabel: draft.accountLabel,
    baseUrl: draft.baseUrl,
    model: draft.model,
    customHeaders: draft.customHeaders,
    userAgent: draft.userAgent,
    platformConfig: draft.platformConfig,
    secretKind: draft.secretKind,
    hasSecret: credentialReady || Boolean(draft.secret),
    profileHome: null,
    status: credentialReady ? "ready" : "needs_login",
    createdAt: 0,
    updatedAt: 0,
  };
}

function platformName(platform: Platform) {
  if (platform === "codex") return "Codex";
  if (platform === "claude_code") return "Claude Code";
  return "Claude Desktop";
}

function kindLabel(kind: CreateProviderRequest["kind"], text: typeof zh) {
  if (kind === "official_subscription") return text.officialSubscription;
  if (kind === "official_api") return text.officialApi;
  return text.thirdParty;
}

const zh = {
  title: "导入当前账号与配置",
  subtitle: "只读取当前全局配置；导入不会切换账号或修改客户端文件",
  rescan: "重新扫描",
  scanning: "正在安全扫描当前配置与凭据…",
  noneFound: "没有发现可导入的当前配置或官方登录凭据",
  select: "选择",
  active: "当前生效",
  residual: "已发现的官方登录",
  alreadyImported: "已导入",
  review: "检查配置",
  reviewTitle: "检查导入配置",
  applyReview: "保存检查结果",
  credentialReady: "凭据已安全检测，不会回显到界面",
  credentialRequired: "需要填写直接凭据后才能导入",
  helperUnsupported: "不会执行凭据 helper；请在检查配置中填写直接凭据",
  selectOne: "请至少选择一个可导入账号",
  importSelected: "导入所选（{count}）",
  cancel: "取消",
  model: "模型",
  officialSubscription: "官方订阅账号",
  officialApi: "官方 API 账号",
  thirdParty: "第三方 Provider",
};

const en: typeof zh = {
  title: "Import current accounts and configuration",
  subtitle:
    "Read-only scan; importing does not switch accounts or modify client files",
  rescan: "Rescan",
  scanning: "Securely scanning the current configuration and credentials…",
  noneFound:
    "No importable active configuration or official credential was found",
  select: "Select",
  active: "Active",
  residual: "Official sign-in found",
  alreadyImported: "Already imported",
  review: "Review",
  reviewTitle: "Review import configuration",
  applyReview: "Save review",
  credentialReady: "Credential detected securely and not returned to the UI",
  credentialRequired: "Enter a direct credential before importing",
  helperUnsupported:
    "Credential helpers are not executed; enter a direct credential in Review",
  selectOne: "Select at least one importable account",
  importSelected: "Import selected ({count})",
  cancel: "Cancel",
  model: "Model",
  officialSubscription: "Official subscription",
  officialApi: "Official API",
  thirdParty: "Third-party provider",
};
