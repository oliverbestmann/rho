import { Component, computed, inject, resource } from '@angular/core';
import { Router, RouterLink } from '@angular/router';
import { MatButtonModule } from '@angular/material/button';
import { MatIconModule } from '@angular/material/icon';
import { MatListModule } from '@angular/material/list';
import { MatProgressBarModule } from '@angular/material/progress-bar';
import { MatToolbarModule } from '@angular/material/toolbar';
import { Acp, errorText } from '../acp';
import { ago } from '../time';
import { dir, listSessions } from './list';

@Component({
  selector: 'app-sessions',
  imports: [
    RouterLink,
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
      <p class="message error">{{ errorText(error) }}</p>
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
  private readonly router = inject(Router);
  protected readonly ago = ago;
  protected readonly dir = dir;
  protected readonly errorText = errorText;

  protected readonly sessions = resource({
    params: () => this.acp.agent(),
    loader: ({ params: agent }) => listSessions(agent),
  });

  /** Sessions with a question for the user. */
  protected readonly waiting = computed(() => new Set(this.acp.asks().map((a) => a.sessionId)));

  protected logout() {
    this.router.navigate(['/login']);
  }
}
