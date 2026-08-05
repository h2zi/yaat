import {
  Ellipsis,
  KeyRound,
  LogIn,
  LoaderCircle,
  Pencil,
  Play,
  RefreshCw,
  Server,
  ShieldCheck,
  Trash2,
} from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardFooter } from "@/components/ui/card";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import type { Translator } from "@/i18n";
import { cn } from "@/lib/utils";
import type {
  ActivationMode,
  ProfileStatus,
  ProviderKind,
  ProviderProfile,
} from "@/types";

interface AccountCardProps {
  profile: ProviderProfile;
  active: boolean;
  activeMode: ActivationMode;
  busy: boolean;
  busyAction: "login" | "capture" | "launch" | null;
  text: Translator;
  onSwitch: () => void;
  onLaunch: () => void;
  onLogin: () => void;
  onCapture: () => void;
  onEdit: () => void;
  onDelete: () => void;
}

const kindIcon: Record<ProviderKind, typeof ShieldCheck> = {
  official_subscription: ShieldCheck,
  official_api: KeyRound,
  third_party: Server,
};

function kindLabel(kind: ProviderKind, text: AccountCardProps["text"]) {
  return text(
    kind === "official_subscription"
      ? "officialSubscription"
      : kind === "official_api"
        ? "officialApi"
        : "thirdParty",
  );
}

function statusMeta(
  status: ProfileStatus,
  text: AccountCardProps["text"],
): { label: string; variant: "success" | "warning" | "danger" | "secondary" } {
  switch (status) {
    case "ready":
      return { label: text("ready"), variant: "success" };
    case "needs_login":
      return { label: text("needsLogin"), variant: "warning" };
  }
}

