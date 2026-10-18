import { Component, computed, inject, resource, signal, viewChild } from '@angular/core';
import { Router, RouterLink } from '@angular/router';
import * as acp from '@agentclientprotocol/sdk';
import { MatAutocompleteModule, MatAutocompleteTrigger } from '@angular/material/autocomplete';
import { MatButtonModule } from '@angular/material/button';
import { MatFormFieldModule } from '@angular/material/form-field';
import { MatIconModule } from '@angular/material/icon';
import { MatInputModule } from '@angular/material/input';
import { MatToolbarModule } from '@angular/material/toolbar';
import { Acp, errorText } from '../acp';

/** `_rho/dirs`: the matching subdirectories of the typed level. */
interface Dirs {
  entries: string[];
  /** Canonical, if the typed path is a directory a session may start in. */
  dir: string | null;
}

const LAST_DIR = 'rho.lastDir';

/** Starts a session in a directory under ~ or /tmp, picked level by level. */
@Component({
  selector: 'app-new-session',
  imports: [
    RouterLink,
    MatAutocompleteModule,
    MatButtonModule,
    MatFormFieldModule,
    MatIconModule,
    MatInputModule,
    MatToolbarModule,
  ],
  template: `
    <mat-toolbar>
      <a matIconButton routerLink="/" aria-label="Back"><mat-icon>arrow_back</mat-icon></a>
      <span class="title">New session</span>
    </mat-toolbar>
    <form (submit)="start($event)">
      <mat-form-field>
        <mat-label>Directory</mat-label>
        <input
          matInput
          autocapitalize="off"
          spellcheck="false"
          [value]="path()"
          (input)="path.set(value($event))"
          [matAutocomplete]="auto"
        />
        <mat-autocomplete #auto (optionSelected)="pick($event.option.value)">
          @for (name of dirs.value()?.entries; track name) {
            <mat-option [value]="level() + name + '/'">{{ name }}</mat-option>
          }
        </mat-autocomplete>
        <mat-hint>Under ~ or /tmp</mat-hint>
      </mat-form-field>
      @if (error(); as error) {
        <p class="error">{{ error }}</p>
      }
      <div class="actions">
        @if (canCreate()) {
          <button matButton type="button" [disabled]="busy()" (click)="create()">
            <mat-icon>create_new_folder</mat-icon> Create folder
          </button>
        }
        <button matButton="filled" type="submit" [disabled]="!dirs.value()?.dir || busy()">
          Start
        </button>
      </div>
    </form>
  `,
  styles: `
    mat-toolbar {
      background: var(--mat-sys-surface);
    }
    .title {
      flex: 1;
      font: var(--mat-sys-headline-small);
    }
    form {
      display: flex;
      flex-direction: column;
      gap: 8px;
      max-width: 760px;
      margin: 0 auto;
      padding: 16px;
    }
    .actions {
      display: flex;
      justify-content: flex-end;
      gap: 8px;
    }
    .error {
      color: var(--mat-sys-error);
    }
  `,
})
export class NewSession {
  private readonly acp = inject(Acp);
  private readonly router = inject(Router);
  private readonly trigger = viewChild.required(MatAutocompleteTrigger);

  protected readonly path = signal(localStorage.getItem(LAST_DIR) ?? '~/');
  protected readonly busy = signal(false);
  private readonly failed = signal<string | undefined>(undefined);

  /** The typed path up to its last `/`: where the entries are. */
  protected readonly level = computed(() => this.path().slice(0, this.path().lastIndexOf('/') + 1));

  protected readonly dirs = resource({
    params: () => ({ agent: this.acp.agent(), path: this.path() }),
    loader: async ({ params: { agent, path }, abortSignal }) => {
      if (!agent) return undefined;
      // Waits for a pause in typing; a newer key press aborts.
      await new Promise((r) => setTimeout(r, 150));
      abortSignal.throwIfAborted();
      return agent.request<Dirs>('_rho/dirs', { path });
    },
  });

  protected readonly error = computed(() => {
    const failed = this.failed();
    if (failed) return failed;
    const e = this.dirs.error();
    return e ? errorText(e) : undefined;
  });

  /** A name typed after the last `/` that is no directory yet. */
  protected readonly canCreate = computed(
    () => this.dirs.hasValue() && !this.dirs.value()?.dir && !this.path().endsWith('/'),
  );

  protected value(e: Event) {
    this.failed.set(undefined);
    return (e.target as HTMLInputElement).value;
  }

  /** Goes into the picked directory and offers its subdirectories. */
  protected pick(path: string) {
    this.path.set(path);
    setTimeout(() => this.trigger().openPanel());
  }

  protected async create() {
    const agent = this.acp.agent();
    if (!agent) return;
    await this.run(async () => {
      const { dir } = await agent.request<{ dir: string }>('_rho/mkdir', { path: this.path() });
      this.path.set(dir);
    });
  }

  protected async start(e: Event) {
    e.preventDefault();
    const agent = this.acp.agent();
    const cwd = this.dirs.value()?.dir;
    if (!agent || !cwd) return;
    await this.run(async () => {
      const { sessionId } = await agent.request(acp.methods.agent.session.new, {
        cwd,
        mcpServers: [],
      });
      localStorage.setItem(LAST_DIR, cwd);
      await this.router.navigate(['/session', sessionId], { queryParams: { cwd } });
    });
  }

  private async run(f: () => Promise<void>) {
    this.busy.set(true);
    this.failed.set(undefined);
    try {
      await f();
    } catch (e) {
      this.failed.set(errorText(e));
    } finally {
      this.busy.set(false);
    }
  }
}
