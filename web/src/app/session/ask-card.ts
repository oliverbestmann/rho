import { Component, computed, inject, input, resource, signal } from '@angular/core';
import { RouterLink } from '@angular/router';
import type * as acp from '@agentclientprotocol/sdk';
import { TextFieldModule } from '@angular/cdk/text-field';
import { MatButtonModule } from '@angular/material/button';
import { MatCardModule } from '@angular/material/card';
import { MatCheckboxModule } from '@angular/material/checkbox';
import { MatFormFieldModule } from '@angular/material/form-field';
import { MatInputModule } from '@angular/material/input';
import { MatRadioModule } from '@angular/material/radio';
import { Acp, Ask } from '../acp';
import { MarkdownPipe } from '../markdown';
import { drafts, written } from '../review/drafts';
import { diffLink, Diffs, totals } from '../review/diffs';
import { PlanComments, planBlocks, planMessage } from './plan-comments';
import { blockText, rhoTool } from './session-state';
import { toolBody } from './tool-body';

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
    PlanComments,
    RouterLink,
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
          @if (review()) {
            <div class="heading">
              <mat-card-subtitle>Review requested</mat-card-subtitle>
              <mat-card-title>{{ call()?.title }}</mat-card-title>
            </div>
          } @else {
            <mat-card-title>{{ a.request.toolCall.title }}</mat-card-title>
          }
        </mat-card-header>
        @let callId = a.request.toolCall.toolCallId;
        @if (plan(); as plan) {
          <mat-card-content class="markdown">
            <app-plan-comments [plan]="plan" [(comments)]="planComments" />
          </mat-card-content>
        } @else if (review()) {
          <mat-card-content class="review">
            @if (diff.value(); as files) {
              @let t = totals(files);
              <div class="summary">
                {{ files.length }} {{ files.length === 1 ? 'file' : 'files' }} ·
                <span class="adds">+{{ t.adds }}</span>
                <span class="dels">−{{ t.dels }}</span>
              </div>
              @for (f of files.slice(0, 5); track $index) {
                <div class="file">
                  <span class="path">{{ f.path }}</span>
                  <span class="adds">+{{ f.adds }}</span>
                  <span class="dels">−{{ f.dels }}</span>
                </div>
              }
              @if (files.length > 5) {
                <div class="more">and {{ files.length - 5 }} more</div>
              }
            } @else if (diff.error()) {
              <div class="more">Diff not available</div>
            }
          </mat-card-content>
        } @else if (details(); as details) {
          <mat-card-content class="markdown" [innerHTML]="details | markdown" />
        }
        <mat-card-actions align="end">
          @let commented = plan() ? planCount() : 0;
          @if (review()) {
            <a
              class="open"
              matButton="filled"
              [routerLink]="['review', callId]"
              queryParamsHandling="preserve"
            >
              Open review
            </a>
          }
          @for (o of a.request.options; track o.optionId) {
            @if (commented && o.kind.startsWith('reject')) {
              <button matButton="outlined" (click)="sendPlanComments()">
                Request changes ({{ commented }})
              </button>
            } @else {
              <button
                [matButton]="o.kind.startsWith('allow') && !review() ? 'filled' : 'outlined'"
                (click)="a.answer({ outcome: { outcome: 'selected', optionId: o.optionId } })"
              >
                {{ o.name }}
              </button>
            }
          }
        </mat-card-actions>
      } @else {
        <mat-card-content class="markdown" [innerHTML]="a.request.message | markdown" />
        <mat-card-content class="fields">
          @for (f of fields(); track f.key) {
            @if (f.type === 'boolean') {
              <mat-checkbox [checked]="!!values()[f.key]" (change)="set(f.key, $event.checked)">
                {{ f.title }}
              </mat-checkbox>
            } @else if (f.type === 'choice' || f.type === 'multi') {
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
                @if (f.type === 'choice') {
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
                } @else {
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
                }
              </div>
            } @else {
              <mat-form-field subscriptSizing="dynamic">
                <mat-label>{{ f.title }}</mat-label>
                @if (f.type === 'number') {
                  <input
                    matInput
                    type="number"
                    [required]="f.required"
                    [value]="values()[f.key] ?? ''"
                    (input)="set(f.key, $any($event.target).value)"
                  />
                } @else {
                  <textarea
                    matInput
                    cdkTextareaAutosize
                    [required]="f.required"
                    [value]="values()[f.key] ?? ''"
                    (input)="set(f.key, $any($event.target).value)"
                  ></textarea>
                }
                @if (f.description) {
                  <mat-hint>{{ f.description }}</mat-hint>
                }
              </mat-form-field>
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
    .review {
      display: flex;
      flex-direction: column;
      gap: 2px;
      padding-top: 8px;
      font: var(--mat-sys-body-medium);
    }
    .heading {
      display: flex;
      flex-direction: column;
      gap: 2px;
      min-width: 0;
    }
    .summary {
      font: var(--mat-sys-title-small);
      margin-bottom: 4px;
    }
    .summary .adds {
      margin-right: 6px;
    }
    .file {
      display: flex;
      gap: 8px;
    }
    .path {
      flex: 1;
      min-width: 0;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
      font-family: Iosevka, ui-monospace, monospace;
    }
    .adds {
      color: light-dark(#1a7f37, #3fb950);
    }
    .dels {
      color: var(--mat-sys-error);
    }
    .more {
      color: var(--mat-sys-on-surface-variant);
    }
    /* Main action first; the answers follow on the right. */
    .open {
      margin-right: auto;
    }
  `,
})
export class AskCard {
  readonly ask = input.required<Ask>();
  /** The tool call the permission request is about, if it is in the transcript. */
  readonly call = input<acp.ToolCall>();
  protected readonly values = signal<Record<string, Value>>({});
  private readonly acp = inject(Acp);
  /** Comments on the plan's blocks, by block index. */
  protected readonly planComments = drafts(
    () => `rho.plan-comments.${this.ask().sessionId}.${this.askedId()}`,
  );
  protected readonly planCount = computed(() => written(this.planComments()).length);
  private readonly askedId = computed(() => {
    const a = this.ask();
    return a.kind === 'permission' ? a.request.toolCall.toolCallId : '';
  });

  /** The plan of a `PresentPlan` call, which gets comments per block. */
  protected readonly plan = computed(() => {
    const call = this.call();
    const plan = (call?.rawInput as { plan?: unknown } | undefined)?.plan;
    return call && rhoTool(call) === 'PresentPlan' && typeof plan === 'string' ? plan : '';
  });

  /** A `PresentDiff` call: the diff is reviewed on its own page. */
  protected readonly review = computed(() => {
    const call = this.call();
    return !!call && rhoTool(call) === 'PresentDiff';
  });

  private readonly diffs = inject(Diffs);
  /** The review's files, for the summary. */
  protected readonly diff = resource({
    params: () => (this.review() ? diffLink(this.call()) : undefined),
    loader: ({ params: link }) => this.diffs.get(link),
  });
  protected readonly totals = totals;

  protected sendPlanComments() {
    const a = this.ask();
    if (a.kind !== 'permission') return;
    const text = planMessage(planBlocks(this.plan()), this.planComments());
    this.planComments.set({});
    void this.acp.rejectWith(a, text);
  }

  /** The permission request's tool call content, else that of the call it is about. */
  protected readonly details = computed(() => {
    const a = this.ask();
    if (a.kind !== 'permission') return '';
    const own = (a.request.toolCall.content ?? [])
      .map((c) => (c.type === 'content' ? blockText(c.content) : ''))
      .join('\n\n');
    const call = this.call();
    return own || (call ? toolBody(call) : '');
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
      if (f.type !== 'number') content[f.key] = v;
      // An empty or partial number input has the value ''.
      else if (v !== '') content[f.key] = Number(v);
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
