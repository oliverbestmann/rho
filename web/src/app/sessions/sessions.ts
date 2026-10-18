import { Component, computed, inject, resource } from '@angular/core';
import { Router, RouterLink } from '@angular/router';
import { MatButtonModule } from '@angular/material/button';
import { MatIconModule } from '@angular/material/icon';
import { MatListModule } from '@angular/material/list';
import { MatProgressBarModule } from '@angular/material/progress-bar';
import { MatProgressSpinnerModule } from '@angular/material/progress-spinner';
import { MatToolbarModule } from '@angular/material/toolbar';
import { Acp, errorText } from '../acp';
import { ago } from '../time';
import { dir, isRunning, listSessions } from './list';

@Component({
  selector: 'app-sessions',
  imports: [
    RouterLink,
    MatButtonModule,
    MatIconModule,
    MatListModule,
    MatProgressBarModule,
    MatProgressSpinnerModule,
    MatToolbarModule,
  ],
  template: `
    <mat-toolbar>
      <span class="title">Sessions</span>
      <a matIconButton routerLink="/new" aria-label="New session"><mat-icon>add</mat-icon></a>
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
            <span matListItemAvatar class="avatar waiting" aria-label="Waiting for an answer">
              <mat-icon>help</mat-icon>
            </span>
          } @else if (isRunning(s)) {
            <span matListItemAvatar class="avatar" aria-label="Running">
              <mat-spinner diameter="20" />
            </span>
          } @else {
            <span matListItemAvatar class="avatar"><mat-icon>chat_bubble</mat-icon></span>
          }
          <span matListItemTitle>{{ s.title || 'Untitled' }}</span>
          <span matListItemLine [title]="s.cwd">{{ dir(s.cwd) }}</span>
          <span matListItemMeta class="time">{{ ago(s.updatedAt) }}</span>
        </a>
      } @empty {
        @if (sessions.hasValue()) {
          <div class="empty">
            <mat-icon>forum</mat-icon>
            <p>No sessions yet.</p>
          </div>
        }
      }
    </mat-nav-list>
  `,
  styles: `
    mat-toolbar {
      background: var(--mat-sys-surface);
    }
    .title {
      flex: 1;
      font: var(--mat-sys-headline-small);
    }
    mat-nav-list {
      display: flex;
      flex-direction: column;
      gap: 8px;
      max-width: 760px;
      margin: 0 auto;
      padding: 8px 16px 24px;
    }
    a.mat-mdc-list-item {
      border-radius: var(--mat-sys-corner-large);
      background: var(--mat-sys-surface-container-low);
    }
    .avatar {
      display: flex;
      align-items: center;
      justify-content: center;
      background: var(--mat-sys-primary-container);
      color: var(--mat-sys-on-primary-container);
      --mat-progress-spinner-active-indicator-color: currentColor;
    }
    .avatar.waiting {
      background: var(--mat-sys-tertiary-container);
      color: var(--mat-sys-on-tertiary-container);
    }
    .time {
      font: var(--mat-sys-label-medium);
      color: var(--mat-sys-on-surface-variant);
    }
    .empty {
      display: flex;
      flex-direction: column;
      align-items: center;
      padding: 48px 16px;
      color: var(--mat-sys-on-surface-variant);
    }
    .empty mat-icon {
      width: 48px;
      height: 48px;
      font-size: 48px;
    }
    .message {
      padding: 16px;
      color: var(--mat-sys-on-surface-variant);
    }
    .error {
      color: var(--mat-sys-error);
    }
  `,
})
export class Sessions {
  private readonly acp = inject(Acp);
  private readonly router = inject(Router);
  protected readonly ago = ago;
  protected readonly dir = dir;
  protected readonly isRunning = isRunning;
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
