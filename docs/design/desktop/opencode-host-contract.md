# Hosted OpenCode contract

The hosted implementation uses the HTTP session API in stock OpenCode. The initial contract below was verified against 1.19.6. It exposes bounded history and pending approvals alongside live events. The existing OX host retains the authoritative session registry and publishes its own desktop and phone snapshots. Antigravity's transport and runtime capability boundaries in [#56](https://github.com/oxstack/ox/pull/56) provide the OX integration pattern.

## Initial contract verification

```sh
python3 desktop/tests/opencode_host_contract.py /path/to/opencode /path/to/ox
```

The fixture starts the real OpenCode server and passes an OX alias through the real Rust gateway to a temporary Messages provider. It uses isolated Git repositories, local authentication, and synthetic credentials. Run it against the installed executable and record the version with the results.

- **Session identity.** OpenCode's authenticated loopback server created, read, and reopened the same session after process restart. The project directory routes the request. The same authenticated client could read a known session ID using another repository directory in the same data store. An independent OpenCode store returned 404.
- **History and events.** An asynchronous prompt streamed text. After disconnect, a bounded history query recovered the missed text and identified the same part. Reconnecting with `Last-Event-ID` did not replay the old delta before the next turn. History pagination supplied a cursor for the previous page. [OpenCode session API](https://github.com/anomalyco/opencode/blob/v1.19.6/packages/opencode/src/server/routes/instance/httpapi/groups/session.ts) · [Event implementation](https://github.com/anomalyco/opencode/blob/v1.19.6/packages/opencode/src/server/routes/instance/httpapi/handlers/event.ts)
- **Approvals.** The event and pending-approval list identified the same session and native request. Allow once executed a fixture command once, reject created no output file, and stale replies returned 404 after resolution. The host can read the pending list after reconnect. [Permission API](https://github.com/anomalyco/opencode/blob/v1.19.6/packages/opencode/src/server/routes/instance/httpapi/groups/permission.ts)
- **Stop.** Aborting during provider streaming reached idle without the buffered tail. Aborting a turn with an open permission returned success and reported idle while the native approval was still present. Rejecting that exact stale approval cleared it without running the command.

## Host implementation gates

1. **Child process and scope.** Own an authenticated private loopback child for the approved runtime route. Bind the child and its OpenCode data directory to the exact host, canonical project, and billing route. Register each session ID to that scope before exposing it in OX. Verify ownership for history, prompt, cancel, and approval actions. Keep the upstream provider credential in the OX gateway. Persist only stable session ownership and configuration references.
2. **Projection and recovery.** Subscribe to the directory's event stream, then read status, pending approvals, and the newest bounded history page from the same child. Identify records by session, message, and part ID. Reconcile against history after interruption and at terminal transitions, then publish through the existing OX sequence and snapshot machinery. Preserve the user's message IDs and the host's send state when a request has an ambiguous outcome.
3. **Cancellation and permissions.** Fence the active turn and close its OX approvals when stopping. Abort upstream, reconcile the pending list, and reject only the exact native approvals left by the fenced turn. Keep writes and new turns blocked for that session until the state is reconciled. Expose confirmed decisions through the existing host policy and ask tokens. Advertise approval actions backed by the installed runtime’s permission contract.
4. **Desktop and phone.** Test starting and reopening on the same model and account, an approval answered from either client, stale replies and mid-stream Stop, reopen after an OX host restart, and two simultaneously active worktrees or routes. Keep moves and handoffs gated by the versioned host ownership contract in [#66](https://github.com/oxstack/ox/pull/66).

The stock binary also advertises ACP. In the tested 1.19.6 implementation, ACP load reads and replays complete history. ACP resume reads up to 20 messages to restore settings and emits no history replay. HTTP gives the first adapter an explicit history cursor and pending-request reads. The OX transport can be revisited after a cross-client latency comparison. [ACP load and resume implementation](https://github.com/anomalyco/opencode/blob/v1.19.6/packages/opencode/src/acp/service.ts)

## Independently tested Rust adapter

The adapter lives in the host package and reuses its existing blocking HTTP transport and OX's shared bounded SSE parser. It prepares separate cache, config, data, and state directories for each canonical owner. It takes the loopback address and authentication secret from the caller that started the child. Before accepting requests, it checks the health, status and permission responses, confirms the server requires authentication, and compares the native project, config and state paths with the owner's profile.

```sh
cargo test --locked --manifest-path desktop/Cargo.toml -p ox-host --lib
```

The unit tests exercise authenticated routes against a bounded loopback fixture. They cover owner partitioning, wrong-owner rejection, prompt model binding, history cursors, scoped SSE, and the cancellation fence after an ambiguous error. The stock fixture separately verifies the wire contract and reported paths on Linux.

## Route-owned child startup

`Hosted::start` launches the OX helper from an I/O worker. The helper reuses the catalog, gateway, and launch configuration from `ox opencode`. It binds gateway inference to the exact model alias in the host's route and gives OpenCode the profile's four XDG roots. An exclusive lock ensures one owner can start that profile. The helper scopes ephemeral credentials to the local launch. OX retains the upstream keys.

The helper reports the actual loopback listener and fresh Basic password over a private inherited Unix socket. The adapter verifies the version and the project, config, and state paths. Closing the control peer asks OpenCode to dispose its instances, including tracked tool processes in separate groups. The final fallback reaps the helper's private group. The host must still persist and verify ownership and cancellation state across host restarts.

```sh
python3 desktop/tests/opencode_host_bootstrap.py /path/to/opencode /path/to/ox
```

The Linux fixture occupies OpenCode's default TCP port, verifies the selected profile and authentication, rejects a duplicate launch, approves Bash to check that configured provider keys are absent, then closes the owner socket while a detached stock tool is running. It confirms the helper, server, and tool have exited. A helper or stock process killed before it can dispose its instances is a separate abrupt-failure case. This slice leaves host dispatch, durable receipt reconciliation, desktop and phone projection, and macOS verification to the integration PR.
