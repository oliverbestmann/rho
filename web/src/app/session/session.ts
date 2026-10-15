import {
  afterNextRender,
  afterRenderEffect,
  Component,
  computed,
  DestroyRef,
  effect,
  ElementRef,
  inject,
  input,
  linkedSignal,
  Pipe,
  PipeTransform,
  resource,
  signal,
  untracked,
  viewChild,
} from '@angular/core';
import { RouterLink } from '@angular/router';
import * as acp from '@agentclientprotocol/sdk';
import { structuredPatch } from 'diff';
import { TextFieldModule } from '@angular/cdk/text-field';
import {
  MatAutocompleteModule,
  MatAutocompleteSelectedEvent,
} from '@angular/material/autocomplete';
import { MatButtonModule } from '@angular/material/button';
import { MatExpansionModule } from '@angular/material/expansion';
import { MatFormFieldModule } from '@angular/material/form-field';
import { MatIconModule } from '@angular/material/icon';
import { MatInputModule } from '@angular/material/input';
import { MatMenuModule } from '@angular/material/menu';
import { MatProgressBarModule } from '@angular/material/progress-bar';
import { MatProgressSpinnerModule } from '@angular/material/progress-spinner';
import { MatSelectChange, MatSelectModule } from '@angular/material/select';
import { MatSnackBar } from '@angular/material/snack-bar';
import { MatToolbarModule } from '@angular/material/toolbar';
import { Acp } from '../acp';
import { MarkdownPipe } from '../markdown';
import { AskCard } from './ask-card';
import { isRunning, listSessions } from '../sessions/sessions';
import { ago } from '../time';
import { blockText, SessionState } from './session-state';

const KIND_ICONS: Record<string, string> = {
  read: 'description',
  edit: 'edit',
  delete: 'delete',
  move: 'drive_file_move',
  search: 'search',
  execute: 'terminal',
  think: 'psychology',
  fetch: 'language',
  switch_mode: 'swap_horiz',
};

/** `toolBody` as a pure pipe, so a diff is only made again when the call changes. */
@Pipe({ name: 'toolBody' })
export class ToolBodyPipe implements PipeTransform {
  transform(call: acp.ToolCall) {
    return toolBody(call);
  }
}

@Component({
  selector: 'app-session',
  imports: [
    RouterLink,
    AskCard,
    MarkdownPipe,
    ToolBodyPipe,
    TextFieldModule,
    MatAutocompleteModule,
    MatButtonModule,
    MatExpansionModule,
    MatFormFieldModule,
    MatIconModule,
    MatInputModule,
    MatMenuModule,
    MatProgressBarModule,
    MatProgressSpinnerModule,
    MatSelectModule,
    MatToolbarModule,
  ],
  templateUrl: './session.html',
  styleUrl: './session.scss',
})
export class Session {
  /** Route parameter. */
  readonly id = input.required<string>();
  /** Query parameter, `session/load` needs it. */
  readonly cwd = input.required<string>();
  /** Query parameter, the title from the list until rho sends a new one. */
  readonly listTitle = input<string | undefined>(undefined, { alias: 'title' });

  private readonly acp = inject(Acp);
  private readonly snackBar = inject(MatSnackBar);

  /** Kept in localStorage per session, so it survives a reload. */
  protected readonly draft = linkedSignal(() => localStorage.getItem(draftKey(this.id())) ?? '');
  /** Model, thinking and profile selectors, hidden behind a button. */
  protected readonly showOptions = linkedSignal({ source: this.id, computation: () => false });

  /** Slash command suggestions for the first word, when it starts with `/`. */
  protected readonly suggestions = computed(() => {
    const draft = this.draft();
    const match = /^\/(\S*)$/.exec(draft);
    if (!match) return [];
    const prefix = match[1].toLowerCase();
    return (this.session.value()?.commands() ?? [])
      .filter((c) => c.name.toLowerCase().startsWith(prefix))
      .slice(0, 8);
  });
  private stopListening = () => {};

