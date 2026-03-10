# App Integration

Hostless supports one product-level action, `Connect to Hostless`, with three runtime families behind it:

- Local `.localhost` apps use direct browser registration with `POST /auth/register`.
- Hosted apps use the `hostless:` native-helper handshake and return via callback data.
- Desktop apps use either direct registration from a webview shell or CLI-provisioned tokens for native GUI apps.

This repo includes two embeddable reference implementations:

- Plain browser module: `test-web/hostless-connect.js`
- React wrapper: `test-web/hostless-connect-react.js`

Both browser references share the same runtime contract and storage semantics.

## Runtime Decision

Use one button and branch by current runtime:

1. If `window.location.hostname.endsWith(".localhost")`, connect directly to `http://localhost:<same-port>/auth/register`.
2. Otherwise, launch `hostless://register?...` and let the native helper complete the remote bootstrap.
3. If running as a desktop app, use the desktop-specific flow below.

This keeps the local web case simple while still supporting hosted web and desktop apps.

## Plain Browser Module

The framework-agnostic implementation lives in `test-web/hostless-connect.js`.

### API surface

```js
import { createHostlessClient } from "./hostless-connect.js";

const hostless = createHostlessClient();

const init = hostless.initialize();
if (init.error) {
  console.error(init.error.message);
}

await hostless.connect();

const response = await hostless.fetchWithBridge("v1/chat/completions", {
  method: "POST",
  headers: { "Content-Type": "application/json" },
  body: JSON.stringify({
    model: "gpt-4o-mini",
    messages: [{ role: "user", content: "Hello" }],
  }),
});
```

### What the module does

- Parses remote callback data from query string plus `#token=...`
- Persists `hostless_token`, `hostless_proxy_base`, and `hostless_state`
- Uses direct JSON registration for `.localhost` apps
- Uses `hostless://register` for remote apps
- Adds `Authorization: Bearer sk_local_...` for `/v1/*` calls
- Clears stored connection state when hostless reports an invalid or expired token

### Minimal HTML example

```html
<button id="connect-hostless" type="button">Connect to Hostless</button>
<pre id="status"></pre>

<script type="module">
  import { createHostlessClient } from "./hostless-connect.js";

  const hostless = createHostlessClient();
  const status = document.getElementById("status");

  function render() {
    const connection = hostless.getConnection();
    status.textContent = JSON.stringify(
      {
        mode: hostless.getMode(),
        connected: Boolean(connection.token && connection.proxyBase),
        proxyBase: connection.proxyBase || null,
      },
      null,
      2,
    );
  }

  const init = hostless.initialize();
  if (init.error) {
    status.textContent = init.error.message;
  }
  render();

  document.getElementById("connect-hostless").addEventListener("click", async () => {
    await hostless.connect();
    render();
  });
</script>
```

## React Wrapper

The React integration lives in `test-web/hostless-connect-react.js`. It wraps the same browser client so React apps do not reimplement callback parsing, storage, or token refresh behavior.

### Hook usage

```js
import { useHostlessConnect } from "./hostless-connect-react.js";

export function ChatScreen() {
  const hostless = useHostlessConnect();

  async function sendPrompt() {
    const response = await hostless.fetchWithHostless("v1/chat/completions", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        model: "gpt-4o-mini",
        messages: [{ role: "user", content: "hello" }],
      }),
    });

    const data = await response.json();
    console.log(data);
  }

  return (
    <div>
      <button type="button" onClick={() => hostless.connect()} disabled={hostless.busy}>
        {hostless.connected ? "Reconnect Hostless" : "Connect to Hostless"}
      </button>
      {hostless.error ? <p>{hostless.error}</p> : null}
      <button type="button" onClick={sendPrompt} disabled={!hostless.connected}>
        Send prompt
      </button>
    </div>
  );
}
```

### Component usage

```js
import { ConnectToHostlessButton } from "./hostless-connect-react.js";

export function Sidebar() {
  return (
    <ConnectToHostlessButton
      showDisconnect
      onConnected={(connection) => {
        console.log("Connected to", connection.proxyBase);
      }}
    />
  );
}
```

## Local `.localhost` Flow

When the app is already loaded from a hostless-managed URL such as `http://myapp.localhost:11434`, the browser can derive the management API base from the current runtime:

- current page origin: `http://myapp.localhost:11434`
- hostless API base: `http://localhost:11434`
- registration endpoint: `http://localhost:11434/auth/register`

The browser should not call `/auth/register` on the subdomain host. `.localhost` traffic is the reverse-proxy plane, not the management plane.

