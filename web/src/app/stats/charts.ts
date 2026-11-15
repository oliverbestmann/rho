import { Component, computed, input } from '@angular/core';

export const HOUR = 3_600_000;
const W = 1000;
/** Styles of the day labels under a chart. */
const AXIS = `
  .axis {
    position: relative;
    height: 16px;
    font: var(--mat-sys-label-small);
    color: var(--mat-sys-on-surface-variant);
  }
  .axis span {
    position: absolute;
    transform: translateX(-50%);
  }
`;
const H = 200;

/** A month as year and zero-based month, like `Date`. */
export interface Month {
  year: number;
  month: number;
}

/** `YYYY-MM`, or the current month if missing or malformed. */
export function parseMonth(key: string | undefined, now = new Date()): Month {
  const m = /^(\d{4})-(\d{2})$/.exec(key ?? '');
  if (m && +m[2] >= 1 && +m[2] <= 12) return { year: +m[1], month: +m[2] - 1 };
  return { year: now.getFullYear(), month: now.getMonth() };
}

export function monthKey({ year, month }: Month): string {
  const d = new Date(year, month, 1);
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}`;
}

/** Local midnights from the 1st to the 1st of the next month, so days follow DST. */
export function dayBounds({ year, month }: Month): number[] {
  const days = new Date(year, month + 1, 0).getDate();
  return Array.from({ length: days + 1 }, (_, i) => new Date(year, month, i + 1).getTime());
}

/** The month before, up to the same point in time, or all of it for a finished month. */
export function compareRange({ year, month }: Month, now = new Date()): [number, number] {
  const from = new Date(year, month - 1, 1).getTime();
  const start = new Date(year, month, 1).getTime();
  if (now.getTime() >= new Date(year, month + 1, 1).getTime() || now.getTime() < start) {
    return [from, start];
  }
  const last = new Date(year, month, 0).getDate();
  const day = Math.min(now.getDate(), last);
  const to = new Date(
    year,
    month - 1,
    day,
    now.getHours(),
    now.getMinutes(),
    now.getSeconds(),
  ).getTime();
  return [from, Math.min(to, start)];
}

/** Relative change, or null without a base. */
export function change(current: number, previous: number): number | null {
  return previous > 0 ? (current - previous) / previous : null;
}

export interface Latest {
  at: number;
  utilization: number;
  resetsAt: number;
}

/**
 * Where a rate-limit window ends up at the current rate: the utilization at reset,
 * and when it reaches 100% if it does before the reset.
 */
export function pace(
  latest: Latest,
  windowMs: number,
): { projected: number; full: number | null } | null {
  const start = latest.resetsAt - windowMs;
  const elapsed = latest.at - start;
  if (elapsed <= 0 || latest.utilization <= 0) return null;
  const rate = latest.utilization / elapsed;
  const projected = rate * windowMs;
  return { projected, full: projected >= 1 ? start + 1 / rate : null };
}

/** An SVG path through the non-null values over `slots` steps, bridging gaps. */
export function linePath(values: (number | null)[], slots: number): string {
  const step = W / Math.max(slots - 1, 1);
  let d = '';
  values.forEach((v, i) => {
    if (v === null) return;
    const y = H - Math.min(v, 1) * H;
    d += `${d ? 'L' : 'M'}${(i * step).toFixed(1)} ${y.toFixed(1)}`;
  });
  return d;
}

/** Labels for every 7th day, at `x` percent of the width. */
function dayTicks(days: number[], center: boolean): { x: number; label: number }[] {
  const n = days.length - 1;
  return Array.from({ length: Math.ceil(n / 7) }, (_, k) => ({
    x: ((k * 7 + (center ? 0.5 : 0)) / n) * 100,
    label: k * 7 + 1,
  }));
}

/** A series of a line chart, hourly from the first day on. */
export interface Series {
  label: string;
  values: (number | null)[];
}

/** Hourly utilization (0..1) of up to two series over the month, with a 90% line. */
@Component({
  selector: 'app-line-chart',
  template: `
    <div class="plot">
      <svg [attr.viewBox]="'0 0 ' + W + ' ' + H" preserveAspectRatio="none" role="img">
        <line class="limit" x1="0" [attr.x2]="W" [attr.y1]="H * 0.1" [attr.y2]="H * 0.1" />
        @for (p of paths(); track $index) {
          <path [attr.d]="p" [class.second]="$index > 0" />
        }
      </svg>
      <span class="y top">100%</span>
      <span class="y bottom">0%</span>
    </div>
    <div class="axis">
      @for (t of ticks(); track t.label) {
        <span [style.left.%]="t.x">{{ t.label }}</span>
      }
    </div>
    <div class="legend">
      @for (s of series(); track s.label) {
        <span [class.second]="$index > 0">{{ s.label }}</span>
      }
    </div>
  `,
  styles:
    `
    .plot {
      position: relative;
    }
    svg {
      display: block;
      width: 100%;
      height: 96px;
      border-bottom: 1px solid var(--mat-sys-outline-variant);
    }
    path,
    line {
      fill: none;
      vector-effect: non-scaling-stroke;
    }
    path {
      stroke: var(--mat-sys-primary);
      stroke-width: 2;
      stroke-linecap: round;
      stroke-linejoin: round;
    }
    path.second {
      stroke: var(--mat-sys-tertiary);
    }
    .limit {
      stroke: var(--mat-sys-error);
      stroke-dasharray: 4 4;
    }
    .y {
      position: absolute;
      right: 0;
      font: var(--mat-sys-label-small);
      color: var(--mat-sys-on-surface-variant);
    }
    .top {
      top: -14px;
    }
    .bottom {
      bottom: 2px;
    }
    .legend {
      display: flex;
      gap: 12px;
      font: var(--mat-sys-label-small);
      color: var(--mat-sys-on-surface-variant);
    }
    .legend span::before {
      content: '';
      display: inline-block;
      width: 12px;
      height: 3px;
      margin-right: 4px;
      vertical-align: middle;
      background: var(--mat-sys-primary);
    }
    .legend span.second::before {
      background: var(--mat-sys-tertiary);
    }
  ` + AXIS,
})
export class LineChart {
  readonly series = input.required<Series[]>();
  /** The n+1 day bounds. */
  readonly days = input.required<number[]>();
  protected readonly W = W;
  protected readonly H = H;

  private readonly slots = computed(() => {
    const d = this.days();
    return Math.round((d[d.length - 1] - d[0]) / HOUR);
  });
  protected readonly paths = computed(() =>
    this.series().map((s) => linePath(s.values, this.slots())),
  );
  protected readonly ticks = computed(() => dayTicks(this.days(), false));
}

/** One bar per day, weekends shaded. `days` are the n+1 day bounds. */
@Component({
  selector: 'app-bar-chart',
  template: `
    <svg [attr.viewBox]="'0 0 ' + W + ' ' + H" preserveAspectRatio="none" role="img">
      @for (b of bars(); track $index) {
        @if (b.weekend) {
          <rect class="weekend" [attr.x]="b.x" y="0" [attr.width]="b.w" [attr.height]="H" />
        }
        <rect
          class="bar"
          [attr.x]="b.x + b.w * 0.15"
          [attr.y]="H - b.h"
          [attr.width]="b.w * 0.7"
          [attr.height]="b.h"
        />
      }
    </svg>
    <div class="axis">
      @for (t of ticks(); track t.label) {
        <span [style.left.%]="t.x">{{ t.label }}</span>
      }
    </div>
  `,
  styles:
    `
    svg {
      display: block;
      width: 100%;
      height: 96px;
    }
    .weekend {
      fill: var(--mat-sys-surface-container-high);
    }
    .bar {
      fill: var(--mat-sys-tertiary);
    }
  ` + AXIS,
})
export class BarChart {
  readonly values = input.required<number[]>();
  readonly days = input.required<number[]>();
  protected readonly W = W;
  protected readonly H = H;

  protected readonly bars = computed(() => {
    const values = this.values();
    const max = Math.max(...values, 0) || 1;
    const w = W / values.length;
    return values.map((v, i) => {
      const day = new Date(this.days()[i] ?? 0).getDay();
      return { x: i * w, w, h: (v / max) * H, weekend: day === 0 || day === 6 };
    });
  });
  protected readonly ticks = computed(() => dayTicks(this.days(), true));
}

/** A cell color from 0 (none) to 1 (the most). */
export const shade = (v: number) =>
  v > 0
    ? `color-mix(in oklch, var(--mat-sys-primary) ${Math.round(15 + v * 70)}%, var(--mat-sys-surface-container-high))`
    : 'var(--mat-sys-surface-container-high)';

/** Monday-first weekday, 0..6. */
const weekday = (d: Date) => (d.getDay() + 6) % 7;

/** Cost per UTC weekday (Monday first) and hour, from hourly costs since `start`. */
export function weekHours(hourly: number[], start: number): number[][] {
  const grid = Array.from({ length: 7 }, () => Array<number>(24).fill(0));
  hourly.forEach((cost, i) => {
    const d = new Date(start + i * HOUR);
    grid[(d.getUTCDay() + 6) % 7][d.getUTCHours()] += cost;
  });
  return grid;
}

/** ISO 8601 week, like `2026-W44`. */
export function isoWeek(at: number): string {
  const d = new Date(at);
  // The Thursday of the same week decides the year.
  const thursday = new Date(d.getFullYear(), d.getMonth(), d.getDate() - weekday(d) + 3);
  const jan1 = new Date(thursday.getFullYear(), 0, 1);
  const week = Math.floor(Math.round((thursday.getTime() - jan1.getTime()) / 86_400_000) / 7) + 1;
  return `${thursday.getFullYear()}-W${String(week).padStart(2, '0')}`;
}

/** Cost per ISO week of the given days, in order. */
export function weekCosts(days: number[], costs: number[]): { week: string; cost: number }[] {
  const out: { week: string; cost: number }[] = [];
  costs.forEach((cost, i) => {
    const week = isoWeek(days[i]);
    const last = out[out.length - 1];
    if (last?.week === week) last.cost += cost;
    else out.push({ week, cost });
  });
  return out;
}

const whole = new Intl.NumberFormat(undefined, {
  style: 'currency',
  currency: 'USD',
  maximumFractionDigits: 0,
});
const cents = new Intl.NumberFormat(undefined, { style: 'currency', currency: 'USD' });

/** Cost per weekday and hour, UTC. */
@Component({
  selector: 'app-heatmap',
  template: `
    <div class="grid">
      <span></span>
      @for (n of names; track n) {
        <span class="day">{{ n }}</span>
      }
      @for (h of hours; track h) {
        <span class="hour">{{ h % 3 === 0 ? (h < 10 ? '0' + h : h) : '' }}</span>
        @for (row of grid(); track $index; let d = $index) {
          <span
            class="cell"
            [style.background]="shade(row[h] / max())"
            [title]="names[d] + ' ' + h + ':00 · ' + cents.format(row[h])"
          ></span>
        }
      }
    </div>
  `,
  styles: `
    .grid {
      display: grid;
      grid-template-columns: auto repeat(7, 1fr);
      gap: 2px;
      font: var(--mat-sys-label-small);
      color: var(--mat-sys-on-surface-variant);
    }
    .day {
      text-align: center;
    }
    .hour {
      padding-right: 6px;
      line-height: 14px;
    }
    .cell {
      border-radius: 3px;
      aspect-ratio: 2;
    }
  `,
})
export class Heatmap {
  /** Cost per hour from `start`. */
  readonly hourly = input.required<number[]>();
  readonly start = input.required<number>();
  protected readonly hours = Array.from({ length: 24 }, (_, i) => i);
  protected readonly names = Array.from({ length: 7 }, (_, i) =>
    new Date(2024, 0, 1 + i).toLocaleDateString(undefined, { weekday: 'short' }),
  );
  protected readonly shade = shade;
  protected readonly cents = cents;
  protected readonly grid = computed(() => weekHours(this.hourly(), this.start()));
  protected readonly max = computed(() => Math.max(...this.grid().flat()) || 1);
}

/** Cost per day as a month calendar, and per ISO week. `days` are the n+1 day bounds. */
@Component({
  selector: 'app-calendar',
  template: `
    <div class="month">
      @for (n of names; track n) {
        <span class="name">{{ n }}</span>
      }
      @for (b of blanks(); track b) {
        <span></span>
      }
      @for (c of costs(); track $index) {
        <span class="day" [style.background]="shade(c / max())" [class.strong]="c / max() > 0.5">
          <b>{{ $index + 1 }}</b>
          @if (c > 0) {
            <small>{{ whole.format(c) }}</small>
          }
        </span>
      }
    </div>
    <h4>Per ISO week</h4>
    @for (w of weeks(); track w.week) {
      <div class="week">
        <span>{{ w.week }}</span>
        <span class="track"><span [style.width.%]="(w.cost / weekMax()) * 100"></span></span>
        <b>{{ whole.format(w.cost) }}</b>
      </div>
    }
  `,
  styles: `
    .month {
      display: grid;
      grid-template-columns: repeat(7, 1fr);
      gap: 4px;
    }
    .name {
      text-align: center;
      font: var(--mat-sys-label-small);
      color: var(--mat-sys-on-surface-variant);
    }
    .day {
      display: flex;
      flex-direction: column;
      justify-content: space-between;
      min-height: 44px;
      padding: 4px 6px;
      border-radius: var(--mat-sys-corner-small);
      font: var(--mat-sys-label-medium);
      overflow: hidden;
    }
    .day.strong {
      color: var(--mat-sys-on-primary);
    }
    small {
      font: var(--mat-sys-label-small);
    }
    h4 {
      margin: 16px 0 4px;
      font: var(--mat-sys-label-medium);
      color: var(--mat-sys-on-surface-variant);
      text-transform: uppercase;
    }
    .week {
      display: grid;
      grid-template-columns: auto 1fr auto;
      align-items: center;
      gap: 12px;
      padding: 2px 0;
      font: var(--mat-sys-body-medium);
    }
    .track {
      height: 10px;
      border-radius: 5px;
      background: var(--mat-sys-surface-container-high);
      overflow: hidden;
    }
    .track span {
      display: block;
      height: 100%;
      background: var(--mat-sys-outline);
    }
  `,
})
export class Calendar {
  readonly days = input.required<number[]>();
  /** Cost per day. */
  readonly costs = input.required<number[]>();
  protected readonly names = Array.from({ length: 7 }, (_, i) =>
    new Date(2024, 0, 1 + i).toLocaleDateString(undefined, { weekday: 'short' }),
  );
  protected readonly shade = shade;
  protected readonly whole = whole;
  protected readonly blanks = computed(() =>
    Array.from({ length: weekday(new Date(this.days()[0])) }, (_, i) => i),
  );
  protected readonly max = computed(() => Math.max(...this.costs()) || 1);
  protected readonly weeks = computed(() => weekCosts(this.days(), this.costs()));
  protected readonly weekMax = computed(() => Math.max(...this.weeks().map((w) => w.cost)) || 1);
}
