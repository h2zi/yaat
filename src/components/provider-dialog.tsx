import { useEffect, useState, type FormEvent } from "react";
import {
  Check,
  Copy,
  Eye,
  EyeOff,
  KeyRound,
  LoaderCircle,
  Plus,
  Server,
  ShieldCheck,
  Trash2,
} from "lucide-react";

import { api } from "@/api";
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
import { Switch } from "@/components/ui/switch";
import { translate, type Language, type TranslationKey } from "@/i18n";
import { optional } from "@/lib/utils";
import type {
  CreateProviderRequest,
  CodexCatalogModel,
  FetchedModel,
  HeaderEntry,
  Platform,
  ProviderKind,
  ProviderProfile,
  SecretKind,
  UpdateProviderRequest,
  ProviderPlatformConfig,
  ReasoningEffort,
} from "@/types";
import { emptyPlatformConfig } from "@/types";

export type ProviderDialogMode = "create" | "edit";

interface ProviderDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  mode: ProviderDialogMode;
  platform: Platform;
  profile?: ProviderProfile | null;
  language: Language;
  busy: boolean;
  credentialAvailable?: boolean;
  lockKind?: boolean;
  titleOverride?: string;
  submitLabel?: string;
  onLoadCredential: (profileId: string) => Promise<string | null>;
  onCreate: (request: CreateProviderRequest) => Promise<void>;
  onUpdate: (request: UpdateProviderRequest) => Promise<void>;
}

