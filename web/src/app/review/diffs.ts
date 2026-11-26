import { Injectable } from '@angular/core';
import { DiffFile, parseDiff } from './diff';

/** Review diffs by share link, fetched and parsed once (ADR 0076). */
@Injectable({ providedIn: 'root' })
export class Diffs {
  private readonly cache = new Map<string, Promise<DiffFile[]>>();

  get(link: string): Promise<DiffFile[]> {
    let files = this.cache.get(link);
    if (!files) {
      files = fetch(link).then(async (res) => {
        if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);
        return parseDiff(await res.text());
      });
      // A failed fetch is tried again the next time.
      files.catch(() => this.cache.delete(link));
      this.cache.set(link, files);
    }
    return files;
  }
}

/** The share link to a `PresentDiff` call's diff file. */
export function diffLink(call: { toolCallId?: string; content?: unknown[] | null } | undefined) {
  for (const c of (call?.content ?? []) as {
    type: string;
    content?: { type: string; uri?: string };
  }[]) {
    const uri = c.type === 'content' && c.content?.type === 'resource_link' && c.content.uri;
    // The daemon ignores the query; it keeps a later call on the same file from
    // getting this one's cached diff.
    if (uri) return `${uri}?call=${encodeURIComponent(call?.toolCallId ?? '')}`;
  }
  return undefined;
}

/** Files, added and removed lines over all files. */
export function totals(files: DiffFile[]) {
  return files.reduce((t, f) => ({ adds: t.adds + f.adds, dels: t.dels + f.dels }), {
    adds: 0,
    dels: 0,
  });
}
