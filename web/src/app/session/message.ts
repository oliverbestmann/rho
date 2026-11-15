import { ClipboardModule } from '@angular/cdk/clipboard';
import { Component, input, signal } from '@angular/core';
import { MatIconModule } from '@angular/material/icon';
import { HighlightCode } from '../highlight';
import { MarkdownPipe } from '../markdown';

/** A chat bubble, right for the user and left for the agent; a note is plain text. */
@Component({
  selector: 'app-message',
  imports: [ClipboardModule, HighlightCode, MarkdownPipe, MatIconModule],
  host: { '[class]': 'from()' },
  template: `
    @let html = text() | markdown;
    @if (from() === 'agent') {
      <button
        class="copy"
        type="button"
        title="Copy"
        aria-label="Copy response"
        [cdkCopyToClipboard]="text()"
        (cdkCopyToClipboardCopied)="copied($event)"
      >
        <mat-icon>{{ done() ? 'check' : 'content_copy' }}</mat-icon>
      </button>
    }
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
      position: relative;
      padding-right: 36px;
      align-self: flex-start;
      max-width: 100%;
      background: var(--mat-sys-tertiary-container);
      color: var(--mat-sys-on-tertiary-container);
    }
    .copy {
      position: absolute;
      top: 4px;
      right: 4px;
      display: flex;
      padding: 4px;
      border: none;
      border-radius: var(--mat-sys-corner-small);
      background: none;
      color: inherit;
      opacity: 0.5;
      cursor: pointer;
    }
    .copy:hover {
      opacity: 1;
    }
    .copy mat-icon {
      font-size: 18px;
      width: 18px;
      height: 18px;
    }
    /* A notice from rho, e.g. a slash command's reply: not a chat bubble. */
    :host(.note) {
      align-self: flex-start;
      max-width: 100%;
      font-style: italic;
      background: var(--mat-sys-surface-container);
      color: var(--mat-sys-on-surface-variant);
    }
  `,
})
export class ChatMessage {
  readonly from = input.required<'user' | 'agent' | 'note'>();
  readonly text = input.required<string>();
  protected readonly done = signal(false);

  protected copied(ok: boolean) {
    if (!ok) return;
    this.done.set(true);
    setTimeout(() => this.done.set(false), 1500);
  }
}
