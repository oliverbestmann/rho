import { signal } from '@angular/core';
import { TestBed } from '@angular/core/testing';
import { provideRouter, Router } from '@angular/router';
import { Acp } from '../acp';
import { NewSession } from './new-session';

/** A daemon where `~/src` exists and `~/sr` does not. */
function setup() {
  const request = vi.fn(async (method: string, params: { path?: string; cwd?: string }) => {
    if (method === '_rho/dirs') {
      const known: Record<string, string> = {
        '~/src': '/home/me/src',
        '/home/me/sr': '/home/me/sr',
      };
      const dir = known[params.path!] ?? null;
      return { entries: params.path === '~/sr' ? ['src'] : [], dir };
    }
    if (method === '_rho/mkdir') return { dir: '/home/me/sr' };
    return { sessionId: 's1' };
  });
  TestBed.configureTestingModule({
    providers: [provideRouter([]), { provide: Acp, useValue: { agent: signal({ request }) } }],
  });
  localStorage.clear();
  const navigate = vi.spyOn(TestBed.inject(Router), 'navigate').mockResolvedValue(true);
  const fixture = TestBed.createComponent(NewSession);
  const el = fixture.nativeElement as HTMLElement;
  const type = async (text: string) => {
    const input = el.querySelector('input')!;
    input.value = text;
    input.dispatchEvent(new Event('input'));
    await new Promise((r) => setTimeout(r, 200));
    await fixture.whenStable();
  };
  const button = (text: string) =>
    [...el.querySelectorAll('button')].find((b) => b.textContent?.includes(text));
  return { request, navigate, fixture, type, button };
}

describe('NewSession', () => {
  it('offers to create a missing directory, then starts in it', async () => {
    const { request, type, button, fixture } = setup();
    await type('~/sr');
    expect(button('Start')!.disabled).toBe(true);
    button('Create folder')!.click();
    await fixture.whenStable();
    expect(request).toHaveBeenCalledWith('_rho/mkdir', { path: '~/sr' });
    await new Promise((r) => setTimeout(r, 200));
    await fixture.whenStable();
    expect(button('Create folder')).toBeUndefined();
    expect(button('Start')!.disabled).toBe(false);
  });

  it('starts a session in the canonical directory', async () => {
    const { request, navigate, type, button, fixture } = setup();
    await type('~/src');
    expect(button('Create folder')).toBeUndefined();
    button('Start')!.click();
    await fixture.whenStable();
    expect(request).toHaveBeenCalledWith('session/new', { cwd: '/home/me/src', mcpServers: [] });
    expect(navigate).toHaveBeenCalledWith(['/session', 's1'], {
      queryParams: { cwd: '/home/me/src' },
    });
  });
});
