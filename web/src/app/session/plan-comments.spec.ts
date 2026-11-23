import { planBlocks, planMessage } from './plan-comments';

describe('planBlocks', () => {
  it('splits top-level lists into their items', () => {
    const blocks = planBlocks('Intro\n\n- a\n  - nested\n- b\n\n```sh\nls\n```\n');
    expect(blocks).toEqual([
      { raw: 'Intro' },
      { raw: '- a\n  - nested', item: { first: true, last: false } },
      { raw: '- b', item: { first: false, last: true } },
      { raw: '```sh\nls\n```' },
    ]);
  });
});

describe('planMessage', () => {
  it('quotes each commented block in plan order', () => {
    const blocks = planBlocks('A\n\nB\nC\n\nD');
    expect(planMessage(blocks, { 2: 'last', 0: ' first ', 1: '  ' })).toBe(
      'Comments on the plan:\n\n> A\n\nfirst\n\n> D\n\nlast',
    );
  });
});
