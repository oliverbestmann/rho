import { Component } from '@angular/core';

/** Like a chat typing indicator, where the next output appears; the content replaces "Working…". */
@Component({
  selector: 'app-working',
  host: { role: 'status' },
  template: `<span class="dots" aria-hidden="true"><i></i><i></i><i></i></span>
    <ng-content>Working…</ng-content>`,
  styles: `
    :host {
      display: flex;
      align-items: center;
      gap: 8px;
      padding-left: 16px;
      color: var(--mat-sys-on-surface-variant);
      font: var(--mat-sys-body-medium);
    }
    .dots {
      display: flex;
      gap: 4px;
    }
    i {
      width: 6px;
      height: 6px;
      border-radius: 50%;
      background: currentColor;
      animation: pulse 1.2s infinite ease-in-out;
    }
    i:nth-child(2) {
      animation-delay: 0.2s;
    }
    i:nth-child(3) {
      animation-delay: 0.4s;
    }
    @keyframes pulse {
      0%,
      80%,
      100% {
        opacity: 0.25;
      }
      40% {
        opacity: 1;
      }
    }
    @media (prefers-reduced-motion: reduce) {
      i {
        animation: none;
        opacity: 0.6;
      }
    }
  `,
})
export class Working {}
