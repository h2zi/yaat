import { useEffect, useMemo, useRef, useState } from "react";
import {
  Activity,
  ArrowDownToLine,
  ArrowUpFromLine,
  DatabaseZap,
  Gauge,
  LoaderCircle,
  RefreshCw,
  Sigma,
  X,
} from "lucide-react";
import {
  Area,
  AreaChart,
  CartesianGrid,
  ResponsiveContainer,
  Tooltip as ChartTooltip,
  XAxis,
  YAxis,
} from "recharts";

import { Button } from "@/components/ui/button";
import { UsageDateRange } from "@/components/usage-date-range";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import type { Language, Translator } from "@/i18n";
import { dateDaysAgo, formatTokens } from "@/lib/utils";
import {
  tokenInput,
  tokenTotal,
  type OperationProgress,
  type Platform,
  type UsageReport,
} from "@/types";

interface UsageDashboardProps {
  platform: Platform;
  timezone: string;
  language: Language;
  text: Translator;
  loading: boolean;
  progress: OperationProgress | null;
  report: UsageReport | null;
  active: boolean;
  refreshIntervalSeconds: 0 | 5 | 10 | 30 | 60;
  onQuery: (
    startDate: string,
    endDate: string,
    rescan?: boolean,
    model?: string | null,
  ) => Promise<void>;
  onRefreshIntervalChange: (seconds: 0 | 5 | 10 | 30 | 60) => Promise<void>;
  onCancel: () => Promise<void>;
}

