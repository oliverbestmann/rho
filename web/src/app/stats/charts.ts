import { Component, computed, input } from '@angular/core';

export const HOUR = 3_600_000;
const W = 1000;
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

/** Hourly utilization (0..1) over `slots` hours, with gaps for null and a 90% line. */
@Component({
  selector: 'app-line-chart',
  template: `
    <svg [attr.viewBox]="'0 0 ' + W + ' ' + H" preserveAspectRatio="none" role="img">
      <line class="limit" x1="0" [attr.x2]="W" [attr.y1]="H * 0.1" [attr.y2]="H * 0.1" />
      <path [attr.d]="path()" />
    </svg>
  `,
  styles: `
    svg {
      display: block;
      width: 100%;
      height: 96px;
    }
    path,
    line {
      fill: none;
      vector-effect: non-scaling-stroke;
    }
    path {
      stroke: var(--mat-sys-primary);
      stroke-width: 2;
    }
    .limit {
      stroke: var(--mat-sys-error);
      stroke-dasharray: 4 4;
    }
  `,
})
export class LineChart {
  readonly values = input.required<(number | null)[]>();
  readonly slots = input.required<number>();
  protected readonly W = W;
  protected readonly H = H;

  protected readonly path = computed(() => {
    const step = W / Math.max(this.slots() - 1, 1);
    let d = '';
    let pen = false;
    this.values().forEach((v, i) => {
      if (v === null) {
        pen = false;
        return;
      }
      const y = H - Math.min(v, 1) * H;
      d += `${pen ? 'L' : 'M'}${(i * step).toFixed(1)} ${y.toFixed(1)}`;
      pen = true;
    });
    return d;
  });
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
  `,
  styles: `
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
  `,
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
}
