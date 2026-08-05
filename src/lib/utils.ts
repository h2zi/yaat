import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}

export function optional(value: string): string | null {
  const trimmed = value.trim();
  return trimmed.length > 0 ? trimmed : null;
}

export function formatTokens(value: number): string {
  return new Intl.NumberFormat(undefined, {
    notation: value >= 10_000 ? "compact" : "standard",
    maximumFractionDigits: value >= 1_000_000 ? 2 : 1,
  }).format(value);
}

export function dateDaysAgo(days: number, timezone: string): string {
  const parts = new Intl.DateTimeFormat("en-US", {
    timeZone: timezone,
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
  }).formatToParts(new Date());
  const value = (type: Intl.DateTimeFormatPartTypes) =>
    Number(parts.find((part) => part.type === type)?.value);
  const calendar = new Date(
    Date.UTC(value("year"), value("month") - 1, value("day")),
  );
  calendar.setUTCDate(calendar.getUTCDate() - days);
  const year = calendar.getUTCFullYear();
  const month = String(calendar.getUTCMonth() + 1).padStart(2, "0");
  const day = String(calendar.getUTCDate()).padStart(2, "0");
  return `${year}-${month}-${day}`;
}
