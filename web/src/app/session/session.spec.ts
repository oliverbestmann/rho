import { signal } from '@angular/core';
import { Opened } from '../acp';
import { keepShown } from './session';
import { SessionState } from './session-state';

function opened(loading: boolean): Opened {
  return { state: new SessionState(), loading: signal(loading), error: signal(undefined) };
}

describe('keepShown', () => {
  it('keeps the old history while offline or reloading', () => {
    const old = opened(false);
    expect(keepShown(undefined, old)).toBe(old);
    expect(keepShown(opened(true), old)).toBe(old);
  });

  it('switches to the open session once loaded or without history', () => {
    const old = opened(false);
    const fresh = opened(false);
    expect(keepShown(fresh, old)).toBe(fresh);
    const loading = opened(true);
    expect(keepShown(loading, undefined)).toBe(loading);
    expect(keepShown(loading, opened(true))).toBe(loading);
  });
});
