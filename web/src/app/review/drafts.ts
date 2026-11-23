import { effect, linkedSignal } from '@angular/core';

/** Comment text by what it is about. */
export type Comments = Record<string, string>;

/**
 * Comments kept in localStorage under `key()`, so a reload on the phone
 * does not lose them. Call in an injection context.
 */
export function drafts(key: () => string) {
  const comments = linkedSignal<Comments>(() => load(key()));
  effect(() => {
    const [k, c] = [key(), comments()];
    if (Object.values(c).some((t) => t.trim())) localStorage.setItem(k, JSON.stringify(c));
    else localStorage.removeItem(k);
  });
  return comments;
}

function load(key: string): Comments {
  try {
    return JSON.parse(localStorage.getItem(key) ?? '{}') as Comments;
  } catch {
    return {};
  }
}

/** `comments` without the empty ones. */
export function written(comments: Comments) {
  return Object.entries(comments).filter(([, t]) => t.trim());
}