export function ProviderDialog({
  open,
  onOpenChange,
  mode,
  platform,
  profile,
  language,
  busy,
  credentialAvailable = false,
  lockKind = false,
  titleOverride,
  submitLabel,
  onLoadCredential,
  onCreate,
  onUpdate,
}: ProviderDialogProps) {
  const t = (key: TranslationKey) => translate(language, key);
  const [kind, setKind] = useState<ProviderKind>("official_subscription");
  const [name, setName] = useState("");
  const [accountLabel, setAccountLabel] = useState("");
  const [baseUrl, setBaseUrl] = useState("");
  const [model, setModel] = useState("");
  const [customHeaders, setCustomHeaders] = useState<HeaderEntry[]>([]);
  const [userAgent, setUserAgent] = useState("");
  const [platformConfig, setPlatformConfig] = useState<ProviderPlatformConfig>(
    () => emptyPlatformConfig(platform),
  );
  const [fetchedModels, setFetchedModels] = useState<FetchedModel[]>([]);
  const [modelsLoading, setModelsLoading] = useState(false);
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
    setCustomHeaders(profile?.customHeaders ?? []);
    setUserAgent(profile?.userAgent ?? "");
    setPlatformConfig(profile?.platformConfig ?? emptyPlatformConfig(platform));
    setFetchedModels([]);
    setModelsLoading(false);
    setSecretKind(
      profile?.secretKind === "none"
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
      !secret.trim() &&
      !credentialAvailable
    ) {
      setValidation(t("required"));
      return;
    }
    if (mode === "edit" && profile) {
      await onUpdate({
        id: profile.id,
        name: name.trim(),
        accountLabel: optional(accountLabel),
        baseUrl: kind === "third_party" ? optional(baseUrl) : profile.baseUrl,
        model:
          kind === "official_subscription" ? profile.model : optional(model),
        customHeaders,
        userAgent: optional(userAgent),
        platformConfig: withDefaultModel(platformConfig, optional(model)),
        secretKind: kind === "official_subscription" ? "none" : secretKind,
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
      model: kind === "official_subscription" ? null : optional(model),
      customHeaders,
      userAgent: optional(userAgent),
      platformConfig: withDefaultModel(platformConfig, optional(model)),
      secretKind: kind === "official_subscription" ? "none" : secretKind,
      secret: kind === "official_subscription" ? null : optional(secret),
      officialCredential:
        kind === "official_subscription" ? optional(secret) : null,
    });
  };

  const fetchModels = async () => {
    setValidation(null);
    setModelsLoading(true);
    try {
      const response = await api.fetchProviderModels({
        platform,
        baseUrl: baseUrl.trim(),
        secretKind,
        credential: secret,
        customHeaders,
        userAgent: optional(userAgent),
      });
      setFetchedModels(response.models);
    } catch (error) {
      setValidation(error instanceof Error ? error.message : String(error));
    } finally {
      setModelsLoading(false);
    }
  };

  const updateHeader = (index: number, patch: Partial<HeaderEntry>) => {
    setCustomHeaders((headers) =>
      headers.map((header, current) =>
        current === index ? { ...header, ...patch } : header,
      ),
    );
  };

  const updateCatalogModel = (
    index: number,
    patch: Partial<CodexCatalogModel>,
  ) => {
    if (platformConfig.platform !== "codex") return;
    setPlatformConfig({
      ...platformConfig,
      catalog: platformConfig.catalog.map((entry, current) =>
        current === index ? { ...entry, ...patch } : entry,
      ),
    });
  };

  const addCatalogModel = () => {
    if (platformConfig.platform !== "codex" || !model.trim()) return;
    if (platformConfig.catalog.some((entry) => entry.id === model.trim()))
      return;
    setPlatformConfig({
      ...platformConfig,
      catalog: [...platformConfig.catalog, newCatalogModel(model.trim())],
    });
  };

  const updateClaudeMapping = (
    key: "sonnet" | "opus" | "haiku" | "fable" | "subagent",
    value: string,
  ) => {
    if (platformConfig.platform !== "claude_code") return;
    setPlatformConfig({ ...platformConfig, [key]: optional(value) });
  };

  const title =
    titleOverride ??
    (mode === "create" ? t("dialogCreateTitle") : t("dialogEditTitle"));
  const fetchedModelListId = `provider-model-options-${platform}`;

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-h-[90vh] max-w-xl overflow-y-auto">
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
        </DialogHeader>
        <form className="grid gap-5" onSubmit={submit}>
          <div className="grid gap-2">
            <Label htmlFor="provider-platform">{t("platformLabel")}</Label>
            <Input
              id="provider-platform"
              readOnly
              value={
                platform === "codex"
                  ? "Codex"
                  : platform === "claude_code"
                    ? "Claude Code"
                    : "Claude Desktop"
              }
            />
          </div>
          <div className="grid gap-2">
            <Label htmlFor="provider-kind">{t("providerType")}</Label>
            <Select
              value={kind}
              disabled={mode === "edit" || lockKind}
              onValueChange={(value) => {
                const next = value as ProviderKind;
                setKind(next);
                if (next === "official_subscription") setSecretKind("api_key");
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

          {kind === "third_party" ? (
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
                  placeholder={
                    platform === "claude_code"
                      ? "https://api.example.com"
                      : "https://api.example.com/v1"
                  }
                />
                {baseUrl.trim().toLowerCase().startsWith("http://") ? (
                  <p className="text-xs text-amber-700 dark:text-amber-300">
                    {t("httpEndpointWarning")}
                  </p>
                ) : null}
              </div>
            </div>
          ) : null}

          {kind === "official_subscription" &&
          !(credentialAvailable && mode === "create") ? (
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

          {kind !== "official_subscription" ? (
            <div className="grid gap-4 rounded-xl border border-border bg-muted/35 p-4">
              <div className="grid gap-2 sm:grid-cols-[10rem_1fr] sm:items-end">
                <div className="grid gap-2">
                  <Label htmlFor="secret-kind">{t("credentialType")}</Label>
                  <Select
                    value={secretKind}
                    onValueChange={(value) =>
                      setSecretKind(value as SecretKind)
                    }
                  >
                    <SelectTrigger id="secret-kind" className="w-full">
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      <SelectItem value="api_key">{t("apiKey")}</SelectItem>
                      <SelectItem value="bearer_token">
                        {t("bearerToken")}
                      </SelectItem>
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
                      placeholder={
                        credentialAvailable && mode === "create"
                          ? language === "zh"
                            ? "已检测到凭据；留空即可使用"
                            : "Credential detected; leave blank to use it"
                          : t(
                              credentialLoading
                                ? "credentialLoading"
                                : "credentialPlaceholder",
                            )
                      }
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

          {kind !== "official_subscription" ? (
            <div className="grid gap-4 rounded-xl border border-border bg-muted/35 p-4">
              <div className="grid gap-2">
                <Label htmlFor="model">{t("model")}</Label>
                <div className="flex gap-2">
                  <Input
                    id="model"
                    list={
                      fetchedModels.some((entry) => entry.directCompatible)
                        ? fetchedModelListId
                        : undefined
                    }
                    value={model}
                    onChange={(event) => setModel(event.target.value)}
                    placeholder={
                      platform === "codex" ? "gpt-5.1-codex" : "claude-sonnet-5"
                    }
                  />
                  {fetchedModels.length ? (
                    <datalist id={fetchedModelListId}>
                      {fetchedModels.map((entry) => (
                        <option
                          key={entry.id}
                          value={entry.id}
                          disabled={!entry.directCompatible}
                        >
                          {entry.warning ? t("routingRequired") : entry.id}
                        </option>
                      ))}
                    </datalist>
                  ) : null}
                  <Button
                    type="button"
                    variant="outline"
                    disabled={
                      modelsLoading || !baseUrl.trim() || !secret.trim()
                    }
                    onClick={() => void fetchModels()}
                  >
                    {modelsLoading ? (
                      <LoaderCircle className="animate-spin" />
                    ) : (
                      <Server />
                    )}
                    {t("fetchModels")}
                  </Button>
                </div>
              </div>

              {platformConfig.platform === "claude_code" ? (
                <div className="grid gap-3 border-t border-border/70 pt-4">
                  <p className="text-sm font-medium">{t("modelMappings")}</p>
                  <div className="grid gap-3 sm:grid-cols-2">
                    {(
                      ["sonnet", "opus", "haiku", "fable", "subagent"] as const
                    ).map((key) => (
                      <div className="grid gap-1.5" key={key}>
                        <Label htmlFor={`mapping-${key}`}>
                          {key === "subagent"
                            ? "SubAgent"
                            : key[0].toUpperCase() + key.slice(1)}
                        </Label>
                        <Input
                          id={`mapping-${key}`}
                          value={platformConfig[key] ?? ""}
                          onChange={(event) =>
                            updateClaudeMapping(key, event.target.value)
                          }
                          placeholder={t("optionalModel")}
                        />
                      </div>
                    ))}
                  </div>
                </div>
              ) : null}

              {platformConfig.platform === "codex" ? (
                <div className="grid gap-3 border-t border-border/70 pt-4">
                  <div className="flex items-center justify-between gap-3">
                    <p className="text-sm font-medium">{t("modelCatalog")}</p>
                    <Button
                      type="button"
                      size="sm"
                      variant="outline"
                      disabled={!model.trim()}
                      onClick={addCatalogModel}
                    >
                      <Plus />
                      {t("addCurrentModel")}
                    </Button>
                  </div>
                  {platformConfig.catalog.map((entry, index) => (
                    <div
                      className="grid gap-3 rounded-lg border border-border bg-background/70 p-3"
                      key={`${entry.id}-${index}`}
                    >
                      <div className="grid gap-3 sm:grid-cols-2">
                        <Input
                          value={entry.id}
                          onChange={(event) =>
                            updateCatalogModel(index, {
                              id: event.target.value,
                            })
                          }
                          placeholder="model-id"
                        />
                        <Input
                          value={entry.displayName}
                          onChange={(event) =>
                            updateCatalogModel(index, {
                              displayName: event.target.value,
                            })
                          }
                          placeholder={t("displayName")}
                        />
                      </div>
                      <Textarea
                        value={entry.description}
                        onChange={(event) =>
                          updateCatalogModel(index, {
                            description: event.target.value,
                          })
                        }
                        placeholder={t("modelDescription")}
                      />
                      <div className="grid gap-2">
                        <p className="text-xs font-medium text-muted-foreground">
                          {t("reasoningEfforts")}
                        </p>
                        <div className="grid grid-cols-4 gap-2 text-xs">
                          {reasoningEffortOrder.map((effort) => {
                            const checked =
                              entry.supportedReasoningEfforts.includes(effort);
                            return (
                              <label
                                className="flex items-center justify-between gap-2 rounded-md border border-border px-2 py-1.5"
                                key={effort}
                              >
                                {effort}
                                <Switch
                                  checked={checked}
                                  disabled={
                                    checked &&
                                    entry.supportedReasoningEfforts.length === 1
                                  }
                                  onCheckedChange={(next) => {
                                    const supported =
                                      reasoningEffortOrder.filter(
                                        (candidate) =>
                                          candidate === effort
                                            ? next
                                            : entry.supportedReasoningEfforts.includes(
                                                candidate,
                                              ),
                                      );
                                    updateCatalogModel(index, {
                                      supportedReasoningEfforts: supported,
                                      defaultReasoningEffort:
                                        supported.includes(
                                          entry.defaultReasoningEffort,
                                        )
                                          ? entry.defaultReasoningEffort
                                          : supported[0],
                                    });
                                  }}
                                />
                              </label>
                            );
                          })}
                        </div>
                      </div>
                      <div className="grid gap-3 sm:grid-cols-[1fr_1fr_auto]">
                        <Input
                          type="number"
                          min={1}
                          value={entry.contextWindow}
                          onChange={(event) =>
                            updateCatalogModel(index, {
                              contextWindow: Number(event.target.value),
                            })
                          }
                        />
                        <Select
                          value={entry.defaultReasoningEffort}
                          onValueChange={(value) =>
                            updateCatalogModel(index, {
                              defaultReasoningEffort:
                                value as CodexCatalogModel["defaultReasoningEffort"],
                            })
                          }
                        >
                          <SelectTrigger>
                            <SelectValue />
                          </SelectTrigger>
                          <SelectContent>
                            {entry.supportedReasoningEfforts.map((effort) => (
                              <SelectItem value={effort} key={effort}>
                                {effort}
                              </SelectItem>
                            ))}
                          </SelectContent>
                        </Select>
                        <Button
                          type="button"
                          variant="ghost"
                          size="icon"
                          onClick={() =>
                            setPlatformConfig({
                              ...platformConfig,
                              catalog: platformConfig.catalog.filter(
                                (_, current) => current !== index,
                              ),
                            })
                          }
                        >
                          <Trash2 />
                        </Button>
                      </div>
                      <div className="grid grid-cols-2 gap-2 text-xs sm:grid-cols-3">
                        {catalogCapabilityFields.map(([field, label]) => (
                          <label
                            className="flex items-center justify-between gap-2 rounded-md border border-border px-2 py-1.5"
                            key={field}
                          >
                            {label}
                            <Switch
                              checked={entry[field]}
                              onCheckedChange={(checked) =>
                                updateCatalogModel(index, { [field]: checked })
                              }
                            />
                          </label>
                        ))}
                      </div>
                    </div>
                  ))}
                </div>
              ) : null}

              {platformConfig.platform === "claude_desktop" ? (
                <div className="grid gap-3 border-t border-border/70 pt-4">
                  <div className="flex items-center justify-between gap-3">
                    <p className="text-sm font-medium">
                      {t("inferenceModels")}
                    </p>
                    <Button
                      type="button"
                      size="sm"
                      variant="outline"
                      disabled={
                        !model.trim() ||
                        platformConfig.models.includes(model.trim())
                      }
                      onClick={() =>
                        setPlatformConfig({
                          ...platformConfig,
                          models: [...platformConfig.models, model.trim()],
                        })
                      }
                    >
                      <Plus />
                      {t("addCurrentModel")}
                    </Button>
                  </div>
                  {platformConfig.models.map((entry, index) => (
                    <div className="flex gap-2" key={`${entry}-${index}`}>
                      <Input
                        value={entry}
                        onChange={(event) =>
                          setPlatformConfig({
                            ...platformConfig,
                            models: platformConfig.models.map(
                              (value, current) =>
                                current === index ? event.target.value : value,
                            ),
                          })
                        }
                      />
                      <Button
                        type="button"
                        variant="ghost"
                        size="icon"
                        onClick={() =>
                          setPlatformConfig({
                            ...platformConfig,
                            models: platformConfig.models.filter(
                              (_, current) => current !== index,
                            ),
                          })
                        }
                      >
                        <Trash2 />
                      </Button>
                    </div>
                  ))}
                </div>
              ) : null}

              {platform === "claude_desktop" ? (
                <p className="text-xs leading-relaxed text-muted-foreground">
                  {t("desktopProviderNote")}
                </p>
              ) : null}
            </div>
          ) : null}

          {kind !== "official_subscription" ? (
            <div className="grid gap-4 rounded-xl border border-border bg-muted/35 p-4">
              <div className="flex items-center justify-between gap-3">
                <p className="text-sm font-medium">{t("customHeaders")}</p>
                <Button
                  type="button"
                  size="sm"
                  variant="outline"
                  onClick={() =>
                    setCustomHeaders((headers) => [
                      ...headers,
                      { name: "", value: "" },
                    ])
                  }
                >
                  <Plus />
                  {t("addHeader")}
                </Button>
              </div>
              {customHeaders.map((header, index) => (
                <div
                  className="grid grid-cols-[1fr_1fr_auto] gap-2"
                  key={index}
                >
                  <Input
                    value={header.name}
                    onChange={(event) =>
                      updateHeader(index, { name: event.target.value })
                    }
                    placeholder="X-Custom-Header"
                  />
                  <Input
                    value={header.value}
                    onChange={(event) =>
                      updateHeader(index, { value: event.target.value })
                    }
                    placeholder={t("headerValue")}
                  />
                  <Button
                    type="button"
                    variant="ghost"
                    size="icon"
                    onClick={() =>
                      setCustomHeaders((headers) =>
                        headers.filter((_, current) => current !== index),
                      )
                    }
                  >
                    <Trash2 />
                  </Button>
                </div>
              ))}
              <div className="grid gap-2 border-t border-border/70 pt-4">
                <Label htmlFor="user-agent">User-Agent</Label>
                <Input
                  id="user-agent"
                  value={userAgent}
                  onChange={(event) => setUserAgent(event.target.value)}
                />
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
              {submitLabel ?? (mode === "create" ? t("create") : t("save"))}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

const reasoningEffortOrder: ReasoningEffort[] = [
  "none",
  "minimal",
  "low",
  "medium",
  "high",
  "xhigh",
  "max",
  "ultra",
];

const catalogCapabilityFields = [
  ["supportsImageInput", "Image"],
  ["supportsImageOriginal", "Original"],
  ["supportsParallelToolCalls", "Parallel"],
  ["supportsReasoningSummaries", "Summary"],
  ["supportsSearchTool", "Search"],
  ["supportsVerbosity", "Verbosity"],
] as const satisfies ReadonlyArray<
  readonly [
    (
      | "supportsImageInput"
      | "supportsImageOriginal"
      | "supportsParallelToolCalls"
      | "supportsReasoningSummaries"
      | "supportsSearchTool"
      | "supportsVerbosity"
    ),
    string,
  ]
>;

function newCatalogModel(id: string): CodexCatalogModel {
  return {
    id,
    displayName: id,
    description: "Custom provider model",
    contextWindow: 128_000,
    supportedReasoningEfforts: [...reasoningEffortOrder],
    defaultReasoningEffort: "medium",
    supportsImageInput: false,
    supportsImageOriginal: false,
    supportsParallelToolCalls: true,
    supportsReasoningSummaries: true,
    supportsSearchTool: false,
    supportsVerbosity: true,
  };
}

function withDefaultModel(
  config: ProviderPlatformConfig,
  model: string | null,
): ProviderPlatformConfig {
  if (config.platform === "codex") return { ...config, defaultModel: model };
  if (config.platform === "claude_code") {
    return { ...config, defaultModel: model };
  }
  return {
    ...config,
    models: model
      ? Array.from(new Set([model, ...config.models.filter(Boolean)]))
      : config.models,
  };
}
