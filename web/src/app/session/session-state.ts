import { signal, WritableSignal } from '@angular/core';
import type * as acp from '@agentclientprotocol/sdk';

export type Entry =
  /** `note`: a notice from rho shown as text, from `_meta.rho.note`. */
  | { kind: 'user' | 'agent' | 'thought' | 'note'; text: WritableSignal<string> }
  | {
      kind: 'tool';
      call: WritableSignal<acp.ToolCall>;
      /** Terminal output, from rho's `_meta.terminal_output`; undefined if not a terminal. */
      terminal: WritableSignal<string | undefined>;
    };

/** The transcript of one session, built from its `session/update`s. */
export class SessionState {
  readonly entries = signal<Entry[]>([]);
  readonly title = signal<string | undefined>(undefined);
  readonly usage = signal<acp.UsageUpdate | undefined>(undefined);
  /** Model, thinking level and other session settings. */
  readonly options = signal<acp.SessionConfigOption[]>([]);
  /** Slash commands advertised by the agent. */
  readonly commands = signal<acp.AvailableCommand[]>([]);
  /** A turn runs, from any client; rho sends it in `_meta.rho.running`. */
  readonly running = signal(false);
  private readonly tools = new Map<string, Extract<Entry, { kind: 'tool' }>>();

  apply(u: acp.SessionUpdate) {
    switch (u.sessionUpdate) {
      case 'user_message_chunk':
        this.chunk('user', u.content);
        break;
      case 'agent_message_chunk':
        // A note comes whole, in one chunk: never merged with its neighbors.
        if (rhoMeta(u._meta)?.['note'] === true) this.add('note', blockText(u.content));
        else this.chunk('agent', u.content);
        break;
      case 'agent_thought_chunk':
        this.chunk('thought', u.content);
        break;
      case 'tool_call': {
        const { sessionUpdate: _, ...call } = u;
        const tool = this.tool(call.toolCallId);
        tool.call.set(call);
        if (u._meta?.['terminal_info']) tool.terminal.set('');
        this.terminal(tool, u._meta);
        break;
      }
      case 'tool_call_update': {
        const { sessionUpdate: _, ...fields } = u;
        // Only the fields that are set change.
        const set = Object.fromEntries(Object.entries(fields).filter(([, v]) => v != null));
        const tool = this.tool(u.toolCallId);
        tool.call.update((c) => ({ ...c, ...set }));
        this.terminal(tool, u._meta);
        break;
      }
      case 'session_info_update': {
        if (u.title) this.title.set(u.title);
        const running = rhoRunning(u._meta);
        if (running !== undefined) this.running.set(running);
        break;
      }
      case 'config_option_update':
        this.options.set(u.configOptions);
        break;
      case 'available_commands_update':
        this.commands.set(u.availableCommands);
        break;
      case 'usage_update':
        this.usage.set({ used: u.used, size: u.size, cost: u.cost });
        break;
    }
  }

  /** A prompt sent by this client; rho echoes it only to the other clients. */
  addUser(text: string) {
    this.add('user', text);
  }

  private add(kind: 'user' | 'note', text: string) {
    this.entries.update((e) => [...e, { kind, text: signal(text) }]);
  }

  private chunk(kind: 'user' | 'agent' | 'thought', content: acp.ContentBlock) {
    const text = blockText(content);
    const last = this.entries().at(-1);
    if (last?.kind === kind) last.text.update((t) => t + text);
    else this.entries.update((e) => [...e, { kind, text: signal(text) }]);
  }

  /** The tool call with this id, if known. */
  call(id: string): acp.ToolCall | undefined {
    return this.tools.get(id)?.call();
  }

  /** The tool call with this id; a new one at the end if unknown. */
  private tool(id: string) {
    let tool = this.tools.get(id);
    if (!tool) {
      const t = {
        kind: 'tool' as const,
        call: signal<acp.ToolCall>({ toolCallId: id, title: '' }),
        terminal: signal<string | undefined>(undefined),
      };
      this.tools.set(id, t);
      this.entries.update((e) => [...e, t]);
      tool = t;
    }
    return tool;
  }

  /** Appends `_meta.terminal_output.data`, the output rho streams to terminal clients. */
  private terminal(tool: Extract<Entry, { kind: 'tool' }>, meta: Meta) {
    const out = meta?.['terminal_output'] as { data?: unknown } | undefined;
    if (typeof out?.data === 'string') {
      const data = out.data;
      tool.terminal.update((t) => (t ?? '') + data);
    }
  }
}

type Meta = { [key: string]: unknown } | null | undefined;

/** `_meta.rho`, rho's own fields; undefined if not sent. */
function rhoMeta(meta: Meta) {
  const rho = meta?.['rho'];
  return typeof rho === 'object' && rho ? (rho as { [key: string]: unknown }) : undefined;
}

/** `_meta.rho.running`, where rho says whether a turn runs; undefined if not sent. */
export function rhoRunning(meta: Meta) {
  const running = rhoMeta(meta)?.['running'];
  return typeof running === 'boolean' ? running : undefined;
}

/** A content block as markdown. */
export function blockText(b: acp.ContentBlock): string {
  switch (b.type) {
    case 'text':
      return b.text;
    case 'image':
      // Own paragraph, so it does not glue onto surrounding text.
      return `\n\n![image](data:${b.mimeType};base64,${b.data})\n\n`;
    case 'resource_link':
      return `[${b.name}](${b.uri})`;
    case 'resource':
      return `*[${b.resource.uri}]*`;
    default:
      return '';
  }
}
