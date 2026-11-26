import { TestBed } from '@angular/core/testing';
import type * as acp from '@agentclientprotocol/sdk';
import { Ask } from '../acp';
import { provideRouter } from '@angular/router';
import { parseDiff } from '../review/diff';
import { Diffs } from '../review/diffs';
import { Acp } from '../acp';
import { AskCard } from './ask-card';

async function render(ask: Ask, call?: acp.ToolCall) {
  const fixture = TestBed.createComponent(AskCard);
  fixture.componentRef.setInput('ask', ask);
  fixture.componentRef.setInput('call', call);
  await fixture.whenStable();
  return { el: fixture.nativeElement as HTMLElement, stable: () => fixture.whenStable() };
}

const buttons = (el: HTMLElement) => [...el.querySelectorAll('button')];

describe('AskCard', () => {
  it('answers a permission request with the chosen option', async () => {
    const answer = vi.fn();
    const { el } = await render({
      kind: 'permission',
      id: 1,
      sessionId: 's',
      answer,
      request: {
        sessionId: 's',
        toolCall: {
          toolCallId: 't',
          title: 'Run ls',
          content: [{ type: 'content', content: { type: 'text', text: '`ls -la`' } }],
        },
        options: [
          { optionId: 'yes', name: 'Allow', kind: 'allow_once' },
          { optionId: 'no', name: 'Reject', kind: 'reject_once' },
        ],
      },
    });
    expect(el.textContent).toContain('Run ls');
    expect(el.querySelector('code')?.textContent).toBe('ls -la');
    buttons(el)
      .find((b) => b.textContent?.includes('Reject'))!
      .click();
    expect(answer).toHaveBeenCalledWith({ outcome: { outcome: 'selected', optionId: 'no' } });
  });

  it('shows the asked tool call when the request has no content', async () => {
    const { el } = await render(
      {
        kind: 'permission',
        id: 1,
        sessionId: 's',
        answer: vi.fn(),
        request: {
          sessionId: 's',
          toolCall: { toolCallId: 't', title: 'Commit the fix' },
          options: [{ optionId: 'yes', name: 'Allow', kind: 'allow_once' }],
        },
      },
      {
        toolCallId: 't',
        title: 'Commit the fix',
        kind: 'execute',
        content: [{ type: 'content', content: { type: 'text', text: '`git commit`' } }],
      },
    );
    expect(el.textContent).toContain('Commit the fix');
    expect(el.querySelector('code')?.textContent).toBe('git commit');
  });

  it('turns plan comments into Request changes', async () => {
    localStorage.clear();
    const answer = vi.fn();
    const { el, stable } = await render(
      {
        kind: 'permission',
        id: 1,
        sessionId: 's',
        answer,
        request: {
          sessionId: 's',
          toolCall: { toolCallId: 'p', title: 'Plan' },
          options: [
            { optionId: 'allow', name: 'Approve', kind: 'allow_once' },
            { optionId: 'reject', name: 'Skip', kind: 'reject_once' },
          ],
        },
      },
      {
        toolCallId: 'p',
        title: 'Plan',
        rawInput: { plan: '# Plan\n\n1. one\n2. two' },
        _meta: { rho: { tool: 'PresentPlan' } },
      },
    );
    const blocks = [...el.querySelectorAll<HTMLElement>('.block')];
    expect(blocks.map((b) => b.textContent?.trim())).toEqual(['Plan', 'one', 'two']);
    expect(blocks[2].querySelector('ol')?.getAttribute('start')).toBe('2');
    blocks[2].click();
    await stable();
    const area = el.querySelector('textarea')!;
    expect(document.activeElement).toBe(area);
    area.value = 'why?';
    area.dispatchEvent(new Event('input'));
    area.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', ctrlKey: true }));
    await stable();
    expect(el.querySelector('textarea')).toBeNull();
    expect(el.querySelector('.comment-text')?.textContent).toBe('why?');
    const request = buttons(el).find((b) => b.textContent?.includes('Request changes (1)'))!;
    request.click();
    expect(answer).toHaveBeenCalledWith({ outcome: { outcome: 'selected', optionId: 'reject' } });
  });

  it('steers plan comments in before an approval', async () => {
    localStorage.clear();
    const steer = vi.fn(() => Promise.resolve());
    TestBed.configureTestingModule({ providers: [{ provide: Acp, useValue: { steer } }] });
    const answer = vi.fn();
    const { el, stable } = await render(
      {
        kind: 'permission',
        id: 1,
        sessionId: 's',
        answer,
        request: {
          sessionId: 's',
          toolCall: { toolCallId: 'p', title: 'Plan' },
          options: [
            { optionId: 'allow', name: 'Approve', kind: 'allow_once' },
            { optionId: 'reject', name: 'Skip', kind: 'reject_once' },
          ],
        },
      },
      {
        toolCallId: 'p',
        title: 'Plan',
        rawInput: { plan: 'Do it.' },
        _meta: { rho: { tool: 'PresentPlan' } },
      },
    );
    el.querySelector<HTMLElement>('.block')!.click();
    await stable();
    const area = el.querySelector('textarea')!;
    area.value = 'carefully';
    area.dispatchEvent(new Event('input'));
    await stable();
    buttons(el)
      .find((b) => b.textContent?.includes('Approve with comments'))!
      .click();
    await stable();
    expect(steer).toHaveBeenCalledWith(
      's',
      expect.stringMatching(/approved[\s\S]*> Do it\.\n\ncarefully$/),
    );
    expect(answer).toHaveBeenCalledWith({ outcome: { outcome: 'selected', optionId: 'allow' } });
  });

  it('summarizes a pending review with one link to it', async () => {
    let fetched = '';
    TestBed.configureTestingModule({
      providers: [
        provideRouter([]),
        {
          provide: Diffs,
          useValue: {
            get: (link: string) =>
              (fetched = link) &&
              Promise.resolve(parseDiff('--- a/x.rs\n+++ b/x.rs\n@@ -1 +1,2 @@\n-a\n+b\n+c\n')),
          },
        },
      ],
    });
    const { el, stable } = await render(
      {
        kind: 'permission',
        id: 1,
        sessionId: 's',
        answer: vi.fn(),
        request: {
          sessionId: 's',
          toolCall: { toolCallId: 'r', title: 'Approve these changes?' },
          options: [
            { optionId: 'allow', name: 'Approve', kind: 'allow_once' },
            { optionId: 'reject', name: 'Request changes', kind: 'reject_once' },
          ],
        },
      },
      {
        toolCallId: 'r',
        title: 'Review the fix',
        content: [
          {
            type: 'content',
            content: { type: 'resource_link', name: 'x.patch', uri: '/share/x.patch' },
          },
        ],
        _meta: { rho: { tool: 'PresentDiff' } },
      },
    );
    await stable();
    expect(el.querySelector('mat-card-title')?.textContent).toContain('Review the fix');
    expect(el.querySelector('.summary')?.textContent).toMatch(/1 file .*\+2.*−1/s);
    const links = [...el.querySelectorAll('a')].filter((a) =>
      a.textContent?.includes('Open review'),
    );
    expect(links.length).toBe(1);
    // Per call, so a later review of the same file is not served from the cache.
    expect(fetched).toBe('/share/x.patch?call=r');
  });

  it('submits a form once the required fields are filled', async () => {
    const answer = vi.fn();
    const { el, stable } = await render({
      kind: 'form',
      id: 2,
      sessionId: 's',
      answer,
      request: {
        mode: 'form',
        sessionId: 's',
        message: 'Which one?',
        requestedSchema: {
          type: 'object',
          required: ['name'],
          properties: {
            name: { type: 'string', title: 'Name' },
            ok: { type: 'boolean', title: 'OK' },
          },
        },
      },
    });
    const submit = buttons(el).find((b) => b.textContent?.includes('Submit'))!;
    expect(submit.disabled).toBe(true);

    const input = el.querySelector('textarea')!;
    input.value = 'rho';
    input.dispatchEvent(new Event('input'));
    await stable();
    expect(submit.disabled).toBe(false);
    submit.click();
    expect(answer).toHaveBeenCalledWith({ action: 'accept', content: { name: 'rho' } });
  });

  it('sends numbers as numbers and leaves out a cleared one', async () => {
    const answer = vi.fn();
    const { el, stable } = await render({
      kind: 'form',
      id: 3,
      sessionId: 's',
      answer,
      request: {
        mode: 'form',
        sessionId: 's',
        message: 'How many?',
        requestedSchema: {
          type: 'object',
          properties: {
            n: { type: 'integer', title: 'N' },
            m: { type: 'number', title: 'M' },
          },
        },
      },
    });
    const [n, m] = el.querySelectorAll('input[type=number]') as NodeListOf<HTMLInputElement>;
    for (const [input, value] of [
      [n, '42'],
      [m, ''],
    ] as const) {
      input.value = value;
      input.dispatchEvent(new Event('input'));
    }
    await stable();
    buttons(el)
      .find((b) => b.textContent?.includes('Submit'))!
      .click();
    expect(answer).toHaveBeenCalledWith({ action: 'accept', content: { n: 42 } });
  });
});
