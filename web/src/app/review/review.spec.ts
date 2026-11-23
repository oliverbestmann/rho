import { parseDiff } from './diff';
import { reviewMessage } from './review';

describe('reviewMessage', () => {
  it('lists line comments in diff order, the overall comment last', () => {
    const files = parseDiff('--- a/x.rs\n+++ b/x.rs\n@@ -1,2 +1,2 @@\n-old `a`\n+new\n ctx\n');
    const text = reviewMessage(files, { '0:0:1': 'why?', overall: 'Looks fine.', '0:0:0': 'gone' });
    expect(text).toBe(
      'Review comments:\n\n' +
        '**x.rs (old line 1)**\n```diff\n-old `a`\n```\n\ngone\n\n' +
        '**x.rs:1**\n```diff\n+new\n```\n\nwhy?\n\n' +
        'Looks fine.',
    );
  });
});
