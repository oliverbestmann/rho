import type { HLJSApi } from 'highlight.js';

export interface DiffLine {
  kind: 'add' | 'del' | 'ctx';
  text: string;
  /** Line numbers in the old and the new file; only the sides the line is on. */
  old?: number;
  new?: number;
}

export interface Hunk {
  header: string;
  lines: DiffLine[];
}

export interface DiffFile {
  path: string;
  oldPath: string;
  status: 'modified' | 'added' | 'deleted' | 'renamed';
  binary: boolean;
  hunks: Hunk[];
  adds: number;
  dels: number;
}

const NULL = '/dev/null';

/** The files of a unified diff, from `git diff` or plain `diff -u`. */
export function parseDiff(text: string): DiffFile[] {
  const files: DiffFile[] = [];
  let file: DiffFile | undefined;
  let hunk: Hunk | undefined;
  // Lines left in the hunk per side: a removed `--- x` line is not a header.
  let oldLeft = 0;
  let newLeft = 0;
  let oldNo = 0;
  let newNo = 0;
  const start = () => {
    file = {
      path: '',
      oldPath: '',
      status: 'modified',
      binary: false,
      hunks: [],
      adds: 0,
      dels: 0,
    };
    files.push(file);
    hunk = undefined;
    return file;
  };
  for (const line of text.split('\n')) {
    if (hunk && file && (oldLeft > 0 || newLeft > 0)) {
      const c = line[0];
      const body = line.slice(1);
      if (c === '+') {
        hunk.lines.push({ kind: 'add', text: body, new: newNo++ });
        newLeft--;
        file.adds++;
      } else if (c === '-') {
        hunk.lines.push({ kind: 'del', text: body, old: oldNo++ });
        oldLeft--;
        file.dels++;
      } else if (c === '\\') {
        // "\ No newline at end of file"
      } else {
        // Some tools strip the space of an empty context line.
        hunk.lines.push({ kind: 'ctx', text: body, old: oldNo++, new: newNo++ });
        oldLeft--;
        newLeft--;
      }
      continue;
    }
    if (line.startsWith('diff --git ')) {
      const f = start();
      const m = /^diff --git a\/(.*) b\/(.*)$/.exec(line);
      if (m) [f.oldPath, f.path] = [m[1], m[2]];
    } else if (line.startsWith('--- ')) {
      // Without `diff --git`, each file starts here.
      const f = file && !file.hunks.length && !file.binary ? file : start();
      f.oldPath = path(line.slice(4), 'a/');
      if (f.oldPath === NULL) f.status = 'added';
    } else if (line.startsWith('+++ ') && file) {
      file.path = path(line.slice(4), 'b/');
      if (file.path === NULL) {
        file.status = 'deleted';
        file.path = file.oldPath;
      }
    } else if (line.startsWith('@@') && file) {
      const m = /^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@/.exec(line);
      if (!m) continue;
      oldNo = +m[1];
      oldLeft = m[2] === undefined ? 1 : +m[2];
      newNo = +m[3];
      newLeft = m[4] === undefined ? 1 : +m[4];
      hunk = { header: line, lines: [] };
      file.hunks.push(hunk);
    } else if (line.startsWith('new file mode') && file) {
      file.status = 'added';
    } else if (line.startsWith('deleted file mode') && file) {
      file.status = 'deleted';
    } else if (line.startsWith('rename from ') && file) {
      file.oldPath = line.slice('rename from '.length);
    } else if (line.startsWith('rename to ') && file) {
      file.path = line.slice('rename to '.length);
    } else if ((line.startsWith('Binary files ') || line === 'GIT binary patch') && file) {
      file.binary = true;
    }
  }
  for (const f of files) {
    if (!f.path) f.path = f.oldPath;
    if (f.status === 'modified' && f.oldPath && f.oldPath !== f.path) f.status = 'renamed';
  }
  return files;
}

/** A path from a `---`/`+++` line, without its `a/` or `b/` and a trailing date. */
function path(raw: string, prefix: string) {
  const p = raw.split('\t')[0];
  return p.startsWith(prefix) ? p.slice(prefix.length) : p;
}

/** The highlight.js language for a file, by extension or name; undefined if unknown. */
export function language(hljs: HLJSApi, path: string) {
  const name = path.split('/').pop()!.toLowerCase();
  const ext = name.includes('.') ? name.split('.').pop()! : name;
  return [ext, name].find((l) => hljs.getLanguage(l));
}

/**
 * The HTML of every line, by hunk. Each hunk's old and new side are highlighted
 * as one text, so strings and comments over several lines come out right.
 */
export function highlightFile(hljs: HLJSApi | undefined, file: DiffFile): string[][] {
  const lang = hljs && language(hljs, file.path);
  const html = (lines: DiffLine[]) => {
    const text = lines.map((l) => l.text).join('\n');
    return lang
      ? splitLines(hljs!.highlight(text, { language: lang, ignoreIllegals: true }).value)
      : lines.map((l) => escape(l.text));
  };
  return file.hunks.map((h) => {
    const [oldHtml, newHtml] = [
      html(h.lines.filter((l) => l.kind !== 'add')),
      html(h.lines.filter((l) => l.kind !== 'del')),
    ];
    let [o, n] = [0, 0];
    // Context lines take the new side's HTML; it is the same text.
    return h.lines.map((l) =>
      l.kind === 'del' ? oldHtml[o++] : (l.kind === 'ctx' && o++, newHtml[n++]),
    );
  });
}

/**
 * highlight.js HTML split into lines; spans open at a line break are closed
 * at its end and opened again on the next line.
 */
export function splitLines(html: string): string[] {
  const out: string[] = [];
  const open: string[] = [];
  let line = '';
  for (const part of html.split(/(<span[^>]*>|<\/span>|\n)/)) {
    if (part === '\n') {
      out.push(line + '</span>'.repeat(open.length));
      line = open.join('');
    } else if (part.startsWith('<span')) {
      open.push(part);
      line += part;
    } else if (part === '</span>') {
      open.pop();
      line += part;
    } else line += part;
  }
  out.push(line + '</span>'.repeat(open.length));
  return out;
}

const ESCAPES: Record<string, string> = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' };
const escape = (text: string) => text.replace(/[&<>"]/g, (c) => ESCAPES[c]);
