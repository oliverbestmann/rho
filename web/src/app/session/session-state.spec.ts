import { blockText, SessionState } from './session-state';

describe('SessionState', () => {
  it('merges chunks of the same kind and tool call updates', () => {
    const s = new SessionState();
    s.apply({ sessionUpdate: 'user_message_chunk', content: { type: 'text', text: 'hi' } });
    s.apply({ sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: 'Hel' } });
    s.apply({ sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: 'lo' } });
    s.apply({
      sessionUpdate: 'tool_call',
      toolCallId: 't1',
      title: 'Read a',
      status: 'in_progress',
      kind: 'read',
    });
    s.apply({
      sessionUpdate: 'tool_call_update',
      toolCallId: 't1',
      status: 'completed',
      title: null,
    });
    s.apply({ sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: 'done' } });

    const e = s.entries();
    expect(e.map((x) => x.kind)).toEqual(['user', 'agent', 'tool', 'agent']);
    expect(e[1].kind === 'agent' && e[1].text()).toBe('Hello');
    const call = e[2].kind === 'tool' ? e[2].call() : undefined;
    expect(call).toMatchObject({ title: 'Read a', status: 'completed', kind: 'read' });
  });

  it('keeps a resent tool call in place', () => {
    const s = new SessionState();
    s.apply({ sessionUpdate: 'tool_call', toolCallId: 't1', title: 'Bash', status: 'in_progress' });
    s.apply({ sessionUpdate: 'tool_call', toolCallId: 't1', title: 'Bash', status: 'in_progress' });
    expect(s.entries().length).toBe(1);
  });

  it('follows rho running in session info meta', () => {
    const s = new SessionState();
    s.apply({ sessionUpdate: 'session_info_update', _meta: { rho: { running: true } } });
    expect(s.running()).toBe(true);
    s.apply({ sessionUpdate: 'session_info_update', title: 'T' });
    expect(s.running()).toBe(true);
    s.apply({ sessionUpdate: 'session_info_update', _meta: { rho: { running: false } } });
    expect(s.running()).toBe(false);
  });

  it('renders an image block as a markdown data URL', () => {
    const md = blockText({ type: 'image', mimeType: 'image/png', data: 'AAAA' });
    expect(md.trim()).toBe('![image](data:image/png;base64,AAAA)');
  });
});
