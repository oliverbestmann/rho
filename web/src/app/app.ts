import { Component, inject } from '@angular/core';
import { RouterOutlet } from '@angular/router';
import { MatIconRegistry } from '@angular/material/icon';
import { QuickSwitch } from './sessions/quick-switch';

@Component({
  selector: 'app-root',
  imports: [RouterOutlet, QuickSwitch],
  // The quick switch goes first: styles.scss makes the last child the scrolling view.
  template: `<app-quick-switch /><router-outlet />`,
})
export class App {
  constructor() {
    inject(MatIconRegistry).setDefaultFontSetClass('material-symbols-outlined');
  }
}