export function UsageDashboard({
  platform,
  timezone,
  language,
  text,
  loading,
  progress,
  report,
  active,
  refreshIntervalSeconds,
  onQuery,
  onRefreshIntervalChange,
  onCancel,
}: UsageDashboardProps) {
  const [startDate, setStartDate] = useState(() => dateDaysAgo(6, timezone));
  const [endDate, setEndDate] = useState(() => dateDaysAgo(0, timezone));
  const [range, setRange] = useState<"today" | "7" | "14" | "30" | "custom">(
    "7",
  );
  const [model, setModel] = useState<string | null>(null);
  const [pageVisible, setPageVisible] = useState(
    () => document.visibilityState === "visible",
  );
  const queryInFlight = useRef(false);
  const queuedQuery = useRef<{
    action: "initial" | "range" | "custom" | "rescan";
    start: string;
    end: string;
    rescan: boolean;
    model: string | null;
  } | null>(null);
  const [cancelling, setCancelling] = useState(false);
  const [pendingAction, setPendingAction] = useState<
    "initial" | "range" | "custom" | "rescan" | null
  >(null);

  useEffect(() => {
    if (!loading) setCancelling(false);
  }, [loading]);

  function runQuery(
    action: "initial" | "range" | "custom" | "rescan",
    start: string,
    end: string,
    rescan = false,
    selectedModel = model,
  ) {
    if (queryInFlight.current) {
      queuedQuery.current = {
        action,
        start,
        end,
        rescan,
        model: selectedModel,
      };
      return;
    }
    queryInFlight.current = true;
    setPendingAction(action);
    void onQuery(start, end, rescan, selectedModel).finally(() => {
      queryInFlight.current = false;
      setPendingAction(null);
      const next = queuedQuery.current;
      queuedQuery.current = null;
      if (next) {
        runQuery(next.action, next.start, next.end, next.rescan, next.model);
      }
    });
  }

  useEffect(() => {
    if (range === "custom") {
      runQuery("initial", startDate, endDate);
      return;
    }
    const start = dateDaysAgo(
      range === "today" ? 0 : range === "7" ? 6 : range === "14" ? 13 : 29,
      timezone,
    );
    const end = dateDaysAgo(0, timezone);
    setStartDate(start);
    setEndDate(end);
    runQuery("initial", start, end);
  }, [platform, timezone]);

  const selectRange = (next: "today" | "7" | "14" | "30") => {
    if (next === range && report) return;
    const start = dateDaysAgo(
      next === "today" ? 0 : next === "7" ? 6 : next === "14" ? 13 : 29,
      timezone,
    );
    const end = dateDaysAgo(0, timezone);
    setRange(next);
    setStartDate(start);
    setEndDate(end);
    runQuery("range", start, end);
  };

  const selectCustomRange = (start: string, end: string) => {
    setRange("custom");
    setStartDate(start);
    setEndDate(end);
    runQuery("custom", start, end);
  };

  useEffect(() => {
    const onVisibilityChange = () =>
      setPageVisible(document.visibilityState === "visible");
    document.addEventListener("visibilitychange", onVisibilityChange);
    return () =>
      document.removeEventListener("visibilitychange", onVisibilityChange);
  }, []);

  useEffect(() => {
    if (!active || !pageVisible || refreshIntervalSeconds === 0) return;
    const timer = window.setInterval(() => {
      if (!loading && pendingAction === null) {
        runQuery("range", startDate, endDate, false, model);
      }
    }, refreshIntervalSeconds * 1_000);
    return () => window.clearInterval(timer);
  }, [
    active,
    endDate,
    loading,
    model,
    pageVisible,
    pendingAction,
    refreshIntervalSeconds,
    startDate,
  ]);

  const chartData = useMemo(
    () =>
      report?.buckets.map((bucket) => ({
        date: bucket.date,
        label: bucket.date.slice(5),
        input: tokenInput(bucket.tokens),
        output: bucket.tokens.output,
        total: tokenTotal(bucket.tokens),
      })) ?? [],
    [report],
  );

  const metric = [
    {
      label: text("totalTokens"),
      value: tokenTotal(
        report?.totals ?? {
          uncachedInput: 0,
          cacheRead: 0,
          cacheWrite: 0,
          output: 0,
          reasoningOutput: 0,
        },
      ),
      icon: Sigma,
      tone: "text-primary bg-primary/9",
    },
    {
      label: text("inputTokens"),
      value: report ? tokenInput(report.totals) : 0,
      icon: ArrowDownToLine,
      tone: "text-sky-600 bg-sky-500/9 dark:text-sky-400",
    },
    {
      label: text("outputTokens"),
      value: report?.totals.output ?? 0,
      icon: ArrowUpFromLine,
      tone: "text-emerald-600 bg-emerald-500/9 dark:text-emerald-400",
    },
    {
      label: text("cacheHitTokens"),
      value: report?.cacheHitTokens ?? 0,
      icon: DatabaseZap,
      tone: "text-cyan-600 bg-cyan-500/9 dark:text-cyan-400",
    },
    {
      label: text("cacheHitRate"),
      value: `${((report?.cacheHitRate ?? 0) * 100).toFixed(1)}%`,
      icon: Gauge,
      tone: "text-violet-600 bg-violet-500/9 dark:text-violet-400",
    },
    {
      label: text("requests"),
      value: report?.requestCount ?? 0,
      icon: Activity,
      tone: "text-amber-600 bg-amber-500/9 dark:text-amber-400",
    },
  ];

  const progressLabel =
    progress?.phase === "processing"
      ? text("progressProcessing")
      : progress?.phase === "saving"
        ? text("progressSaving")
        : text("progressDiscovering");
  const progressPercent =
    progress?.total && progress.total > 0
      ? Math.min(100, (progress.processed / progress.total) * 100)
      : null;
  const progressPanel = (
    <div className="rounded-xl border border-border bg-card p-4 shadow-card">
      <div className="flex items-center gap-2 text-sm text-muted-foreground">
        <LoaderCircle className="size-4 animate-spin text-primary" />
        <span>{progressLabel}</span>
        {progress && progress.processed > 0 ? (
          <span className="ml-auto tabular-nums">
            {progress.processed}
            {progress.total === null ? "" : ` / ${progress.total}`}
          </span>
        ) : null}
        <Button
          className="disabled:opacity-100"
          size="sm"
          variant="ghost"
          disabled={cancelling}
          onClick={() => {
            setCancelling(true);
            void onCancel();
          }}
        >
          {cancelling ? <LoaderCircle className="animate-spin" /> : <X />}
          {text("cancel")}
        </Button>
      </div>
      <div className="mt-3 h-1.5 overflow-hidden rounded-full bg-muted">
        <div
          className={`h-full rounded-full bg-primary transition-[width] duration-200 ${progressPercent === null ? "w-1/3 animate-pulse" : ""}`}
          style={
            progressPercent === null
              ? undefined
              : { width: `${progressPercent}%` }
          }
        />
      </div>
    </div>
  );

  return (
    <div className="mx-auto w-full max-w-[1240px] px-6 pb-10 pt-7 lg:px-8">
      <div className="flex flex-col justify-between gap-4 sm:flex-row sm:items-start">
        <div>
          <h1 className="text-2xl font-semibold tracking-[-0.035em]">
            {text("localUsage")}
          </h1>
          <p className="mt-1.5 text-sm text-muted-foreground">
            {text("localUsageDescription")}
          </p>
        </div>
        <div className="relative flex items-center gap-2">
          <Button
            className="min-w-28 disabled:opacity-100"
            variant="secondary"
            onClick={() => runQuery("rescan", startDate, endDate, true)}
            disabled={loading}
          >
            <RefreshCw
              className={pendingAction === "rescan" ? "animate-spin" : ""}
            />
            {text("refresh")}
          </Button>
          {loading && report ? (
            <div className="absolute right-0 top-11 z-30 w-[min(22rem,calc(100vw_-_3rem))]">
              {progressPanel}
            </div>
          ) : null}
        </div>
      </div>

      {loading && !report ? <div className="mt-5">{progressPanel}</div> : null}

      {loading && !report ? null : (
        <>
          <div className="mt-6 flex flex-wrap items-center gap-2 rounded-xl border border-border bg-card p-2 shadow-card">
            <UsageDateRange
              startDate={startDate}
              endDate={endDate}
              range={range}
              timezone={timezone}
              language={language}
              disabled={loading}
              text={text}
              onPresetSelect={selectRange}
              onCustomSelect={selectCustomRange}
            />
            <Select
              value={model ?? "__all__"}
              onValueChange={(value) => {
                const next = value === "__all__" ? null : value;
                setModel(next);
                runQuery("range", startDate, endDate, false, next);
              }}
            >
              <SelectTrigger className="h-8 w-48">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="__all__">{text("allModels")}</SelectItem>
                {report?.availableModels.map((value) => (
                  <SelectItem value={value} key={value}>
                    {value}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            <Select
              value={String(refreshIntervalSeconds)}
              onValueChange={(value) =>
                void onRefreshIntervalChange(
                  Number(value) as 0 | 5 | 10 | 30 | 60,
                )
              }
            >
              <SelectTrigger className="ml-auto h-8 w-40">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {[0, 5, 10, 30, 60].map((seconds) => (
                  <SelectItem value={String(seconds)} key={seconds}>
                    {seconds === 0
                      ? text("autoRefreshOff")
                      : `${text("autoRefresh")} · ${seconds}s`}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>

          <div className="mt-5 grid gap-3 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-6">
            {metric.map(({ label, value, icon: Icon, tone }) => (
              <Card key={label} className="overflow-hidden">
                <CardContent className="flex items-center gap-4 p-4">
                  <div
                    className={`grid size-10 shrink-0 place-items-center rounded-xl ${tone}`}
                  >
                    <Icon className="size-[18px]" />
                  </div>
                  <div className="min-w-0">
                    <p className="text-xs font-medium text-muted-foreground">
                      {label}
                    </p>
                    <p className="mt-0.5 truncate text-xl font-semibold tracking-[-0.035em] tabular-nums">
                      {typeof value === "number" ? formatTokens(value) : value}
                    </p>
                  </div>
                </CardContent>
              </Card>
            ))}
          </div>

          <div className="mt-5 grid gap-5 lg:grid-cols-[minmax(0,1.65fr)_minmax(18rem,0.75fr)]">
            <Card>
              <CardHeader className="pb-2">
                <CardTitle>{text("usageTrend")}</CardTitle>
                <CardDescription>
                  {report
                    ? `${report.startDate} — ${report.endDate} · ${report.timezone}`
                    : timezone}
                </CardDescription>
              </CardHeader>
              <CardContent className="pt-3">
                {chartData.length === 0 ? (
                  <div className="grid h-[290px] place-items-center text-sm text-muted-foreground">
                    {text("noUsage")}
                  </div>
                ) : (
                  <div className="h-[290px] w-full">
                    <ResponsiveContainer width="100%" height="100%">
                      <AreaChart
                        data={chartData}
                        margin={{ top: 10, right: 8, bottom: 0, left: -12 }}
                      >
                        <defs>
                          <linearGradient
                            id="input-fill"
                            x1="0"
                            y1="0"
                            x2="0"
                            y2="1"
                          >
                            <stop
                              offset="5%"
                              stopColor="var(--chart-1)"
                              stopOpacity={0.28}
                            />
                            <stop
                              offset="95%"
                              stopColor="var(--chart-1)"
                              stopOpacity={0.015}
                            />
                          </linearGradient>
                          <linearGradient
                            id="output-fill"
                            x1="0"
                            y1="0"
                            x2="0"
                            y2="1"
                          >
                            <stop
                              offset="5%"
                              stopColor="var(--chart-2)"
                              stopOpacity={0.24}
                            />
                            <stop
                              offset="95%"
                              stopColor="var(--chart-2)"
                              stopOpacity={0.01}
                            />
                          </linearGradient>
                        </defs>
                        <CartesianGrid
                          stroke="var(--border)"
                          strokeDasharray="3 5"
                          vertical={false}
                        />
                        <XAxis
                          dataKey="label"
                          axisLine={false}
                          tickLine={false}
                          tick={{
                            fill: "var(--muted-foreground)",
                            fontSize: 11,
                          }}
                          dy={8}
                        />
                        <YAxis
                          axisLine={false}
                          tickLine={false}
                          tick={{
                            fill: "var(--muted-foreground)",
                            fontSize: 11,
                          }}
                          tickFormatter={(value) => formatTokens(Number(value))}
                          width={56}
                        />
                        <ChartTooltip
                          cursor={{
                            stroke: "var(--border-strong)",
                            strokeWidth: 1,
                          }}
                          contentStyle={{
                            background: "var(--popover)",
                            border: "1px solid var(--border)",
                            borderRadius: 12,
                            boxShadow: "var(--shadow-menu)",
                            fontSize: 12,
                          }}
                          labelStyle={{
                            color: "var(--foreground)",
                            fontWeight: 600,
                            marginBottom: 6,
                          }}
                          itemStyle={{ color: "var(--muted-foreground)" }}
                          formatter={(value, name) => [
                            formatTokens(Number(value)),
                            name === "input"
                              ? text("inputTokens")
                              : text("outputTokens"),
                          ]}
                          labelFormatter={(_, payload) =>
                            payload?.[0]?.payload?.date ?? ""
                          }
                        />
                        <Area
                          type="monotone"
                          dataKey="input"
                          stroke="var(--chart-1)"
                          strokeWidth={2}
                          fill="url(#input-fill)"
                          isAnimationActive={false}
                        />
                        <Area
                          type="monotone"
                          dataKey="output"
                          stroke="var(--chart-2)"
                          strokeWidth={2}
                          fill="url(#output-fill)"
                          isAnimationActive={false}
                        />
                      </AreaChart>
                    </ResponsiveContainer>
                  </div>
                )}
              </CardContent>
            </Card>

            <Card>
              <CardHeader className="pb-3">
                <CardTitle>{text("usageBreakdown")}</CardTitle>
                <CardDescription>
                  {platform === "codex" ? "Codex" : "Claude Code"}
                </CardDescription>
              </CardHeader>
              <CardContent className="grid gap-4">
                {[
                  [
                    text("inputTokens"),
                    report ? tokenInput(report.totals) : 0,
                    "bg-chart-1",
                  ],
                  [
                    text("cacheRead"),
                    report?.totals.cacheRead ?? 0,
                    "bg-chart-2",
                  ],
                  [
                    text("cacheWrite"),
                    report?.totals.cacheWrite ?? 0,
                    "bg-chart-3",
                  ],
                  [
                    text("outputTokens"),
                    report?.totals.output ?? 0,
                    "bg-chart-4",
                  ],
                ].map(([label, value, color]) => {
                  const numeric = Number(value);
                  const total = Math.max(
                    1,
                    tokenTotal(
                      report?.totals ?? {
                        uncachedInput: 0,
                        cacheRead: 0,
                        cacheWrite: 0,
                        output: 0,
                        reasoningOutput: 0,
                      },
                    ),
                  );
                  return (
                    <div className="grid gap-1.5" key={String(label)}>
                      <div className="flex items-center justify-between gap-3 text-xs">
                        <span className="text-muted-foreground">{label}</span>
                        <span className="font-medium tabular-nums">
                          {formatTokens(numeric)}
                        </span>
                      </div>
                      <div className="h-1.5 overflow-hidden rounded-full bg-muted">
                        <div
                          className={`h-full rounded-full ${color}`}
                          style={{
                            width: `${Math.max(2, (numeric / total) * 100)}%`,
                          }}
                        />
                      </div>
                    </div>
                  );
                })}
                <div className="mt-2 rounded-xl border border-border bg-muted/30 p-3 text-xs leading-relaxed text-muted-foreground">
                  <div className="flex items-start gap-2">
                    <DatabaseZap className="mt-0.5 size-3.5 shrink-0 text-primary" />
                    <span>
                      {report
                        ? `${report.diagnostics.filesScanned} ${text("files")} · ${report.diagnostics.duplicateRecords} ${text("duplicated")}`
                        : "—"}
                    </span>
                  </div>
                  {report?.diagnostics.isPartial ? (
                    <p className="mt-2 text-amber-600 dark:text-amber-400">
                      {text("partialData")}
                    </p>
                  ) : null}
                </div>
              </CardContent>
            </Card>
          </div>
        </>
      )}
    </div>
  );
}
