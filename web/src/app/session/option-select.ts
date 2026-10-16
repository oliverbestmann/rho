import { Component, computed, input, output } from '@angular/core';
import type * as acp from '@agentclientprotocol/sdk';
import { MatFormFieldModule } from '@angular/material/form-field';
import { MatSelectChange, MatSelectModule } from '@angular/material/select';

/** A session config option with choices, such as the model. */
@Component({
  selector: 'app-option-select',
  imports: [MatFormFieldModule, MatSelectModule],
  template: `
    <mat-form-field subscriptSizing="dynamic">
      <mat-select
        [aria-label]="option().name"
        [panelWidth]="null"
        [value]="option().currentValue"
        (selectionChange)="selected.emit($event)"
      >
        @for (group of groups(); track group.group) {
          @if (group.name) {
            <mat-optgroup [label]="group.name">
              @for (o of group.options; track o.value) {
                <mat-option [value]="o.value" [title]="o.description ?? ''">{{
                  o.name
                }}</mat-option>
              }
            </mat-optgroup>
          } @else {
            @for (o of group.options; track o.value) {
              <mat-option [value]="o.value" [title]="o.description ?? ''">{{ o.name }}</mat-option>
            }
          }
        }
      </mat-select>
    </mat-form-field>
  `,
  styles: `
    :host {
      display: block;
    }
    mat-form-field {
      width: 100%;
    }
  `,
})
export class OptionSelect {
  readonly option = input.required<acp.SessionConfigOption & { type: 'select' }>();
  /** The parent sets it on rho, and puts `source.value` back if that fails. */
  readonly selected = output<MatSelectChange<string>>();

  /** The choices, flat or grouped. */
  protected readonly groups = computed(() => {
    const o = this.option().options;
    return o.length && 'group' in o[0]
      ? (o as acp.SessionConfigSelectGroup[])
      : [{ group: '', name: '', options: o as acp.SessionConfigSelectOption[] }];
  });
}
