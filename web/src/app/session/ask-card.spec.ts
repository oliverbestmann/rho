import { TestBed } from '@angular/core/testing';
import { Ask } from '../acp';
import { AskCard } from './ask-card';

async function render(ask: Ask) {
  const fixture = TestBed.createComponent(AskCard);
  fixture.componentRef.setInput('ask', ask);
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
