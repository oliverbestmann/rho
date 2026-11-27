import { TestBed } from '@angular/core/testing';
import { Router } from '@angular/router';
import { Acp } from '../acp';
import { QuickSwitch, fuzzyMatch } from './quick-switch';

describe('fuzzyMatch', () => {
  it('matches the characters in order, ignoring case and spaces', () => {
    expect(fuzzyMatch('rwb', 'Rho web client')).toBe(true);
    expect(fuzzyMatch('rho cli', '/home/x/rho-client')).toBe(true);
    expect(fuzzyMatch('bwr', 'Rho web client')).toBe(false);
    expect(fuzzyMatch('', 'anything')).toBe(true);
  });
});

describe('QuickSwitch', () => {
  it('opens on Ctrl+K, filters, moves with the arrows and opens on Enter', async () => {
    const sessions = [
      { sessionId: 'a', title: 'Fix login', cwd: '/home/x/web' },
      { sessionId: 'b', title: 'Add search', cwd: '/home/x/rho' },
      { sessionId: 'c', title: 'Search docs', cwd: '/home/x/docs' },
    ];
    // jsdom has no scrollIntoView.
    Element.prototype.scrollIntoView = vi.fn();
    const agent = { request: vi.fn().mockResolvedValue({ sessions }) };
    TestBed.inject(Acp).agent.set(agent as never);
    const navigate = vi.spyOn(TestBed.inject(Router), 'navigate').mockResolvedValue(true);

    const fixture = TestBed.createComponent(QuickSwitch);
    const el = fixture.nativeElement as HTMLElement;
    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'k', ctrlKey: true }));
    await fixture.whenStable();

    const input = el.querySelector('input')!;
    input.value = 'srch';
    input.dispatchEvent(new Event('input'));
    await fixture.whenStable();
    expect([...el.querySelectorAll('li')].map((li) => li.textContent)).toEqual([
      'Add searchrho',
      'Search docsdocs',
    ]);

    input.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowDown' }));
    input.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter' }));
    expect(navigate).toHaveBeenCalledWith(['/session', 'c']);
    await fixture.whenStable();
    expect(el.querySelector('input')).toBeNull();
  });
});
