import { Component, computed, input, model, output } from '@angular/core';
import type * as acp from '@agentclientprotocol/sdk';
import { TextFieldModule } from '@angular/cdk/text-field';
import {
  MatAutocompleteModule,
  MatAutocompleteSelectedEvent,
} from '@angular/material/autocomplete';
import { MatButtonModule } from '@angular/material/button';
import { MatFormFieldModule } from '@angular/material/form-field';
import { MatIconModule } from '@angular/material/icon';
import { MatInputModule } from '@angular/material/input';

/** The message field with slash command suggestions, send and stop. */
@Component({
  selector: 'app-composer',
  imports: [
    TextFieldModule,
    MatAutocompleteModule,
    MatButtonModule,
    MatFormFieldModule,
    MatIconModule,
    MatInputModule,
  ],
  template: `
    <button
      matIconButton
      aria-label="Session settings"
      aria-controls="session-options"
      [attr.aria-expanded]="optionsOpen()"
      (click)="optionsOpen.update((v) => !v)"
    >
      <mat-icon>{{ optionsOpen() ? 'expand_more' : 'tune' }}</mat-icon>
    </button>
    <mat-form-field subscriptSizing="dynamic" class="field">
      <textarea
        matInput
        cdkTextareaAutosize
        cdkAutosizeMaxRows="8"
        placeholder="Message"
        aria-label="Message"
        [matAutocomplete]="slash"
        [value]="draft()"
        (input)="draft.set($any($event.target).value)"
        (keydown)="onKey($event)"
      ></textarea>
      <mat-autocomplete #slash autoActiveFirstOption (optionSelected)="pick($event)">
        @for (c of suggestions(); track c.name) {
          <mat-option [value]="c">
            <span class="slash-name">/{{ c.name }}</span>
            <span class="slash-description">{{
              c.description.length > 120 ? c.description.slice(0, 120) + '…' : c.description
            }}</span>
          </mat-option>
        }
      </mat-autocomplete>
    </mat-form-field>
    @if (running()) {
      <button matIconButton aria-label="Stop" (click)="cancel.emit()">
        <mat-icon>stop</mat-icon>
      </button>
    }
    <button
      matIconButton
      [attr.aria-label]="running() ? 'Queue' : 'Send'"
      [disabled]="!draft().trim() || !ready()"
      (click)="send.emit()"
    >
      <mat-icon>{{ running() ? 'schedule_send' : 'send' }}</mat-icon>
    </button>
  `,
  styles: `
    :host {
      display: flex;
      align-items: center;
      gap: 4px;
    }
    .field {
      flex: 1;
    }
    .slash-name {
      font: var(--mat-sys-body-small);
      font-weight: bold;
      margin-right: 12px;
    }
    .slash-description {
      color: var(--mat-sys-on-surface-variant);
      font: var(--mat-sys-body-small);
    }
  `,
})
export class Composer {
  readonly draft = model.required<string>();
  readonly commands = input<acp.AvailableCommand[]>([]);
  /** A turn runs: sending queues, and stop shows. */
  readonly running = input(false);
  /** The session takes prompts. */
  readonly ready = input(false);
  /** The session options, toggled by the settings button. */
  readonly optionsOpen = model(false);
  readonly send = output();
  readonly cancel = output();

  /** Slash command suggestions for the first word, when it starts with `/`. */
  protected readonly suggestions = computed(() => {
    const match = /^\/(\S*)$/.exec(this.draft());
    if (!match) return [];
    const prefix = match[1].toLowerCase();
    return this.commands()
      .filter((c) => c.name.toLowerCase().startsWith(prefix))
      .slice(0, 8);
  });

  protected onKey(e: KeyboardEvent) {
    if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
      e.preventDefault();
      this.send.emit();
    }
  }

  protected pick(e: MatAutocompleteSelectedEvent) {
    const name = (e.option.value as acp.AvailableCommand).name;
    // A command without input gets a trailing space; one with input a space to type into.
    this.draft.set(`/${name} `);
  }
}
