import { Injectable, signal } from '@angular/core';

export interface Server {
  url: string;
  token: string;
}

const KEY = 'rho.server';

/** The daemon to talk to, kept in localStorage. */
@Injectable({ providedIn: 'root' })
export class Settings {
  readonly server = signal<Server | undefined>(load());

  save(server: Server) {
    localStorage.setItem(KEY, JSON.stringify(server));
    this.server.set(server);
  }
}

function load(): Server | undefined {
  try {
    const raw = localStorage.getItem(KEY);
    return raw ? (JSON.parse(raw) as Server) : undefined;
  } catch {
    return undefined;
  }
}
