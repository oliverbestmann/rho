import { Component, inject, input } from '@angular/core';
import { Acp } from '../acp';
import type * as acp from '@agentclientprotocol/sdk';
import { RouterLink } from '@angular/router';
import { MatButtonModule } from '@angular/material/button';
import { MatExpansionModule } from '@angular/material/expansion';
import { MatIconModule } from '@angular/material/icon';
import { MatProgressSpinnerModule } from '@angular/material/progress-spinner';
import { HighlightCode } from '../highlight';
import { MarkdownPipe } from '../markdown';
import { TerminalView } from './terminal';
import { LazyImage } from './lazy-image';
import { imageRefs, rhoTool } from './session-state';
import { hasBody, toolIcon, toolLogo, ToolBodyPipe } from './tool-body';

/** A tool call: status and title, its output when expanded. */
@Component({
  selector: 'app-tool-card',
  imports: [
    HighlightCode,
    LazyImage,
    MarkdownPipe,
    ToolBodyPipe,
    RouterLink,
    MatButtonModule,
    MatExpansionModule,
    MatIconModule,
    MatProgressSpinnerModule,
    TerminalView,
  ],
  template: `
    @let c = call();
    @let t = terminal();
    <mat-expansion-panel
      class="tool"
      hideToggle
      [disabled]="!t && !hasBody(c)"
      [expanded]="opened(c)"
      [class.failed]="c.status === 'failed'"
    >
      <mat-expansion-panel-header>
        <mat-panel-title>
          @if (c.status === 'in_progress' || c.status === 'pending') {
            <mat-spinner class="status" diameter="20" />
          } @else if (c.status === 'failed') {
            <mat-icon class="status">error</mat-icon>
          } @else if (logo(c); as path) {
            <svg class="status" viewBox="0 0 24 24" aria-hidden="true">
              <path [attr.d]="path" />
            </svg>
          } @else {
            <mat-icon class="status">{{ icon(c) }}</mat-icon>
          }
          <span class="tool-title">{{ c.title }}</span>
          @if (isReview(c) && !asked(c)) {
            <a
              class="review"
              matIconButton
              aria-label="Open review"
              title="Open review"
              [routerLink]="['review', c.toolCallId]"
              (click)="$event.stopPropagation()"
            >
              <mat-icon>rate_review</mat-icon>
            </a>
          }
        </mat-panel-title>
      </mat-expansion-panel-header>
      <ng-template matExpansionPanelContent>
        @if (t !== undefined) {
          <app-terminal [data]="t" />
        } @else {
          @let html = c | toolBody | markdown;
          <div class="markdown" [innerHTML]="html" [appHighlight]="html"></div>
          @for (ref of imageRefs(c); track ref.uri) {
            <app-lazy-image [sessionId]="sessionId()" [image]="ref" />
          }
        }
      </ng-template>
    </mat-expansion-panel>
  `,
  styleUrl: './tool-card.scss',
})
export class ToolCard {
  readonly call = input.required<acp.ToolCall>();
  /** For fetching the call's images. */
  readonly sessionId = input.required<string>();
  /** Output for a terminal card, shown with xterm.js instead of markdown. */
  readonly terminal = input<string>();
  protected readonly icon = toolIcon;
  protected readonly logo = toolLogo;
  protected readonly hasBody = hasBody;
  protected readonly imageRefs = imageRefs;

  private readonly acp = inject(Acp);

  /** A `PresentDiff` call: its diff has its own page (ADR 0076). */
  protected isReview(c: acp.ToolCall) {
    return rhoTool(c) === 'PresentDiff';
  }

  /** Its approval waits: the approval card links the review then. */
  protected asked(c: acp.ToolCall) {
    return this.acp
      .asks()
      .some((a) => a.kind === 'permission' && a.request.toolCall.toolCallId === c.toolCallId);
  }

  /** ShowImage and ShareFile are there to be seen: open once they have the image or link. */
  protected opened(c: acp.ToolCall) {
    const tool = rhoTool(c);
    return (tool === 'ShowImage' || tool === 'ShareFile') && c.status === 'completed';
  }
}
