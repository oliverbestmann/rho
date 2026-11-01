import * as acp from '@agentclientprotocol/sdk';
import { rhoRunning } from '../session/session-state';

/** All sessions, most recently updated first. */
export async function listSessions(agent: acp.ClientContext) {
  const all: acp.SessionInfo[] = [];
  let cursor: string | undefined;
  do {
    const page = await agent.request(acp.methods.agent.session.list, { cursor });
    all.push(...page.sessions);
    cursor = page.nextCursor ?? undefined;
  } while (cursor);
  return all.sort((a, b) => (b.updatedAt ?? '').localeCompare(a.updatedAt ?? ''));
}

/** A turn runs in the session. */
export function isRunning(s: acp.SessionInfo) {
  return rhoRunning(s._meta) === true;
}

/** A hue (0-359) from the working directory, so sessions of one directory share a color. */
export function hue(cwd: string) {
  let h = 0;
  for (const c of cwd) h = (Math.imul(h, 31) + c.charCodeAt(0)) | 0;
  return (h >>> 0) % 360;
}

/** The last part of a path, for showing a session's working directory. */
export function dir(cwd: string) {
  return cwd.split('/').filter(Boolean).at(-1) ?? cwd;
}
