import { Component, input } from '@angular/core';
import { MatExpansionModule } from '@angular/material/expansion';
import { MatIconModule } from '@angular/material/icon';
import { MarkdownPipe } from '../markdown';

/** The agent's thinking, styled like a tool card; it collapses once done. */
@Component({
  selector: 'app-thought-card',
  imports: [MarkdownPipe, MatExpansionModule, MatIconModule],
  template: `
    <mat-expansion-panel class="tool" hideToggle [expanded]="!done()">
      <mat-expansion-panel-header>
        <mat-panel-title>
          <mat-icon class="status">psychology</mat-icon>
          <span class="tool-title">Thinking</span>
        </mat-panel-title>
      </mat-expansion-panel-header>
      <div class="scroll">
        <div class="markdown" [innerHTML]="text() | markdown"></div>
      </div>
    </mat-expansion-panel>
  `,
  styleUrl: './tool-card.scss',
  styles: `
    /* About 10 lines; column-reverse keeps it pinned to the newest line, like the list. */
    .scroll {
      max-height: 10lh;
      overflow-y: auto;
      display: flex;
      flex-direction: column-reverse;
    }
    .markdown {
      color: var(--mat-sys-on-surface-variant);
      font-style: italic;
    }
  `,
})
export class ThoughtCard {
  readonly text = input.required<string>();
  /** Three newer entries followed it. */
  readonly done = input(false);
}
