import { signal } from '@angular/core';
import { TestBed } from '@angular/core/testing';
import { Acp } from '../acp';
import type { Image } from './composer';
import { LazyImage } from './lazy-image';

describe('LazyImage', () => {
  it('keeps the image space with a spinner until it arrives', async () => {
    let resolve!: (i: Image) => void;
    const image = vi.fn(() => new Promise<Image>((r) => (resolve = r)));
    TestBed.configureTestingModule({
      providers: [{ provide: Acp, useValue: { agent: signal({}), image } }],
    });
    const fixture = TestBed.createComponent(LazyImage);
    fixture.componentRef.setInput('sessionId', 's');
    fixture.componentRef.setInput('image', { uri: 'rho-image:ab', width: 400, height: 200 });
    fixture.detectChanges();
    const el = fixture.nativeElement as HTMLElement;
    const box = el.querySelector<HTMLElement>('.placeholder')!;
    expect(box.style.aspectRatio).toBe('400 / 200');
    expect(box.querySelector('mat-spinner')).not.toBeNull();
    await vi.waitFor(() => expect(image).toHaveBeenCalledWith('s', 'rho-image:ab'));

    resolve({ mimeType: 'image/png', data: 'AA' });
    await fixture.whenStable();
    expect(el.querySelector('.placeholder')).toBeNull();
    expect(el.querySelector('img')!.src).toBe('data:image/png;base64,AA');
  });
});
