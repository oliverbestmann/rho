import { Component, inject, signal } from '@angular/core';
import { Router } from '@angular/router';
import { MatButtonModule } from '@angular/material/button';
import { MatCardModule } from '@angular/material/card';
import { MatFormFieldModule } from '@angular/material/form-field';
import { MatInputModule } from '@angular/material/input';
import { Acp } from '../acp';
import { Settings } from '../settings';

@Component({
  selector: 'app-login',
  imports: [MatButtonModule, MatCardModule, MatFormFieldModule, MatInputModule],
  template: `
    <mat-card appearance="outlined">
      <mat-card-header>
        <mat-card-title>Connect to rho</mat-card-title>
        <mat-card-subtitle
          >Start the daemon with <code>rho daemon --listen ADDR</code></mat-card-subtitle
        >
      </mat-card-header>
      <form (submit)="connect($event)">
        <mat-card-content>
          <mat-form-field>
            <mat-label>Daemon URL</mat-label>
            <input matInput type="url" required [value]="url()" (input)="url.set(value($event))" />
          </mat-form-field>
          <mat-form-field>
            <mat-label>Token</mat-label>
            <input
              matInput
              type="password"
              required
              autocomplete="current-password"
              [value]="token()"
              (input)="token.set(value($event))"
            />
            <mat-hint>From ~/.config/rho/acp-token</mat-hint>
          </mat-form-field>
          @if (acp.error(); as error) {
            <p class="error">{{ error }}</p>
          }
        </mat-card-content>
        <mat-card-actions align="end">
          <button matButton="filled" type="submit" [disabled]="!url() || !token()">Connect</button>
        </mat-card-actions>
      </form>
    </mat-card>
  `,
  styles: `
    :host {
      display: flex;
      justify-content: center;
      padding: 24px 16px;
    }
    mat-card {
      width: 100%;
      max-width: 480px;
    }
    mat-card-content {
      display: flex;
      flex-direction: column;
      gap: 8px;
      padding-top: 16px;
    }
    .error {
      color: var(--mat-sys-error);
    }
  `,
})
export class Login {
  protected readonly acp = inject(Acp);
  private readonly settings = inject(Settings);
  private readonly router = inject(Router);

  protected readonly url = signal(this.settings.server()?.url ?? defaultUrl());
  protected readonly token = signal(this.settings.server()?.token ?? '');

  protected value(e: Event) {
    return (e.target as HTMLInputElement).value;
  }

  protected connect(e: Event) {
    e.preventDefault();
    this.settings.save({ url: this.url().trim(), token: this.token().trim() });
    this.router.navigate(['/']);
  }
}

/** The daemon behind the dev server's proxy (proxy.conf.json), on the page's own host. */
function defaultUrl() {
  return `${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/dev/acp`;
}
