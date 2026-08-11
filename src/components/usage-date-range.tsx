import { useMemo, useState } from "react";
import {
  CalendarDays,
  Check,
  ChevronDown,
  ChevronLeft,
  ChevronRight,
} from "lucide-react";
import { Popover as PopoverPrimitive } from "radix-ui";

import { Button } from "@/components/ui/button";
import { selectTriggerStyles } from "@/components/ui/select";
import type { Language, Translator } from "@/i18n";
import { cn, dateDaysAgo } from "@/lib/utils";

type Preset = "today" | "7" | "14" | "30";
type RangeKind = Preset | "custom";

interface UsageDateRangeProps {
  startDate: string;
  endDate: string;
  range: RangeKind;
  timezone: string;
  language: Language;
  disabled?: boolean;
  text: Translator;
  onPresetSelect: (preset: Preset) => void;
  onCustomSelect: (startDate: string, endDate: string) => void;
}

interface MonthCell {
  date: string;
  day: number;
  inMonth: boolean;
}

const PRESETS: Preset[] = ["today", "7", "14", "30"];

function parseDate(value: string): Date {
  const [year, month, day] = value.split("-").map(Number);
  return new Date(Date.UTC(year, month - 1, day));
}

function formatDate(date: Date): string {
  return `${date.getUTCFullYear()}-${String(date.getUTCMonth() + 1).padStart(2, "0")}-${String(date.getUTCDate()).padStart(2, "0")}`;
}

function addDays(value: string, days: number): string {
  const date = parseDate(value);
  date.setUTCDate(date.getUTCDate() + days);
  return formatDate(date);
}

function monthCells(month: Date): MonthCell[] {
  const year = month.getUTCFullYear();
  const monthIndex = month.getUTCMonth();
  const firstDay = new Date(Date.UTC(year, monthIndex, 1));
  const mondayOffset = (firstDay.getUTCDay() + 6) % 7;
  const firstCell = new Date(Date.UTC(year, monthIndex, 1 - mondayOffset));

  return Array.from({ length: 42 }, (_, index) => {
    const date = new Date(firstCell);
    date.setUTCDate(firstCell.getUTCDate() + index);
    return {
      date: formatDate(date),
      day: date.getUTCDate(),
      inMonth: date.getUTCMonth() === monthIndex,
    };
  });
}

function presetLabel(preset: Preset, text: Translator): string {
  if (preset === "today") return text("today");
  if (preset === "7") return text("sevenDays");
  if (preset === "14") return text("fourteenDays");
  return text("thirtyDays");
}

