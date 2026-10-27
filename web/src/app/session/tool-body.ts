import { Pipe, PipeTransform } from '@angular/core';
import type * as acp from '@agentclientprotocol/sdk';
import { structuredPatch } from 'diff';
import {
  siC,
  siCplusplus,
  siGo,
  siKotlin,
  siNixos,
  siOpenjdk,
  siPython,
  siReact,
  siRuby,
  siRust,
  siSwift,
  siTypescript,
  type SimpleIcon,
} from 'simple-icons';
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
};

/** Language logos (Simple Icons) for code files, which have no Material icon. */
const LOGOS: Record<string, SimpleIcon> = {
  rs: siRust,
  go: siGo,
  ts: siTypescript,
  tsx: siReact,
  jsx: siReact,
  py: siPython,
  java: siOpenjdk,
  kt: siKotlin,
  c: siC,
  h: siC,
  cpp: siCplusplus,
  swift: siSwift,
  rb: siRuby,
  nix: siNixos,
};

/** The file a Read, Write or Edit is about. */
function filePath(call: acp.ToolCall) {
  const path = call.locations?.[0]?.path;
  return call.kind === 'read' || call.kind === 'edit' ? path : undefined;
}

/** The SVG path of the language logo for a Read, Write or Edit of a code file. */
export function toolLogo(call: acp.ToolCall) {
  const path = filePath(call);
  return path ? LOGOS[ext(path)]?.path : undefined;
}

/**
 * The Material icon for a tool call: the file type for a Read, Write or Edit
 * (`code` for one with a `toolLogo`), a commit icon for a git commit or push,
 * else the icon of its kind.
 */
export function toolIcon(call: acp.ToolCall) {
  const path = filePath(call);
  if (path) {
    const name = path.slice(path.lastIndexOf('/') + 1).toLowerCase();
    const icon = FILE_ICONS[name] ?? FILE_ICONS[ext(path)] ?? (LOGOS[ext(path)] && 'code');
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
  const parts = (call.content ?? []).map((c, i) => {
    if (c.type === 'diff') return fence(diff(c.path, c.oldText ?? '', c.newText), 'diff');
    if (c.type !== 'content') return '';
    const text = blockText(c.content);
    // A read file is code, fenced with its extension for `HighlightCode`.
    if (lang !== undefined && c.content.type === 'text' && text) return fence(text, lang);
    // A shell call's first block is the command, already fenced; then the output.
    return call.kind === 'execute' && i > 0 && c.content.type === 'text' && text
      ? fence(text)
      : text;
  });
  // A Write's content comes only in its input; shown above the result.
  const written = (call.rawInput as { content?: unknown } | undefined)?.content;
  const path = call.locations?.[0]?.path;
  if (call.kind === 'edit' && typeof written === 'string' && written && path)
    parts.unshift(fence(written, ext(path)));
  return parts.filter((t) => t).join('\n\n');
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
