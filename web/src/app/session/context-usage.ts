import { Component, DestroyRef, computed, inject, input, signal } from '@angular/core';
import type * as acp from '@agentclientprotocol/sdk';
import { MatButtonModule } from '@angular/material/button';
import { MatMenuModule } from '@angular/material/menu';
import { Acp } from '../acp';
import { rhoMeta } from './session-state';

/** Token counts in `_meta.rho.request` and `.session`. */
interface Counts {
  input: number;
  output: number;
  cacheRead: number;
  cacheWrite: number;
}

/** `_rho/session_usage`: the session's recorded requests over all epochs, see rho/src/store/stats.rs. */
export interface SessionUsage {
  requests: number;
  cost: number;
  inputCost: number;
  outputCost: number;
  cacheReadCost: number;
  cacheWriteCost: number;
  first: number | null;
  last: number | null;
  activeHours: number;
  /** Some rows are estimated from turn notes, before requests were recorded. */
  estimated: boolean;
  /** Compactions and clears. */
  epochs: number;
  models: { provider: string; model: string; requests: number; cost: number }[];
}

const ROWS: [string, (c: Counts) => number][] = [
  ['Input', (c) => c.input],
  ['Cache read', (c) => c.cacheRead],
  ['Cache write', (c) => c.cacheWrite],
  ['Output', (c) => c.output],
];

/** A ring that fills with the context used, like Zed's; tap it for the numbers. */
@Component({
  selector: 'app-context-usage',
  imports: [MatButtonModule, MatMenuModule],
  template: `
    <button
      matIconButton
      [attr.aria-label]="'Context used: ' + percent() + '%'"
      [matMenuTriggerFor]="details"
      (menuOpened)="opened()"
      (menuClosed)="closed()"
    >
      <svg viewBox="0 0 20 20" aria-hidden="true">
        <circle class="track" cx="10" cy="10" r="8" />
        <circle
          class="fill"
          [class]="level()"
          cx="10"
          cy="10"
          r="8"
          pathLength="100"
          [attr.stroke-dasharray]="percent() + ' 100'"
        />
      </svg>
    </button>
    <mat-menu #details xPosition="before">
      <div class="details">
        <h4>Context</h4>
        <span>Used</span>
        <b>{{ tokens(usage().used) }} / {{ tokens(usage().size) }} · {{ percent() }}%</b>
        @if (cache(); as cache) {
          <span>Cache</span>
          <b>{{ cache }}</b>
        }
        @if (epochRequests() !== undefined) {
          <span>Requests</span>
          <b>{{ epochRequests() }}</b>
        }
        @if (sizes(); as s) {
          <span>History</span>
          <b>{{ s.history }}</b>
          @if (s.request) {
            <span>Last request</span>
            <b>{{ s.request }}</b>
          }
        }

        @if (table(); as rows) {
          <h4 class="cols"><span>Tokens</span><span>Last</span><span>Session</span></h4>
          @for (r of rows; track r.label) {
            <span>{{ r.label }}</span>
            <i>{{ r.last }}</i>
            <i>{{ r.session }}</i>
          }
        }

        <h4>Cost</h4>
        @if (cost(); as cost) {
          <span>Session</span>
          <b>{{ cost }}</b>
        }
        @if (all(); as a) {
          @if (a.inputCost + a.outputCost + a.cacheReadCost + a.cacheWriteCost > 0) {
            @for (t of costTypes(a); track t.label) {
              <span class="sub">{{ t.label }}</span>
              <b>{{ usd(t.cost) }}</b>
            }
          }
          @for (m of a.models; track m.provider + m.model) {
            <span class="sub" [class.gap]="$first" [title]="count(m.requests) + ' requests'">{{
              m.model
            }}</span>
            <b [class.gap]="$first">{{ usd(m.cost) }}</b>
          }

          <h4>All epochs</h4>
          <span>Requests</span>
          <b>{{ count(a.requests) }}</b>
          <span>Compactions</span>
          <b>{{ a.epochs }}</b>
          <span>Active hours</span>
          <b>{{ a.activeHours }}</b>
          @if (a.first !== null && a.last !== null) {
            <span>First</span>
            <b>{{ date(a.first) }}</b>
            <span>Last</span>
            <b>{{ date(a.last) }}</b>
          }
          @if (a.estimated) {
            <p>Older requests are estimated from turn notes.</p>
          }
        } @else if (allError()) {
          <p>{{ allError() }}</p>
        }
      </div>
    </mat-menu>
  `,
  styles: `
    svg {
      width: 20px;
      height: 20px;
      transform: rotate(-90deg);
    }
    circle {
      fill: none;
      stroke-width: 2.5;
    }
    .track {
      stroke: var(--mat-sys-outline-variant);
    }
    .fill {
      stroke: var(--mat-sys-primary);
    }
    .fill.yellow {
      stroke: light-dark(#9a6700, #d29922);
    }
    .fill.orange {
      stroke: light-dark(#bc4c00, #f0883e);
    }
    .fill.red {
      stroke: light-dark(#cf222e, #f85149);
    }
    .details {
      display: grid;
      grid-template-columns: 1fr auto auto;
      column-gap: 16px;
      row-gap: 2px;
      width: 248px;
      padding: 0 16px 8px;
      font: var(--mat-sys-body-medium);
    }
    .details > * {
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    h4 {
      grid-column: 1 / -1;
      margin: 12px -16px 4px;
      padding: 8px 16px 0;
      border-top: 1px solid var(--mat-sys-outline-variant);
      font: var(--mat-sys-label-small);
      color: var(--mat-sys-on-surface-variant);
      text-transform: uppercase;
      letter-spacing: 0.06em;
    }
    h4:first-child {
      margin-top: 0;
      border-top: 0;
    }
    h4.cols {
      display: grid;
      grid-template-columns: subgrid;
    }
    h4.cols span + span {
      text-align: right;
    }
    b,
    i {
      font-style: normal;
      font-weight: normal;
      text-align: right;
      font-variant-numeric: tabular-nums;
    }
    b {
      grid-column: 2 / -1;
    }
    .gap {
      margin-top: 8px;
    }
    .sub {
      padding-left: 12px;
      color: var(--mat-sys-on-surface-variant);
    }
    p {
      grid-column: 1 / -1;
      margin: 8px 0 0;
      white-space: normal;
      font: var(--mat-sys-body-small);
      color: var(--mat-sys-on-surface-variant);
    }
  `,
})
export class ContextUsage {
  readonly usage = input.required<acp.UsageUpdate>();
  readonly sessionId = input.required<string>();
  private readonly acp = inject(Acp);

