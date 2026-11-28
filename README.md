# OX

**Run Codex, Claude Code, and Antigravity from your desk or phone. Continue a plan across runtimes when you’re ready to switch.**

OX starts the official CLIs on a per-user host so you can close the window, follow a turn from your phone, and pick it up again. Codex and Claude Code use your subscription or an OX API route. Antigravity uses the Google account signed in on the host.

## Install and first run

**macOS 11 or later.** Download the DMG for [Apple Silicon](https://github.com/oxstack/ox/releases/download/dev/ox-desktop-macos-arm64.dmg) or [Intel](https://github.com/oxstack/ox/releases/download/dev/ox-desktop-macos-x86_64.dmg). Open it, drag **OX Desktop** onto **Applications**, then launch OX Desktop from Applications. To update, quit OX Desktop and replace it with the app from a newer DMG.

**Linux (glibc 2.39 or later, x86-64 or ARM64) and macOS terminal installs.** Use the installer for CLI commands and background updates:

```sh
curl -fsSL https://raw.githubusercontent.com/oxstack/ox/main/install.sh | sh -s -- --desktop
cd ~/Work/my-project && ox desktop .
```

OX Desktop opens on the project folder you pass. If launched from the application menu, it opens your home directory and offers **Choose project folder…**. Install the official CLI for the route you want to use.

| You have | Install the CLI | Then |
| --- | --- | --- |
| ChatGPT plan | `npm install -g @openai/codex@latest` (Codex 0.171.0 or later; the same command upgrades it) | `codex login`, or **Sign in** in the app |
| Claude plan | `curl -fsSL https://claude.ai/install.sh \| bash` (Claude Code 2.1.331 or later; `claude update` upgrades it) | `claude`, then `/login`, or **Sign in** in the app |
| Google account | [Antigravity CLI](https://antigravity.google/docs/cli/) 1.5.15 or later | `agy acp --login`, or **Sign in** in the app |
| Any model through an API key | Codex or Claude Code | `ox setup` (Anthropic, OpenAI, OpenRouter or a local server), then pick the model in OX |

The first-run card can install or update Codex and links to setup for Claude Code and Antigravity. You can switch to an installed runtime or add an API provider.

### macOS notes

The installer verifies the download's SHA-256 checksum, installs `ox`, `ox-desktop` and `ox-host` in `~/.local/bin`, and puts **OX Desktop** in `~/Applications`. If your shell cannot find `ox`, add the line the installer prints to your shell profile.

### Linux notes

The same installer adds **OX Desktop** to your application launcher. It runs on Wayland or X11 with a Vulkan driver.

### First session

1. **Connect.** Choose Codex, Claude Code, Antigravity or an API key on the first-run card. It checks again while you install or sign in. Later, choose the runtime and billing route in the header. To add an API model, open **Settings → Accounts → Configure provider**, or run `ox setup`. A saved provider becomes the active route. Run `ox help settings` for the desktop and phone screens, file paths and checks.
2. **Send a prompt.** Choose **New session**, type, press Enter. Existing Codex and Claude Code sessions from your terminal appear in the sidebar.
3. **Keep it running.** Run `ox host install` so the session host starts at login. Close the window during a turn; reopen it and the turn is still there.
4. **Check the setup.** `ox host doctor` checks the host, Tailscale Serve, paired devices and the stock client versions, and names the fix for anything that blocks.

## What OX is

- **The relay (`ox`).** One small binary between the stock clients and your model provider. Add a provider key once and both clients get the same models, picker and web search. Any model in either client: a Claude model in Codex, a GPT model in Claude Code, OpenRouter or a self-hosted model in both. [Relay and CLI](docs/relay.md)
- **OX Desktop (`ox-desktop`).** A native Linux and macOS workbench for Codex, Claude Code and Antigravity sessions across projects and billing routes. [Desktop guide](desktop/README.md)
- **The session host (`ox-host`).** A per-user background process that owns every session, its approvals and its workers. Closing the desktop does not stop a turn. It starts on demand, or at login with `ox host install`. [Host design](docs/design/desktop/host-notes.md)
- **The phone.** A web app served by the host over your private Tailscale network. Pair it with a QR code, then follow turns, answer approvals and questions, start sessions and get notifications. Nothing is exposed to the internet: OX refuses Tailscale Funnel.
- **Workers.** A session can start a background Codex goal, Claude Code workflow or plain session as a separate job, on an API model or a subscription. Workers appear under their parent on both screens. Without the session host, workers start read-only unless the main agent asks for write, and classifier or YOLO workers are refused.
- **Handoffs.** Continue a conversation in another runtime, billing route, model or paired host. Use **Continue in…** to choose where it goes, or type `/handoff antigravity --mode plan Review this approach`. The new session receives selected messages and the latest visible plan. [Handoff setup](docs/configuration.md#continue-a-session-in-another-runtime-route-or-model)
- **One mode model.** Every permission preset maps to one of four levels: read (Codex Read only, Claude Plan), write (Workspace, Accept edits), classifier (Auto-review, Auto) and YOLO (YOLO, bypass permissions). Workers and handoffs start at their source's level or lower. Agents can lower a child's mode but never raise it. An agent-launched worker inherits a verified YOLO parent without another prompt. Explicit YOLO requests and handoffs use the operator confirmation or a stored grant. [Mode model](docs/design/desktop/host-notes.md#permission-modes-for-workers-and-handoffs)

### Pair a phone

1. Install Tailscale on the computer and the phone and sign in to the same tailnet with MagicDNS and HTTPS enabled.
2. On the computer, open **Settings → Remote access → Enable Tailscale Serve**, then **Pair a device**.
3. On the phone, open the HTTPS address shown, add it to the Home Screen (iPhone: Safari → Share → Add to Home Screen), open it from the icon, tap **Scan pairing QR** and scan the computer's screen.
4. Open **Settings → Notifications** on the phone to enable push and choose which session events send alerts.

[Phone setup in detail](desktop/README.md#keep-sessions-running-and-pair-a-phone)

### Pair a headless host

On the server, run the installer with `--host`, then run `ox host pair` (if HTTPS port 8443 is already used in Tailscale Serve, pass `--port <port>`, or `--listen-port <port>` if loopback port 48162 is taken). Paste the invitation into **Settings → Hosts** on OX Desktop. To reuse configured API keys, select the server and choose **Send API keys…**. From a paired browser, open **Settings → Credentials → Copy keys to another host…** on the host that already has the keys. Choose the destination, select the provider or Brave Search keys you want, and send. The receiver can use those keys and incur charges until you remove or rotate them.

Managed headless hosts install updates automatically when idle. Use **Settings → Host updates** in the paired web UI, or select the server in desktop **Settings → Hosts**, to choose Automatic, Download only or Manual and the Dev or Stable channel.

### Terminal only

If you only want the relay in your terminal, leave out `--desktop`:

```sh
curl -fsSL https://raw.githubusercontent.com/oxstack/ox/main/install.sh | sh
ox setup
ox codex      # or: ox claude
```

With stock [OpenCode](https://opencode.ai/download) on PATH, run `ox opencode`
to use the same OX API providers from OpenCode's terminal interface. Run
`ox opencode models` for the model IDs accepted by OpenCode and
`ox help opencode` for interactive and scripted examples.

[CLI guide](docs/relay.md)

## Upgrade

Installer-managed copies of OX Desktop download and verify a complete release in the background. Choose **Restart to update** to switch the desktop, CLI, session host and bundled phone interface together, **Restart when idle** to switch automatically once all host-managed agents finish, or **Later** to defer the decision. You can bring the prompt back from **Settings → Accounts → Restart to update**.

For an older installation, rerun the install command once to enable background updates. Include `--desktop` for the full suite or omit it for the standalone CLI. Run `ox update` to apply an update from the terminal, including on a headless machine. A running host gives active turns up to ten minutes to finish. Check the running version with `ox host doctor`.

To remove the login service, run `ox host uninstall`. It keeps your sessions and pairings. `ox host uninstall --purge` also deletes OX's host state. Neither touches Codex or Claude Code sessions or sign-ins.

## When something is wrong

| What you see | What to do |
| --- | --- |
| Anything in the desktop or phone | `ox host doctor`. Each FAIL line ends with the fix. |
| Provider, key or model errors | `ox doctor`, then the [relay troubleshooting table](docs/relay.md#troubleshooting). |
| "The running OX session host speaks protocol …" | `ox host restart`. The desktop and host versions differ. |
| The phone says the host is restarting for an update | Wait a few seconds and send again. |
| `codex` or `claude` is below OX's floor | Update the stock client; `ox host doctor` prints the command. |
| Remote access fails or port 8443 is taken in Tailscale | Run `ox host pair --port <port>` (or enter another HTTPS port in **Settings → Remote access**). If doctor reports Funnel, run `tailscale funnel --https=8443 off`. |

## Documentation

| Topic | Page |
| --- | --- |
| Relay, CLI, speed, release channels | [docs/relay.md](docs/relay.md) |
| Providers, models, search, API workers, install options | [docs/configuration.md](docs/configuration.md) |
| Desktop features, keyboard, building from source | [desktop/README.md](desktop/README.md) |
| Session host, workers, handoffs, modes, remote access | [docs/design/desktop/host-notes.md](docs/design/desktop/host-notes.md) |
| Home-server gateway for other machines | [docs/shared-gateway.md](docs/shared-gateway.md) |
| Supported clients and API boundaries | [docs/compatibility.md](docs/compatibility.md) |
| How releases are checked | [docs/verification.md](docs/verification.md), [release checklist](docs/design/desktop/release-checklist.md), [real-tailnet check](docs/design/desktop/tailnet-check.md) |
