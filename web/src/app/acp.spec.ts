import { TestBed } from '@angular/core/testing';
import type * as acp from '@agentclientprotocol/sdk';
import { Acp } from './acp';

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
    await vi.waitFor(() => expect(first.error()?.message).toBe('gone'));
    expect(a.session(agent, 's', '/x')).not.toBe(first);
  });
});
