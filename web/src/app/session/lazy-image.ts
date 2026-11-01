import { Component, computed, inject, input, resource } from '@angular/core';
import { MatProgressSpinnerModule } from '@angular/material/progress-spinner';
import { Acp } from '../acp';
import type { ImageRef } from './session-state';

/**
 * A tool call image that rho sent as a `rho-image:` uri: fetched when shown,
 * a box of the image's size with a spinner until then.
 */
@Component({
  selector: 'app-lazy-image',
  imports: [MatProgressSpinnerModule],
  template: `
    @if (loaded.hasValue()) {
      @let img = loaded.value();
      <img [src]="'data:' + img.mimeType + ';base64,' + img.data" alt="Tool image" />
    } @else {
      <div class="placeholder" [style.width]="box().width" [style.aspect-ratio]="box().ratio">
        @if (loaded.error()) {
          <span>Image not available</span>
        } @else {
          <mat-spinner diameter="32" aria-label="Loading image" />
        }
      </div>
    }
  `,
  styles: `
    :host {
      display: block;
      margin: 8px 0;
    }
    img {
      display: block;
      max-width: 100%;
      max-height: 60vh;
    }
    .placeholder {
      display: flex;
      align-items: center;
      justify-content: center;
      border-radius: var(--mat-sys-corner-small);
      background: var(--mat-sys-surface-container-high);
      color: var(--mat-sys-on-surface-variant);
      font: var(--mat-sys-body-small);
    }
  `,
})
export class LazyImage {
  private readonly acp = inject(Acp);
  readonly sessionId = input.required<string>();
  readonly image = input.required<ImageRef>();

  /** The size the image will show at, like the `img` rules above; 4:3 if unknown. */
  protected readonly box = computed(() => {
    const { width, height } = this.image();
    if (!width || !height) return { width: 'min(100%, 320px)', ratio: '4 / 3' };
    return {
      width: `min(100%, ${width}px, calc(60vh * ${width / height}))`,
      ratio: `${width} / ${height}`,
    };
  });

  /** Waits for a connection; fetches again after a reconnect if it failed. */
  protected readonly loaded = resource({
    params: () =>
      this.acp.agent() ? { sessionId: this.sessionId(), uri: this.image().uri } : undefined,
    loader: ({ params }) => this.acp.image(params.sessionId, params.uri),
  });
}
