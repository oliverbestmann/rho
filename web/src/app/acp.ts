import {
  DestroyRef,
  effect,
  inject,
  Injectable,
  Injector,
  signal,
  untracked,
  WritableSignal,
} from '@angular/core';
import * as acp from '@agentclientprotocol/sdk';
import { MatSnackBar, MatSnackBarRef, TextOnlySnackBar } from '@angular/material/snack-bar';
import { createWebSocketStream } from '@agentclientprotocol/sdk/experimental/ws-client';
import { Server, Settings } from './settings';
import { blockText, rhoMeta, SessionState } from './session/session-state';
import { cursorOf, HistoryDb } from './session/history-db';
import type { Image } from './session/composer';

/** A session loaded over the current connection; its state follows the updates. */
export interface Opened {
  readonly state: SessionState;
  readonly loading: WritableSignal<boolean>;
  readonly error: WritableSignal<string | undefined>;
}

/** A question from the agent that waits for the user. */
export type Ask =
  | {
      kind: 'permission';
      id: number;
      sessionId: string;
      request: acp.RequestPermissionRequest;
      answer(response: acp.RequestPermissionResponse): void;
    }
  | {
      kind: 'form';
      id: number;
      sessionId: string | undefined;
      request: acp.CreateElicitationRequest;
      answer(response: acp.CreateElicitationResponse): void;
    };

type Status = 'offline' | 'connecting' | 'online';

/** Wait after a lost connection before connecting again. */
const LOST_MS = 1000;
/** How long a connection attempt may take. */
const ATTEMPT_MS = 10_000;
/** Time between the starts of two attempts. */
const RETRY_MS = 2000;
/** Entries shown from the browser's copy on a reload, and how many more each "Show earlier" adds. */
const WINDOW = 50;

/** An error with the JSON-RPC `data` rho puts the cause in. */
export function errorText(e: unknown) {
  if (e instanceof acp.RequestError && typeof e.data === 'string') return `${e.message}: ${e.data}`;
  // The socket's `error` event rejects the pending requests.
  if (e instanceof Event) return 'Connection lost';
  return e instanceof Error ? e.message : String(e);
}

/**
 * The one ACP connection to the rho daemon. Reconnects on its own; whoever
 * reads `agent()` reloads when it changes.
 */
@Injectable({ providedIn: 'root' })
export class Acp {
  private readonly settings = inject(Settings);
  private readonly snackBar = inject(MatSnackBar);

  readonly status = signal<Status>('offline');
  readonly error = signal<string | undefined>(undefined);
  /** The agent of the open connection, undefined until `initialize` is done. */
  readonly agent = signal<acp.ClientContext | undefined>(undefined);
  /** Permission requests and forms waiting for an answer, over all sessions. */
  readonly asks = signal<Ask[]>([]);
  /** Sessions with a `session/prompt` from this client still running. */
  readonly prompting = signal<ReadonlySet<string>>(new Set());

  /** Sessions loaded over this connection; rho keeps sending their updates. */
  private readonly opened = new Map<string, Opened>();
  /** Sessions of lost connections to the same server; loading one again sends only what it missed. */
  private readonly kept = new Map<string, SessionState>();
  /** Tool call images by their `rho-image:` uri, fetched once. */
  private readonly images = new Map<string, Promise<Image>>();
  /** The transcripts kept in the browser, for reloads (ADR 0074). */
  private readonly history = new HistoryDb();
  /** Blocks of prompts sent from here: rho answers each with an empty update, kept as the block. */
  private readonly echoes = new Map<string, acp.ContentBlock[]>();
  /** Updates that came while `earlier` read the kept entries, to apply again. */
  private readonly buffers = new Map<string, acp.SessionNotification[]>();
  /** Entries read from the kept copy, per session; `earlier` reads more. */
  private readonly windows = new Map<string, number>();
  private url?: string;
  private connection?: acp.ClientConnection;
  private timer?: ReturnType<typeof setTimeout>;
  private seq = 0;
  private readonly injector = inject(Injector);

