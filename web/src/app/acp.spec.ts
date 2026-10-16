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

describe('Acp queue', () => {
  /** An Acp with a loaded session `s`; prompts wait until `end` is called. */
  async function setup() {
    const prompts: { text: string; end(stopReason: string): void }[] = [];
    const request = vi.fn((method: string, params: { prompt?: { text: string }[] }) =>
      method === acp.methods.agent.session.prompt
        ? new Promise((end) =>
            prompts.push({
              text: params.prompt![0].text,
              end: (stopReason) => end({ stopReason }),
            }),
          )
        : Promise.resolve({ configOptions: [] }),
    );
    const agent = { request, notify: vi.fn() } as unknown as acp.ClientContext;
    const a = TestBed.inject(Acp);
    TestBed.tick(); // the connecting effect, which starts with an empty cache
    a.agent.set(agent);
    const opened = a.session(agent, 's', '/x');
    await vi.waitFor(() => expect(opened.loading()).toBe(false));
    const running = (on: boolean) =>
      (a as unknown as { update(p: acp.SessionNotification): void }).update({
        sessionId: 's',
        update: { sessionUpdate: 'session_info_update', _meta: { rho: { running: on } } },
      });
    return { a, prompts, running };
  }

  it('sends the next message when a turn ends', async () => {
    const { a, prompts } = await setup();
    void a.prompt('s', 'one');
    a.queue('s').set(['two']);
    prompts[0].end('end_turn');
    await vi.waitFor(() => expect(prompts.map((p) => p.text)).toEqual(['one', 'two']));
    expect(a.queue('s')()).toEqual([]);
  });

  it('holds the queue after a stop until the next turn ends', async () => {
    const { a, prompts } = await setup();
    void a.prompt('s', 'one');
    a.queue('s').set(['two']);
    a.cancel('s');
    prompts[0].end('cancelled');
    await vi.waitFor(() => expect(a.prompting().size).toBe(0));
    expect(prompts.length).toBe(1);

    void a.prompt('s', 'three');
    prompts[1].end('end_turn');
    await vi.waitFor(() => expect(prompts.map((p) => p.text)).toEqual(['one', 'three', 'two']));
  });

  it('sends after a turn of another client', async () => {
    const { a, prompts, running } = await setup();
    running(true);
    a.queue('s').set(['one']);
    running(false);
    expect(prompts.map((p) => p.text)).toEqual(['one']);
  });
});

describe('errorText', () => {
  it('names a socket error event instead of [object Event]', () => {
    expect(errorText(new Event('error'))).toBe('Connection lost');
  });
});
