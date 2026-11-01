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
import { MatMenuModule } from '@angular/material/menu';

/** An image to send, as in an ACP image block. */
export interface Image {
  mimeType: string;
  data: string;
}

/** A file as base64; rho scales it down. */
export function readImage(file: File): Promise<Image> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => {
      const url = reader.result as string;
      resolve({ mimeType: file.type, data: url.slice(url.indexOf(',') + 1) });
    };
    reader.onerror = () => reject(reader.error);
    reader.readAsDataURL(file);
  });
}

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
    MatMenuModule,
  ],
  template: `
    @if (images().length) {
      <ul class="images" aria-label="Attached images">
        @for (img of images(); track $index) {
          <li>
            <img [src]="'data:' + img.mimeType + ';base64,' + img.data" alt="Attached image" />
            <button matIconButton aria-label="Remove image" (click)="removeImage($index)">
              <mat-icon>close</mat-icon>
            </button>
          </li>
        }
      </ul>
    }
    <button matIconButton class="narrow" aria-label="More actions" [matMenuTriggerFor]="more">
      <mat-icon>more_vert</mat-icon>
    </button>
    <mat-menu #more>
      <button mat-menu-item (click)="optionsOpen.update((v) => !v)">
        <mat-icon>tune</mat-icon>
        <span>{{ optionsOpen() ? 'Hide settings' : 'Session settings' }}</span>
      </button>
      <button mat-menu-item (click)="file.click()">
        <mat-icon>image</mat-icon>
        <span>Attach image</span>
      </button>
      @if (running()) {
        <button mat-menu-item (click)="cancel.emit()">
          <mat-icon>stop</mat-icon>
          <span>Stop</span>
        </button>
      }
    </mat-menu>
    <button
      matIconButton
      class="wide"
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
        (paste)="onPaste($event)"
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
    <input
      #file
      type="file"
      accept="image/*"
      multiple
      hidden
      (change)="attach($any($event.target).files); file.value = ''"
    />
    <button matIconButton class="wide" aria-label="Attach image" (click)="file.click()">
      <mat-icon>image</mat-icon>
    </button>
    @if (running()) {
      <button matIconButton class="wide" aria-label="Stop" (click)="cancel.emit()">
        <mat-icon>stop</mat-icon>
      </button>
    }
    <button
      matIconButton
      [attr.aria-label]="running() ? 'Queue' : 'Send'"
      [disabled]="!canSend()"
      (click)="send.emit()"
    >
      <mat-icon>{{ running() ? 'schedule_send' : 'send' }}</mat-icon>
    </button>
  `,
  styles: `
    :host {
      display: flex;
      flex-wrap: wrap;
      align-items: center;
      gap: 4px;
    }
    .images {
      flex-basis: 100%;
      display: flex;
      flex-wrap: wrap;
      gap: 8px;
      margin: 0;
      padding: 0 0 4px;
      list-style: none;
    }
    .images li {
      position: relative;
    }
    .images img {
      display: block;
      height: 64px;
      max-width: 128px;
      object-fit: cover;
      border-radius: var(--mat-sys-corner-small);
    }
    .images button {
      position: absolute;
      top: -8px;
      right: -8px;
      transform: scale(0.75);
      background: var(--mat-sys-surface-container-high);
    }
    .field {
      flex: 1;
    }
    .narrow {
      display: none;
    }
    @media (max-width: 599px) {
      .narrow {
        display: inline-flex;
      }
      .wide {
        display: none;
      }
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
  /** Images sent with the draft. */
  readonly images = model<Image[]>([]);
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

  /** rho queues text only, so images wait until the turn ended. */
  protected readonly canSend = computed(
    () => this.ready() && (this.images().length ? !this.running() : !!this.draft().trim()),
  );

  protected onPaste(e: ClipboardEvent) {
    const files = [...(e.clipboardData?.files ?? [])].filter((f) => f.type.startsWith('image/'));
    if (!files.length) return;
    e.preventDefault();
    void this.attach(files);
  }

  protected async attach(files: Iterable<File>) {
    const images = await Promise.all(
      [...files].filter((f) => f.type.startsWith('image/')).map(readImage),
    );
    this.images.update((i) => [...i, ...images]);
  }

  protected removeImage(index: number) {
    this.images.update((i) => i.filter((_, j) => j !== index));
  }

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
