import { Component, computed, inject, input, resource } from '@angular/core';
import { RouterLink } from '@angular/router';
import { MatButtonModule } from '@angular/material/button';
import { MatIconModule } from '@angular/material/icon';
import { MatProgressBarModule } from '@angular/material/progress-bar';
import { MatToolbarModule } from '@angular/material/toolbar';
import { Acp, errorText } from '../acp';
import {
  BarChart,
  Calendar,
  HOUR,
  Heatmap,
  LineChart,
  change,
  compareRange,
  dayBounds,
  monthKey,
  pace,
  parseMonth,
  type Latest,
} from './charts';

interface Totals {
  cost: number;
  requests: number;
  sessions: number;
  activeHours: number;
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
  cacheWriteTokens: number;
}

interface WindowStats {
  peak: number;
  highMs: number;
  hourly: (number | null)[];
  latest: Latest;
}

interface ProviderStats {
  provider: string;
  cost: number;
  requests: number;
  days: number[];
  fiveHour: WindowStats | null;
  sevenDay: WindowStats | null;
}

interface ModelStats {
  provider: string;
  model: string;
  requests: number;
  cost: number;
  outputTokens: number;
}

interface ProjectStats {
  /** Null for API proxy requests and deleted sessions. */
  path: string | null;
  requests: number;
  cost: number;
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
  cacheWriteTokens: number;
  inputCost: number;
  outputCost: number;
  cacheReadCost: number;
  cacheWriteCost: number;
}

/** Tokens and cost per usage type of a project. */
export const usageTypes = (p: ProjectStats) => [
  { label: 'Input', tokens: p.inputTokens, cost: p.inputCost },
  { label: 'Cache write', tokens: p.cacheWriteTokens, cost: p.cacheWriteCost },
  { label: 'Cache read', tokens: p.cacheReadTokens, cost: p.cacheReadCost },
  { label: 'Output', tokens: p.outputTokens, cost: p.outputCost },
];

/** Cost of requests without a known price per token type. */
export const unpriced = (p: ProjectStats) =>
  p.cost - usageTypes(p).reduce((sum, t) => sum + t.cost, 0);

/** The last segment of a path. */
export const projectName = (path: string) => path.replace(/\/+$/, '').split('/').pop() || path;

/** `_rho/stats`: a month's usage, see rho/src/acp/stats.rs. */
export interface StatsResponse {
  totals: Totals;
  previous: Totals;
  providers: ProviderStats[];
  models: ModelStats[];
  /** Cost per hour from the first day on. */
  hourly: number[];
  projects: ProjectStats[];
  first: number | null;
  estimatedUntil: number | null;
}

const money = new Intl.NumberFormat(undefined, { style: 'currency', currency: 'USD' });
const count = new Intl.NumberFormat();
const compact = new Intl.NumberFormat(undefined, { notation: 'compact' });
const percent = new Intl.NumberFormat(undefined, { style: 'percent' });
const signed = new Intl.NumberFormat(undefined, { style: 'percent', signDisplay: 'exceptZero' });
const hours = new Intl.NumberFormat(undefined, { maximumFractionDigits: 1 });
const date = new Intl.DateTimeFormat(undefined, { day: 'numeric', month: 'short' });
const dateTime = new Intl.DateTimeFormat(undefined, {
  weekday: 'short',
  hour: 'numeric',
  minute: '2-digit',
});

const cacheHit = (t: Totals) => {
  const all = t.inputTokens + t.cacheReadTokens + t.cacheWriteTokens;
  return all ? t.cacheReadTokens / all : 0;
};

const duration = (ms: number) => {
  const m = Math.round(ms / 60_000);
  return m < 60 ? `${m} min` : `${Math.floor(m / 60)} h ${m % 60} min`;
};

