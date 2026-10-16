import { TestBed } from '@angular/core/testing';
import { Composer } from './composer';

async function render(draft: string, ready = true) {
  const fixture = TestBed.createComponent(Composer);
  fixture.componentRef.setInput('draft', draft);
  fixture.componentRef.setInput('ready', ready);
  const send = vi.fn();
  fixture.componentInstance.send.subscribe(send);
  await fixture.whenStable();
  const el = fixture.nativeElement as HTMLElement;
  const sendButton = el.querySelector<HTMLButtonElement>('button[aria-label="Send"]')!;
  return { el, send, sendButton };
}

describe('Composer', () => {
  it('sends on Ctrl+Enter and with the button', async () => {
    const { el, send, sendButton } = await render('hello');
    el.querySelector('textarea')!.dispatchEvent(
      new KeyboardEvent('keydown', { key: 'Enter', ctrlKey: true }),
    );
    sendButton.click();
    expect(send).toHaveBeenCalledTimes(2);
  });

  it('cannot send a blank draft or while not ready', async () => {
    expect((await render('  ')).sendButton.disabled).toBe(true);
    expect((await render('hello', false)).sendButton.disabled).toBe(true);
  });
});