  constructor() {
    effect(() => {
      const server = this.settings.server();
      untracked(() => this.open(server));
    });
    // Phones drop sockets while asleep or without network: connect at once when both are back,
    // dropping a pending attempt.
    const done = new AbortController();
    inject(DestroyRef).onDestroy(() => done.abort());
    const { signal } = done;
    const resume = () => {
      if (this.canConnect() && this.status() !== 'online') this.open(this.settings.server());
    };
    document.addEventListener('visibilitychange', resume, { signal });
    let note: MatSnackBarRef<TextOnlySnackBar> | undefined;
    window.addEventListener(
      'online',
      () => {
        note?.dismiss();
        resume();
      },
      { signal },
    );
    const offline = () => (note = this.snackBar.open('No network'));
    window.addEventListener('offline', offline, { signal });
    if (!navigator.onLine) offline();
  }

  /** Hidden pages and devices without network wait for the events that end that. */
  private canConnect() {
    return !document.hidden && navigator.onLine;
  }

  /** Only rho's own errors: a lost connection shows as the reconnecting marker. */
  report(e: unknown) {
    if (e instanceof acp.RequestError) this.snackBar.open(errorText(e), 'OK');
  }

  /**
   * The session, loaded with `session/load` the first time it is asked for
   * on this connection. Later calls return the same, kept current.
   */
  session(agent: acp.ClientContext, sessionId: string, cwd: string): Opened {
    const cached = this.opened.get(sessionId);
    if (cached) return cached;
    const kept = this.kept.get(sessionId);
    this.kept.delete(sessionId);
    const opened: Opened = {
      state: kept ?? new SessionState(),
      loading: signal(true),
      error: signal(undefined),
    };
    this.opened.set(sessionId, opened);
    void this.load(agent, opened, sessionId, cwd, !kept);
    return opened;
  }

  private key(sessionId: string) {
    return `${this.url} ${sessionId}`;
  }

  /** `session/load`, after filling a new state from the browser's copy. */
  private async load(
    agent: acp.ClientContext,
    opened: Opened,
    sessionId: string,
    cwd: string,
    restore: boolean,
  ) {
    const { state } = opened;
    if (restore) {
      const kept = await this.history.load(this.key(sessionId), WINDOW);
      if (kept.cursor) {
        state.restore(kept.entries, kept.more);
        state.cursor = kept.cursor;
        this.windows.set(sessionId, WINDOW);
      }
    }
    // rho replays the history as updates before it answers: after `since` if it still has it,
    // else it sends `_meta.rho.reset` and all.
    const since = state.cursor;
    await agent
      .request(acp.methods.agent.session.load, {
        sessionId,
        cwd,
        mcpServers: [],
        ...(since !== undefined && { _meta: { rho: { since } } }),
      })
      .then((res) => opened.state.options.set(res.configOptions ?? []))
      .catch((e: unknown) => {
        // Not kept, so the next visit tries again.
        if (this.opened.get(sessionId) === opened) this.opened.delete(sessionId);
        opened.error.set(errorText(e));
      })
      .finally(() => opened.loading.set(false));
  }

  /** Shows `WINDOW` more of the entries kept in the browser. */
  async earlier(sessionId: string) {
    const opened = this.opened.get(sessionId);
    if (!opened || this.buffers.has(sessionId)) return;
    const buffer: acp.SessionNotification[] = [];
    this.buffers.set(sessionId, buffer);
    const window = (this.windows.get(sessionId) ?? WINDOW) + WINDOW;
    const kept = await this.history.load(this.key(sessionId), window);
    this.buffers.delete(sessionId);
    // Gone meanwhile, or reset: what it shows is newer.
    if (this.opened.get(sessionId) !== opened || !kept.cursor || !opened.state.earlier()) return;
    this.windows.set(sessionId, window);
    opened.state.restore(kept.entries, kept.more);
    for (const n of buffer) opened.state.receive(n);
  }

  /**
   * Queues a prompt in rho, which sends it after the current turn even when
   * this client is gone; it starts right away if the session is idle.
   */
  async enqueue(sessionId: string, text: string) {
    await this.extRequest('_rho/queue', { sessionId, text });
  }

