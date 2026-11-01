import { Component, computed, input } from '@angular/core';
import type * as acp from '@agentclientprotocol/sdk';
import { MatButtonModule } from '@angular/material/button';
import { MatMenuModule } from '@angular/material/menu';
import { rhoMeta } from './session-state';

/** A ring that fills with the context used, like Zed's; tap it for the numbers. */
@Component({
  selector: 'app-context-usage',
  imports: [MatButtonModule, MatMenuModule],
  template: `
    <button
      matIconButton
      [attr.aria-label]="'Context used: ' + percent() + '%'"
      [matMenuTriggerFor]="details"
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
        @if (cost(); as cost) {
          <div>Cost: {{ cost }}</div>
        }
        @if (sizes(); as s) {
          <div>History in memory: ~{{ s.history }}</div>
          @if (s.request) {
            <div>Last request: ~{{ s.request }}</div>
          }
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
  `,
})
export class ContextUsage {
  readonly usage = input.required<acp.UsageUpdate>();

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

  protected tokens(n: number) {
    return `${Math.round(n / 1000)}k`;
  }
}

/** 1536 -> "1.5 KB", in binary units like rho's `/usage`. */
export function bytes(n: number) {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}
