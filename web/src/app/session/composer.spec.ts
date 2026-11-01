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

  it('attaches a pasted image and sends it without text', async () => {
    const { el, sendButton } = await render('');
    // jsdom has no DataTransfer.
    const paste = Object.assign(new Event('paste', { cancelable: true }), {
      clipboardData: { files: [new File(['png'], 'a.png', { type: 'image/png' })] },
    });
    el.querySelector('textarea')!.dispatchEvent(paste);
    expect(paste.defaultPrevented).toBe(true);
    await vi.waitFor(() => expect(el.querySelectorAll('.images img').length).toBe(1));
    expect(el.querySelector('img')!.src).toBe('data:image/png;base64,cG5n');
    expect(sendButton.disabled).toBe(false);
  });

  it('cannot queue images while a turn runs', async () => {
    const fixture = TestBed.createComponent(Composer);
    fixture.componentRef.setInput('draft', 'hi');
    fixture.componentRef.setInput('ready', true);
    fixture.componentRef.setInput('running', true);
    fixture.componentRef.setInput('images', [{ mimeType: 'image/png', data: 'AA' }]);
    await fixture.whenStable();
    const queue = (fixture.nativeElement as HTMLElement).querySelector<HTMLButtonElement>(
      'button[aria-label="Queue"]',
    )!;
    expect(queue.disabled).toBe(true);
  });
});
