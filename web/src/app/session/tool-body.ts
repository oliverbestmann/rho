import { Pipe, PipeTransform } from '@angular/core';
import type * as acp from '@agentclientprotocol/sdk';
import { structuredPatch } from 'diff';
import { blockText } from './session-state';

const KIND_ICONS: Record<string, string> = {
  read: 'description',
  edit: 'edit',
  delete: 'delete',
  move: 'drive_file_move',
  search: 'search',
  execute: 'terminal',
  think: 'psychology',
  fetch: 'language',
  switch_mode: 'swap_horiz',
};

/** The Material icon for a tool call's kind. */
export function toolIcon(call: acp.ToolCall) {
  return KIND_ICONS[call.kind ?? ''] ?? 'build';
}

/** `toolBody` as a pure pipe, so a diff is only made again when the call changes. */
@Pipe({ name: 'toolBody' })
export class ToolBodyPipe implements PipeTransform {
  transform(call: acp.ToolCall) {
    return toolBody(call);
  }
}

/** Whether a tool card has anything to show; cheap, unlike `toolBody` with its diffs. */
export function hasBody(call: acp.ToolCall) {
  return (call.content ?? []).some(
    (c) => c.type === 'diff' || (c.type === 'content' && !!blockText(c.content)),
  );
}

/** A tool call's content as markdown; empty when there is nothing to show. */
export function toolBody(call: acp.ToolCall) {
  const lang = readLang(call);
  return (call.content ?? [])
    .map((c, i) => {
      if (c.type === 'diff') return fence(diff(c.path, c.oldText ?? '', c.newText), 'diff');
      if (c.type !== 'content') return '';
      const text = blockText(c.content);
      // A read file is code, fenced with its extension for `HighlightCode`.
      if (lang !== undefined && c.content.type === 'text' && text) return fence(text, lang);
      // A shell call's first block is the command, already fenced; then the output.
      return call.kind === 'execute' && i > 0 && c.content.type === 'text' && text
        ? fence(text)
        : text;
    })
    .filter((t) => t)
    .join('\n\n');
}

/**
 * The language of a successful Read, or undefined for other calls: the file
 * extension, or the name of a file without one, like `makefile` or `dockerfile`.
 */
function readLang(call: acp.ToolCall) {
  const path = call.locations?.[0]?.path;
  if (call.kind !== 'read' || call.status === 'failed' || !path) return undefined;
  const name = path.slice(path.lastIndexOf('/') + 1);
  const dot = name.lastIndexOf('.');
  if (dot === 0) return ''; // like .env
  return (dot > 0 ? name.slice(dot + 1) : name).toLowerCase();
}

/** `text` as a fenced code block, with a fence longer than any backtick run in it. */
function fence(text: string, lang = '') {
  const longest = Math.max(0, ...(text.match(/`+/g) ?? []).map((r) => r.length));
  const f = '`'.repeat(Math.max(3, longest + 1));
  return `${f}${lang}\n${text}\n${f}`;
}

/** The unified diff hunks of an edit, without the file header. */
function diff(path: string, old: string, next: string) {
  return structuredPatch(path, path, old, next, '', '', { context: 3 })
    .hunks.map((h) =>
      [`@@ -${h.oldStart},${h.oldLines} +${h.newStart},${h.newLines} @@`, ...h.lines].join('\n'),
    )
    .join('\n');
}
