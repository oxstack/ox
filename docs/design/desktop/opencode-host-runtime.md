# Hosted OpenCode route controller

The route controller is a local proof that uses the stock OpenCode installed on the execution host. The private OX host socket accepts explicit subscription to one enrolled project and API alias. An operator starts the local host with `OX_EXPERIMENTAL_HOSTED_OPENCODE_PROJECT` set to the exact canonical path and `OX_EXPERIMENTAL_HOSTED_OPENCODE_ALIAS` set to the exact OX alias. Explicit subscriptions for this runtime require wire protocol 3. Public route discovery, catalog watches and broad subscriptions omit this runtime. The standalone `ox opencode` launcher has its own lifecycle and configuration.

## Authority

- The host selects the canonical project directory and exact OX API model alias. A relay for that pair uses the existing Rust gateway and the same route catalog as the standalone launcher. The host checks native health and attests the project, state and config paths over authenticated loopback. It verifies the session-status and pending-permission response shapes and confirms that session status requires authentication before opening the route. Session-specific operations verify the responses they need when invoked. The reported OpenCode version is diagnostic metadata.
- Each owner has a separate profile and registry. A process-lifetime lock holds the registry even if the stock server exits. The server helper separately locks its profile. The registry binds OX-issued UUIDs to stock session IDs, and its version and owner fingerprint are checked on open. Caller-supplied stock session IDs do not open a session.
- The registry persists one monotonically increasing send sequence per session and its current receipt. A retry of the current sequence is matched to the original prompt digest. Older, skipped or conflicting sequences are rejected. The journal records a pending send before the native asynchronous POST. A lost reply leaves an unresolved receipt. Resubmission does not replay that POST. The host operation-receipt layer holds acknowledgments while the route worker is processing a request and keeps pending requests outside the bounded cache of completed replies. It preserves replies through a device reconnect to the same host generation. The route worker translates a new send into the next sequence. Host operation IDs retain the existing host lifetime, while native sequence fencing persists in the route registry.
- Tool permissions use a session-level native ruleset, including when the native project or selected agent allows tools. Once and reject are the controller's two choices. The first controller disables native questions and delegated tasks because those use separate request or session ownership. The host supplies a single-use token that is scoped to the pending ask and retired when the turn ends. An ambiguous native answer is never retried using the old token. Pending approvals and commands stay in memory.
- Stop is scoped to the current receipt. A pending `prompt_async` can acknowledge before the native run is registered. The controller waits for stock to report an active run or produce the corresponding assistant record before relying on abort. A timeout leaves the receipt fenced. The host then requires native idle and resolves any leftover permission for that exact session. On a new native child the host reconciles an interrupted receipt against durable native history before allowing work again.

## Reconciliation

The native SSE stream promptly announces changes. It is volatile and can drop events. Subscribe before the first read, and use authoritative native status, history and permission reads after reconnect. The controller emits OX `SessionView`, `Timeline` and `AskView` from those reads. Timeline identities retain the stock message and part identity so repeated snapshots or paging can replace and deduplicate the same records. The newest snapshot is bounded to 50 native messages, and a separate bounded page call returns an opaque older cursor supplied by stock.

A terminal assistant must belong to the exact submitted user message. With default stock behavior, rejecting a pending tool can end a turn with the tool rejection marker observed in OpenCode 1.19.6. Host restart retires unfinished approval tokens and preserves already completed transcript data. An immediate Stop can leave the original user message in history. Stock may combine it with the next adjacent user message in the upstream conversation, while the controller still reconciles the new assistant to the new receipt.

The host runs controller I/O on one route worker with a bounded command queue. It opens one native event feed for the owner before the first send. In the stock 1.19.6 test, OpenCode emitted in-progress text before it persisted the completed part to history. The host coalesces fragments every 50 ms during an active turn and displays them only for an assistant part verified against exact native history for that turn. The feed carries no authoritative session or approval state. On loss or queue overflow, the worker discards incomplete fragments, reconciles history and resumes observing new native text parts. Approval, Stop, send receipts and final results still use the controller's exact native reads. The worker keeps its 250 ms active reconciliation as a watchdog and blocks on its command queue while idle. Benchmark active request volume and reconnect before public activation. The controller's native session IDs and permission tokens stay private. A device protocol client can reconnect to the same canonical session ID and read older pages through normal host actions.

The enrolled capability set covers start, select, send text, page history, resolve a tool approval and stop a running turn. Cross-host move stays gated until an OpenCode continuity contract has been verified. Native command menus, terminal attachment, handoff, workers and managed permission presets have no activation path in this proof. The phone keeps protocol 2 and the existing desktop runtime discovery is unchanged.

## Subprocess credentials

Hosted stock reads the route-local relay token through a private temporary config file. The relay still requires authentication and the stock child starts without `CODEOX_TOKEN`. The bootstrap fixture checks file permissions, model routing and the absence of an inherited relay or configured provider key inside an approved command. The generated file belongs to the managed helper's lifetime. The standalone launch mode retains its existing environment-based gateway configuration.

The tested 1.19.6 server reads its Basic credential from its startup environment. In a Linux fixture the default approved Bash tool, direct user shell, PTY and project-configured MCP subprocess inherited it. A managed and pre-provisioned `shell.env` hook blanked it for Bash, user shell and PTY, including one project-hook ordering. Local MCP still inherited it. Removing the environment variable during plugin initialization kept Basic authentication working in the tested server lifecycle and left credentials inherited by PTY. Stock attempted dependency installation before loading a first-use plugin unless the fixture pre-provisioned its dependency metadata. These checks log presence only. [Shell tool](https://github.com/anomalyco/opencode/blob/v1.19.6/packages/opencode/src/tool/shell.ts) · [PTY handler](https://github.com/anomalyco/opencode/blob/v1.19.6/packages/opencode/src/server/routes/instance/httpapi/handlers/pty.ts) · [Local MCP transport](https://github.com/anomalyco/opencode/blob/v1.19.6/packages/opencode/src/mcp/index.ts)

Project plugins run inside the stock process. Public activation requires an explicit project configuration policy and evidence for the Basic credential boundary across every enabled subprocess launcher. It also requires desktop and phone lifecycle and reconnect testing and review of the capability and setup flow. Additional verification should exercise `prompt_async` failures before a durable assistant record and the current bounded signal for ambiguous cancellation.

## Verification

Build the opt-in test executable.

```sh
cargo test --manifest-path desktop/Cargo.toml -p ox-host --test opencode_stock_runtime --no-run
```

Run it against an official stock binary and an OX CLI containing the managed host helper. A fixed release can serve as a regression reference alongside the current stable release. The official 2.4.2 binary started a separate local server in the isolated test and returned HTML at the health endpoint this adapter requires. OX reported that incompatible health response and kept the route closed before creating a session. [OpenCode 2.4.2](https://github.com/anomalyco/opencode/releases/tag/v2.4.2)

```sh
python3 desktop/tests/opencode_host_runtime.py STOCK OX TEST_BINARY
```

The fixture uses a mock localhost Messages provider. Its fixture project allows Bash and file read, so a native approval appears only if the host session's permissions actually override the permissive defaults. The test exercises send receipt duplication, stream loss with transcript recovery, history pages, exact approval matching, tool allow and reject, stop during a tool request and immediately after send, session restart and a provider failure. Only fixture files are read or written.


The host socket fixture also exercises enrollment and the devices' existing action protocol. Build the session host from this branch, then run the fixture with the same stock OpenCode and gateway helper.

```sh
python3 desktop/tests/host_opencode_session.py STOCK OX OX_HOST
```
