import { Pipe, PipeTransform } from '@angular/core';
import { Marked } from 'marked';

const ESCAPES: Record<string, string> = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' };
const escape = (text: string) => text.replace(/[&<>"]/g, (c) => ESCAPES[c]);
const DIFF_LINES: Record<string, string> = { '+': 'add', '-': 'del', '@': 'hunk' };

const marked = new Marked({
  gfm: true,
  // Raw HTML (like the `<file uri=...>` blocks of prompts) is shown as text, not rendered.
  renderer: {
    html: ({ text }) => escape(text),
    // Diff lines as blocks with a class each, for added/removed colours.
    code({ text, lang }) {
      if (lang !== 'diff') return false;
      const lines = text
        .split('\n')
        .map((l) => `<span class="${DIFF_LINES[l[0]] ?? 'ctx'}">${escape(l)}</span>`);
      return `<pre><code class="language-diff">${lines.join('')}</code></pre>\n`;
    },
    // Angular strips `<input>`, so task list boxes are characters.
    checkbox: ({ checked }) => (checked ? '☑ ' : '☐ '),
  },
});

/** Markdown to HTML; Angular's [innerHTML] sanitizer strips anything unsafe. */
@Pipe({ name: 'markdown' })
export class MarkdownPipe implements PipeTransform {
  transform(text: string): string {
    return marked.parse(text, { async: false });
  }
}