## Remote Hosted Flow

Hosted apps cannot discover or trust the local daemon from in-browser JavaScript alone. They must use a native helper or control panel app that handles:

- daemon port discovery via `~/.hostless/hostless.port`
- origin allowlisting
- `POST /auth/register`
- callback return data with `#token=...`

The browser module expects the existing callback contract:

- `port`, `local_url`, `state`, `expires_in` in the query string
- `token` in the fragment

## Desktop App Flow

Desktop apps split into two cases:

- webview desktop apps: Electron, Tauri, or Electrobun apps whose frontend runs in a browser-like renderer
- native GUI apps: desktop apps whose networking is performed by native code rather than a webview frontend

Both can connect to Hostless, but they do not use the same bootstrap path.

### Webview Desktop Apps

If the desktop app uses a webview frontend and that frontend can present a stable runtime origin, it can use the same direct registration contract as other local browser clients:

1. discover the active daemon port from `~/.hostless/hostless.port` or fall back to `11434`
2. call `POST http://localhost:<port>/auth/register`
3. persist the returned bridge token and `local_url` in native app storage
4. send `Authorization: Bearer sk_local_...` on subsequent `/v1/*` requests

Use this path when the renderer is the caller and the app can keep request origin behavior stable across registration and later API calls.

Callback redirects are optional for desktop webviews. They are only needed if a separate helper process owns the bootstrap and needs to hand results back to the renderer.

### Native GUI Apps

Native GUI apps should currently use CLI provisioning instead of browser-style registration.

Recommended flow:

1. ensure the local daemon is running
2. mint a token with `hostless token create --name <app-name>`
3. store the returned token in the platform's secure credential storage
4. send `Authorization: Bearer sk_local_...` on `/v1/*` requests to `http://localhost:<port>`

This is intentionally an admin/bootstrap flow, not an interactive browser consent flow. `hostless token create` uses the daemon's local admin authentication path and does not show the `/auth/register` approval dialog.

Use native GUI provisioning when the app does not have a stable browser-style origin, or when requests are performed by native code that does not naturally send an `Origin` header.

### Native GUI Example

One practical pattern is:

1. ask the user to install and start Hostless
2. provision a token once with the CLI
3. save the token in secure desktop storage
4. use the token for later `/v1/*` calls from native code

Example bootstrap command:

```bash
hostless token create --name my-desktop-app --providers openai --models gpt-4o-mini --ttl 86400
```

Example response summary:

```text
✓ Bridge token created
  App name:   my-desktop-app
  Origin:     *
  Providers:  openai
  Models:     gpt-4o-mini
  TTL:        86400s
  Token:      sk_local_...
```

After provisioning, the native app can call Hostless directly:

```http
POST http://localhost:11434/v1/chat/completions
Authorization: Bearer sk_local_...
Content-Type: application/json

{
  "model": "gpt-4o-mini",
  "messages": [
    { "role": "user", "content": "Hello from desktop" }
  ]
}
```

Operational notes:

- the CLI command must run on the same machine as the Hostless daemon
- the app should treat the token as a bearer secret and store it in an OS-backed credential store
- if the token expires or is revoked, prompt the user to provision a new one with `hostless token create`
- use narrower `--providers`, `--models`, and `--ttl` values when possible instead of relying on broad defaults

### Desktop Storage Guidance

- do not use browser `localStorage` for packaged desktop apps unless the token truly belongs to an embedded web app profile
- prefer platform credential stores such as macOS Keychain, Windows Credential Manager, or a secure OS-backed store exposed by the desktop framework
- treat bridge tokens like bearer secrets; they grant access to the local Hostless proxy until expiry or revocation

## Security Notes

- Browser apps only receive bridge tokens (`sk_local_*`), never provider API keys.
- Tokens are origin-bound. A token minted for `http://myapp.localhost:11434` will not work for another origin.
- Remote callback delivery uses the URL fragment so the token stays out of normal query logs.
- `.localhost` subdomains are the origin isolation boundary. All apps share the hostless daemon port and are distinguished by subdomain.
- Webview desktop apps should keep registration and request origin behavior consistent; changing renderer origin after registration will invalidate the token.
- Native GUI apps currently rely on CLI-provisioned tokens rather than interactive `/auth/register` approval.

## Reference Files

- `test-web/hostless-connect.js`
- `test-web/hostless-connect-react.js`
- `test-web/index.html`
- `docs/cli-commands.md`
- `docs/auth-and-security.md`
- `docs/reverse-proxy.md`