  /** Removes a queued prompt; one that started already is not an error. */
  async unqueue(sessionId: string, id: number) {
    await this.extRequest('_rho/unqueue', { sessionId, id });
  }

  /**
   * Takes a queued prompt out, stops the turn and sends the prompt once the
   * turn ended; rho holds the rest of the queue after a stop.
   */
  async sendNow(sessionId: string, id: number, text: string) {
    const state = this.opened.get(sessionId)?.state;
    if (!state) return;
    await this.unqueue(sessionId, id);
    this.cancel(sessionId);
    // shortcut: waits forever if the connection drops first, the prompt is then lost.
    await new Promise<void>((done) => {
      const ref = effect(
        () => {
          if (this.prompting().has(sessionId) || state.running()) return;
          ref.destroy();
          done();
        },
        { injector: this.injector },
      );
    });
    await this.prompt(sessionId, text);
  }

  /** The data of a tool call image that rho sent as a `rho-image:` uri. */
  image(sessionId: string, uri: string): Promise<Image> {
    let image = this.images.get(uri);
    if (!image) {
      const agent = this.agent();
      if (!agent) return Promise.reject(new Error('Not connected'));
      image = agent.request('_rho/image', { sessionId, uri }) as Promise<Image>;
      // A failed fetch is tried again the next time.
      image.catch(() => this.images.delete(uri));
      this.images.set(uri, image);
    }
    return image;
  }

  private async extRequest(method: string, params: Record<string, unknown>) {
    try {
      await this.agent()?.request(method, params);
    } catch (e) {
      this.report(e);
    }
  }

  /** Sends a prompt, with images after the text, to an opened session. */
  async prompt(sessionId: string, text: string, images: Image[] = []) {
    const agent = this.agent();
    const opened = this.opened.get(sessionId);
    if (!agent || !opened) return;
    const prompting = (on: boolean) =>
      this.prompting.update((s) => {
        const next = new Set(s);
        if (on) next.add(sessionId);
        else next.delete(sessionId);
        return next;
      });
    // A command shows only as the note it replies with, like rho shows it to other clients.
    const name = text.trim().match(/^\/(\S+)/)?.[1];
    const prompt: acp.ContentBlock[] = [
      ...(text ? [{ type: 'text' as const, text }] : []),
      ...images.map((i) => ({ type: 'image' as const, ...i })),
    ];
    if (!opened.state.commands().some((c) => c.name === name)) {
      opened.state.addUser(prompt.map(blockText).join(''));
      this.echoes.set(sessionId, [...prompt]);
    }
    prompting(true);
    try {
      const res = await agent.request(acp.methods.agent.session.prompt, {
        sessionId,
        prompt,
      });
      if (res.stopReason !== 'end_turn' && res.stopReason !== 'cancelled') {
        this.snackBar.open(`Stopped: ${res.stopReason}`, 'OK', { duration: 5000 });
      }
    } catch (e) {
      this.report(e);
    } finally {
      // rho echoes before the turn starts.
      this.echoes.delete(sessionId);
      prompting(false);
    }
  }

  /** Stops the session's turns; rho holds its queue until a turn ends normally. */
  cancel(sessionId: string) {
    this.agent()?.notify(acp.methods.agent.session.cancel, { sessionId });
  }

  private update(params: acp.SessionNotification) {
    const { sessionId } = params;
    const opened = this.opened.get(sessionId);
    if (!opened) return;
    const key = this.key(sessionId);
    const u = params.update;
    if (u.sessionUpdate === 'session_info_update' && rhoMeta(u._meta)?.['reset'] === true) {
      this.history.clear(key);
      this.windows.delete(sessionId);
    }
    const cursor = cursorOf(params);
    if (cursor) {
      // The echo of our own prompt: kept as the block it stands for.
      const echo = Object.keys(u).every((k) => k === 'sessionUpdate')
        ? this.echoes.get(sessionId)?.shift()
        : undefined;
      const update: acp.SessionUpdate = echo
        ? { sessionUpdate: 'user_message_chunk', content: echo }
        : u;
      this.history.record(key, update, cursor);
      // Shown already by `prompt`, but not in what `earlier` reads.
      this.buffers.get(sessionId)?.push({ ...params, update });
    } else {
      this.buffers.get(sessionId)?.push(params);
    }
    opened.state.receive(params);
  }

