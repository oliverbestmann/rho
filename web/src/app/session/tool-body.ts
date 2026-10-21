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

const FILE_ICONS: Record<string, string> = {
  md: 'markdown',
  html: 'html',
  css: 'css',
  scss: 'css',
  js: 'javascript',
  mjs: 'javascript',
  php: 'php',
  json: 'data_object',
  yaml: 'data_object',
  yml: 'data_object',
  toml: 'data_object',
  xml: 'data_object',
  sql: 'database',
  csv: 'csv',
  pdf: 'picture_as_pdf',
  png: 'image',
  jpg: 'image',
  jpeg: 'image',
  gif: 'image',
  webp: 'image',
  svg: 'image',
  sh: 'terminal',
  bash: 'terminal',
  zsh: 'terminal',
  // Files known by name; `ext` gives the lowercase name of a file without an extension.
  makefile: 'build',
  gnumakefile: 'build',
  justfile: 'build',
  dockerfile: 'deployed_code',
  containerfile: 'deployed_code',
  '.dockerignore': 'deployed_code',
  '.gitignore': 'commit',
  '.gitattributes': 'commit',
  '.gitmodules': 'commit',
  '.env': 'settings',
  '.editorconfig': 'settings',
  ...Object.fromEntries(
    ['rs', 'go', 'ts', 'tsx', 'jsx', 'py', 'java', 'kt', 'c', 'h', 'cpp', 'swift', 'rb', 'nix'].map(
      (e) => [e, 'code'],
    ),
  ),
};

/**
 * The Material icon for a tool call: the file type for a Read, Write or Edit,
 * a commit icon for a git commit or push, else the icon of its kind.
 */
export function toolIcon(call: acp.ToolCall) {
  const path = call.locations?.[0]?.path;
  if ((call.kind === 'read' || call.kind === 'edit') && path) {
    const name = path.slice(path.lastIndexOf('/') + 1).toLowerCase();
    const icon = FILE_ICONS[name] ?? FILE_ICONS[ext(path)];
    if (icon) return icon;
  }
  const command = (call.rawInput as { command?: unknown } | undefined)?.command;
  if (
    call.kind === 'execute' &&
    typeof command === 'string' &&
    /\bgit\s+(commit|push)\b/.test(command)
  )
    return 'commit';
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
  return ext(path);
}

/** The lowercase extension of a path, the name of a file without one, or '' for a dotfile. */
function ext(path: string) {
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
