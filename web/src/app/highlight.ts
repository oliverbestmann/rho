import { afterRenderEffect, Directive, ElementRef, inject, input } from '@angular/core';
import type { HLJSApi } from 'highlight.js';

/**
 * Highlights the `language-*` code blocks in its element. highlight.js, with all
 * its languages, loads on first use, so only when such a block is shown.
 * Unknown languages stay plain.
 */
@Directive({ selector: '[appHighlight]' })
export class HighlightCode {
  /** The element's HTML; it runs again when this changes. */
  readonly appHighlight = input<string>();
  private readonly el = inject<ElementRef<HTMLElement>>(ElementRef);

  constructor() {
    afterRenderEffect(() => {
      this.appHighlight();
      const blocks = [
        ...this.el.nativeElement.querySelectorAll<HTMLElement>(
          'code[class^="language-"]:not(.language-diff)',
        ),
      ];
      if (!blocks.length) return;
      // Once loaded, synchronously: a streaming message is rebuilt per chunk,
      // and its finished blocks must not show plain for a frame.
      if (hljs) highlight(hljs, blocks);
      else void loadHighlighter().then((h) => highlight(h, blocks));
    });
  }
}

let hljs: HLJSApi | undefined;
let loading: Promise<HLJSApi> | undefined;

/** Loads highlight.js; the session view starts it early so it is ready when needed. */
export function loadHighlighter() {
  return (loading ??= import('highlight.js').then((m) => (hljs = m.default)));
}

/** Highlighted HTML by language and code, so a rebuilt block is not highlighted again. */
const cache = new Map<string, string>();
const CACHE_SIZE = 500;

function highlight(hljs: HLJSApi, blocks: HTMLElement[]) {
  for (const block of blocks) {
    const lang = block.className.slice('language-'.length);
    // The block may have been replaced while highlight.js loaded.
    if (!block.isConnected || !hljs.getLanguage(lang)) continue;
    const key = `${lang}\0${block.textContent}`;
    let html = cache.get(key);
    if (html === undefined) {
      html = hljs.highlight(block.textContent, { language: lang, ignoreIllegals: true }).value;
      // shortcut: drops all entries when full, use an LRU if that shows up as rework.
      if (cache.size >= CACHE_SIZE) cache.clear();
      cache.set(key, html);
    }
    block.innerHTML = html; // highlight.js escapes the code
    block.classList.add('hljs');
  }
}
