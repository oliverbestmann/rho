import { TestBed } from '@angular/core/testing';
import * as acp from '@agentclientprotocol/sdk';
import { Acp, errorText } from './acp';
import { Settings } from './settings';

describe('Acp.session', () => {
  it('loads a session once and keeps it', async () => {
    const request = vi.fn().mockResolvedValue({ configOptions: [] });
    const agent = { request } as unknown as acp.ClientContext;
    const a = TestBed.inject(Acp);
    TestBed.tick(); // the connecting effect, which starts with an empty cache

    const first = a.session(agent, 's', '/x');
    expect(first.loading()).toBe(true);
    await vi.waitFor(() => expect(first.loading()).toBe(false));
    expect(a.session(agent, 's', '/x')).toBe(first);
    expect(request).toHaveBeenCalledTimes(1);
  });

  it('loads again after a failed load', async () => {
    const request = vi.fn().mockRejectedValue(new Error('gone'));
    const agent = { request } as unknown as acp.ClientContext;
    const a = TestBed.inject(Acp);
    TestBed.tick(); // the connecting effect, which starts with an empty cache

    const first = a.session(agent, 's', '/x');
    await vi.waitFor(() => expect(first.error()).toBe('gone'));
    expect(a.session(agent, 's', '/x')).not.toBe(first);
  });
});

describe('Acp reconnects', () => {
  it('loads a kept session again from its cursor', async () => {
    const request = vi.fn().mockResolvedValue({ configOptions: [] });
    const agent = { request } as unknown as acp.ClientContext;
    const a = TestBed.inject(Acp);
    TestBed.tick(); // the connecting effect, which starts with an empty cache

    const first = a.session(agent, 's', '/x');
    await vi.waitFor(() => expect(first.loading()).toBe(false));
    first.state.cursor = { transcript: 't', entry: 3, len: 0 };
    (a as unknown as { disconnected(): void }).disconnected();

    const again = a.session(agent, 's', '/x');
    expect(again).not.toBe(first);
    expect(again.state).toBe(first.state);
    expect(request).toHaveBeenLastCalledWith(acp.methods.agent.session.load, {
      sessionId: 's',
      cwd: '/x',
      mcpServers: [],
      _meta: { rho: { since: { transcript: 't', entry: 3, len: 0 } } },
    });
  });
});

/** A socket to rho; answers `initialize` if `answering` was set when it was made, else never opens. */
class FakeSocket extends EventTarget {
  static all: FakeSocket[] = [];
  static answering = true;
  readyState = FakeSocket.answering ? 1 : 0;
  closed = false;

  constructor() {
    super();
    FakeSocket.all.push(this);
  }

  send(data: string) {
    const { id } = JSON.parse(data) as { id: number };
    const result = { protocolVersion: acp.PROTOCOL_VERSION, agentCapabilities: {} };
    queueMicrotask(() =>
      this.dispatchEvent(
        new MessageEvent('message', { data: JSON.stringify({ jsonrpc: '2.0', id, result }) }),
      ),
    );
  }

  close() {
    if (this.closed) return;
    this.closed = true;
    this.readyState = 3;
    this.dispatchEvent(new Event('close'));
  }
}

