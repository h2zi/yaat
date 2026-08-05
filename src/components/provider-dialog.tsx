import { useEffect, useState, type FormEvent } from "react";
import {
  Check,
  Copy,
  Eye,
  EyeOff,
  KeyRound,
  LoaderCircle,
  Server,
  ShieldCheck,
} from "lucide-react";

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
import { Textarea } from "@/components/ui/textarea";
import { translate, type Language, type TranslationKey } from "@/i18n";
import { optional } from "@/lib/utils";
import type {
  CreateProviderRequest,
  ImportCurrentRequest,
  Platform,
  ProviderKind,
  ProviderProfile,
  SecretKind,
  UpdateProviderRequest,
} from "@/types";

export type ProviderDialogMode = "create" | "edit" | "import";

interface ProviderDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  mode: ProviderDialogMode;
  platform: Platform;
  profile?: ProviderProfile | null;
  language: Language;
  busy: boolean;
  onLoadCredential: (profileId: string) => Promise<string | null>;
  onCreate: (request: CreateProviderRequest) => Promise<void>;
  onUpdate: (request: UpdateProviderRequest) => Promise<void>;
  onImport: (request: ImportCurrentRequest) => Promise<void>;
}

export function ProviderDialog({
  open,
  onOpenChange,
  mode,
  platform,
  profile,
  language,
  busy,
  onLoadCredential,
  onCreate,
  onUpdate,
  onImport,
}: ProviderDialogProps) {
  const t = (key: TranslationKey) => translate(language, key);
  const [kind, setKind] = useState<ProviderKind>("official_subscription");
  const [name, setName] = useState("");
  const [accountLabel, setAccountLabel] = useState("");
  const [baseUrl, setBaseUrl] = useState("");
  const [model, setModel] = useState("");
  const [secretKind, setSecretKind] = useState<SecretKind>("api_key");
  const [secret, setSecret] = useState("");
  const [initialSecret, setInitialSecret] = useState("");
  const [credentialLoading, setCredentialLoading] = useState(false);
  const [credentialVisible, setCredentialVisible] = useState(true);
  const [credentialCopied, setCredentialCopied] = useState(false);
  const [validation, setValidation] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    setKind(profile?.kind ?? "official_subscription");
    setName(profile?.name ?? "");
    setAccountLabel(profile?.accountLabel ?? "");
    setBaseUrl(profile?.baseUrl ?? "");
    setModel(profile?.model ?? "");
    setSecretKind(
      platform !== "codex"
        ? "api_key"
        : profile?.secretKind === "none"
          ? "api_key"
          : (profile?.secretKind ?? "api_key"),
    );
    setSecret("");
    setInitialSecret("");
    setCredentialLoading(false);
    setCredentialVisible(true);
    setCredentialCopied(false);
    setValidation(null);
    if (mode !== "edit" || !profile) return;

    let cancelled = false;
    setCredentialLoading(true);
    void onLoadCredential(profile.id)
      .then((credential) => {
        if (cancelled) return;
        const value = credential ?? "";
        setSecret(value);
        setInitialSecret(value);
      })
      .catch((error) => {
        if (!cancelled) {
          setValidation(error instanceof Error ? error.message : String(error));
        }
      })
      .finally(() => {
        if (!cancelled) setCredentialLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [mode, onLoadCredential, open, platform, profile]);

  const copyCredential = async () => {
    if (!secret) return;
    try {
      if (navigator.clipboard?.writeText) {
        await navigator.clipboard.writeText(secret);
      } else {
        const field = document.createElement("textarea");
        field.value = secret;
        field.style.position = "fixed";
        field.style.opacity = "0";
        document.body.append(field);
        field.select();
        const copied = document.execCommand("copy");
        field.remove();
        if (!copied) throw new Error(t("copyCredential"));
      }
      setCredentialCopied(true);
    } catch (error) {
      setValidation(error instanceof Error ? error.message : String(error));
    }
  };

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (!name.trim()) {
      setValidation(t("required"));
      return;
    }
    if (kind === "third_party" && (!baseUrl.trim() || !model.trim())) {
      setValidation(t("required"));
      return;
    }
    if (
      mode === "create" &&
      kind !== "official_subscription" &&
      !secret.trim()
    ) {
      setValidation(t("required"));
      return;
    }

    if (mode === "import") {
      await onImport({
        platform,
        name: name.trim(),
        accountLabel: optional(accountLabel),
      });
      return;
    }
    if (mode === "edit" && profile) {
      await onUpdate({
        id: profile.id,
        name: name.trim(),
        accountLabel: optional(accountLabel),
        baseUrl: kind === "third_party" ? optional(baseUrl) : profile.baseUrl,
        model: kind === "third_party" ? optional(model) : profile.model,
        secretKind:
          kind === "official_subscription"
            ? "none"
            : platform !== "codex"
              ? "api_key"
              : secretKind,
        replacementSecret:
          kind === "official_subscription" || secret === initialSecret
            ? null
            : optional(secret),
        replacementOfficialCredential:
          kind === "official_subscription" && secret !== initialSecret
            ? optional(secret)
            : null,
      });
      return;
    }
    await onCreate({
      platform,
      kind,
      name: name.trim(),
      accountLabel: optional(accountLabel),
      baseUrl: kind === "third_party" ? optional(baseUrl) : null,
      model: kind === "third_party" ? optional(model) : null,
      secretKind:
        kind === "official_subscription"
          ? "none"
          : platform !== "codex"
            ? "api_key"
            : secretKind,
      secret: kind === "official_subscription" ? null : optional(secret),
      officialCredential:
        kind === "official_subscription" ? optional(secret) : null,
    });
  };

  const title =
    mode === "create"
      ? t("dialogCreateTitle")
      : mode === "edit"
        ? t("dialogEditTitle")
        : t("dialogImportTitle");

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-xl">
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
        </DialogHeader>
        <form className="grid gap-5" onSubmit={submit}>
          {mode !== "import" ? (
            <div className="grid gap-2">
              <Label htmlFor="provider-kind">{t("providerType")}</Label>
              <Select
                value={kind}
                disabled={mode === "edit"}
                onValueChange={(value) => {
                  const next = value as ProviderKind;
                  setKind(next);
                  if (next === "official_subscription")
                    setSecretKind("api_key");
                }}
              >
                <SelectTrigger id="provider-kind" className="w-full">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="official_subscription">
                    <span className="flex items-center gap-2">
                      <ShieldCheck className="size-4 text-primary" />
                      {t("officialSubscription")}
                    </span>
                  </SelectItem>
                  <SelectItem value="official_api">
                    <span className="flex items-center gap-2">
                      <KeyRound className="size-4 text-amber-600" />
                      {t("officialApi")}
                    </span>
                  </SelectItem>
                  <SelectItem value="third_party">
                    <span className="flex items-center gap-2">
                      <Server className="size-4 text-sky-600" />
                      {t("thirdParty")}
                    </span>
                  </SelectItem>
                </SelectContent>
              </Select>
            </div>
          ) : null}

          <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
            <div className="grid gap-2">
              <Label htmlFor="provider-name">{t("displayName")}</Label>
              <Input
                id="provider-name"
                maxLength={80}
                value={name}
                onChange={(event) => setName(event.target.value)}
                placeholder={t("displayNamePlaceholder")}
                autoFocus
              />
            </div>
            <div className="grid gap-2">
              <Label htmlFor="account-label">{t("accountLabel")}</Label>
              <Input
                id="account-label"
                maxLength={160}
                value={accountLabel}
                onChange={(event) => setAccountLabel(event.target.value)}
                placeholder={t("accountLabelPlaceholder")}
              />
            </div>
          </div>

          {kind === "third_party" && mode !== "import" ? (
            <div className="grid gap-4 rounded-xl border border-border bg-muted/35 p-4">
              <div className="flex items-center gap-2 text-sm font-medium">
                <Server className="size-4 text-primary" />
                {t("thirdParty")}
              </div>
              <div className="grid gap-2">
                <Label htmlFor="base-url">{t("baseUrl")}</Label>
                <Input
                  id="base-url"
                  type="url"
                  value={baseUrl}
                  onChange={(event) => setBaseUrl(event.target.value)}
                  placeholder="https://api.example.com/v1"
                />
                {baseUrl.trim().toLowerCase().startsWith("http://") ? (
                  <p className="text-xs text-amber-700 dark:text-amber-300">
                    {t("httpEndpointWarning")}
                  </p>
                ) : null}
              </div>
              <div className="grid gap-2">
                <Label htmlFor="model">{t("model")}</Label>
                <Input
                  id="model"
                  value={model}
                  onChange={(event) => setModel(event.target.value)}
                  placeholder={
                    platform === "codex" ? "gpt-5.1-codex" : "claude-sonnet-5"
                  }
                />
              </div>
              {platform === "claude_desktop" ? (
                <p className="text-xs leading-relaxed text-muted-foreground">
                  {t("desktopProviderNote")}
                </p>
              ) : null}
            </div>
          ) : null}

          {kind === "official_subscription" && mode !== "import" ? (
            <div className="grid gap-2 rounded-xl border border-border bg-muted/35 p-4">
              <div className="flex items-center justify-between gap-3">
                <Label htmlFor="official-credential">
                  {t("officialCredential")}
                </Label>
                <Button
                  type="button"
                  variant="ghost"
                  size="icon-sm"
                  disabled={credentialLoading || !secret}
                  aria-label={t(
                    credentialCopied ? "credentialCopied" : "copyCredential",
                  )}
                  title={t(
                    credentialCopied ? "credentialCopied" : "copyCredential",
                  )}
                  onClick={() => void copyCredential()}
                >
                  {credentialLoading ? (
                    <LoaderCircle className="animate-spin" />
                  ) : credentialCopied ? (
                    <Check />
                  ) : (
                    <Copy />
                  )}
                </Button>
              </div>
              <Textarea
                id="official-credential"
                className="min-h-40 font-mono text-xs leading-relaxed"
                disabled={credentialLoading}
                spellCheck={false}
                value={secret}
                onChange={(event) => {
                  setSecret(event.target.value);
                  setCredentialCopied(false);
                }}
                placeholder={t(
                  credentialLoading
                    ? "credentialLoading"
                    : "officialCredentialPlaceholder",
                )}
              />
            </div>
          ) : null}

          {kind !== "official_subscription" && mode !== "import" ? (
            <div className="grid gap-4 rounded-xl border border-border bg-muted/35 p-4">
              <div className="grid gap-2 sm:grid-cols-[10rem_1fr] sm:items-end">
                <div className="grid gap-2">
                  <Label htmlFor="secret-kind">{t("credentialType")}</Label>
                  <Select
                    value={platform !== "codex" ? "api_key" : secretKind}
                    disabled={platform !== "codex"}
                    onValueChange={(value) =>
                      setSecretKind(value as SecretKind)
                    }
                  >
                    <SelectTrigger id="secret-kind" className="w-full">
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      <SelectItem value="api_key">{t("apiKey")}</SelectItem>
                      {platform === "codex" ? (
                        <SelectItem value="bearer_token">
                          {t("bearerToken")}
                        </SelectItem>
                      ) : null}
                    </SelectContent>
                  </Select>
                </div>
                <div className="grid gap-2">
                  <Label htmlFor="secret">{t("credential")}</Label>
                  <div className="flex gap-2">
                    <Input
                      id="secret"
                      className="font-mono"
                      type={credentialVisible ? "text" : "password"}
                      autoComplete="off"
                      disabled={credentialLoading}
                      value={secret}
                      onChange={(event) => {
                        setSecret(event.target.value);
                        setCredentialCopied(false);
                      }}
                      placeholder={t(
                        credentialLoading
                          ? "credentialLoading"
                          : "credentialPlaceholder",
                      )}
                    />
                    <Button
                      type="button"
                      variant="outline"
                      size="icon"
                      disabled={credentialLoading || !secret}
                      aria-label={t(
                        credentialVisible ? "hideCredential" : "showCredential",
                      )}
                      title={t(
                        credentialVisible ? "hideCredential" : "showCredential",
                      )}
                      onClick={() =>
                        setCredentialVisible((visible) => !visible)
                      }
                    >
                      {credentialVisible ? <EyeOff /> : <Eye />}
                    </Button>
                    <Button
                      type="button"
                      variant="outline"
                      size="icon"
                      disabled={credentialLoading || !secret}
                      aria-label={t(
                        credentialCopied
                          ? "credentialCopied"
                          : "copyCredential",
                      )}
                      title={t(
                        credentialCopied
                          ? "credentialCopied"
                          : "copyCredential",
                      )}
                      onClick={() => void copyCredential()}
                    >
                      {credentialLoading ? (
                        <LoaderCircle className="animate-spin" />
                      ) : credentialCopied ? (
                        <Check />
                      ) : (
                        <Copy />
                      )}
                    </Button>
                  </div>
                </div>
              </div>
            </div>
          ) : null}

          {validation ? (
            <p className="rounded-lg bg-destructive/8 px-3 py-2 text-sm text-destructive">
              {validation}
            </p>
          ) : null}

          <DialogFooter>
            <Button
              type="button"
              variant="ghost"
              onClick={() => onOpenChange(false)}
            >
              {t("cancel")}
            </Button>
            <Button
              className="disabled:opacity-100"
              type="submit"
              disabled={busy}
            >
              {busy ? (
                <LoaderCircle className="animate-spin" />
              ) : (
                <ShieldCheck />
              )}
              {mode === "create"
                ? t("create")
                : mode === "import"
                  ? t("import")
                  : t("save")}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
