import { effect, inject, Injectable, signal, untracked, WritableSignal } from '@angular/core';
import * as acp from '@agentclientprotocol/sdk';
import { MatSnackBar } from '@angular/material/snack-bar';
import { createWebSocketStream } from '@agentclientprotocol/sdk/experimental/ws-client';
import { Server, Settings } from './settings';
import { SessionState } from './session/session-state';

/** A session loaded over the current connection; its state follows the updates. */
export interface Opened {
  readonly state: SessionState;
  readonly loading: WritableSignal<boolean>;
  readonly error: WritableSignal<Error | undefined>;
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

/** An error with the JSON-RPC `data` rho puts the cause in. */
export function errorText(e: unknown) {
  if (e instanceof acp.RequestError && typeof e.data === 'string') return `${e.message}: ${e.data}`;
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
  /** Messages typed while a turn ran, sent one by one after it; kept over reconnects. */
  private readonly queues = new Map<string, WritableSignal<string[]>>();
  /** Sessions whose queue waits for the next turn to end: stopped or failed. */
  private readonly held = new Set<string>();
  private connection?: acp.ClientConnection;
  private retries = 0;
  private timer?: ReturnType<typeof setTimeout>;
  private seq = 0;

  constructor() {
    effect(() => {
      const server = this.settings.server();
      untracked(() => this.open(server));
    });
    // Phones drop sockets while asleep: retry at once when the page is back.
    document.addEventListener('visibilitychange', () => {
      if (!document.hidden && this.status() === 'offline') this.open(this.settings.server());
    });
  }

  /**
   * The session, loaded with `session/load` the first time it is asked for
   * on this connection. Later calls return the same, kept current.
   */
  session(agent: acp.ClientContext, sessionId: string, cwd: string): Opened {
    const cached = this.opened.get(sessionId);
    if (cached) return cached;
    const opened: Opened = {
      state: new SessionState(),
      loading: signal(true),
      error: signal(undefined),
    };
    this.opened.set(sessionId, opened);
    // rho replays the history as updates before it answers.
    agent
      .request(acp.methods.agent.session.load, { sessionId, cwd, mcpServers: [] })
      .then((res) => opened.state.options.set(res.configOptions ?? []))
      .catch((e: unknown) => {
        // Not kept, so the next visit tries again.
        if (this.opened.get(sessionId) === opened) this.opened.delete(sessionId);
        opened.error.set(e instanceof Error ? e : new Error(String(e)));
      })
      .finally(() => {
        opened.loading.set(false);
        // A turn may have ended while we were away.
        this.next(sessionId);
      });
    return opened;
  }

  /** The messages queued for the session, see `next`. */
  queue(sessionId: string): WritableSignal<string[]> {
    let queue = this.queues.get(sessionId);
    if (!queue) {
      queue = signal<string[]>([]);
      this.queues.set(sessionId, queue);
    }
    return queue;
  }

  /** Sends a prompt to an opened session; when its turn ends, the next queued one. */
  async prompt(sessionId: string, text: string) {
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
    this.held.delete(sessionId);
    opened.state.addUser(text);
    prompting(true);
    let ended = false;
    try {
      const res = await agent.request(acp.methods.agent.session.prompt, {
        sessionId,
        prompt: [{ type: 'text', text }],
      });
      ended = res.stopReason === 'end_turn';
      if (!ended && res.stopReason !== 'cancelled') {
        this.snackBar.open(`Stopped: ${res.stopReason}`, 'OK', { duration: 5000 });
      }
    } catch (e) {
      this.snackBar.open(errorText(e), 'OK');
    } finally {
      prompting(false);
    }
    // A failing turn should not use up the queue.
    if (ended) this.next(sessionId);
    else this.held.add(sessionId);
  }

  /** Stops the session's turns; the queue waits for the next turn to end. */
  cancel(sessionId: string) {
    this.held.add(sessionId);
    this.agent()?.notify(acp.methods.agent.session.cancel, { sessionId });
  }

  /** Sends the first queued message, if the session is idle and not held. */
  private next(sessionId: string) {
    const opened = this.opened.get(sessionId);
    const queue = this.queues.get(sessionId);
    if (!opened || opened.loading() || opened.error() || !queue?.().length) return;
    if (this.held.has(sessionId) || this.prompting().has(sessionId) || opened.state.running()) {
      return;
    }
    const [text, ...rest] = queue();
    queue.set(rest);
    void this.prompt(sessionId, text);
  }

  /** Applies an update; a turn starting ends the hold, one ending sends the queue. */
  private update(params: acp.SessionNotification) {
    const { sessionId } = params;
    const opened = this.opened.get(sessionId);
    if (!opened) return;
    const was = opened.state.running();
    opened.state.apply(params.update);
    const now = opened.state.running();
    if (now && !was) this.held.delete(sessionId);
    if (was && !now) this.next(sessionId);
  }

  /** No connection: rho forgot what we viewed, the next visit loads again. */
  private disconnected() {
    this.agent.set(undefined);
    this.asks.set([]);
    this.opened.clear();
  }

  private async open(server: Server | undefined) {
    clearTimeout(this.timer);
    this.connection?.close();
    this.connection = undefined;
    this.disconnected();
    if (!server) {
      this.status.set('offline');
      return;
    }
    this.status.set('connecting');

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

    conn.closed
      .catch(() => undefined)
      .then(() => {
        if (this.connection !== conn) return;
        this.connection = undefined;
        this.disconnected();
        this.status.set('offline');
        this.error.set(`Cannot reach ${server.url}`);
        const delay = Math.min(1000 * 2 ** this.retries++, 30_000);
        this.timer = setTimeout(() => this.open(this.settings.server()), delay);
      });

    try {
      await conn.agent.request(acp.methods.agent.initialize, {
        protocolVersion: acp.PROTOCOL_VERSION,
        clientCapabilities: { elicitation: { form: {} } },
        clientInfo: { name: 'rho-web', version: '0.1.0' },
      });
    } catch (e) {
      if (this.connection === conn) conn.close(e);
      return;
    }
    if (this.connection !== conn) return;
    this.retries = 0;
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
