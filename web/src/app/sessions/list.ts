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

/** The last part of a path, for showing a session's working directory. */
export function dir(cwd: string) {
  return cwd.split('/').filter(Boolean).at(-1) ?? cwd;
}
