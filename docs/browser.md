# Verify a local app with OX Browser

Run this on the host serving your development app. OX installs its browser component on request and uses an existing Chrome or Chromium executable when available. If the host needs Chrome for Testing, OX downloads it during installation. Linux ARM64 requires a system Chromium installation. You can choose an installed executable with `OX_BROWSER_EXECUTABLE=/absolute/path/to/chromium`.

Run `ox browser install` again to refresh the component. It checks the official `vercel-labs/agent-browser` releases and verifies the checksum and size of the binary for your platform. The stable release and its binary must both be at least 24 hours old. The wait gives maintainers and users time to find and withdraw a bad or compromised binary before it runs on your machine. For an urgent fix, `ox browser install --allow-fresh` skips the wait and keeps the other checks. Other browser commands use the installed component and never check for updates. Open sessions keep the component that started them. Close and reopen a session to use the refreshed version.

```sh
ox browser install
ox browser open http://localhost:3000
ox browser preview
ox browser url
ox browser snapshot
```

Open the printed preview link in your browser to watch the same page the agent is using. Snapshots identify controls with references such as `@e1`. Use the references from your own snapshot for the next action.

For integrations, `ox browser preview` checks the session's passive stream status and prints one loopback preview address. `ox browser url` reads the current page of an existing session and prints one HTTP or HTTPS address in that scope. Both commands leave stopped Chromium instances closed and report errors on stderr. If the daemon disconnects during inspection, the component may start an empty daemon. Exit status 2 means the component needs installation. Status 3 means the session has no available browser or page.

```sh
ox browser fill @e1 'Issue title'
ox browser click @e2
ox browser snapshot
ox browser screenshot verified.png
ox browser close
```

Take another snapshot after the page changes. For scripts use `ox browser snapshot --json`. Pass `--session NAME` before each command to run two independent browser sessions in the same Git worktree. Within the same worktree and session name, commands from its subdirectories use the same browser. Outside Git, use the same directory throughout.

`ox browser open` starts the session. If the browser has closed, run it again before asking for a snapshot, interaction, or screenshot. Open and close wait for commands already running in that session. An open queued behind a close uses the current installed component.

The browser defaults to HTTP and HTTPS on `localhost`, `127.0.0.1`, and `[::1]`. Those hosts are allowed on any port so a local app can talk to another local service. This default helps keep local preview data, such as form values and URLs containing development tokens, from being sent to an external host through an embedded resource, form submission, or redirect.

For a trusted LAN address or custom development hostname, add one exact host with `--allow-host`. Use the same host and session name before each command, including `preview`, `url`, and `close`. The worktree, session name, and allowed host together select the browser, so the override stays separate from the default session and other trusted hosts.

```sh
ox browser --session lan --allow-host devbox.lan open http://devbox.lan:3000
ox browser --session lan --allow-host devbox.lan snapshot
ox browser --session lan --allow-host devbox.lan preview
ox browser --session lan --allow-host devbox.lan close
```

Each session uses a fresh browser profile and follows the installed browser component's idle behavior. Run `ox browser close` when you're done. The preview listens on the local host and can accept input.

## Check the verification flow

The opt-in fixture serves a local issue tracker, checks form validation, submits a repair, and verifies the saved record, isolation, preview, screenshot, page address, and close/reopen behavior.

```sh
cargo build --locked
python3 tests/fixtures/browser_verification.py target/debug/ox --artifacts /tmp/ox-browser-results
```

To check Open and Close ordering during a component refresh, pass the executable from an older official release to the refresh fixture.

```sh
python3 tests/fixtures/browser_component_refresh.py target/debug/ox /path/to/older/agent-browser
```
