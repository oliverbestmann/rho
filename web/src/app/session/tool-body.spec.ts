import * as acp from '@agentclientprotocol/sdk';
import { hasBody, toolBody } from './session';

const text = (t: string) => ({
  type: 'content' as const,
  content: { type: 'text' as const, text: t },
});

describe('toolBody', () => {
  it('is empty for a notice without text, so the card does not expand', () => {
    expect(toolBody({ toolCallId: 'n', title: 'Note', content: [] })).toBe('');
    expect(toolBody({ toolCallId: 'n', title: 'Note' })).toBe('');
    expect(hasBody({ toolCallId: 'n', title: 'Note', content: [text('')] })).toBe(false);
  });

  it('shows an edit as diff hunks with context', () => {
    const old = 'a\nb\nc\nd\ne\nf\ng\nh\n';
    const call: acp.ToolCall = {
      toolCallId: 'e',
      title: 'Edit',
      content: [{ type: 'diff', path: '/w/f', oldText: old, newText: old.replace('h', 'H') }],
    };
    expect(hasBody(call)).toBe(true);
    expect(toolBody(call)).toBe('```diff\n@@ -5,4 +5,4 @@\n e\n f\n g\n-h\n+H\n```');
  });

  it('fences shell output after the command block', () => {
    const body = toolBody({
      toolCallId: 't',
      title: 'Bash ls',
      kind: 'execute',
      content: [text('```sh\nls\n```'), text('a ``` b\n# not a heading')],
    });
    expect(body).toBe('```sh\nls\n```\n\n````\na ``` b\n# not a heading\n````');
  });

  it('keeps other tools as markdown', () => {
    const body = toolBody({ toolCallId: 't', title: 'Read', kind: 'read', content: [text('# x')] });
    expect(body).toBe('# x');
  });
});