  protected readonly session = resource({
    params: () => {
      const agent = this.acp.agent();
      return agent && { agent, id: this.id(), cwd: this.cwd() };
    },
    loader: async ({ params: { agent, id, cwd } }) => {
      const state = new SessionState();
      this.stopListening();
      this.stopListening = this.acp.listen(id, (u) => state.apply(u));
      // rho replays the history as updates before it answers.
      const res = await agent.request(acp.methods.agent.session.load, {
        sessionId: id,
        cwd,
        mcpServers: [],
      });
      state.options.set(res.configOptions ?? []);
      return state;
    },
  });

  protected readonly ago = ago;
  protected readonly isRunning = isRunning;
  /** For the switcher; reloaded each time it opens. */
  protected readonly sessions = resource({
    params: () => this.acp.agent(),
    loader: ({ params: agent }) => listSessions(agent),
  });
  /** The five last updated other sessions. */
  protected readonly recent = computed(() =>
    (this.sessions.value() ?? []).filter((s) => s.sessionId !== this.id()).slice(0, 5),
  );

  protected readonly asks = computed(() =>
    this.acp.asks().filter((a) => a.sessionId === this.id()),
  );

  protected readonly title = computed(
    () => this.session.value()?.title() ?? this.listTitle() ?? 'Session',
  );

  protected readonly usage = computed(() => {
    const u = this.session.value()?.usage();
    return u && u.size > 0 ? Math.round((100 * u.used) / u.size) : undefined;
  });

  protected readonly running = computed(
    () =>
      this.acp.prompting().has(this.id()) ||
      !!this.session.value()?.running() ||
      (this.session.value()?.entries() ?? []).some(
        (e) => e.kind === 'tool' && e.call().status === 'in_progress',
      ),
  );

  /** Shows the jump button when false. */
  protected readonly atBottom = signal(true);
  /**
   * A `column-reverse` list: the browser keeps it pinned to the end (scrollTop 0)
   * when output arrives or it shrinks, and holds the reading position when
   * scrolled up. scrollTop counts negative from the end.
   */
  private readonly scroller = viewChild.required<ElementRef<HTMLElement>>('scroller');

  constructor() {
    const destroyRef = inject(DestroyRef);
    destroyRef.onDestroy(() => this.stopListening());
    effect(() => {
      const [key, draft] = [draftKey(this.id()), this.draft()];
      if (draft) localStorage.setItem(key, draft);
      else localStorage.removeItem(key);
    });
    // The switcher keeps this component: start the next session at its end.
    afterRenderEffect(() => {
      this.id();
      untracked(() => (this.scroller().nativeElement.scrollTop = 0));
    });
    afterNextRender(() => {
      const el = this.scroller().nativeElement;
      const onScroll = () => this.atBottom.set(el.scrollTop > -80);
      el.addEventListener('scroll', onScroll, { passive: true });
      destroyRef.onDestroy(() => {
        el.removeEventListener('scroll', onScroll);
      });
    });
  }

  /** Jumps to the end, which then follows new output by itself. */
  protected scrollToEnd() {
    // Wait a few frames so a closing keyboard has resized the list first.
    const el = this.scroller().nativeElement;
    const wait = (frames: number) =>
      frames
        ? requestAnimationFrame(() => wait(frames - 1))
        : el.scrollTo({ top: 0, behavior: 'smooth' });
    wait(3);
  }

  protected icon(call: acp.ToolCall) {
    return KIND_ICONS[call.kind ?? ''] ?? 'build';
  }

  protected readonly hasBody = hasBody;

