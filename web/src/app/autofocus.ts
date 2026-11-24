import { Directive, ElementRef, afterNextRender, inject } from '@angular/core';

/** Focuses the element once it is rendered; `autofocus` only works on page load. */
@Directive({ selector: '[appAutofocus]' })
export class Autofocus {
  constructor() {
    const el = inject<ElementRef<HTMLElement>>(ElementRef).nativeElement;
    afterNextRender(() => el.focus());
  }
}
