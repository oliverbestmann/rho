import { signal, WritableSignal } from '@angular/core';
import type * as acp from '@agentclientprotocol/sdk';

export type Entry =
  | { kind: 'user' | 'agent' | 'thought'; text: WritableSignal<string> }
  | { kind: 'tool'; call: WritableSignal<acp.ToolCall> };

/** The transcript of one session, built from its `session/update`s. */
export class SessionState {
  readonly entries = signal<Entry[]>([]);
  readonly title = signal<string | undefined>(undefined);
  readonly usage = signal<{ used: number; size: number } | undefined>(undefined);
  /** Model, thinking level and other session settings. */
  readonly options = signal<acp.SessionConfigOption[]>([]);
  /** Slash commands advertised by the agent. */
  readonly commands = signal<acp.AvailableCommand[]>([]);
  /** A turn runs, from any client; rho sends it in `_meta.rho.running`. */
  readonly running = signal(false);
  private readonly tools = new Map<string, WritableSignal<acp.ToolCall>>();

  apply(u: acp.SessionUpdate) {
    switch (u.sessionUpdate) {
      case 'user_message_chunk':
        this.chunk('user', u.content);
        break;
      case 'agent_message_chunk':
        this.chunk('agent', u.content);
        break;
      case 'agent_thought_chunk':
        this.chunk('thought', u.content);
        break;
      case 'tool_call': {
        const { sessionUpdate: _, ...call } = u;
        this.tool(call.toolCallId).set(call);
        break;
      }
      case 'tool_call_update': {
        const { sessionUpdate: _, ...fields } = u;
        // Only the fields that are set change.
        const set = Object.fromEntries(Object.entries(fields).filter(([, v]) => v != null));
        this.tool(u.toolCallId).update((c) => ({ ...c, ...set }));
        break;
      }
      case 'session_info_update':
        if (u.title) this.title.set(u.title);
        if (typeof u._meta?.['rho'] === 'object') {
          const running = (u._meta['rho'] as { running?: unknown }).running;
          if (typeof running === 'boolean') this.running.set(running);
        }
        break;
      case 'config_option_update':
        this.options.set(u.configOptions);
        break;
      case 'available_commands_update':
        this.commands.set(u.availableCommands);
        break;
      case 'usage_update':
        this.usage.set({ used: u.used, size: u.size });
        break;
    }
  }

  /** A prompt sent by this client; rho echoes it only to the other clients. */
  addUser(text: string) {
    this.entries.update((e) => [...e, { kind: 'user', text: signal(text) }]);
  }

  private chunk(kind: 'user' | 'agent' | 'thought', content: acp.ContentBlock) {
    const text = blockText(content);
    const last = this.entries().at(-1);
    if (last?.kind === kind) last.text.update((t) => t + text);
    else this.entries.update((e) => [...e, { kind, text: signal(text) }]);
  }

  /** The tool call with this id; a new one at the end if unknown. */
  private tool(id: string): WritableSignal<acp.ToolCall> {
    let call = this.tools.get(id);
    if (!call) {
      call = signal<acp.ToolCall>({ toolCallId: id, title: '' });
      this.tools.set(id, call);
      this.entries.update((e) => [...e, { kind: 'tool', call: call! }]);
    }
    return call;
  }
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