export function UsageDateRange({
  startDate,
  endDate,
  range,
  timezone,
  language,
  disabled,
  text,
  onPresetSelect,
  onCustomSelect,
}: UsageDateRangeProps) {
  const [open, setOpen] = useState(false);
  const [visibleMonth, setVisibleMonth] = useState(() => {
    const date = parseDate(endDate);
    return new Date(Date.UTC(date.getUTCFullYear(), date.getUTCMonth(), 1));
  });
  const [draftStart, setDraftStart] = useState(startDate);
  const [draftEnd, setDraftEnd] = useState(endDate);
  const [selectingEnd, setSelectingEnd] = useState(false);

  const today = dateDaysAgo(0, timezone);
  const earliest = dateDaysAgo(365, timezone);
  const cells = useMemo(() => monthCells(visibleMonth), [visibleMonth]);
  const weekdays = useMemo(() => {
    const formatter = new Intl.DateTimeFormat(
      language === "zh" ? "zh-CN" : "en-US",
      {
        weekday: "narrow",
        timeZone: "UTC",
      },
    );
    return Array.from({ length: 7 }, (_, index) =>
      formatter.format(new Date(Date.UTC(2026, 0, 5 + index))),
    );
  }, [language]);
  const monthLabel = useMemo(
    () =>
      new Intl.DateTimeFormat(language === "zh" ? "zh-CN" : "en-US", {
        month: "long",
        year: "numeric",
        timeZone: "UTC",
      }).format(visibleMonth),
    [language, visibleMonth],
  );

  const setPopoverOpen = (next: boolean) => {
    setOpen(next);
    if (next) {
      const date = parseDate(endDate);
      setVisibleMonth(
        new Date(Date.UTC(date.getUTCFullYear(), date.getUTCMonth(), 1)),
      );
      setDraftStart(startDate);
      setDraftEnd(endDate);
      setSelectingEnd(false);
    }
  };

  const selectPreset = (preset: Preset) => {
    onPresetSelect(preset);
    setOpen(false);
  };

  const selectDate = (date: string) => {
    if (!selectingEnd) {
      setDraftStart(date);
      setDraftEnd("");
      setSelectingEnd(true);
      return;
    }

    if (date < draftStart) {
      setDraftStart(date);
      return;
    }

    const latestAllowed = addDays(draftStart, 365);
    if (date > latestAllowed) return;

    setDraftEnd(date);
    setSelectingEnd(false);
    setOpen(false);
    onCustomSelect(draftStart, date);
  };

  const previousMonth = new Date(
    Date.UTC(visibleMonth.getUTCFullYear(), visibleMonth.getUTCMonth() - 1, 1),
  );
  const nextMonth = new Date(
    Date.UTC(visibleMonth.getUTCFullYear(), visibleMonth.getUTCMonth() + 1, 1),
  );
  const canGoPrevious =
    formatDate(
      new Date(
        Date.UTC(
          previousMonth.getUTCFullYear(),
          previousMonth.getUTCMonth() + 1,
          0,
        ),
      ),
    ) >= earliest;
  const canGoNext = formatDate(nextMonth) <= today;
  const triggerLabel =
    range === "custom" ? text("custom") : presetLabel(range, text);

  return (
    <PopoverPrimitive.Root open={open} onOpenChange={setPopoverOpen}>
      <PopoverPrimitive.Trigger asChild>
        <button
          type="button"
          data-size="sm"
          disabled={disabled}
          className={cn(
            selectTriggerStyles,
            "text-left",
            open && "border-ring ring-[3px] ring-ring/20",
          )}
        >
          <CalendarDays className="size-3.5 text-primary" />
          <span className="text-xs font-semibold">{triggerLabel}</span>
          <span className="h-3.5 w-px bg-border" aria-hidden="true" />
          <span className="text-[10px] font-normal tabular-nums text-muted-foreground">
            {startDate} — {endDate}
          </span>
          <ChevronDown
            className={cn(
              "ml-0.5 size-3.5 text-muted-foreground transition-transform",
              open && "rotate-180",
            )}
          />
        </button>
      </PopoverPrimitive.Trigger>
      <PopoverPrimitive.Portal>
        <PopoverPrimitive.Content
          align="start"
          sideOffset={8}
          collisionPadding={16}
          className="z-[70] w-[min(24rem,calc(100vw_-_2rem))] overflow-hidden rounded-2xl border border-border bg-popover text-popover-foreground shadow-dialog outline-none data-[state=closed]:animate-out data-[state=open]:animate-in data-[state=closed]:fade-out data-[state=open]:fade-in data-[state=closed]:zoom-out-95 data-[state=open]:zoom-in-95"
        >
          <div className="border-b border-border bg-muted/35 px-4 py-3.5">
            <div className="flex flex-col items-stretch gap-2 sm:flex-row sm:items-center sm:justify-between sm:gap-4">
              <div>
                <p className="text-sm font-semibold tracking-[-0.015em]">
                  {text("dateRange")}
                </p>
                <p className="mt-0.5 text-xs text-muted-foreground">
                  {selectingEnd
                    ? text("selectEndDate")
                    : text("selectStartDate")}
                </p>
              </div>
              <div className="rounded-lg border border-border bg-card px-2.5 py-1.5 text-[11px] tabular-nums text-muted-foreground shadow-xs">
                {draftStart}
                <span className="px-1.5 text-border-strong">→</span>
                {draftEnd || "····-··-··"}
              </div>
            </div>
          </div>

          <div className="border-b border-border p-2.5">
            <div className="grid grid-cols-4 gap-1 rounded-xl bg-muted/60 p-1">
              {PRESETS.map((preset) => {
                const selected = range === preset && !selectingEnd;
                return (
                  <button
                    type="button"
                    key={preset}
                    aria-label={presetLabel(preset, text)}
                    onClick={() => selectPreset(preset)}
                    className={cn(
                      "flex h-8 items-center justify-center gap-1.5 rounded-lg px-2 text-xs font-medium outline-none transition-colors focus-visible:ring-[3px] focus-visible:ring-ring/25",
                      selected
                        ? "bg-card text-foreground shadow-sm ring-1 ring-border"
                        : "text-muted-foreground hover:bg-card/65 hover:text-foreground",
                    )}
                  >
                    {presetLabel(preset, text)}
                    {selected ? <Check className="size-3" /> : null}
                  </button>
                );
              })}
            </div>
          </div>

          <div className="p-3.5">
            <div className="mb-3 flex items-center justify-between">
              <Button
                type="button"
                variant="ghost"
                size="icon-sm"
                aria-label={text("previousMonth")}
                disabled={!canGoPrevious}
                onClick={() => setVisibleMonth(previousMonth)}
              >
                <ChevronLeft />
              </Button>
              <p className="text-sm font-semibold capitalize">{monthLabel}</p>
              <Button
                type="button"
                variant="ghost"
                size="icon-sm"
                aria-label={text("nextMonth")}
                disabled={!canGoNext}
                onClick={() => setVisibleMonth(nextMonth)}
              >
                <ChevronRight />
              </Button>
            </div>

            <div className="grid grid-cols-7 text-center">
              {weekdays.map((weekday, index) => (
                <span
                  className="pb-1.5 text-[10px] font-medium text-muted-foreground"
                  key={`${weekday}-${index}`}
                >
                  {weekday}
                </span>
              ))}
              {cells.map((cell) => {
                const unavailable =
                  !cell.inMonth || cell.date < earliest || cell.date > today;
                const start = cell.date === draftStart;
                const end = draftEnd !== "" && cell.date === draftEnd;
                const inside =
                  draftEnd !== "" &&
                  cell.date > draftStart &&
                  cell.date < draftEnd;
                return (
                  <div
                    key={cell.date}
                    className={cn(
                      "relative grid h-9 place-items-center",
                      inside && "bg-primary/9",
                      start && draftEnd !== "" && "rounded-l-lg bg-primary/9",
                      end && "rounded-r-lg bg-primary/9",
                    )}
                  >
                    <button
                      type="button"
                      aria-label={cell.date}
                      data-date={cell.date}
                      data-range-start={start || undefined}
                      data-range-end={end || undefined}
                      disabled={unavailable}
                      onClick={() => selectDate(cell.date)}
                      className={cn(
                        "grid size-8 place-items-center rounded-lg text-xs tabular-nums outline-none transition-[color,background-color,box-shadow] focus-visible:ring-[3px] focus-visible:ring-ring/30",
                        !unavailable &&
                          !start &&
                          !end &&
                          "text-foreground hover:bg-muted",
                        unavailable && "text-muted-foreground/20",
                        (start || end) &&
                          "bg-primary font-semibold text-primary-foreground shadow-sm",
                        cell.date === today &&
                          !start &&
                          !end &&
                          "font-semibold text-primary",
                      )}
                    >
                      {cell.day}
                    </button>
                  </div>
                );
              })}
            </div>
          </div>
        </PopoverPrimitive.Content>
      </PopoverPrimitive.Portal>
    </PopoverPrimitive.Root>
  );
}