@Component({
  selector: 'app-stats',
  imports: [
    RouterLink,
    MatButtonModule,
    MatIconModule,
    MatProgressBarModule,
    MatToolbarModule,
    BarChart,
    Calendar,
    Heatmap,
    LineChart,
  ],
  template: `
    <mat-toolbar>
      <a matIconButton routerLink="/" aria-label="Back"><mat-icon>arrow_back</mat-icon></a>
      <span class="title">{{ title() }}</span>
      <a
        matIconButton
        [routerLink]="['/stats', prev()]"
        [disabled]="!hasPrev()"
        aria-label="Previous month"
        ><mat-icon>chevron_left</mat-icon></a
      >
      <a
        matIconButton
        [routerLink]="['/stats', next()]"
        [disabled]="current()"
        aria-label="Next month"
        ><mat-icon>chevron_right</mat-icon></a
      >
    </mat-toolbar>
    @if (stats.isLoading()) {
      <mat-progress-bar mode="indeterminate" />
    }
    <main>
      <p class="note">
        @if (current()) {
          Running · data up to {{ today }} ·
        }
        {{ zone }}
        @if (estimated(); as until) {
          <br />Estimated from turn notes before {{ until }}
        }
      </p>
      @if (stats.error(); as error) {
        <p class="error">{{ errorText(error) }}</p>
      }
      @if (stats.value(); as s) {
        <section class="cards">
          @for (c of cards(); track c.label) {
            <div class="card">
              <span class="label">{{ c.label }}</span>
              <span class="value">{{ c.value }}</span>
              <span class="sub">
                @if (c.delta) {
                  <b>{{ c.delta }}</b> ·
                }
                {{ c.before }}
                {{ current() ? 'by this point the month before' : 'the month before' }}
              </span>
            </div>
          }
        </section>

        <div class="pair">
          <section class="account">
            <h2>When you work</h2>
            <span class="sub">Cost per weekday and local hour</span>
            <app-heatmap [hourly]="s.hourly" [start]="days()[0]" />
          </section>
          <section class="account">
            <h2>Calendar</h2>
            <span class="sub">Cost per day of the month</span>
            <app-calendar [days]="days()" [costs]="dailyCost()" />
          </section>
        </div>

        @for (p of s.providers; track p.provider) {
          <section class="account">
            <h2>
              {{ p.provider }}
              <span class="sub"
                >{{ money.format(p.cost) }} · {{ count.format(p.requests) }} requests</span
              >
            </h2>
            <div class="chips">
              @for (c of chips(p); track c) {
                <span class="chip">{{ c }}</span>
              }
            </div>
            @if (p.sevenDay; as w) {
              <h3>7 d limit, hourly</h3>
              <app-line-chart [values]="w.hourly" [slots]="slots()" />
            }
            <h3>Cost per day</h3>
            <app-bar-chart [values]="p.days" [days]="days()" />
          </section>
        } @empty {
          <p class="note">No requests this month.</p>
        }

        @if (s.projects.length) {
          <section class="account">
            <h2>Projects</h2>
            @for (p of s.projects; track p.path) {
              <div class="project">
                <div class="head">
                  <b [title]="p.path ?? ''">{{ p.path ? projectName(p.path) : 'No session' }}</b>
                  <span class="sub">{{ count.format(p.requests) }} requests</span>
                  <b>{{ money.format(p.cost) }}</b>
                </div>
                <div class="types">
                  @for (t of usageTypes(p); track t.label) {
                    <div>
                      <span class="sub">{{ t.label }}</span>
                      <span>{{ compact.format(t.tokens) }} · {{ money.format(t.cost) }}</span>
                    </div>
                  }
                </div>
                @if (unpriced(p) > 0.005) {
                  <span class="sub">{{ money.format(unpriced(p)) }} without a breakdown</span>
                }
              </div>
            }
          </section>
        }

        @if (s.models.length) {
          <section class="account">
            <h2>Models</h2>
            <table>
              <tr>
                <th>Model</th>
                <th>Requests</th>
                <th>Output</th>
                <th>Cost</th>
              </tr>
              @for (m of s.models; track m.provider + m.model) {
                <tr>
                  <td>
                    {{ m.model }}<br /><span class="sub">{{ m.provider }}</span>
                  </td>
                  <td>{{ count.format(m.requests) }}</td>
                  <td>{{ compact.format(m.outputTokens) }}</td>
                  <td>{{ money.format(m.cost) }}</td>
                </tr>
              }
            </table>
          </section>
        }
      }
    </main>
  `,
  styles: `
    mat-toolbar {
      background: var(--mat-sys-surface);
    }
    .title {
      flex: 1;
      font: var(--mat-sys-headline-small);
    }
    main {
      max-width: 1000px;
      margin: 0 auto;
      padding: 0 16px 24px;
    }
    .note,
    .sub,
    .label,
    h3 {
      color: var(--mat-sys-on-surface-variant);
      font: var(--mat-sys-body-small);
    }
    .error {
      color: var(--mat-sys-error);
    }
    .cards {
      display: grid;
      grid-template-columns: repeat(auto-fill, minmax(150px, 1fr));
      gap: 8px;
    }
    .card,
    .account {
      display: flex;
      flex-direction: column;
      gap: 4px;
      padding: 12px 16px;
      border-radius: var(--mat-sys-corner-large);
      background: var(--mat-sys-surface-container-low);
    }
    .value {
      font: var(--mat-sys-headline-small);
    }
    .account {
      margin-top: 16px;
    }
    h2 {
      margin: 0;
      font: var(--mat-sys-title-medium);
    }
    h3 {
      margin: 8px 0 0;
    }
    .chips {
      display: flex;
      flex-wrap: wrap;
      gap: 6px;
    }
    .chip {
      padding: 2px 10px;
      border-radius: 999px;
      background: var(--mat-sys-secondary-container);
      color: var(--mat-sys-on-secondary-container);
      font: var(--mat-sys-label-medium);
    }
    .pair {
      display: grid;
      grid-template-columns: repeat(auto-fit, minmax(min(100%, 400px), 1fr));
      gap: 0 16px;
    }
    .project {
      padding: 8px 0;
      border-top: 1px solid var(--mat-sys-outline-variant);
    }
    .project:first-of-type {
      border-top: none;
    }
    .head {
      display: flex;
      align-items: baseline;
      gap: 8px;
    }
    .head .sub {
      flex: 1;
    }
    .types {
      display: grid;
      grid-template-columns: repeat(auto-fill, minmax(130px, 1fr));
      gap: 4px 12px;
      margin-top: 4px;
      font: var(--mat-sys-body-medium);
    }
    .types div {
      display: flex;
      flex-direction: column;
    }
    table {
      width: 100%;
      border-collapse: collapse;
    }
    th,
    td {
      padding: 6px 4px;
      text-align: right;
      font: var(--mat-sys-body-medium);
    }
    th:first-child,
    td:first-child {
      text-align: left;
      overflow-wrap: anywhere;
    }
    th {
      color: var(--mat-sys-on-surface-variant);
    }
  `,
})
export class Stats {
  protected readonly acp = inject(Acp);
  protected readonly errorText = errorText;
  protected readonly money = money;
  protected readonly count = count;
  protected readonly compact = compact;
  protected readonly projectName = projectName;
  protected readonly usageTypes = usageTypes;
  protected readonly unpriced = unpriced;
  protected readonly zone = Intl.DateTimeFormat().resolvedOptions().timeZone;
  protected readonly today = date.format(new Date());

