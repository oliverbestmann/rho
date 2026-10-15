import { Component, computed, inject, resource } from '@angular/core';
import { Router, RouterLink } from '@angular/router';
import * as acp from '@agentclientprotocol/sdk';
import { MatBadgeModule } from '@angular/material/badge';
import { MatButtonModule } from '@angular/material/button';
import { MatIconModule } from '@angular/material/icon';
import { MatListModule } from '@angular/material/list';
import { MatProgressBarModule } from '@angular/material/progress-bar';
import { MatToolbarModule } from '@angular/material/toolbar';
import { Acp } from '../acp';
import { Settings } from '../settings';
import { ago } from '../time';

/** All sessions, most recently updated first. */
export async function listSessions(agent: acp.ClientContext) {
  const all: acp.SessionInfo[] = [];
  let cursor: string | undefined;
  do {
    const page = await agent.request(acp.methods.agent.session.list, { cursor });
    all.push(...page.sessions);
    cursor = page.nextCursor ?? undefined;
  } while (cursor);
  return all.sort((a, b) => (b.updatedAt ?? '').localeCompare(a.updatedAt ?? ''));
}

/** A turn runs in the session; rho sends it in `_meta.rho.running`. */
export function isRunning(s: acp.SessionInfo) {
  const rho = s._meta?.['rho'];
  return typeof rho === 'object' && (rho as { running?: unknown }).running === true;
}

@Component({
  selector: 'app-sessions',
  imports: [
    RouterLink,
    MatBadgeModule,
    MatButtonModule,
    MatIconModule,
    MatListModule,
    MatProgressBarModule,
    MatToolbarModule,
  ],
  template: `
    <mat-toolbar>
      <span class="title">Sessions</span>
      <button matIconButton aria-label="Refresh" (click)="sessions.reload()">
        <mat-icon>refresh</mat-icon>
      </button>
      <button matIconButton aria-label="Change server" (click)="logout()">
        <mat-icon>logout</mat-icon>
      </button>
    </mat-toolbar>
    @if (sessions.isLoading()) {
      <mat-progress-bar mode="indeterminate" />
    }
    @if (sessions.error(); as error) {
      <p class="message error">{{ error.message }}</p>
    }
    <mat-nav-list>
      @for (s of sessions.value(); track s.sessionId) {
        <a
          mat-list-item
          [routerLink]="['/session', s.sessionId]"
          [queryParams]="{ cwd: s.cwd, title: s.title }"
        >
          @if (waiting().has(s.sessionId)) {
            <mat-icon matListItemIcon class="waiting" aria-label="Waiting for an answer"
              >help</mat-icon
            >
          }
          <span matListItemTitle>{{ s.title || 'Untitled' }}</span>
          <span matListItemLine>{{ dir(s.cwd) }} · {{ ago(s.updatedAt) }}</span>
        </a>
      } @empty {
        @if (sessions.hasValue()) {
          <p class="message">No sessions yet.</p>
        }
      }
    </mat-nav-list>
  `,
  styles: `
    .title {
      flex: 1;
    }
    .message {
      padding: 16px;
      color: var(--mat-sys-on-surface-variant);
    }
    .error {
      color: var(--mat-sys-error);
    }
    .waiting {
      color: var(--mat-sys-tertiary);
    }
  `,
})
export class Sessions {
  private readonly acp = inject(Acp);
  private readonly settings = inject(Settings);
  private readonly router = inject(Router);
  protected readonly ago = ago;

  protected readonly sessions = resource({
    params: () => this.acp.agent(),
    loader: ({ params: agent }) => listSessions(agent),
  });

  /** Sessions with a question for the user. */
  protected readonly waiting = computed(() => new Set(this.acp.asks().map((a) => a.sessionId)));

  protected dir(cwd: string) {
    return cwd.split('/').filter(Boolean).at(-1) ?? cwd;
  }

  protected logout() {
    this.router.navigate(['/login']);
  }
}
