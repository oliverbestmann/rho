import * as acp from '@agentclientprotocol/sdk';
import { hasBody, toolBody, toolIcon } from './tool-body';

const text = (t: string) => ({
  type: 'content' as const,
  content: { type: 'text' as const, text: t },
});

describe('toolIcon', () => {
  const call = (kind: acp.ToolKind, extra: Partial<acp.ToolCall> = {}): acp.ToolCall => ({
    toolCallId: 't',
    title: 'x',
    kind,
    ...extra,
  });

  it('shows the file type for reads and edits', () => {
    expect(toolIcon(call('read', { locations: [{ path: '/w/README.md' }] }))).toBe('markdown');
    expect(toolIcon(call('edit', { locations: [{ path: '/w/main.RS' }] }))).toBe('code');
    expect(toolIcon(call('edit', { locations: [{ path: '/w/Makefile' }] }))).toBe('build');
    expect(toolIcon(call('read', { locations: [{ path: '/w/Dockerfile' }] }))).toBe(
      'deployed_code',
    );
    expect(toolIcon(call('read', { locations: [{ path: '/w/.gitignore' }] }))).toBe('commit');
    expect(toolIcon(call('edit', { locations: [{ path: '/w/LICENSE' }] }))).toBe('edit');
    expect(toolIcon(call('read'))).toBe('description');
  });

  it('shows a commit icon for git commit and push', () => {
    expect(
      toolIcon(call('execute', { rawInput: { command: 'git add . && git commit -m x' } })),
    ).toBe('commit');
    expect(toolIcon(call('execute', { rawInput: { command: 'git push' } }))).toBe('commit');
    expect(toolIcon(call('execute', { rawInput: { command: 'git status' } }))).toBe('terminal');
  });
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

  it('fences a read file with its extension', () => {
    const read = (path: string, status: acp.ToolCallStatus = 'completed') =>
      toolBody({
        toolCallId: 't',
        title: 'Read',
        kind: 'read',
        status,
        locations: [{ path }],
        content: [text('# x')],
      });
    expect(read('/w/src/main.RS')).toBe('```rs\n# x\n```');
    expect(read('/w/Makefile')).toBe('```makefile\n# x\n```');
    expect(read('/w/Dockerfile')).toBe('```dockerfile\n# x\n```');
    expect(read('/w/.env')).toBe('```\n# x\n```');
    expect(read('/w/a.rs', 'failed')).toBe('# x');
  });
});
