import { Component, computed, input, model, signal } from '@angular/core';
import { TextFieldModule } from '@angular/cdk/text-field';
import { MatButtonModule } from '@angular/material/button';
import { lexer } from 'marked';
import { MarkdownPipe } from '../markdown';
import { Comments, written } from '../review/drafts';

/** A top-level block of the plan, or one item of a top-level list. */
export interface Block {
  raw: string;
  /** A list item: `first` and `last` of its list keep the list's spacing. */
  item?: { first: boolean; last: boolean };
}

/**
 * A plan as markdown, block by block: tapping a block highlights it and
 * opens a comment box under it (ADR 0076).
 */
@Component({
  selector: 'app-plan-comments',
  imports: [MarkdownPipe, TextFieldModule, MatButtonModule],
  template: `
    @for (b of blocks(); track $index; let i = $index) {
      <div
        class="block"
        [class.item]="b.item"
        [class.first]="b.item?.first"
        [class.last]="b.item?.last"
        [class.selected]="selected() === i"
        [class.commented]="!!comments()[i]?.trim()"
        [innerHTML]="b.raw | markdown"
        (click)="select(i, $event)"
      ></div>
      @if (selected() === i) {
        <div class="thread">
          <textarea
            class="comment-input"
            placeholder="Comment"
            aria-label="Comment"
            cdkTextareaAutosize
            cdkAutosizeMinRows="2"
            [value]="comments()[i] ?? ''"
            (input)="set(i, $any($event.target).value)"
          ></textarea>
          <div class="comment-actions">
            <button matButton (click)="set(i, ''); selected.set(undefined)">Delete</button>
            <button matButton="tonal" (click)="selected.set(undefined)">Done</button>
          </div>
        </div>
      } @else if (comments()[i]?.trim()) {
        <div class="thread">
          <div class="comment-text" (click)="selected.set(i)">{{ comments()[i] }}</div>
        </div>
      }
    }
  `,
  styles: `
    :host {
      display: block;
    }
    .block {
      cursor: pointer;
      border-radius: var(--mat-sys-corner-small);
      transition: background 0.15s;
    }
    .block.commented {
      background: color-mix(in srgb, var(--mat-sys-tertiary) 10%, transparent);
    }
    .block.selected {
      background: color-mix(in srgb, var(--mat-sys-tertiary) 22%, transparent);
    }
    .item > :is(ul, ol) {
      margin-block: 0;
    }
    .item.first > :is(ul, ol) {
      margin-top: 1em;
    }
    .item.last > :is(ul, ol) {
      margin-bottom: 1em;
    }
    .thread {
      margin: 4px 0 12px;
      padding: 8px 12px;
      border-radius: var(--mat-sys-corner-small);
      background: var(--mat-sys-surface-container);
    }
  `,
})
export class PlanComments {
  readonly plan = input.required<string>();
  /** By block index. */
  readonly comments = model.required<Comments>();

  protected readonly blocks = computed(() => planBlocks(this.plan()));
  protected readonly selected = signal<number | undefined>(undefined);

  protected select(i: number, event: MouseEvent) {
    // Links still open.
    if ((event.target as HTMLElement).closest('a')) return;
    this.selected.update((s) => (s === i ? undefined : i));
  }

  protected set(i: number, text: string) {
    this.comments.update((c) => ({ ...c, [i]: text }));
  }
}

export function planBlocks(plan: string): Block[] {
  const out: Block[] = [];
  for (const t of lexer(plan)) {
    if (t.type === 'space') continue;
    if (t.type !== 'list') {
      out.push({ raw: t.raw.trimEnd() });
      continue;
    }
    const items = (t as { items: { raw: string }[] }).items;
    items.forEach((item, i) =>
      out.push({
        // Its own number, so an ordered list still counts on: marked sets `start`.
        raw: item.raw.trimEnd(),
        item: { first: i === 0, last: i === items.length - 1 },
      }),
    );
  }
  return out;
}

export function planMessage(
  blocks: Block[],
  comments: Comments,
  heading = 'Comments on the plan:',
) {
  const parts = written(comments)
    .sort(([a], [b]) => +a - +b)
    .map(([i, text]) => `${quote(blocks[+i]?.raw ?? '')}\n\n${text.trim()}`);
  return `${heading}\n\n${parts.join('\n\n')}`;
}

const quote = (text: string) =>
  text
    .split('\n')
    .map((l) => `> ${l}`)
    .join('\n');
