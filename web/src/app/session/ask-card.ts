import { Component, computed, input, signal } from '@angular/core';
import type * as acp from '@agentclientprotocol/sdk';
import { TextFieldModule } from '@angular/cdk/text-field';
import { MatButtonModule } from '@angular/material/button';
import { MatCardModule } from '@angular/material/card';
import { MatCheckboxModule } from '@angular/material/checkbox';
import { MatFormFieldModule } from '@angular/material/form-field';
import { MatInputModule } from '@angular/material/input';
import { MatRadioModule } from '@angular/material/radio';
import { Ask } from '../acp';
import { MarkdownPipe } from '../markdown';
import { blockText } from './session-state';

type Value = string | string[] | boolean;

/** One form field from an elicitation schema property. */
interface Field {
  key: string;
  title: string;
  description?: string;
  required: boolean;
  type: 'text' | 'number' | 'choice' | 'multi' | 'boolean';
  options: { value: string; title: string; description?: string }[];
}

/** A permission request or form from the agent. */
@Component({
  selector: 'app-ask-card',
  imports: [
    MarkdownPipe,
    TextFieldModule,
    MatButtonModule,
    MatCardModule,
    MatCheckboxModule,
    MatFormFieldModule,
    MatInputModule,
    MatRadioModule,
  ],
  template: `
    @let a = ask();
    <mat-card appearance="outlined">
      @if (a.kind === 'permission') {
        <mat-card-header>
          <mat-card-title>{{ a.request.toolCall.title }}</mat-card-title>
        </mat-card-header>
        @if (details(); as details) {
          <mat-card-content class="markdown" [innerHTML]="details | markdown" />
        }
        <mat-card-actions align="end">
          @for (o of a.request.options; track o.optionId) {
            <button
              [matButton]="o.kind.startsWith('allow') ? 'filled' : 'outlined'"
              (click)="a.answer({ outcome: { outcome: 'selected', optionId: o.optionId } })"
            >
              {{ o.name }}
            </button>
          }
        </mat-card-actions>
      } @else {
        <mat-card-content class="markdown" [innerHTML]="a.request.message | markdown" />
        <mat-card-content class="fields">
          @for (f of fields(); track f.key) {
            @switch (f.type) {
              @case ('boolean') {
                <mat-checkbox [checked]="!!values()[f.key]" (change)="set(f.key, $event.checked)">
                  {{ f.title }}
                </mat-checkbox>
              }
              @case ('choice') {
                <div class="choice">
                  <div class="question">
                    {{ f.title }}
                    @if (f.required) {
                      *
                    }
                  </div>
                  @if (f.description) {
                    <div class="hint">{{ f.description }}</div>
                  }
                  <mat-radio-group [value]="values()[f.key]" (change)="set(f.key, $event.value)">
                    @for (o of f.options; track o.value) {
                      <mat-radio-button [value]="o.value">
                        <div>{{ o.title }}</div>
                        @if (o.description) {
                          <div class="hint">{{ o.description }}</div>
                        }
                      </mat-radio-button>
                    }
                  </mat-radio-group>
                </div>
              }
              @case ('multi') {
                <div class="choice">
                  <div class="question">
                    {{ f.title }}
                    @if (f.required) {
                      *
                    }
                  </div>
                  @if (f.description) {
                    <div class="hint">{{ f.description }}</div>
                  }
                  @for (o of f.options; track o.value) {
                    <mat-checkbox
                      [checked]="picked(f.key, o.value)"
                      (change)="toggle(f.key, o.value, $event.checked)"
                    >
                      <div>{{ o.title }}</div>
                      @if (o.description) {
                        <div class="hint">{{ o.description }}</div>
                      }
                    </mat-checkbox>
                  }
                </div>
              }
              @default {
                <mat-form-field subscriptSizing="dynamic">
                  <mat-label>{{ f.title }}</mat-label>
                  <textarea
                    matInput
                    cdkTextareaAutosize
                    [required]="f.required"
                    [value]="values()[f.key] ?? ''"
                    (input)="set(f.key, $any($event.target).value)"
                  ></textarea>
                  @if (f.description) {
                    <mat-hint>{{ f.description }}</mat-hint>
                  }
                </mat-form-field>
              }
            }
          }
        </mat-card-content>
        <mat-card-actions align="end">
          <button matButton="outlined" (click)="a.answer({ action: 'decline' })">Decline</button>
          <button matButton="filled" [disabled]="!valid()" (click)="submit()">Submit</button>
        </mat-card-actions>
      }
    </mat-card>
  `,
  styles: `
    mat-card {
      border-color: var(--mat-sys-tertiary);
    }
    mat-card-actions {
      flex-wrap: wrap;
      gap: 8px;
    }
    .fields {
      display: flex;
      flex-direction: column;
      gap: 16px;
      padding-top: 16px;
    }
    .choice,
    mat-radio-group {
      display: flex;
      flex-direction: column;
      gap: 4px;
    }
    .question {
      font: var(--mat-sys-title-small);
    }
    .hint {
      color: var(--mat-sys-on-surface-variant);
      font: var(--mat-sys-body-small);
    }
  `,
})
export class AskCard {
  readonly ask = input.required<Ask>();
  protected readonly values = signal<Record<string, Value>>({});

