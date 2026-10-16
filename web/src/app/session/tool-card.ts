import { Component, input } from '@angular/core';
import type * as acp from '@agentclientprotocol/sdk';
import { MatExpansionModule } from '@angular/material/expansion';
import { MatIconModule } from '@angular/material/icon';
import { MatProgressSpinnerModule } from '@angular/material/progress-spinner';
import { MarkdownPipe } from '../markdown';
import { hasBody, toolIcon, ToolBodyPipe } from './tool-body';

/** A tool call: status and title, its output when expanded. */
@Component({
  selector: 'app-tool-card',
  imports: [
    MarkdownPipe,
    ToolBodyPipe,
    MatExpansionModule,
    MatIconModule,
    MatProgressSpinnerModule,
  ],
  template: `
    @let c = call();
    <mat-expansion-panel
      class="tool"
      hideToggle
      [disabled]="!hasBody(c)"
      [class.failed]="c.status === 'failed'"
    >
      <mat-expansion-panel-header>
        <mat-panel-title>
          @if (c.status === 'in_progress' || c.status === 'pending') {
            <mat-spinner class="status" diameter="20" />
          } @else if (c.status === 'failed') {
            <mat-icon class="status">error</mat-icon>
          } @else {
            <mat-icon class="status">{{ icon(c) }}</mat-icon>
          }
          <span class="tool-title">{{ c.title }}</span>
        </mat-panel-title>
      </mat-expansion-panel-header>
      <ng-template matExpansionPanelContent>
        <div class="markdown" [innerHTML]="c | toolBody | markdown"></div>
      </ng-template>
    </mat-expansion-panel>
  `,
  styleUrl: './tool-card.scss',
})
export class ToolCard {
  readonly call = input.required<acp.ToolCall>();
  protected readonly icon = toolIcon;
  protected readonly hasBody = hasBody;
}
