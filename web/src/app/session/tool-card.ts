import { Component, input } from '@angular/core';
import type * as acp from '@agentclientprotocol/sdk';
import { MatExpansionModule } from '@angular/material/expansion';
import { MatIconModule } from '@angular/material/icon';
import { MatProgressSpinnerModule } from '@angular/material/progress-spinner';
import { MarkdownPipe } from '../markdown';
import { TerminalView } from './terminal';
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
    TerminalView,
  ],
  template: `
    @let c = call();
    @let t = terminal();
    <mat-expansion-panel
      class="tool"
      hideToggle
      [disabled]="!t && !hasBody(c)"
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
        @if (t !== undefined) {
          <app-terminal [data]="t" />
        } @else {
          <div class="markdown" [innerHTML]="c | toolBody | markdown"></div>
        }
      </ng-template>
    </mat-expansion-panel>
  `,
  styleUrl: './tool-card.scss',
})
export class ToolCard {
  readonly call = input.required<acp.ToolCall>();
  /** Output for a terminal card, shown with xterm.js instead of markdown. */
  readonly terminal = input<string>();
  protected readonly icon = toolIcon;
  protected readonly hasBody = hasBody;
}
