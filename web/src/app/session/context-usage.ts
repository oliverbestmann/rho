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
        <div>{{ tokens(usage().used) }} of {{ tokens(usage().size) }} tokens, {{ percent() }}%</div>
        @if (cache(); as cache) {
          <div>{{ cache }}</div>
        }
        @if (table(); as rows) {
          <table>
            <tr>
              <th></th>
              <th>Last request</th>
              <th>Session</th>
            </tr>
            @for (r of rows; track r.label) {
              <tr>
                <td>{{ r.label }}</td>
                <td>{{ r.last }}</td>
                <td>{{ r.session }}</td>
              </tr>
            }
          </table>
        }
        @if (epochRequests() !== undefined) {
          <div>Requests since compaction or clear: {{ epochRequests() }}</div>
        }
        @if (cost(); as cost) {
          <div>Cost: {{ cost }}</div>
        }
        @if (sizes(); as s) {
          <div>History in memory: ~{{ s.history }}</div>
          @if (s.request) {
            <div>Last request: ~{{ s.request }}</div>
          }
        }
        @if (all(); as a) {
          <h4>All epochs</h4>
          <div>
            {{ count(a.requests) }} requests in {{ a.activeHours }} active hours,
            {{ a.epochs }} compactions or clears
          </div>
          @if (a.first !== null && a.last !== null) {
            <div>{{ date(a.first) }} – {{ date(a.last) }}</div>
          }
          @if (a.inputCost + a.outputCost + a.cacheReadCost + a.cacheWriteCost > 0) {
            <div>
              Input {{ usd(a.inputCost) }} · Cache read {{ usd(a.cacheReadCost) }} · Cache write
              {{ usd(a.cacheWriteCost) }} · Output {{ usd(a.outputCost) }}
            </div>
          }
          @for (m of a.models; track m.provider + m.model) {
            <div>{{ m.model }}: {{ count(m.requests) }} requests, {{ usd(m.cost) }}</div>
          }
          @if (a.estimated) {
            <div class="note">Partly estimated from turn notes</div>
          }
        } @else if (allError()) {
          <div class="note">{{ allError() }}</div>
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
      padding: 8px 16px;
      font: var(--mat-sys-body-medium);
      white-space: nowrap;
    }
    table {
      margin: 4px 0;
      border-spacing: 12px 0;
      margin-left: -12px;
    }
    th {
      font: var(--mat-sys-label-medium);
      color: var(--mat-sys-on-surface-variant);
      text-align: right;
    }
    td + td {
      text-align: right;
      font-variant-numeric: tabular-nums;
    }
    h4 {
      margin: 12px 0 2px;
      font: var(--mat-sys-label-medium);
      color: var(--mat-sys-on-surface-variant);
      text-transform: uppercase;
    }
    .note {
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
      last: this.count(f(last)),
      session: this.count(f(session)),
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
    if (left <= 0) return `Cache: cold (no request in the last ${Math.round(ttl / 60)}m)`;
    return `Cache: warm, expires in ${Math.floor(left / 60)}m ${String(left % 60).padStart(2, '0')}s`;
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
    return `${Math.round(n / 1000)}k`;
  }

  protected count(n: number) {
    return n.toLocaleString();
  }

  protected usd(n: number) {
    return new Intl.NumberFormat(undefined, { style: 'currency', currency: 'USD' }).format(n);
  }

  protected date(ms: number) {
    return new Date(ms).toLocaleString(undefined, { dateStyle: 'medium', timeStyle: 'short' });
  }
}

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
