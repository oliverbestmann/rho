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
  resource,
  signal,
  untracked,
  viewChild,
} from '@angular/core';
import { RouterLink } from '@angular/router';
import * as acp from '@agentclientprotocol/sdk';
import { MatButtonModule } from '@angular/material/button';
import { MatIconModule } from '@angular/material/icon';
import { MatProgressSpinnerModule } from '@angular/material/progress-spinner';
import { MatSelectChange } from '@angular/material/select';
import { MatSnackBar } from '@angular/material/snack-bar';
import { MatToolbarModule } from '@angular/material/toolbar';
import { MatMenuModule } from '@angular/material/menu';
import { Acp, Ask, errorText, Opened } from '../acp';
import { loadHighlighter } from '../highlight';
import { listSessions } from '../sessions/list';
import { AskCard } from './ask-card';
import { Composer } from './composer';
import { ContextUsage } from './context-usage';
import { ChatMessage } from './message';
import { OptionSelect } from './option-select';
import { QueuedList } from './queued-list';
import { SessionSwitcher } from './session-switcher';
import { ThoughtCard } from './thought-card';
import { ToolCard } from './tool-card';
import { Working } from './working';

/** The open session: loads it from rho and passes its state to the parts. */
@Component({
  selector: 'app-session',
  imports: [
    RouterLink,
    AskCard,
    ChatMessage,
    Composer,
    ContextUsage,
    OptionSelect,
    QueuedList,
    SessionSwitcher,
    ThoughtCard,
    ToolCard,
    Working,
    MatButtonModule,
    MatIconModule,
    MatProgressSpinnerModule,
    MatToolbarModule,
    MatMenuModule,
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

  protected readonly commands = computed(() => this.shown()?.state.commands() ?? []);
  /** Synchronous, unlike a resource: a session opened before shows at once. */
  protected readonly opened = computed(() => {
    const agent = this.acp.agent();
    return agent && this.acp.session(agent, this.id(), this.cwd());
  });
  /** What the view shows: the last loaded state while offline or reloading. */
  protected readonly shown = linkedSignal<
    { id: string; opened: Opened | undefined; loading: boolean | undefined },
    Opened | undefined
  >({
    source: () => {
      const opened = this.opened();
      return { id: this.id(), opened, loading: opened?.loading() };
    },
    computation: ({ id, opened }, prev) =>
      keepShown(opened, prev?.source.id === id ? prev.value : undefined),
  });
  /** The view shows history from a lost connection. */
  protected readonly stale = computed(() => !!this.shown() && this.shown() !== this.opened());
  /** Loaded without error: rho takes prompts for it. */
  protected readonly ready = computed(() => {
    const o = this.opened();
    return !!o && !o.loading() && !o.error();
  });

  /** For the switcher; reloaded each time it opens. */
  protected readonly sessions = resource({
    params: () => this.acp.agent(),
    loader: ({ params: agent }) => listSessions(agent),
  });
  /** The five last updated sessions, plus this one if it is older. */
  protected readonly recent = computed(() =>
    (this.sessions.value() ?? []).filter((s, i) => i < 5 || s.sessionId === this.id()),
  );

  /**
   * The entries in chunks of `CHUNK`, keyed per session so the switcher does
   * not reuse a rendered chunk. Only the chunks with the last `CHUNK` entries
   * render at once, the older ones when scrolled to: `@defer` keeps them once shown.
   */
  protected readonly chunks = computed(() => {
    const entries = this.shown()?.state.entries() ?? [];
    const id = this.id();
    const chunks = [];
    for (let start = 0; start < entries.length; start += CHUNK) {
      const end = start + CHUNK;
      chunks.push({
        key: `${id}:${start}`,
        entries: entries.slice(start, end),
        eager: end > entries.length - CHUNK,
      });
    }
    return chunks;
  });

  /** Typed while a turn ran; sent after it, see `Acp.prompt`. */
  protected readonly queued = computed(() => this.acp.queue(this.id())());

  protected readonly asks = computed(() =>
    this.acp.asks().filter((a) => a.sessionId === this.id()),
  );

  /** The tool call a permission request is about, e.g. a gated `git commit`. */
  protected askedCall(a: Ask) {
    return a.kind === 'permission'
      ? this.opened()?.state.call(a.request.toolCall.toolCallId)
      : undefined;
  }

  protected readonly title = computed(
    () => this.shown()?.state.title() ?? this.listTitle() ?? 'Session',
  );

  protected readonly usage = computed(() => {
    const u = this.shown()?.state.usage();
    return u && u.size > 0 ? u : undefined;
  });

  /** A turn runs, so a send waits in the queue; like `Acp.next`. */
  protected readonly running = computed(
    () => this.acp.prompting().has(this.id()) || !!this.opened()?.state.running(),
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
      void loadHighlighter(); // in the background, not waited for
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

  /** Sends the draft, or queues it while a turn runs. */
  protected send() {
    const text = this.draft().trim();
    if (!this.ready() || !text) return;
    this.draft.set('');
    if (this.running()) {
      this.acp.queue(this.id()).update((q) => [...q, text]);
    } else {
      void this.acp.prompt(this.id(), text);
      this.scrollToEnd();
    }
  }

  protected unqueue(index: number) {
    this.acp.queue(this.id()).update((q) => q.filter((_, i) => i !== index));
  }

  protected async setOption(option: acp.SessionConfigOption, change: MatSelectChange<string>) {
    const agent = this.acp.agent();
    const state = this.opened()?.state;
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

  protected cancel() {
    this.acp.cancel(this.id());
  }
}

/**
 * The session to show: the open one, unless it is gone (offline) or still
 * reloading after a reconnect and `old`, shown before, has its history.
 */
export function keepShown(opened: Opened | undefined, old: Opened | undefined) {
  return opened && !(opened.loading() && old && !old.loading()) ? opened : old;
}

/** Entries per chunk of the chat, see `Session.chunks`. */
const CHUNK = 20;

function draftKey(sessionId: string) {
  return `rho.draft.${sessionId}`;
}