  /** `YYYY-MM` from the route, the current month if missing. */
  readonly month = input<string>();

  private readonly parsed = computed(() => parseMonth(this.month()));
  protected readonly days = computed(() => dayBounds(this.parsed()));
  protected readonly slots = computed(() => {
    const d = this.days();
    return Math.round((d[d.length - 1] - d[0]) / HOUR);
  });
  protected readonly current = computed(() => this.days()[this.days().length - 1] > Date.now());
  protected readonly title = computed(() =>
    new Date(this.days()[0]).toLocaleDateString(undefined, { month: 'long', year: 'numeric' }),
  );
  protected readonly prev = computed(() => {
    const m = this.parsed();
    return monthKey({ year: m.year, month: m.month - 1 });
  });
  protected readonly next = computed(() => {
    const m = this.parsed();
    return monthKey({ year: m.year, month: m.month + 1 });
  });
  protected readonly hasPrev = computed(() => {
    const first = this.stats.value()?.first;
    return first != null && first < this.days()[0];
  });

  protected readonly stats = resource({
    params: () => ({ agent: this.acp.agent(), month: this.parsed() }),
    loader: ({ params: { agent, month } }) => {
      if (!agent) return Promise.reject(new Error('Not connected'));
      return agent.request<StatsResponse>('_rho/stats', {
        days: dayBounds(month),
        compare: compareRange(month),
      });
    },
  });

  /** Cost per day over all providers. */
  protected readonly dailyCost = computed(() =>
    this.days()
      .slice(1)
      .map((_, i) => (this.stats.value()?.providers ?? []).reduce((sum, p) => sum + p.days[i], 0)),
  );

  protected readonly estimated = computed(() => {
    const until = this.stats.value()?.estimatedUntil;
    return until != null && until >= this.days()[0] ? date.format(until) : null;
  });

  protected readonly cards = computed(() => {
    const s = this.stats.value();
    if (!s) return [];
    const t = s.totals;
    const p = s.previous;
    const card = (label: string, cur: number, prev: number, fmt: (n: number) => string) => {
      const c = change(cur, prev);
      return {
        label,
        value: fmt(cur),
        delta: c === null ? null : signed.format(c),
        before: fmt(prev),
      };
    };
    const hit = cacheHit(t);
    const before = cacheHit(p);
    return [
      card('Cost', t.cost, p.cost, (n) => money.format(n)),
      card('Requests', t.requests, p.requests, (n) => count.format(n)),
      card('Sessions', t.sessions, p.sessions, (n) => count.format(n)),
      card('Active hours', t.activeHours, p.activeHours, (n) => hours.format(n)),
      card('Output tokens', t.outputTokens, p.outputTokens, (n) => compact.format(n)),
      {
        label: 'Cache hit',
        value: percent.format(hit),
        delta: p.requests ? `${Math.round((hit - before) * 100)} pt` : null,
        before: percent.format(before),
      },
    ];
  });

  protected chips(p: ProviderStats): string[] {
    const out: string[] = [];
    const window = (name: string, w: WindowStats | null, ms: number) => {
      if (!w) return;
      out.push(`${name} peak ${percent.format(w.peak)}`);
      if (w.highMs > 0) out.push(`${name} at 90%+ for ${duration(w.highMs)}`);
      if (!this.current() || w.latest.resetsAt < Date.now()) return;
      const pc = pace(w.latest, ms);
      if (!pc) return;
      out.push(
        pc.full === null
          ? `${name} pace: ${percent.format(pc.projected)} at reset`
          : `${name} pace: 100% by ${dateTime.format(pc.full)}`,
      );
    };
    window('5 h', p.fiveHour, 5 * HOUR);
    window('7 d', p.sevenDay, 7 * 24 * HOUR);
    return out;
  }
}