export function AccountCard({
  profile,
  active,
  activeMode,
  busy,
  busyAction,
  text,
  onSwitch,
  onLaunch,
  onLogin,
  onCapture,
  onEdit,
  onDelete,
}: AccountCardProps) {
  const KindIcon = kindIcon[profile.kind];
  const status = statusMeta(profile.status, text);
  const secondary =
    profile.accountLabel || profile.baseUrl || profile.model || "—";

  return (
    <Card
      className={cn(
        "group relative overflow-hidden transition-[border-color,box-shadow,transform] hover:-translate-y-0.5 hover:border-border-strong hover:shadow-[0_12px_36px_rgb(15_23_42/0.07)]",
        active &&
          "border-primary/35 shadow-[0_0_0_1px_color-mix(in_oklab,var(--primary)_12%,transparent),0_10px_32px_rgb(71_71_184/0.08)]",
      )}
    >
      {active ? (
        <div className="absolute inset-x-0 top-0 h-0.5 bg-gradient-to-r from-primary via-sky-500 to-cyan-400" />
      ) : null}
      <CardContent className="p-5">
        <div className="flex items-start gap-4">
          <div
            className={cn(
              "grid size-11 shrink-0 place-items-center rounded-xl border shadow-xs",
              profile.platform === "codex"
                ? "border-indigo-500/15 bg-indigo-500/9 text-indigo-600 dark:text-indigo-400"
                : profile.platform === "claude_desktop"
                  ? "border-violet-500/15 bg-violet-500/9 text-violet-700 dark:text-violet-400"
                  : "border-orange-500/15 bg-orange-500/9 text-orange-700 dark:text-orange-400",
            )}
          >
            <span className="text-[15px] font-bold tracking-[-0.04em]">
              {profile.platform === "codex"
                ? "CX"
                : profile.platform === "claude_desktop"
                  ? "CD"
                  : "CL"}
            </span>
          </div>
          <div className="min-w-0 flex-1">
            <div className="flex min-w-0 items-center gap-2">
              <h3 className="truncate text-[15px] font-semibold tracking-[-0.015em]">
                {profile.name}
              </h3>
              {active ? (
                <Badge className="shrink-0">
                  {text(
                    activeMode === "managed_launch" ? "lastManaged" : "active",
                  )}
                </Badge>
              ) : null}
            </div>
            <p
              className="mt-1 truncate text-sm text-muted-foreground"
              title={secondary}
            >
              {secondary}
            </p>
          </div>
          <DropdownMenu>
            <Tooltip>
              <TooltipTrigger asChild>
                <DropdownMenuTrigger asChild>
                  <Button
                    variant="ghost"
                    size="icon-sm"
                    aria-label={text("more")}
                  >
                    <Ellipsis />
                  </Button>
                </DropdownMenuTrigger>
              </TooltipTrigger>
              <TooltipContent>{text("more")}</TooltipContent>
            </Tooltip>
            <DropdownMenuContent align="end">
              <DropdownMenuItem onSelect={onEdit}>
                <Pencil className="size-4" />
                {text("edit")}
              </DropdownMenuItem>
              <DropdownMenuSeparator />
              <DropdownMenuItem destructive onSelect={onDelete}>
                <Trash2 className="size-4" />
                {text("delete")}
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        </div>

        <div className="mt-5 flex flex-wrap items-center gap-2">
          <Badge variant="secondary">
            <KindIcon className="size-3" />
            {kindLabel(profile.kind, text)}
          </Badge>
          <Badge variant={status.variant}>
            <span
              className={cn(
                "size-1.5 rounded-full",
                status.variant === "success"
                  ? "bg-emerald-500"
                  : status.variant === "warning"
                    ? "bg-amber-500"
                    : status.variant === "danger"
                      ? "bg-destructive"
                      : "bg-muted-foreground",
              )}
            />
            {status.label}
          </Badge>
          {active ? (
            <span className="ml-auto text-xs text-muted-foreground">
              {text(
                activeMode === "managed_launch"
                  ? "managedLaunch"
                  : "globalCredential",
              )}
            </span>
          ) : null}
        </div>
      </CardContent>
      <CardFooter className="gap-2 bg-muted/20">
        {profile.kind === "official_subscription" ? (
          <Button
            className={cn(busyAction === "login" && "disabled:opacity-100")}
            variant="outline"
            size="sm"
            onClick={onLogin}
            disabled={busy}
          >
            {busyAction === "login" ? (
              <LoaderCircle className="animate-spin" />
            ) : (
              <LogIn />
            )}
            {text(profile.status === "needs_login" ? "login" : "relogin")}
          </Button>
        ) : null}
        {profile.kind === "official_subscription" &&
        profile.platform === "claude_desktop" &&
        profile.status === "needs_login" ? (
          <Button
            className={cn(busyAction === "capture" && "disabled:opacity-100")}
            variant="ghost"
            size="sm"
            onClick={onCapture}
            disabled={busy}
          >
            {busyAction === "capture" ? (
              <LoaderCircle className="animate-spin" />
            ) : (
              <RefreshCw />
            )}
            {text("completeLogin")}
          </Button>
        ) : profile.kind === "official_subscription" &&
          profile.platform === "claude_desktop" ? (
          <Tooltip>
            <TooltipTrigger asChild>
              <Button
                variant="ghost"
                size="icon-sm"
                className={cn(
                  busyAction === "capture" && "disabled:opacity-100",
                )}
                onClick={onCapture}
                disabled={busy}
                aria-label={text("verifyLogin")}
              >
                {busyAction === "capture" ? (
                  <LoaderCircle className="animate-spin" />
                ) : (
                  <RefreshCw />
                )}
              </Button>
            </TooltipTrigger>
            <TooltipContent>{text("verifyLogin")}</TooltipContent>
          </Tooltip>
        ) : profile.kind === "official_subscription" ? (
          <Tooltip>
            <TooltipTrigger asChild>
              <Button
                variant="ghost"
                size="icon-sm"
                className={cn(
                  busyAction === "capture" && "disabled:opacity-100",
                )}
                onClick={onCapture}
                disabled={busy}
                aria-label={text("capture")}
              >
                {busyAction === "capture" ? (
                  <LoaderCircle className="animate-spin" />
                ) : (
                  <RefreshCw />
                )}
              </Button>
            </TooltipTrigger>
            <TooltipContent>{text("capture")}</TooltipContent>
          </Tooltip>
        ) : null}
        {activeMode === "managed_launch" ? (
          <Button
            className={cn(
              "ml-auto",
              busyAction === "launch" && "disabled:opacity-100",
            )}
            size="sm"
            onClick={onLaunch}
            disabled={busy}
          >
            {busyAction === "launch" ? (
              <LoaderCircle className="animate-spin" />
            ) : (
              <Play />
            )}
            {text("launch")}
          </Button>
        ) : (
          <Button
            className="ml-auto"
            size="sm"
            onClick={onSwitch}
            disabled={busy}
          >
            {text(active ? "reapply" : "switch")}
          </Button>
        )}
      </CardFooter>
    </Card>
  );
}
