import type * as acp from '@agentclientprotocol/sdk';
import { rhoMeta } from './session-state';

/** `_meta.rho.cursor`: where an update ends in rho's transcript (ADR 0064, 0074). */
export interface Cursor {
  transcript: string;
  /** The entry, counted from the transcript's start. */
  entry: number;
  /** Bytes of a merged text, thinking or terminal output entry; 0 for others. */
  len: number;
  /** Where the previous epoch starts: rho no longer has the entries before it. */
  start?: number;
}

/** One entry of rho's transcript as kept: a merged entry's update has all of its text. */
export interface Stored {
  key: string;
  entry: number;
  len: number;
  update: acp.SessionUpdate;
}

interface Meta {
  key: string;
  transcript: string;
  cursor: Cursor;
}

/** What `HistoryDb.load` found. */
export interface Loaded {
  /** Undefined if nothing is kept: the entries are of no use without it. */
  cursor?: Cursor;
  entries: acp.SessionUpdate[];
  /** Older entries are kept than were read. */
  more: boolean;
}

export function cursorOf(n: acp.SessionNotification): Cursor | undefined {
  const c = rhoMeta(n._meta)?.['cursor'] as Cursor | undefined;
  return typeof c?.transcript === 'string' && typeof c.entry === 'number' ? c : undefined;
}

const bytes = (s: string) => new TextEncoder().encode(s).length;

/** The text of an update that rho merges into one entry, with a setter for the whole text. */
function mergeable(
  u: acp.SessionUpdate,
): [string, (text: string) => acp.SessionUpdate] | undefined {
  switch (u.sessionUpdate) {
    case 'agent_message_chunk':
    case 'agent_thought_chunk':
      if (u.content.type !== 'text' || rhoMeta(u._meta)?.['note'] === true) return undefined;
      return [u.content.text, (text) => ({ ...u, content: { ...u.content, text } })];
    case 'tool_call_update': {
      const out = u._meta?.['terminal_output'] as { data?: unknown } | undefined;
      if (typeof out?.data !== 'string') return undefined;
      return [
        out.data,
        (data) => ({ ...u, _meta: { ...u._meta, terminal_output: { ...out, data } } }),
      ];
    }
    default:
      return undefined;
  }
}

/**
 * The entry `stored` becomes with `update`, which ends at `cursor`;
 * undefined to leave it. A merged entry grows only by the bytes that follow
 * what it has, so tabs that write the same entry do not double it.
 */
export function merge(
  key: string,
  stored: Stored | undefined,
  update: acp.SessionUpdate,
  cursor: Cursor,
): Stored | undefined {
  const text = mergeable(update);
  const entry = { key, entry: cursor.entry, len: cursor.len };
  if (!text) return { ...entry, update };
  const [chunk] = text;
  if (stored && stored.len >= cursor.len) return undefined;
  if (!stored) return bytes(chunk) === cursor.len ? { ...entry, update } : undefined;
  const [old, set] = mergeable(stored.update) ?? [];
  if (old === undefined || !set || stored.len + bytes(chunk) !== cursor.len) return undefined;
  return { ...entry, update: set(old + chunk) };
}

function done<T>(r: IDBRequest<T>) {
  return new Promise<T>((resolve, reject) => {
    r.onsuccess = () => resolve(r.result);
    r.onerror = () => reject(r.error);
  });
}

function committed(t: IDBTransaction) {
  return new Promise<void>((resolve, reject) => {
    t.oncomplete = () => resolve();
    t.onerror = t.onabort = () => reject(t.error);
  });
}

/** All entries of a session. */
const all = (key: string) => IDBKeyRange.bound([key, 0], [key, Infinity]);

/**
 * The transcripts of the sessions this browser showed, in IndexedDB, keyed
 * by server and session (ADR 0074). Tabs write the same entries; each
 * write is one transaction, so they do not interleave. Without IndexedDB
 * it keeps nothing.
 */
export class HistoryDb {
  private db?: Promise<IDBDatabase | undefined>;
  /** Per session, so its writes and reads keep their order. */
  private readonly chains = new Map<string, Promise<unknown>>();

  private open() {
    this.db ??= new Promise((resolve) => {
      if (typeof indexedDB === 'undefined') return resolve(undefined);
      const r = indexedDB.open('rho-history', 1);
      r.onupgradeneeded = () => {
        r.result.createObjectStore('entries', { keyPath: ['key', 'entry'] });
        r.result.createObjectStore('sessions', { keyPath: 'key' });
      };
      r.onsuccess = () => resolve(r.result);
      r.onerror = () => resolve(undefined);
    });
    return this.db;
  }

  /** Runs `f` after the session's earlier calls; undefined without IndexedDB or on errors. */
  private queue<T>(key: string, f: (db: IDBDatabase) => Promise<T>): Promise<T | undefined> {
    const next = (this.chains.get(key) ?? Promise.resolve())
      .then(() => this.open())
      .then((db) => (db ? f(db) : undefined))
      .catch((e: unknown) => {
        console.warn('rho history', e);
        return undefined;
      });
    this.chains.set(key, next);
    return next;
  }

  /** Keeps an update rho sent with a cursor. */
  record(key: string, update: acp.SessionUpdate, cursor: Cursor) {
    void this.queue(key, async (db) => {
      const t = db.transaction(['entries', 'sessions'], 'readwrite');
      const entries = t.objectStore('entries');
      const sessions = t.objectStore('sessions');
      const meta: Meta | undefined = await done(sessions.get(key));
      const same = meta?.transcript === cursor.transcript;
      // Another transcript: rho sent everything again.
      if (meta && !same) await done(entries.delete(all(key)));
      if (cursor.start) {
        await done(entries.delete(IDBKeyRange.bound([key, 0], [key, cursor.start], false, true)));
      }
      const stored: Stored | undefined = await done(entries.get([key, cursor.entry]));
      const next = merge(key, stored, update, cursor);
      if (next) entries.put(next);
      const later =
        !same ||
        cursor.entry > meta.cursor.entry ||
        (cursor.entry === meta.cursor.entry && cursor.len > meta.cursor.len);
      if (later) sessions.put({ key, transcript: cursor.transcript, cursor } satisfies Meta);
      await committed(t);
    });
  }

  /** rho sends everything again: what is kept goes. */
  clear(key: string) {
    void this.queue(key, async (db) => {
      const t = db.transaction(['entries', 'sessions'], 'readwrite');
      t.objectStore('entries').delete(all(key));
      t.objectStore('sessions').delete(key);
      await committed(t);
    });
  }

  /** The cursor and the last `limit` entries, after the writes so far. */
  async load(key: string, limit: number): Promise<Loaded> {
    const loaded = await this.queue(key, async (db) => {
      const t = db.transaction(['entries', 'sessions'], 'readonly');
      const meta: Meta | undefined = await done(t.objectStore('sessions').get(key));
      if (!meta) return undefined;
      const found: Stored[] = [];
      await new Promise<void>((resolve, reject) => {
        const r = t.objectStore('entries').openCursor(all(key), 'prev');
        r.onerror = () => reject(r.error);
        r.onsuccess = () => {
          const c = r.result;
          // One more than asked tells whether there are more.
          if (!c || found.length > limit) return resolve();
          found.push(c.value as Stored);
          c.continue();
        };
      });
      const more = found.length > limit;
      const entries = found
        .slice(0, limit)
        .reverse()
        .map((s) => s.update);
      return { cursor: meta.cursor, entries, more };
    });
    return loaded ?? { entries: [], more: false };
  }
}