describe('Acp reconnect policy', () => {
  let hidden = false;
  let a: Acp;

  beforeEach(async () => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout', 'Date'] });
    vi.stubGlobal('WebSocket', FakeSocket);
    Object.defineProperty(document, 'hidden', { configurable: true, get: () => hidden });
    hidden = false;
    FakeSocket.all = [];
    FakeSocket.answering = true;
    TestBed.inject(Settings).server.set({ url: 'ws://rho', token: 't' });
    a = TestBed.inject(Acp);
    TestBed.tick();
    await vi.waitFor(() => expect(a.status()).toBe('online'));
  });

  afterEach(() => {
    // Without a server this Acp ignores later tests' `visibilitychange`.
    TestBed.inject(Settings).server.set(undefined);
    TestBed.tick();
    vi.useRealTimers();
    vi.unstubAllGlobals();
    delete (document as { hidden?: boolean }).hidden;
  });

  function setHidden(value: boolean) {
    hidden = value;
    document.dispatchEvent(new Event('visibilitychange'));
  }

  it('waits 1 s after a loss, then tries every 2 s with a 2 s timeout', async () => {
    FakeSocket.answering = false;
    FakeSocket.all[0].close();
    await vi.advanceTimersByTimeAsync(999);
    expect(a.status()).toBe('offline');
    expect(FakeSocket.all).toHaveLength(1);

    await vi.advanceTimersByTimeAsync(1);
    expect(FakeSocket.all).toHaveLength(2);
    expect(a.status()).toBe('connecting');

    await vi.advanceTimersByTimeAsync(2000);
    expect(FakeSocket.all[1].closed).toBe(true);
    // The next attempt is due at once, after the close settled.
    await vi.advanceTimersByTimeAsync(1);
    expect(FakeSocket.all).toHaveLength(3);

    // A quick failure still waits for the 2 s.
    FakeSocket.all[2].close();
    await vi.advanceTimersByTimeAsync(1999);
    expect(FakeSocket.all).toHaveLength(3);
    FakeSocket.answering = true;
    await vi.advanceTimersByTimeAsync(1);
    expect(FakeSocket.all).toHaveLength(4);
    await vi.waitFor(() => expect(a.status()).toBe('online'));
  });

  it('waits while hidden and connects at once when visible', async () => {
    setHidden(true);
    FakeSocket.answering = false;
    FakeSocket.all[0].close();
    await vi.advanceTimersByTimeAsync(10_000);
    expect(FakeSocket.all).toHaveLength(1);

    setHidden(false);
    expect(FakeSocket.all).toHaveLength(2);

    // Back again during an attempt: that one is dropped for a new one.
    FakeSocket.answering = true;
    setHidden(true);
    setHidden(false);
    expect(FakeSocket.all[1].closed).toBe(true);
    expect(FakeSocket.all).toHaveLength(3);
    await vi.waitFor(() => expect(a.status()).toBe('online'));
  });
});

describe('Acp prompts', () => {
  /** An Acp with a loaded session `s`; prompts never end. */
  async function setup() {
    const request = vi.fn((method: string) =>
      method === acp.methods.agent.session.prompt
        ? new Promise(() => {})
        : Promise.resolve({ configOptions: [] }),
    );
    const agent = { request, notify: vi.fn() } as unknown as acp.ClientContext;
    const a = TestBed.inject(Acp);
    TestBed.tick(); // the connecting effect, which starts with an empty cache
    a.agent.set(agent);
    const opened = a.session(agent, 's', '/x');
    await vi.waitFor(() => expect(opened.loading()).toBe(false));
    return { a, request, opened };
  }

  it('shows a prompt but not a known command', async () => {
    const { a, opened } = await setup();
    opened.state.commands.set([{ name: 'usage', description: '' }]);
    void a.prompt('s', '/usage');
    void a.prompt('s', '/other x');
    void a.prompt('s', 'hi /usage');
    const users = opened.state.entries().filter((e) => e.kind === 'user');
    expect(users.map((e) => (e.kind === 'user' ? e.text() : ''))).toEqual([
      '/other x',
      'hi /usage',
    ]);
  });

  it('queues and unqueues in rho', async () => {
    const { a, request } = await setup();
    await a.enqueue('s', 'two');
    await a.unqueue('s', 7);
    expect(request).toHaveBeenCalledWith('_rho/queue', { sessionId: 's', text: 'two' });
    expect(request).toHaveBeenCalledWith('_rho/unqueue', { sessionId: 's', id: 7 });
  });
});

describe('errorText', () => {
  it('names a socket error event instead of [object Event]', () => {
    expect(errorText(new Event('error'))).toBe('Connection lost');
  });
});
