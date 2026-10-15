# rho web

An ACP client for the rho daemon, for following and answering sessions from
a phone or browser. Angular, Material 3 and `@agentclientprotocol/sdk` over
WebSocket.

- Lists the sessions, newest first.
- Opens a session (`session/load`): transcript, tool calls, usage.
- Sends prompts and stops turns.
- Answers permission requests and forms, also ones raised while away.

## Run

```sh
rho daemon --listen 127.0.0.1:8766     # token in ~/.config/rho/acp-token
nix develop path:.. -c npm start       # http://localhost:4200
```

`ng serve` forwards `/dev/acp` to the daemon on `localhost:8766`
(`proxy.conf.json`), so log in with the suggested `ws://<host>/dev/acp` and
the token. Both are kept in localStorage. The token goes as a
`bearer.<token>` subprotocol, since browsers cannot set headers.

From a phone, reach the daemon through Tailscale or similar. Don't expose
`ws://` with a token without TLS.

## Develop

```sh
nix develop path:.. -c npx ng test --watch=false
nix develop path:.. -c npx ng build
```
