import { Component, inject } from '@angular/core';
import { RouterOutlet } from '@angular/router';
import { MatIconRegistry } from '@angular/material/icon';
import { Acp } from './acp';
import { Settings } from './settings';

@Component({
  selector: 'app-root',
  imports: [RouterOutlet],
  template: `
    @if (settings.server() && acp.status() !== 'online') {
      <div class="offline" role="status">
        {{ acp.status() === 'connecting' ? 'Connecting…' : 'Offline, retrying…' }}
      </div>
    }
    <router-outlet />
  `,
  styles: `
    .offline {
      position: sticky;
      top: 0;
      z-index: 3;
      padding: 4px 16px;
      text-align: center;
      font: var(--mat-sys-label-medium);
      background: var(--mat-sys-error-container);
      color: var(--mat-sys-on-error-container);
    }
  `,
})
export class App {
  protected readonly acp = inject(Acp);
  protected readonly settings = inject(Settings);

  constructor() {
    inject(MatIconRegistry).setDefaultFontSetClass('material-symbols-outlined');
  }
}
