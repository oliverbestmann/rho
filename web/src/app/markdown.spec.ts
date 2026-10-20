import { SecurityContext } from '@angular/core';
import { TestBed } from '@angular/core/testing';
import { DomSanitizer } from '@angular/platform-browser';
import { MarkdownPipe } from './markdown';

describe('MarkdownPipe', () => {
  const md = new MarkdownPipe();

  it('shows raw HTML as text', () => {
    expect(md.transform('<file uri="a">x</file>')).toContain('&lt;file uri=&quot;a&quot;&gt;');
    expect(md.transform('a <textarea> b')).toContain('a &lt;textarea&gt; b');
  });

  it('still renders markdown', () => {
    expect(md.transform('**b** `<i>`')).toBe('<p><strong>b</strong> <code>&lt;i&gt;</code></p>\n');
  });

  it('opens links in a new tab', () => {
    expect(md.transform('[a](https://x.org) <https://y.org>')).toBe(
      '<p><a target="_blank" rel="noopener noreferrer" href="https://x.org">a</a> ' +
        '<a target="_blank" rel="noopener noreferrer" href="https://y.org">https://y.org</a></p>\n',
    );
  });

  it('marks diff lines', () => {
    expect(md.transform('```diff\n@@ -1 +1 @@\n-<a>\n+b\n c\n```')).toBe(
      '<pre><code class="language-diff"><span class="hunk">@@ -1 +1 @@</span>' +
        '<span class="del">-&lt;a&gt;</span><span class="add">+b</span><span class="ctx"> c</span>' +
        '</code></pre>\n',
    );
  });

  it('gives a fence its language only once closed', () => {
    expect(md.transform('```rs\nlet <a>')).toBe('<pre><code>let &lt;a&gt;</code></pre>\n');
    expect(md.transform('```rs\nlet a\n```')).toContain('<code class="language-rs">');
    expect(md.transform('text\n\n~~~~rs\nlet a\n~~~~\n')).toContain('class="language-rs"');
  });

  it('produces nothing the sanitizer strips', () => {
    const warn = vi.spyOn(console, 'warn');
    const text = [
      '# Title',
      'Some **bold**, `code`, a [link](https://example.com) and <b>raw</b>.',
      '',
      '- [x] done',
      '- [ ] open',
      '',
      '| a | b |',
      '|---|:-:|',
      '| 1 | 2 |',
      '',
      '```diff',
      '-old',
      '+new',
      '```',
      '',
      '![image](data:image/png;base64,AAAA)',
      '',
      '<file uri="file:///x">content</file>',
    ].join('\n');
    TestBed.inject(DomSanitizer).sanitize(SecurityContext.HTML, md.transform(text));
    expect(warn).not.toHaveBeenCalled();
  });
});
