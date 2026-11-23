import hljs from 'highlight.js';
import { highlightFile, parseDiff, splitLines } from './diff';

const GIT = `diff --git a/src/a.rs b/src/a.rs
index 1..2 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,2 +1,2 @@ fn main
 keep
--- removed with dashes
+added
\\ No newline at end of file
diff --git a/new.txt b/new.txt
new file mode 100644
--- /dev/null
+++ b/new.txt
@@ -0,0 +1 @@
+hi
diff --git a/old.txt b/old.txt
deleted file mode 100644
--- a/old.txt
+++ /dev/null
@@ -1 +0,0 @@
-bye
diff --git a/x b/y
similarity index 100%
rename from x
rename to y
diff --git a/img.png b/img.png
Binary files a/img.png and b/img.png differ
`;

describe('parseDiff', () => {
  it('reads git diffs', () => {
    const files = parseDiff(GIT);
    expect(files.map((f) => [f.path, f.status, f.adds, f.dels, f.binary])).toEqual([
      ['src/a.rs', 'modified', 1, 1, false],
      ['new.txt', 'added', 1, 0, false],
      ['old.txt', 'deleted', 0, 1, false],
      ['y', 'renamed', 0, 0, false],
      ['img.png', 'modified', 0, 0, true],
    ]);
    expect(files[0].hunks[0].lines).toEqual([
      { kind: 'ctx', text: 'keep', old: 1, new: 1 },
      { kind: 'del', text: '-- removed with dashes', old: 2 },
      { kind: 'add', text: 'added', new: 2 },
    ]);
    expect(files[3].oldPath).toBe('x');
  });

  it('reads plain unified diffs with several files', () => {
    const files = parseDiff(
      '--- a.txt\t2024-01-01\n+++ a.txt\t2024-01-02\n@@ -1 +1 @@\n-a\n+b\n' +
        '--- b.txt\n+++ b.txt\n@@ -5,2 +5,2 @@\n x\n-c\n+d\n',
    );
    expect(files.map((f) => f.path)).toEqual(['a.txt', 'b.txt']);
    expect(files[1].hunks[0].lines[2]).toEqual({ kind: 'add', text: 'd', new: 6 });
  });
});

describe('splitLines', () => {
  it('closes and reopens spans at line breaks', () => {
    expect(splitLines('a<span class="c">/*\nx*/</span>b')).toEqual([
      'a<span class="c">/*</span>',
      '<span class="c">x*/</span>b',
    ]);
  });
});

describe('highlightFile', () => {
  it('highlights each side as one text', () => {
    const [file] = parseDiff('--- a/x.js\n+++ b/x.js\n@@ -1,2 +1,2 @@\n /* a\n-b */\n+c */\n');
    const [html] = highlightFile(hljs, file);
    expect(html.length).toBe(3);
    expect(html.every((h) => h.includes('hljs-comment'))).toBe(true);
  });

  it('escapes unknown languages', () => {
    const [file] = parseDiff('--- a/x.zzz\n+++ b/x.zzz\n@@ -1 +1 @@\n-<a>\n+&\n');
    expect(highlightFile(hljs, file)).toEqual([['&lt;a&gt;', '&amp;']]);
  });
});
