import { Component, ElementRef, afterRenderEffect, computed, inject, signal } from '@angular/core';
import { Router } from '@angular/router';
import type * as acp from '@agentclientprotocol/sdk';
import { Acp } from '../acp';
import { Autofocus } from '../autofocus';
import { dir } from './list';

/** The query's characters appear in this order in `text`, like `q.*u.*e.*r.*y`; spaces are ignored. */
export function fuzzyMatch(query: string, text: string) {
  const q = query.toLowerCase().replace(/\s+/g, '');
  const t = text.toLowerCase();
  let at = 0;
  for (const c of q) {
    at = t.indexOf(c, at) + 1;
    if (at === 0) return false;
  }
  return true;
}

/** Ctrl-K opens a search over the titles and directories of all sessions; Enter opens the selected one. */
@Component({
  selector: 'app-quick-switch',
  imports: [Autofocus],
  host: { '(document:keydown.control.k)': 'toggle($event)' },
  template: `
    @if (open()) {
      <div class="backdrop" (click)="close()"></div>
      <div class="panel" role="dialog" aria-label="Switch session">
        <input
          appAutofocus
          placeholder="Search sessions"
          aria-label="Search sessions"
          [value]="query()"
          (input)="search($any($event.target).value)"
          (keydown)="onKey($event)"
        />
        <ul role="listbox">
          @for (s of results(); track s.sessionId; let i = $index) {
            <li
              role="option"
              [attr.aria-selected]="i === selected()"
              [class.selected]="i === selected()"
              (mousedown)="go(s)"
            >
              <span>{{ s.title || 'Untitled' }}</span>
              <span class="dir" [title]="s.cwd">{{ dir(s.cwd) }}</span>
            </li>
          } @empty {
            <li class="empty">{{ sessions() ? 'No match' : 'Loading…' }}</li>
          }
        </ul>
      </div>
    }
  `,
  styles: `
    .backdrop {
      position: fixed;
      inset: 0;
      z-index: 1000;
    }
    .panel {
      position: fixed;
      top: 64px;
      left: 50%;
      transform: translateX(-50%);
      z-index: 1001;
      width: min(560px, calc(100vw - 32px));
      border-radius: var(--mat-sys-corner-large);
      background: var(--mat-sys-surface-container-high);
      box-shadow: var(--mat-sys-level3);
      overflow: hidden;
    }
    input {
      box-sizing: border-box;
      width: 100%;
      padding: 14px 16px;
      border: none;
      border-bottom: 1px solid var(--mat-sys-outline-variant);
      outline: none;
      background: transparent;
      color: var(--mat-sys-on-surface);
      font: var(--mat-sys-body-large);
    }
    ul {
      margin: 0;
      padding: 4px 0;
      list-style: none;
      max-height: 60vh;
      overflow-y: auto;
    }
    li {
      display: flex;
      gap: 12px;
      justify-content: space-between;
      padding: 8px 16px;
      cursor: pointer;
      font: var(--mat-sys-body-medium);
    }
    li.selected {
      background: var(--mat-sys-secondary-container);
      color: var(--mat-sys-on-secondary-container);
    }
    .dir,
    .empty {
      color: var(--mat-sys-on-surface-variant);
    }
    .dir {
      flex-shrink: 0;
      font: var(--mat-sys-body-small);
      align-self: center;
    }
  `,
})
export class QuickSwitch {
  private readonly acp = inject(Acp);
  private readonly router = inject(Router);
  protected readonly dir = dir;

  protected readonly open = signal(false);
  protected readonly query = signal('');
  protected readonly selected = signal(0);
  /** Loaded each time the search opens; undefined while loading. */
  protected readonly sessions = signal<acp.SessionInfo[] | undefined>(undefined);
  protected readonly results = computed(() =>
    (this.sessions() ?? [])
      .filter((s) => fuzzyMatch(this.query(), `${s.title ?? ''} ${s.cwd}`))
      .slice(0, 50),
  );

  constructor() {
    const host = inject<ElementRef<HTMLElement>>(ElementRef).nativeElement;
    afterRenderEffect(() => {
      this.selected();
      this.results();
      host.querySelector('li.selected')?.scrollIntoView({ block: 'nearest' });
    });
  }

  protected toggle(e: Event) {
    const agent = this.acp.agent();
    if (!agent) return;
    e.preventDefault();
    if (this.open()) return this.close();
    this.query.set('');
    this.selected.set(0);
    this.sessions.set(undefined);
    this.open.set(true);
    this.acp.listSessions(agent).then(
      (all) => this.sessions.set(all),
      () => this.sessions.set([]),
    );
  }

  protected close() {
    this.open.set(false);
  }

  protected search(query: string) {
    this.query.set(query);
    this.selected.set(0);
  }

  protected onKey(e: KeyboardEvent) {
    const n = this.results().length;
    if (e.key === 'Escape') {
      this.close();
    } else if (e.key === 'ArrowDown' && n) {
      this.selected.update((i) => (i + 1) % n);
    } else if (e.key === 'ArrowUp' && n) {
      this.selected.update((i) => (i - 1 + n) % n);
    } else if (e.key === 'Enter') {
      const s = this.results()[this.selected()];
      if (s) this.go(s);
    } else {
      return;
    }
    e.preventDefault();
  }

  protected go(s: acp.SessionInfo) {
    this.close();
    this.router.navigate(['/session', s.sessionId]);
  }
}
