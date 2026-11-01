import { TestBed } from '@angular/core/testing';
import { ContextUsage } from './context-usage';

describe('ContextUsage', () => {
  it('fills the ring and shows tokens and cost when tapped', async () => {
    const fixture = TestBed.createComponent(ContextUsage);
    fixture.componentRef.setInput('usage', {
      used: 30_000,
      size: 1_000_000,
      cost: { amount: 10.23, currency: 'USD' },
      _meta: { rho: { historyBytes: 3 * 1024 * 1024, requestBytes: 1536 } },
    });
    await fixture.whenStable();
    const el = fixture.nativeElement as HTMLElement;
    expect(el.querySelector('.fill')?.getAttribute('stroke-dasharray')).toBe('3 100');

    el.querySelector('button')!.click();
    await fixture.whenStable();
    const details = document.querySelector('.details')?.textContent;
    expect(details).toContain('30k of 1000k tokens, 3%');
    expect(details).toContain('10.23');
    expect(details).toContain('History in memory: ~3.0 MB');
    expect(details).toContain('Last request: ~1.5 KB');
  });
});
