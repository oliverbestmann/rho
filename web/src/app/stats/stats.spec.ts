import { signal } from '@angular/core';
import { TestBed } from '@angular/core/testing';
import { provideRouter } from '@angular/router';
import { Acp } from '../acp';
import {
  HOUR,
  compareRange,
  dayBounds,
  isoWeek,
  linePath,
  pace,
  parseMonth,
  weekCosts,
  weekHours,
} from './charts';
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

  it('groups costs by ISO week and by weekday and hour', () => {
    expect(isoWeek(new Date(2026, 10, 1).getTime())).toBe('2026-W44');
    expect(isoWeek(new Date(2027, 0, 1).getTime())).toBe('2026-W53');
    expect(isoWeek(new Date(2026, 0, 1).getTime())).toBe('2026-W01');
    const days = dayBounds({ year: 2026, month: 10 });
    const weeks = weekCosts(
      days,
      days.slice(1).map(() => 1),
    );
    expect(weeks[0]).toEqual({ week: '2026-W44', cost: 1 });
    expect(weeks[1]).toEqual({ week: '2026-W45', cost: 7 });
    // 2026-11-01 is a Sunday; 10:00 UTC is 11:00 in Berlin.
    const hourly = Array(24).fill(0);
    hourly[10] = 5;
    const grid = weekHours(hourly, Date.UTC(2026, 10, 1));
    expect(grid[6][11]).toBe(5);
    expect(grid.flat().reduce((a, b) => a + b)).toBe(5);
  });

  it('draws lines across hours without samples', () => {
    expect(linePath([null, 0.5, null, 1], 5)).toBe('M250.0 100.0L750.0 0.0');
    expect(linePath([null, null], 5)).toBe('');
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
    const usage = {
      inputTokens: 1,
      outputTokens: 2,
      cacheReadTokens: 3_000_000,
      cacheWriteTokens: 4,
      inputCost: 0,
      outputCost: 0,
      cacheReadCost: 0,
      cacheWriteCost: 0,
    };
    const response: StatsResponse = {
      totals,
      previous: { ...totals, cost: 100 },
      providers: [
        {
          provider: 'anthropic',
          cost: 120,
          requests: 1000,
          days: [100, 20, ...Array(26).fill(0)],
          fiveHour: null,
          sevenDay: {
            peak: 0.95,
            highMs: 2 * HOUR,
            hourly: [null, 0.1, null, 0.95],
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
      projects: [
        { ...usage, path: '/home/me/src/rho/', requests: 900, cost: 100, cacheReadCost: 12.5 },
        { ...usage, path: null, requests: 100, cost: 20 },
      ],
      hourly: [1, 2, 3],
      first: 0,
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
    expect(text).toContain('Limits, hourly');
    expect(text).toContain('recorded since');
    expect(text).toContain('Projectsrho900 requests');
    expect(text).not.toContain('/home/me');
    expect(text).toContain('No session');
    expect(text).toContain('Cache read');
    expect(text).toContain('3M · $12.50');
    expect(text).toContain('2026-W09');
    expect(text).toContain('$87.50 without a breakdown');
  });
});