  protected readonly all = signal<SessionUsage | undefined>(undefined);
  protected readonly allError = signal('');
  /** Ticks while the menu is open, for the cache countdown. */
  private readonly now = signal(Date.now());
  private timer?: ReturnType<typeof setInterval>;

  constructor() {
    inject(DestroyRef).onDestroy(() => clearInterval(this.timer));
  }

  protected readonly percent = computed(() => {
    const { used, size } = this.usage();
    return size > 0 ? Math.min(100, Math.round((100 * used) / size)) : 0;
  });

  protected readonly level = computed(() => {
    const p = this.percent();
    return p >= 75 ? 'red' : p >= 50 ? 'orange' : p >= 25 ? 'yellow' : '';
  });

  protected readonly cost = computed(() => {
    const c = this.usage().cost;
    return (
      c &&
      new Intl.NumberFormat(undefined, { style: 'currency', currency: c.currency }).format(c.amount)
    );
  });

  protected readonly epochRequests = computed(() => {
    const n = rhoMeta(this.usage()._meta)?.['epochRequests'];
    return typeof n === 'number' ? n : undefined;
  });

  /** Tokens per type of the last request and the session, and the cache hit rate. */
  protected readonly table = computed(() => {
    const rho = rhoMeta(this.usage()._meta);
    const last = rho?.['request'] as Counts | undefined;
    const session = rho?.['session'] as Counts | undefined;
    if (!last || !session) return undefined;
    const rows = ROWS.map(([label, f]) => ({
      label,
      last: compact.format(f(last)),
      session: compact.format(f(session)),
    }));
    rows.push({ label: 'Cache hit', last: hitRate(last), session: hitRate(session) });
    return rows;
  });

  /** Whether the provider's prompt cache is still warm, from the last recorded request. */
  protected readonly cache = computed(() => {
    const last = this.all()?.last;
    const ttl = rhoMeta(this.usage()._meta)?.['cacheTtlSecs'];
    if (last == null || typeof ttl !== 'number') return undefined;
    const left = Math.round((last + ttl * 1000 - this.now()) / 1000);
    if (left <= 0) return 'cold';
    return `warm · ${Math.floor(left / 60)}:${String(left % 60).padStart(2, '0')}`;
  });

  /** rho's approximate history and last request sizes, from `_meta.rho`. */
  protected readonly sizes = computed(() => {
    const rho = rhoMeta(this.usage()._meta);
    const [history, request] = [rho?.['historyBytes'], rho?.['requestBytes']];
    if (typeof history !== 'number') return undefined;
    return {
      history: bytes(history),
      request: typeof request === 'number' && request > 0 ? bytes(request) : undefined,
    };
  });

  protected async opened() {
    this.now.set(Date.now());
    this.timer = setInterval(() => this.now.set(Date.now()), 1000);
    const agent = this.acp.agent();
    if (!agent) return;
    try {
      this.all.set(
        await agent.request<SessionUsage>('_rho/session_usage', { sessionId: this.sessionId() }),
      );
      this.allError.set('');
    } catch (e) {
      this.allError.set(`All epochs: ${e instanceof Error ? e.message : String(e)}`);
    }
  }

  protected closed() {
    clearInterval(this.timer);
  }

  protected tokens(n: number) {
    return compact.format(n);
  }

  protected costTypes(a: SessionUsage) {
    return [
      { label: 'Input', cost: a.inputCost },
      { label: 'Cache read', cost: a.cacheReadCost },
      { label: 'Cache write', cost: a.cacheWriteCost },
      { label: 'Output', cost: a.outputCost },
    ];
  }

  protected count(n: number) {
    return n.toLocaleString();
  }

  protected usd(n: number) {
    return new Intl.NumberFormat(undefined, { style: 'currency', currency: 'USD' }).format(n);
  }

  protected date(ms: number) {
    return new Date(ms).toLocaleString(undefined, {
      day: 'numeric',
      month: 'short',
      hour: '2-digit',
      minute: '2-digit',
    });
  }
}

/** 207566626 -> "208M". */
const compact = new Intl.NumberFormat(undefined, {
  notation: 'compact',
  maximumFractionDigits: 1,
});

/** Share of the prompt read from cache, like rho's `/usage`. */
function hitRate(c: Counts) {
  const prompt = c.input + c.cacheRead + c.cacheWrite;
  return prompt > 0 ? `${Math.round((100 * c.cacheRead) / prompt)}%` : '–';
}

/** 1536 -> "1.5 KB", in binary units like rho's `/usage`. */
export function bytes(n: number) {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}