  protected onKey(e: KeyboardEvent) {
    if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
      e.preventDefault();
      this.send();
    }
  }

  protected pick(e: MatAutocompleteSelectedEvent) {
    const name = (e.option.value as acp.AvailableCommand).name;
    // A command without input gets a trailing space; one with input a space to type into.
    this.draft.set(`/${name} `);
  }

  protected async send() {
    const agent = this.acp.agent();
    const state = this.session.value();
    const text = this.draft().trim();
    if (!agent || !state || !text) return;
    // The component stays when the switcher changes the id, so keep this one.
    const sessionId = this.id();
    const prompting = (on: boolean) =>
      this.acp.prompting.update((s) => {
        const next = new Set(s);
        if (on) next.add(sessionId);
        else next.delete(sessionId);
        return next;
      });
    this.draft.set('');
    state.addUser(text);
    this.scrollToEnd();
    prompting(true);
    try {
      const res = await agent.request(acp.methods.agent.session.prompt, {
        sessionId,
        prompt: [{ type: 'text', text }],
      });
      if (res.stopReason !== 'end_turn' && res.stopReason !== 'cancelled') {
        this.snackBar.open(`Stopped: ${res.stopReason}`, 'OK', { duration: 5000 });
      }
    } catch (e) {
      this.snackBar.open(errorText(e), 'OK');
    } finally {
      prompting(false);
    }
  }

  protected async setOption(option: acp.SessionConfigOption, change: MatSelectChange<string>) {
    const agent = this.acp.agent();
    const state = this.session.value();
    if (!agent || !state) return;
    try {
      const res = await agent.request(acp.methods.agent.session.setConfigOption, {
        sessionId: this.id(),
        configId: option.id,
        value: change.value,
      });
      state.options.set(res.configOptions);
    } catch (e) {
      this.snackBar.open(errorText(e), 'OK');
      // Back to the value rho still has.
      change.source.value = option.currentValue;
    }
  }

  /** The choices of a select option, flat or grouped. */
  protected groups(option: acp.SessionConfigOption & { type: 'select' }) {
    const o = option.options;
    return o.length && 'group' in o[0]
      ? (o as acp.SessionConfigSelectGroup[])
      : [{ group: '', name: '', options: o as acp.SessionConfigSelectOption[] }];
  }

  protected cancel() {
    this.acp.agent()?.notify(acp.methods.agent.session.cancel, { sessionId: this.id() });
  }
}

function draftKey(sessionId: string) {
  return `rho.draft.${sessionId}`;
}

/** Whether a tool card has anything to show; cheap, unlike `toolBody` with its diffs. */
export function hasBody(call: acp.ToolCall) {
  return (call.content ?? []).some(
    (c) => c.type === 'diff' || (c.type === 'content' && !!blockText(c.content)),
  );
}

/** A tool call's content as markdown; empty when there is nothing to show. */
export function toolBody(call: acp.ToolCall) {
  return (call.content ?? [])
    .map((c, i) => {
      if (c.type === 'diff') return fence(diff(c.path, c.oldText ?? '', c.newText), 'diff');
      if (c.type !== 'content') return '';
      const text = blockText(c.content);
      // A shell call's first block is the command, already fenced; then the output.
      return call.kind === 'execute' && i > 0 && c.content.type === 'text' && text
        ? fence(text)
        : text;
    })
    .filter((t) => t)
    .join('\n\n');
}

/** An error with the JSON-RPC `data` rho puts the cause in. */
function errorText(e: unknown) {
  if (e instanceof acp.RequestError && typeof e.data === 'string') return `${e.message}: ${e.data}`;
  return e instanceof Error ? e.message : String(e);
}

/** `text` as a fenced code block, with a fence longer than any backtick run in it. */
function fence(text: string, lang = '') {
  const longest = Math.max(0, ...(text.match(/`+/g) ?? []).map((r) => r.length));
  const f = '`'.repeat(Math.max(3, longest + 1));
  return `${f}${lang}\n${text}\n${f}`;
}

/** The unified diff hunks of an edit, without the file header. */
function diff(path: string, old: string, next: string) {
  return structuredPatch(path, path, old, next, '', '', { context: 3 })
    .hunks.map((h) =>
      [`@@ -${h.oldStart},${h.oldLines} +${h.newStart},${h.newLines} @@`, ...h.lines].join('\n'),
    )
    .join('\n');
}
