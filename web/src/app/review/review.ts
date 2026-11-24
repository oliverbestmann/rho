import { Component, computed, inject, input, resource, signal } from '@angular/core';
import { ActivatedRoute, Router, RouterLink } from '@angular/router';
import { TextFieldModule } from '@angular/cdk/text-field';
import { MatButtonModule } from '@angular/material/button';
import { MatCheckboxModule } from '@angular/material/checkbox';
import { MatIconModule } from '@angular/material/icon';
import { MatProgressSpinnerModule } from '@angular/material/progress-spinner';
import { MatToolbarModule } from '@angular/material/toolbar';
import { Acp, Ask } from '../acp';
import { loadHighlighter } from '../highlight';
import { DiffFile, highlightFile } from './diff';
import { diffLink, Diffs, totals } from './diffs';
import { Comments, drafts, written } from './drafts';

/** The key of the overall comment in the drafts. */
const OVERALL = 'overall';

/**
 * The diff of a `PresentDiff` call, GitHub style: tap a line to comment on
 * it, then approve or request changes (ADR 0076).
 */
@Component({
  selector: 'app-review',
  imports: [
    RouterLink,
    TextFieldModule,
    MatButtonModule,
    MatCheckboxModule,
    MatIconModule,
    MatProgressSpinnerModule,
    MatToolbarModule,
  ],
  templateUrl: './review.html',
  styleUrl: './review.scss',
})
export class Review {
  /** Route parameters. */
  readonly id = input.required<string>();
  readonly callId = input.required<string>();
  /** Query parameter, `session/load` needs it. */
  readonly cwd = input.required<string>();

  private readonly acp = inject(Acp);
  private readonly router = inject(Router);
  private readonly route = inject(ActivatedRoute);
  private readonly diffs = inject(Diffs);

  private readonly opened = computed(() => {
    const agent = this.acp.agent();
    return agent && this.acp.session(agent, this.id(), this.cwd());
  });

  /** The share link to the diff file, from the call's content. */
  protected readonly link = computed(() => {
    const state = this.opened()?.state;
    // `call` reads a map: new entries are what changes it.
    state?.entries();
    return diffLink(state?.call(this.callId()));
  });

  protected readonly loading = computed(() => !!this.opened()?.loading());

  protected readonly diff = resource({
    params: () => this.link(),
    loader: ({ params: link }) => this.diffs.get(link),
  });

  private readonly hljs = resource({ loader: () => loadHighlighter() });

  protected readonly files = computed(() => this.diff.value() ?? []);

  /** Highlighted lines per file, made when the file first shows. */
  private readonly highlighted = new WeakMap<DiffFile, { hljs: unknown; html: string[][] }>();

  /** The file's line HTML by hunk; plain until highlight.js is loaded. */
  protected html(f: DiffFile) {
    const hljs = this.hljs.value();
    const done = this.highlighted.get(f);
    if (done && done.hljs === hljs) return done.html;
    const html = highlightFile(hljs, f);
    this.highlighted.set(f, { hljs, html });
    return html;
  }

  /** About the height of a file's lines, held while they are not rendered yet. */
  protected estimate(f: DiffFile) {
    const rows = f.hunks.reduce((n, h) => n + 1 + h.lines.length, 0);
    return rows * 19;
  }

  protected readonly totals = computed(() => totals(this.files()));

  /** The permission request on the call, while it waits. */
  protected readonly ask = computed(() =>
    this.acp
      .asks()
      .find(
        (a): a is Extract<Ask, { kind: 'permission' }> =>
          a.kind === 'permission' && a.request.toolCall.toolCallId === this.callId(),
      ),
  );

  protected readonly comments = drafts(() => `rho.review-comments.${this.id()}.${this.callId()}`);
  protected readonly count = computed(() => written(this.comments()).length);
  /** The line with the open comment box, by `key`. */
  protected readonly selected = signal<string | undefined>(undefined);
  protected readonly collapsed = signal<ReadonlySet<number>>(new Set());
  protected readonly viewed = signal<ReadonlySet<number>>(new Set());
  protected readonly OVERALL = OVERALL;

  protected key(file: number, hunk: number, line: number) {
    return `${file}:${hunk}:${line}`;
  }

  protected select(key: string) {
    this.selected.update((s) => (s === key ? undefined : key));
  }

  protected set(key: string, text: string) {
    this.comments.update((c) => ({ ...c, [key]: text }));
  }

  protected toggle(file: number) {
    this.collapsed.update((s) => flip(s, file, !s.has(file)));
  }

  /** A viewed file collapses, like on GitHub. */
  protected setViewed(file: number, on: boolean) {
    this.viewed.update((s) => flip(s, file, on));
    this.collapsed.update((s) => flip(s, file, on));
  }

  protected async approve() {
    const ask = this.ask();
    const allow = ask?.request.options.find((o) => o.kind.startsWith('allow'));
    if (!ask || !allow) return;
    // Sent first, so the model gets them with the approval.
    if (this.count()) {
      await this.acp.steer(
        this.id(),
        `I approved the changes, with comments:\n\n${this.message()}`,
      );
    }
    ask.answer({ outcome: { outcome: 'selected', optionId: allow.optionId } });
    this.done();
  }

  /** Request changes while the call waits, else send the comments as a prompt. */
  protected send() {
    const ask = this.ask();
    const text = this.message();
    if (ask) void this.acp.rejectWith(ask, text);
    else if (this.count()) void this.acp.send(this.id(), text);
    this.done();
  }

  private message() {
    return reviewMessage(this.files(), this.comments());
  }

  private done() {
    this.comments.set({});
    void this.router.navigate(['../..'], {
      relativeTo: this.route,
      queryParamsHandling: 'preserve',
    });
  }
}

function flip(set: ReadonlySet<number>, n: number, on: boolean) {
  const next = new Set(set);
  if (on) next.add(n);
  else next.delete(n);
  return next;
}

/** The prompt with the line comments in diff order, the overall comment last. */
export function reviewMessage(files: DiffFile[], comments: Comments) {
  const parts: string[] = [];
  files.forEach((f, fi) =>
    f.hunks.forEach((h, hi) =>
      h.lines.forEach((l, li) => {
        const text = comments[`${fi}:${hi}:${li}`]?.trim();
        if (!text) return;
        const where = l.new !== undefined ? `${f.path}:${l.new}` : `${f.path} (old line ${l.old})`;
        const sign = { add: '+', del: '-', ctx: ' ' }[l.kind];
        parts.push(`**${where}**\n${fence(`${sign}${l.text}`, 'diff')}\n\n${text}`);
      }),
    ),
  );
  const overall = comments[OVERALL]?.trim();
  if (overall) parts.push(overall);
  return `Review comments:\n\n${parts.join('\n\n')}`;
}

/** `code` in a fence longer than any backtick run in it. */
function fence(code: string, lang: string) {
  const longest = Math.max(0, ...(code.match(/`+/g) ?? []).map((r) => r.length));
  const f = '`'.repeat(Math.max(3, longest + 1));
  return `${f}${lang}\n${code}\n${f}`;
}
