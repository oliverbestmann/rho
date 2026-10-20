import {
  afterNextRender,
  Component,
  DestroyRef,
  effect,
  ElementRef,
  inject,
  input,
} from '@angular/core';
import type { Terminal as Xterm } from '@xterm/xterm';
import type { FitAddon } from '@xterm/addon-fit';

/** Terminal output rendered by xterm.js, which loads on first use. */
@Component({
  selector: 'app-terminal',
  template: '',
  styles: `
    :host {
      display: block;
      padding: 8px;
      border-radius: var(--mat-sys-corner-small);
      background: var(--mat-sys-surface-container-highest);
      color: var(--mat-sys-on-surface);
    }
  `,
})
export class TerminalView {
  /** All output so far; only the new part is written on a change. */
  readonly data = input.required<string>();

  private term?: Xterm;
  private fit?: FitAddon;
  private written = 0;

  constructor() {
    const host = inject<ElementRef<HTMLElement>>(ElementRef).nativeElement;
    const destroyRef = inject(DestroyRef);
    afterNextRender(async () => {
      const [{ Terminal }, { FitAddon }] = await Promise.all([
        import('@xterm/xterm'),
        import('@xterm/addon-fit'),
      ]);
      if (destroyRef.destroyed) return;
      // Resolved colours of the host, since xterm takes no CSS variables.
      const style = getComputedStyle(host);
      this.term = new Terminal({
        rows: 1,
        convertEol: true,
        disableStdin: true,
        cursorInactiveStyle: 'none',
        fontFamily: 'monospace',
        fontSize: 13,
        scrollback: 10_000,
        tabStopWidth: 2,
        theme: {
          background: style.backgroundColor,
          foreground: style.color,
          cursor: style.backgroundColor,
        },
      });
      this.fit = new FitAddon();
      this.term.loadAddon(this.fit);
      this.term.open(host);
      const resize = new ResizeObserver(() => this.resize());
      resize.observe(host);
      destroyRef.onDestroy(() => {
        resize.disconnect();
        this.term?.dispose();
      });
      this.write();
    });
    effect(() => {
      this.data();
      this.write();
    });
  }

  private write() {
    const data = this.data();
    if (!this.term || this.written === data.length) return;
    this.term.write(data.slice(this.written), () => this.resize());
    this.written = data.length;
  }

  /** Width from the fit addon; height grows with the output, so it never scrolls. */
  private resize() {
    const term = this.term;
    const cols = this.fit?.proposeDimensions()?.cols;
    if (!term || !cols) return;
    const rows = Math.max(term.buffer.active.length, 1);
    if (cols !== term.cols || rows !== term.rows) term.resize(cols, rows);
  }
}
