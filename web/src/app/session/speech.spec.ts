import { floats, wav } from './speech';

describe('speech', () => {
  it('reads 16 bit little endian samples', () => {
    expect(Array.from(floats(new Uint8Array([0x00, 0x40, 0x00, 0xc0, 0xff, 0x7f])))).toEqual([
      0.5,
      -0.5,
      32767 / 32768,
    ]);
  });

  it('puts a 24 kHz mono WAV header in front', () => {
    const file = wav(new Uint8Array([1, 2, 3, 4]));
    const view = new DataView(file.buffer);
    expect(new TextDecoder().decode(file.slice(0, 4))).toBe('RIFF');
    expect(view.getUint32(4, true)).toBe(40);
    expect(view.getUint32(24, true)).toBe(24000);
    expect(view.getUint32(40, true)).toBe(4);
    expect(Array.from(file.slice(44))).toEqual([1, 2, 3, 4]);
  });
});
