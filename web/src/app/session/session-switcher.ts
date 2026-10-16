import { Component, input, output } from '@angular/core';
import { RouterLink } from '@angular/router';
import type * as acp from '@agentclientprotocol/sdk';
import { MatButtonModule } from '@angular/material/button';
import { MatIconModule } from '@angular/material/icon';
import { MatMenuModule } from '@angular/material/menu';
import { MatProgressSpinnerModule } from '@angular/material/progress-spinner';
import { dir, isRunning } from '../sessions/list';
import { ago } from '../time';

/** A toolbar button with a menu of sessions to switch to. */
@Component({
  selector: 'app-session-switcher',
  imports: [RouterLink, MatButtonModule, MatIconModule, MatMenuModule, MatProgressSpinnerModule],
  template: `
    <button
      matIconButton
      aria-label="Switch session"
      [matMenuTriggerFor]="menu"
      (menuOpened)="opened.emit()"
    >
      <mat-icon>swap_horiz</mat-icon>
    </button>
    <mat-menu #menu xPosition="before">
      @for (s of sessions(); track s.sessionId) {
        <a
          mat-menu-item
          [routerLink]="['/session', s.sessionId]"
          [queryParams]="{ cwd: s.cwd, title: s.title }"
          [class.current]="s.sessionId === current()"
          [attr.aria-current]="s.sessionId === current() ? 'page' : null"
        >
          <span>{{ s.title || 'Untitled' }}</span>
          @if (isRunning(s)) {
            <mat-spinner class="busy" diameter="16" aria-label="Running" />
          }
          <span class="time" [title]="s.cwd">{{ dir(s.cwd) }} · {{ ago(s.updatedAt) }}</span>
        </a>
      } @empty {
        <button mat-menu-item disabled>{{ loading() ? 'Loading…' : 'No sessions' }}</button>
      }
    </mat-menu>
  `,
  styles: `
    /* The item is sized for one line; titles wrap and have the time below. */
    a {
      padding-block: 10px;
    }
    .current {
      background: var(--mat-sys-secondary-container);
      color: var(--mat-sys-on-secondary-container);
    }
    .busy {
      display: inline-block;
      vertical-align: middle;
      margin-left: 12px;
    }
    /* Its own line under the title. */
    .time {
      display: block;
      margin-top: 2px;
      font: var(--mat-sys-body-small);
      color: var(--mat-sys-on-surface-variant);
    }
  `,
})
export class SessionSwitcher {
  readonly sessions = input<acp.SessionInfo[]>([]);
  /** The open session, marked in the menu. */
  readonly current = input.required<string>();
  readonly loading = input(false);
  /** The menu opened, so the list can be reloaded. */
  readonly opened = output();

  protected readonly ago = ago;
  protected readonly dir = dir;
  protected readonly isRunning = isRunning;
}
