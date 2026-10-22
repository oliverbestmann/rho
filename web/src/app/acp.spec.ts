import { TestBed } from '@angular/core/testing';
import * as acp from '@agentclientprotocol/sdk';
import { Acp, errorText } from './acp';

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
