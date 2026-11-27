import { ClipboardModule } from '@angular/cdk/clipboard';
import { Component, DestroyRef, ElementRef, inject, input, signal, viewChild } from '@angular/core';
import { MatIconModule } from '@angular/material/icon';
import { HighlightCode } from '../highlight';
import { MarkdownPipe } from '../markdown';
import { Settings } from '../settings';
import { Speech } from './speech';

/** A chat bubble, right for the user and left for the agent; a note is plain text. */
@Component({
  selector: 'app-message',
  imports: [ClipboardModule, HighlightCode, MarkdownPipe, MatIconModule],
  host: { '[class]': "from() + ' ' + cls()" },
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
    <div #body class="markdown" [innerHTML]="html" [appHighlight]="html"></div>
    @if (speech(); as s) {
      <div class="player">
        @if (s.error(); as error) {
          <span class="status">Cannot read aloud: {{ error }}</span>
        } @else if (s.url(); as url) {
          <audio
            controls
            [src]="url"
            (loadedmetadata)="takeOver($any($event.target), s)"
            (play)="s.stopStream()"
          ></audio>
        } @else if (!s.received()) {
          <span class="status">Preparing audio…</span>
        } @else {
          <span class="status">Streaming, {{ s.received().toFixed(0) }} s so far…</span>
        }
      </div>
    }
    <!-- Hidden while the player shows; after an error it tries again. -->
    @if (from() === 'agent' && (!speech() || speech()?.error())) {
      <button
        class="speak"
        type="button"
        title="Read aloud"
        aria-label="Read response aloud"
        (click)="speak()"
      >
        <mat-icon>volume_up</mat-icon>
      </button>
    }
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
      /* Room for the copy and speaker buttons. */
      min-height: 64px;
      align-self: flex-start;
      max-width: 100%;
      background: var(--mat-sys-tertiary-container);
      color: var(--mat-sys-on-tertiary-container);
    }
    .copy,
    .speak {
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
    .speak {
      top: auto;
      bottom: 4px;
    }
    .copy:hover,
    .speak:hover {
      opacity: 1;
    }
    .copy mat-icon,
    .speak mat-icon {
      font-size: 18px;
      width: 18px;
      height: 18px;
    }
    /* Ends 4px above the bubble's edge, like the speaker button. */
    .player {
      margin: 4px 0 2px;
    }
    .player audio {
      display: block;
      width: 100%;
      margin-bottom: 6px;
    }
    /* As tall as the speaker button, so the text sits where it was. */
    .status {
      display: flex;
      align-items: center;
      min-height: 26px;
      font-size: 0.8em;
      opacity: 0.7;
    }
    /* A notice from rho, e.g. a slash command's reply: not a chat bubble. */
    :host(.note) {
      align-self: flex-start;
      max-width: 100%;
      font-style: italic;
      background: var(--mat-sys-surface-container);
      color: var(--mat-sys-on-surface-variant);
    }
    /* A suggestion from rho, e.g. which project to join. */
    :host(.note.hint) {
      font-style: normal;
      background: var(--mat-sys-secondary-container);
      color: var(--mat-sys-on-secondary-container);
    }
  `,
})
export class ChatMessage {
  readonly from = input.required<'user' | 'agent' | 'note'>();
  /** Extra classes, e.g. `hint` for a note. */
  readonly cls = input('');
  readonly text = input.required<string>();
  protected readonly done = signal(false);
  protected readonly speech = signal<Speech | undefined>(undefined);
  private readonly settings = inject(Settings);
  private readonly body = viewChild.required<ElementRef<HTMLElement>>('body');

  constructor() {
    inject(DestroyRef).onDestroy(() => this.speech()?.close());
  }

  protected speak() {
    const server = this.settings.server();
    this.speech()?.close();
    if (!server) return;
    // The rendered text, with code blocks fenced: rho has them described.
    const text = this.body().nativeElement.cloneNode(true) as HTMLElement;
    text
      .querySelectorAll('pre')
      .forEach((pre) => pre.replaceWith(`\n\`\`\`\n${pre.textContent}\n\`\`\`\n`));
    this.speech.set(new Speech(server, text.textContent?.trim() ?? ''));
  }

  /** The whole audio is there: the player goes on where the stream is. */
  protected takeOver(audio: HTMLAudioElement, speech: Speech) {
    const at = speech.position();
    // Played to the end already; starting would begin again from the top.
    if (at >= audio.duration - 0.1) return speech.stopStream();
    audio.currentTime = at;
    audio.play().catch(() => {});
  }

  protected copied(ok: boolean) {
    if (!ok) return;
    this.done.set(true);
    setTimeout(() => this.done.set(false), 1500);
  }
}
