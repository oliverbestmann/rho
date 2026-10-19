import { Component, input } from '@angular/core';
import { HighlightCode } from '../highlight';
import { MarkdownPipe } from '../markdown';

/** A chat bubble, right for the user and left for the agent. */
@Component({
  selector: 'app-message',
  imports: [HighlightCode, MarkdownPipe],
  host: { '[class]': 'from()' },
  template: `
    @let html = text() | markdown;
    <div class="markdown" [innerHTML]="html" [appHighlight]="html"></div>
  `,
  styles: `
    :host {
      display: block;
      box-sizing: border-box;
      padding: 2px 16px;
      border-radius: var(--mat-sys-corner-large);
    }
    :host(.user) {
      align-self: flex-end;
      max-width: 85%;
      background: var(--mat-sys-primary-container);
      color: var(--mat-sys-on-primary-container);
    }
    :host(.agent) {
      align-self: flex-start;
      max-width: 100%;
      background: var(--mat-sys-tertiary-container);
      color: var(--mat-sys-on-tertiary-container);
    }
  `,
})
export class ChatMessage {
  readonly from = input.required<'user' | 'agent'>();
  readonly text = input.required<string>();
}
