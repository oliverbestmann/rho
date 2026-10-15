import { effect, inject, Injectable, signal, untracked, WritableSignal } from '@angular/core';
import * as acp from '@agentclientprotocol/sdk';
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

/**
 * The one ACP connection to the rho daemon. Reconnects on its own; whoever
 * reads `agent()` reloads when it changes.
 */
@Injectable({ providedIn: 'root' })
export class Acp {
  private readonly settings = inject(Settings);

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
      .finally(() => opened.loading.set(false));
    return opened;
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
      .onNotification(acp.methods.client.session.update, ({ params }) =>
        this.opened.get(params.sessionId)?.state.apply(params.update),
      )
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
