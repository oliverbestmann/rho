import { signal } from '@angular/core';
import { TestBed } from '@angular/core/testing';
import { Acp } from '../acp';
import { ContextUsage, type SessionUsage } from './context-usage';

describe('ContextUsage', () => {
  it('fills the ring and shows tokens, cache and all epochs when tapped', async () => {
    const now = Date.now();
    const all: SessionUsage = {
      requests: 7913,
      cost: 526.6,
      inputCost: 2.73,
      outputCost: 82.66,
      cacheReadCost: 232.54,
      cacheWriteCost: 208.67,
      first: now - 86_400_000,
      last: now - 60_000,
      activeHours: 141,
      estimated: true,
      epochs: 82,
      models: [{ provider: 'anthropic', model: 'claude-opus', requests: 7909, cost: 526.59 }],
    };
    const request = vi.fn(async () => all);
    TestBed.configureTestingModule({
      providers: [{ provide: Acp, useValue: { agent: signal({ request }) } }],
    });
    const fixture = TestBed.createComponent(ContextUsage);
    fixture.componentRef.setInput('sessionId', 's1');
    fixture.componentRef.setInput('usage', {
      used: 30_000,
      size: 1_000_000,
      cost: { amount: 10.23, currency: 'USD' },
      _meta: {
        rho: {
          historyBytes: 3 * 1024 * 1024,
          requestBytes: 1536,
          cacheTtlSecs: 300,
          request: { input: 10, output: 5, cacheRead: 90, cacheWrite: 0 },
          session: { input: 100, output: 50, cacheRead: 300, cacheWrite: 100 },
        },
      },
    });
    await fixture.whenStable();
    const el = fixture.nativeElement as HTMLElement;
    expect(el.querySelector('.fill')?.getAttribute('stroke-dasharray')).toBe('3 100');

    el.querySelector('button')!.click();
    await fixture.whenStable();
    fixture.detectChanges();
    expect(request).toHaveBeenCalledWith('_rho/session_usage', { sessionId: 's1' });
    const details = document.querySelector('.details')!.textContent!.replace(/\s+/g, ' ');
    expect(details).toContain('30k of 1000k tokens, 3%');
    expect(details).toContain('10.23');
    expect(details).toContain('History in memory: ~3.0 MB');
    expect(details).toContain('Last request: ~1.5 KB');
    expect(details).toContain('Cache hit90%60%');
    expect(details).toMatch(/Cache: warm, expires in (4m 00s|3m 59s)/);
    expect(details).toContain('7,913 requests in 141 active hours, 82 compactions or clears');
    expect(details).toContain('Cache read $232.54');
    expect(details).toContain('claude-opus: 7,909 requests, $526.59');
    expect(details).toContain('Partly estimated');
  });
});
