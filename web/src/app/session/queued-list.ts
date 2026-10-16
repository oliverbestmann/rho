import { Component, input, output } from '@angular/core';
import { MatButtonModule } from '@angular/material/button';
import { MatIconModule } from '@angular/material/icon';

/** Messages waiting for the turn to end, one line each, cut off at the end. */
@Component({
  selector: 'app-queued-list',
  imports: [MatButtonModule, MatIconModule],
  template: `
    <ul aria-label="Queued messages">
      @for (text of items(); track $index) {
        <li>
          <mat-icon class="icon">schedule</mat-icon>
          <span class="text">{{ text }}</span>
          <button matIconButton aria-label="Remove queued message" (click)="remove.emit($index)">
            <mat-icon>close</mat-icon>
          </button>
        </li>
      }
    </ul>
  `,
  styles: `
    ul {
      margin: 0;
      padding: 0;
      list-style: none;
    }
    li {
      display: flex;
      align-items: center;
      gap: 8px;
      padding-left: 12px;
      color: var(--mat-sys-on-surface-variant);
      font: var(--mat-sys-body-medium);
    }
    .icon {
      flex: none;
      font-size: 20px;
      width: 20px;
      height: 20px;
    }
    .text {
      flex: 1;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
  `,
})
export class QueuedList {
  readonly items = input.required<readonly string[]>();
  /** The index of the message to drop. */
  readonly remove = output<number>();
}