  /** The permission request's tool call content. */
  protected readonly details = computed(() => {
    const a = this.ask();
    if (a.kind !== 'permission') return '';
    return (a.request.toolCall.content ?? [])
      .map((c) => (c.type === 'content' ? blockText(c.content) : ''))
      .join('\n\n');
  });

  protected readonly fields = computed<Field[]>(() => {
    const a = this.ask();
    if (a.kind !== 'form' || a.request.mode !== 'form') return [];
    const schema = (a.request as acp.ElicitationFormMode).requestedSchema;
    const required = new Set(schema.required ?? []);
    return Object.entries(schema.properties ?? {}).map(([key, p]) =>
      toField(key, p, required.has(key)),
    );
  });

  protected readonly valid = computed(() =>
    this.fields().every((f) => {
      const v = this.values()[f.key];
      return (
        !f.required ||
        (Array.isArray(v) ? v.length > 0 : typeof v === 'string' ? v.trim() !== '' : v != null)
      );
    }),
  );

  protected set(key: string, value: Value) {
    this.values.update((v) => ({ ...v, [key]: value }));
  }

  protected picked(key: string, value: string) {
    const v = this.values()[key];
    return Array.isArray(v) && v.includes(value);
  }

  protected toggle(key: string, value: string, on: boolean) {
    const v = this.values()[key];
    const list = (Array.isArray(v) ? v : []).filter((x) => x !== value);
    this.set(key, on ? [...list, value] : list);
  }

  protected submit() {
    const a = this.ask();
    if (a.kind !== 'form') return;
    const content: Record<string, acp.ElicitationContentValue> = {};
    for (const f of this.fields()) {
      const v = this.values()[f.key];
      if (v == null) continue;
      content[f.key] = f.type === 'number' && typeof v === 'string' ? Number(v) : v;
    }
    a.answer({ action: 'accept', content });
  }
}

function toField(key: string, p: acp.ElicitationPropertySchema, required: boolean): Field {
  const s = p as Record<string, unknown>;
  const field: Field = {
    key,
    title: (s['title'] as string) ?? key,
    description: (s['description'] as string) ?? undefined,
    required,
    type: 'text',
    options: [],
  };
  const titled = (list: unknown) =>
    ((list as acp.EnumOption[] | undefined) ?? []).map((o) => ({
      value: o.const,
      title: o.title,
      description: o.description ?? undefined,
    }));
  const plain = (list: unknown) =>
    ((list as string[] | undefined) ?? []).map((v) => ({ value: v, title: v }));
  switch (p.type) {
    case 'boolean':
      return { ...field, type: 'boolean' };
    case 'number':
    case 'integer':
      return { ...field, type: 'number' };
    case 'array': {
      const items = (s['items'] ?? {}) as Record<string, unknown>;
      return {
        ...field,
        type: 'multi',
        options: items['anyOf'] ? titled(items['anyOf']) : plain(items['enum']),
      };
    }
    case 'string':
      if (s['oneOf']) return { ...field, type: 'choice', options: titled(s['oneOf']) };
      if (s['enum']) return { ...field, type: 'choice', options: plain(s['enum']) };
      return field;
    default:
      return field;
  }
}
