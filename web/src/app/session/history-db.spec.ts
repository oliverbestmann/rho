import type * as acp from '@agentclientprotocol/sdk';
import { Cursor, merge, Stored } from './history-db';

const text = (t: string): acp.SessionUpdate => ({
  sessionUpdate: 'agent_message_chunk',
  content: { type: 'text', text: t },
});
const at = (entry: number, len: number): Cursor => ({ transcript: 't', entry, len });
const textOf = (s: Stored | undefined) =>
  s && s.update.sessionUpdate === 'agent_message_chunk' && s.update.content.type === 'text'
    ? s.update.content.text
    : undefined;

describe('merge', () => {
  it('appends chunks that follow what is kept, counting bytes', () => {
    const first = merge('k', undefined, text('hé'), at(0, 3));
    expect(textOf(first)).toBe('hé');
    const next = merge('k', first, text('llo'), at(0, 6));
    expect(textOf(next)).toBe('héllo');
    expect(next?.len).toBe(6);
  });

  it('skips what another tab wrote already, and gaps', () => {
    const kept = merge('k', undefined, text('hello'), at(0, 5));
    expect(merge('k', kept, text('lo'), at(0, 5))).toBeUndefined();
    expect(merge('k', kept, text('!'), at(0, 9))).toBeUndefined();
    // The rest of an entry without its start.
    expect(merge('k', undefined, text('lo'), at(0, 5))).toBeUndefined();
  });

  it('merges terminal output and keeps other updates whole', () => {
    const out = (data: string): acp.SessionUpdate => ({
      sessionUpdate: 'tool_call_update',
      toolCallId: 'c',
      _meta: { terminal_output: { terminal_id: 'c', data } },
    });
    const kept = merge('k', merge('k', undefined, out('a'), at(1, 1)), out('b'), at(1, 2));
    expect(kept?.update._meta?.['terminal_output']).toEqual({ terminal_id: 'c', data: 'ab' });

    const note: acp.SessionUpdate = { ...text('note'), _meta: { rho: { note: true } } };
    expect(merge('k', undefined, note, at(2, 0))?.update).toEqual(note);
  });
});
