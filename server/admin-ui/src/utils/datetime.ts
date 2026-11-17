// 后台统一的时间显示。
//
// 服务端所有时间都是带 Z 的 RFC 3339 UTC（少数日期字段是纯 `YYYY-MM-DD`）。
// 界面一律按东八区显示，不跟随浏览器时区，避免同一份数据在不同机器上
// 显示成不同的时刻。其它页面不要再自己 slice / replace。

const OFFSET_MS = 8 * 60 * 60 * 1000;
const TZ_SUFFIX = "UTC+8";
const EMPTY = "—";

type Zone = "zoned" | "none" | "date";

interface Parsed {
  /** 绝对时刻；`zone === "none"` 时只是把字面字段当成 UTC 存放。 */
  ms: number;
  zone: Zone;
}

function parseValue(value: string | null | undefined): Parsed | null {
  const text = String(value ?? "").trim();
  if (!text) return null;
  if (/^\d{4}-\d{2}-\d{2}$/.test(text)) {
    return { ms: Date.parse(`${text}T00:00:00Z`), zone: "date" };
  }
  const zoned = /(?:Z|[+-]\d{2}:?\d{2})$/i.test(text);
  const ms = Date.parse(zoned ? text : `${text.replace(" ", "T")}Z`);
  if (Number.isNaN(ms)) return null;
  return { ms, zone: zoned ? "zoned" : "none" };
}

function parts(ms: number) {
  const at = new Date(ms);
  return {
    year: at.getUTCFullYear(),
    month: at.getUTCMonth() + 1,
    day: at.getUTCDate(),
    hour: at.getUTCHours(),
    minute: at.getUTCMinutes(),
    second: at.getUTCSeconds()
  };
}

function pad(value: number) {
  return String(value).padStart(2, "0");
}

function dateAndTime(parsed: Parsed) {
  if (parsed.zone === "date") {
    const date = new Date(parsed.ms).toISOString().slice(0, 10);
    return { date, time: "", dateTime: date };
  }
  // 没有时区的值不知道真实时刻，按字面字段原样显示并标注，不能再加 8 小时。
  const at = parts(parsed.zone === "zoned" ? parsed.ms + OFFSET_MS : parsed.ms);
  const date = `${at.year}-${pad(at.month)}-${pad(at.day)}`;
  const time = `${pad(at.hour)}:${pad(at.minute)}:${pad(at.second)}`;
  return { date, time, dateTime: `${date} ${time}` };
}

/** `2026-11-16 18:00:00`；纯日期保持 `2026-11-16`。 */
export function formatDateTime(value: string | null | undefined): string {
  const parsed = parseValue(value);
  if (!parsed) return EMPTY;
  return dateAndTime(parsed).dateTime;
}

/** 表格用：`11-16 18:00`。 */
export function formatShortDateTime(value: string | null | undefined): string {
  const parsed = parseValue(value);
  if (!parsed) return EMPTY;
  const { date, time } = dateAndTime(parsed);
  return time ? `${date.slice(5)} ${time.slice(0, 5)}` : date.slice(5);
}

/** 悬停/详情用完整时间，并明确时区；没有时区的字段会标出来。 */
export function formatFullDateTime(value: string | null | undefined): string {
  const parsed = parseValue(value);
  if (!parsed) return EMPTY;
  const { dateTime } = dateAndTime(parsed);
  if (parsed.zone === "date") return `${dateTime}（仅日期）`;
  if (parsed.zone === "none") return `${dateTime}（无时区）`;
  return `${dateTime} ${TZ_SUFFIX}`;
}

/** 只取日期（东八区）：`2026-11-16`。 */
export function formatDate(value: string | null | undefined): string {
  const parsed = parseValue(value);
  if (!parsed) return EMPTY;
  return dateAndTime(parsed).date;
}

/** 日期选择器的默认值：今天的东八区日期。 */
export function todayLocalDate(): string {
  return dateAndTime({ ms: Date.now(), zone: "zoned" }).date;
}

export const TIMEZONE_LABEL = TZ_SUFFIX;
