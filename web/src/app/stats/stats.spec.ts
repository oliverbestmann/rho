import { signal } from '@angular/core';
import { TestBed } from '@angular/core/testing';
import { provideRouter } from '@angular/router';
import { Acp } from '../acp';
import { HOUR, compareRange, dayBounds, pace, parseMonth } from './charts';
import { Stats, type StatsResponse } from './stats';

describe('stats helpers', () => {
  it('parses months and falls back to the current one', () => {
    expect(parseMonth('2026-02')).toEqual({ year: 2026, month: 1 });
    const now = new Date(2026, 10, 5);
    expect(parseMonth('2026-13', now)).toEqual({ year: 2026, month: 10 });
    expect(parseMonth(undefined, now)).toEqual({ year: 2026, month: 10 });
  });

  it('bounds every day of the month by local midnights', () => {
    const days = dayBounds({ year: 2026, month: 1 });
    expect(days).toHaveLength(29);
    expect(days[0]).toBe(new Date(2026, 1, 1).getTime());
    expect(days[28]).toBe(new Date(2026, 2, 1).getTime());
  });

  it('compares with the same point of the month before', () => {
    const now = new Date(2026, 2, 31, 14, 30);
    expect(compareRange({ year: 2026, month: 2 }, now)).toEqual([
      new Date(2026, 1, 1).getTime(),
      new Date(2026, 1, 28, 14, 30).getTime(),
    ]);
    expect(compareRange({ year: 2026, month: 0 }, now)).toEqual([
      new Date(2025, 11, 1).getTime(),
      new Date(2026, 0, 1).getTime(),
    ]);
  });

  it('projects a window to its reset', () => {
    const resetsAt = 7 * 24 * HOUR;
    // A day in at 10%: 70% at reset.
    expect(pace({ at: 24 * HOUR, utilization: 0.1, resetsAt }, resetsAt)).toEqual({
      projected: expect.closeTo(0.7),
      full: null,
    });
    // A day in at 20%: full after five days.
    const p = pace({ at: 24 * HOUR, utilization: 0.2, resetsAt }, resetsAt)!;
    expect(p.full).toBeCloseTo(5 * 24 * HOUR);
  });
});

describe('Stats', () => {
  it('shows totals, accounts and models', async () => {
    const totals = {
      cost: 120,
      requests: 1000,
      sessions: 10,
      activeHours: 12.5,
      inputTokens: 100,
      outputTokens: 50_000,
      cacheReadTokens: 800,
      cacheWriteTokens: 100,
    };
    const response: StatsResponse = {
      totals,
      previous: { ...totals, cost: 100 },
      providers: [
        {
          provider: 'anthropic',
          cost: 120,
          requests: 1000,
          days: [100, 20],
          fiveHour: null,
          sevenDay: {
            peak: 0.95,
            highMs: 2 * HOUR,
            hourly: [0.1, null, 0.95],
            latest: { at: 0, utilization: 0.95, resetsAt: 0 },
          },
        },
      ],
      models: [
        {
          provider: 'anthropic',
          model: 'claude-x',
          requests: 1000,
          cost: 120,
          outputTokens: 50_000,
        },
      ],
      first: 0,
      estimatedUntil: null,
    };
    const request = vi.fn(async () => response);
    TestBed.configureTestingModule({
      providers: [provideRouter([]), { provide: Acp, useValue: { agent: signal({ request }) } }],
    });
    const fixture = TestBed.createComponent(Stats);
    fixture.componentRef.setInput('month', '2026-02');
    await fixture.whenStable();

    const [method, params] = request.mock.calls[0] as unknown as [string, { days: number[] }];
    expect(method).toBe('_rho/stats');
    expect(params.days).toHaveLength(29);
    const text = (fixture.nativeElement as HTMLElement).textContent!;
    expect(text).toContain('+20%');
    expect(text).toContain('90%');
    expect(text).toContain('7 d at 90%+ for 2 h 0 min');
    expect(text).toContain('claude-x');
    expect(text).toContain('Cache hit');
  });
});