  /** No connection: rho forgot what we viewed, the next visit loads again. */
  private disconnected() {
    this.agent.set(undefined);
    this.asks.set([]);
    for (const [id, opened] of this.opened) this.kept.set(id, opened.state);
    this.opened.clear();
  }

  private async open(server: Server | undefined) {
    clearTimeout(this.timer);
    this.connection?.close();
    this.connection = undefined;
    this.disconnected();
    // Another rho has other transcripts.
    if (server?.url !== this.url) this.kept.clear();
    this.url = server?.url;
    if (!server) {
      this.status.set('offline');
      return;
    }
    this.status.set('connecting');
    const started = Date.now();

    const stream = createWebSocketStream(server.url, {
      // Browsers cannot set headers, rho takes the token as a subprotocol.
      protocols: ['acp.v1', `bearer.${server.token}`],
    });
    const conn = acp
      .client({ name: 'rho-web' })
      .onNotification(acp.methods.client.session.update, ({ params }) => this.update(params))
      .onRequest(acp.methods.client.session.requestPermission, ({ params, signal }) =>
        this.ask<acp.RequestPermissionResponse>(signal, (id, answer) => ({
          kind: 'permission',
          id,
          sessionId: params.sessionId,
          request: params,
          answer,
        })),
      )
      .onRequest(acp.methods.client.elicitation.create, ({ params, signal }) =>
        this.ask<acp.CreateElicitationResponse>(signal, (id, answer) => ({
          kind: 'form',
          id,
          sessionId: 'sessionId' in params ? (params.sessionId as string) : undefined,
          request: params,
          answer,
        })),
      )
      .connect(stream);
    this.connection = conn;
    const timeout = setTimeout(() => conn.close(new Error('Timed out')), ATTEMPT_MS);

    conn.closed
      .catch(() => undefined)
      .then(() => {
        clearTimeout(timeout);
        if (this.connection !== conn) return;
        const lost = this.status() === 'online';
        this.connection = undefined;
        this.disconnected();
        this.status.set('offline');
        this.error.set(`Cannot reach ${server.url}`);
        // Attempts start RETRY_MS apart.
        const delay = lost ? LOST_MS : started + RETRY_MS - Date.now();
        if (this.canConnect())
          this.timer = setTimeout(() => this.open(this.settings.server()), delay);
      });

    try {
      await conn.agent.request(acp.methods.agent.initialize, {
        protocolVersion: acp.PROTOCOL_VERSION,
        // `terminal_output`: Bash output streams in as Zed's terminal `_meta`.
        // `rho.imageRefs`: tool call images come as `rho-image:` uris, see `image`.
        clientCapabilities: {
          elicitation: { form: {} },
          _meta: { terminal_output: true, rho: { imageRefs: true } },
        },
        clientInfo: { name: 'rho-web', version: '0.1.0' },
      });
    } catch (e) {
      if (this.connection === conn) conn.close(e);
      return;
    }
    if (this.connection !== conn) return;
    clearTimeout(timeout);
    this.error.set(undefined);
    this.status.set('online');
    this.agent.set(conn.agent);
  }

  /** Queues a question until the user answers it or the agent withdraws it. */
  private ask<R>(
    signal: AbortSignal,
    make: (id: number, answer: (r: R) => void) => Ask,
  ): Promise<R> {
    return new Promise<R>((resolve, reject) => {
      const id = ++this.seq;
      const remove = () => this.asks.update((asks) => asks.filter((a) => a.id !== id));
      // `$/cancel_request`: another client answered first.
      signal.addEventListener(
        'abort',
        () => {
          remove();
          reject(signal.reason);
        },
        { once: true },
      );
      const ask = make(id, (response) => {
        remove();
        resolve(response);
      });
      this.asks.update((asks) => [...asks, ask]);
    });
  }
}
