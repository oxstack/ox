# Verification and performance

[Quickstart](../README.md) · [Compatibility matrix](compatibility.md)

## Reproduce the checks

Run these from the repository root. The stock-client fixtures use disposable homes,
dummy credentials and loopback vendor and Brave servers. They do not need API keys.

```sh
cargo fetch --locked
cargo test --locked --offline
cargo build --release --locked --offline
python3 tests/fixtures/setup_selector.py

CODEOX_TEST_CODEX_BIN=/path/to/codex-0.171.0 \
  cargo test --locked --offline --features test-brave --test stock_codex_smoke -- \
    --ignored --test-threads=1

OX_TEST_CLAUDE_BIN=/path/to/claude-2.1.330 \
  cargo test --locked --offline --features test-brave --test stock_claude_smoke -- \
    --ignored --test-threads=1

CODEOX_TEST_CODEX_BIN=/path/to/codex-0.171.0 OX_TEST_CLAUDE_BIN=/path/to/claude-2.1.330 \
  cargo test --locked --offline --test shared_gateway -- --ignored --nocapture

CODEOX_TEST_CODEX_BIN=/path/to/codex-0.171.0 OX_TEST_CLAUDE_BIN=/path/to/claude-2.1.330 \
  cargo test --locked --offline --test shared_codex_home -- \
    --ignored --test-threads=1

CODEOX_TEST_CODEX_BIN=/path/to/codex-0.171.0 OX_TEST_CLAUDE_BIN=/path/to/claude-2.1.330 \
  python3 tests/fixtures/background_workers.py

CODEOX_TEST_CODEX_BIN=/path/to/codex-0.171.0 \
  cargo test --locked --offline --bin ox stock_codex_emits_web_run -- --ignored
```

The setup selector check uses a POSIX pseudoterminal and Python 3.
It runs the release binary and checks which stock client command each choice
would invoke, including reuse of existing native sign-ins. The Codex auth
fixture needs Python 3 and OpenSSL for a throwaway HTTPS server. It compares the
native profile before and after a relay turn, including logs and databases
whose paths are set in the native config. The stock Claude Code fixture
compares its native profile before and after account detection, a relay turn,
and a resume.

The stock Codex and Claude Code worker fixtures each make a real MCP call from
the stock client to an API provider fixture. They check that the worker receives
only the delegated task and its provider credential, and that the reply reaches
the main model over its original route. [API workers](configuration.md#subscription-main-session-with-api-workers)

The background worker fixture starts a stock Codex goal and a stock Claude
Code workflow through MCP, polls for results, exercises a workspace edit, and
cancels a running job. Set `OX_TEST_WORKER_HOLD_SECONDS=95` to check a goal
that takes longer than the short text tool's timeout.
The coding fixtures need working stock-client sandbox support and permission to bind
loopback sockets. See the [Codex fixtures](../tests/fixtures/stock_codex/README.md)
and [Claude Code fixtures](../tests/fixtures/stock_claude/README.md) for details.

`cargo test` also runs the installer against a local archive: a fresh install,
an upgrade, a wrong checksum, a missing build and an unknown option. It uses
`file://` URLs and needs no network. To lint the installer the way CI does:

```sh
sh -n install.sh && bash -n install.sh && shellcheck install.sh
```

GitHub Actions runs the offline checks and packages native binaries for both
architectures on Linux and macOS. Each platform runs the setup selector from the
packaged binary, tests updater downloads and atomic replacement over loopback,
and installs its own archive with `install.sh` twice (fresh, then upgrade). See
[install options](configuration.md#install-options).

### How releases are published

For maintainers. Both publishing jobs run only after both stock-client tracks
and all four platform builds pass.

- **Dev.** Every push to `main` replaces the pre-release tagged `dev`. CI
  uploads the new files to a hidden draft first and then swaps it in, so the
  previous dev build stays downloadable until the new one is complete. The job
  then installs the published build with `sh install.sh --dev` and checks its
  version. If a dev publish fails, use **Run workflow** on `main` to publish
  again. A re-run of an older commit does not replace a newer dev build.
- **Stable.** Pushing a tag `vX.Y.Z` publishes that release and marks it as
  the latest. The tag sets the version the binaries report. `Cargo.toml` does
  not need to match it.
- **Version numbers.** A dev build is the highest stable version with the
  patch raised by one, then `-dev.` and the workflow run number. With no
  stable release it is `0.0.1-dev.N`.

Pull-request runs build and test the same binaries and keep them as workflow
artifacts. They publish nothing. A tag-protection rule or ruleset that covers
the `dev` tag, or immutable releases, would make the dev job fail at its swap
step.

## Live smoke checks

The Linux ARM64 build returned a short, exact reply through both gateway
frontends with OpenAI Responses, Anthropic Messages (`2026-10-21`), OpenRouter
Responses, OpenCode Go Chat Completions and Alibaba Cloud Token Plan Chat
Completions. See [provider setup](configuration.md#add-a-provider) for endpoint
details.

A separate tmux walkthrough ran the installed Codex CLI 0.171.0 and Claude Code
2.1.330 through `ox codex` and `ox claude`. Both clients completed live turns
with Alibaba Token Plan, Anthropic, OpenCode Go, and OpenRouter. Claude Code
also completed a turn with OpenAI. The same OpenAI key received an organization
access error from the OpenAI API when used through Codex.

Both interactive clients replied with the selected Alibaba alias. Codex's
`/model` picker listed all five configured models. Claude Code showed a subset
in its stock picker, and a qualified `--model` alias selected the other routes.
A separate turn in each client read a disposable README through its file tools,
returned the marker text, and left the file intact.

Both clients used live Brave search and returned the official Rust Book link.
Codex used the Alibaba route and Claude Code used the Anthropic route. These
checks ran with disposable client homes and private local credentials. CI
uses loopback vendors and Brave fixtures.

### Antigravity ACP

The official Linux x86_64 Antigravity CLI 1.5.19 advertised its stdio server with `agy acp --help` and accepted ACP `initialize` using OX’s client identity. The signed-out probe used an isolated temporary profile. Session creation, listing, loading and resume returned `Authentication required`. Authenticated requests require a Google sign-in on the host. [Google's ACP documentation](https://antigravity.google/docs/cli/acp/)

The host fixture exercises a Claude Code plan handoff, mode selection, permission replies, cancellation, and history replay over ACP. Pass the Linux Antigravity CLI as the third argument to exercise the real signed-out launch through `ox-host` as well.

```sh
python3 desktop/tests/host_antigravity_session.py /path/to/ox-host /path/to/ox /path/to/agy
```

### Standalone OpenCode

With stock OpenCode 1.19.6 installed, run the local provider fixture against
the candidate OX binary:

```sh
cargo build --locked --bin ox
python3 tests/fixtures/stock_opencode.py target/debug/ox "$(command -v opencode)"
```

The fixture checks model listing, streamed Messages, Responses and Chat
Completions, a stock read tool call, resume, and interrupt. It uses a temporary
project and separate XDG roots. No live vendor account is required.

## Client updates

ox launches the `codex` or `claude` executable on `PATH` with its launch-scoped
provider settings and relay credential. Updating either client takes effect on
the next launch.

The [CI workflow](https://github.com/oxstack/ox/actions/workflows/ci.yml)
runs the same coding, tool, search, resume and auth scenarios against two inputs:

- **Baseline** uses checksum-pinned Codex 0.171.0 and Claude Code 2.1.330 on PRs and
  pushes to `main`. It keeps a repeatable compatibility reference.
- **Latest** downloads the current official Codex release and Claude Code’s `latest`
  channel using their published SHA-256 values. It runs on PRs, pushes to `main`,
  and daily on the default branch. Each run records the resolved versions in its job
  summary. Use **Run workflow** to check for a new upstream release on demand.

A fixture that fails against a new client turns that job red. Check the Actions
logs for the failing scenario and client version. To reproduce with your installed
client, replace the fixture paths above with the output of `command -v codex` or
`command -v claude`. The tests exercise the behavior of the supplied binary.

Enable **Actions → Only notify for failed workflows** in your GitHub notification
settings for failure alerts. GitHub sends scheduled-run notifications to the account
associated with the schedule. [GitHub Actions notifications](https://docs.github.com/en/actions/concepts/workflows-and-actions/notifications-for-workflow-runs)

## How ox stays fast

ox is on the path of every token, so the design goal is to add nothing you can
notice.

- **One process, one thread.** A single-threaded async runtime. There is no
  channel hop between reading a token from the provider and writing it to the
  client, and no polling loop while idle.
- **Three system calls per token.** Wait, read, write. Tokens are never
  batched or held for a timer.
- **Pass-through where the formats match.** Codex on a Responses API and
  Claude Code on a Messages API have their streams forwarded as they arrive.
- **Event-by-event translation elsewhere.** A translated stream is reframed
  one event at a time and never buffered end to end.
- **No Nagle delay.** Client sockets use `TCP_NODELAY`. Without it, a client
  that is busy rendering holds back the tokens behind it for up to 40 ms.
- **Connections are reused.** One pooled HTTPS client per process. After a
  translated stream ends, ox reads the short rest of the provider's HTTP body
  in the background so the connection returns to the pool and the next request
  skips the TCP and TLS handshake.
- **Connections survive a pause.** An unused provider connection is kept for
  five minutes, with TCP keep-alive every 30 seconds so a NAT gateway does not
  drop it while you read or type.
- **HTTP/2 where the provider offers it.** Parallel requests, such as a
  background worker, a classifier and the main conversation, share one
  connection. A provider that only speaks HTTP/1.1 is used as before. Set
  `OX_HTTP1=1` to force HTTP/1.1 for a provider or proxy with a broken HTTP/2
  implementation.
- **The first prompt does not wait for a handshake.** Once the launch model
  is known, ox opens the connection to its provider while the client starts.
  Model discovery already does this for providers that list their models; for
  a provider with `discovery = false`, ox sends one request without
  credentials to open the connection.
- **Provider prompt caches are used.** Both clients replay the whole
  conversation on every turn, so a cache miss costs seconds and the full
  input price. What ox does on each route:

  | Client | Provider API | Caching |
  | --- | --- | --- |
  | Codex | Responses | Passed through unchanged, with Codex's `prompt_cache_key`. |
  | Codex | Messages | ox marks the system prompt and the end of the conversation as cache breakpoints. Without them the Messages API caches nothing. |
  | Codex | Chat Completions | The provider's automatic prefix cache. `prompt_cache_key` is not sent, because some providers reject it. |
  | Claude Code | Messages | Passed through with Claude Code's own cache markers. |
  | Claude Code | Responses | The provider's automatic prefix cache, with the session ID as `prompt_cache_key`. |
  | Claude Code | Chat Completions | The provider's automatic prefix cache. |

  On every translated route the request for a later turn starts with exactly
  what the earlier turn sent, which is what a prefix cache needs. Claude
  Code's billing line, whose fingerprint changes with each conversation, is
  left out of translated requests so sessions, subagents and workers share
  one cached system prompt and tool list. On OpenRouter, which caches Claude
  models only on request, ox adds the top-level `cache_control` field to
  Responses and Chat Completions requests.
- **Parallel startup.** `ox codex` loads Codex's bundled model catalog while
  provider model lists are fetched, so a launch waits for the slower of the
  two and not their sum.
- **Small release build.** Whole-program optimisation and no symbol table.
  This makes the download and memory use smaller. It did not measurably change
  speed. Panics still unwind, so one failed connection cannot take the relay
  down.
- **Nothing extra on the request path.** No request logging and no disk
  writes while relaying.

Translation copies the request body: a 1 MB conversation adds about 3 to 5 ms
before its first byte in the measurement below. HTTP/2, the connection warm-up
and the cache markers are covered by unit tests but have not been measured
against a live provider, because CI makes no public inference requests.

These tests fail if a change holds tokens back or stops reusing connections.
They need no timing thresholds and run in the normal test suite:

```sh
cargo test --locked --bin ox -- \
  a_translated_token_is_forwarded_without_waiting_for_the_next_chunk \
  a_responses_token_is_forwarded_without_waiting_for_the_next_chunk \
  a_finished_translated_stream_keeps_its_upstream_connection \
  accepted_relay_connections_disable_nagle \
  the_bundled_catalog_loads_while_providers_are_discovered
```

## Benchmark a release on Linux

```sh
cargo fetch --locked
cargo build --release --locked --offline
ox_release_copy=$(mktemp -d)
cp target/release/ox "$ox_release_copy/ox"

CODEOX_BENCH_BIN="$ox_release_copy/ox" \
  cargo test --release --locked --offline --test relay_benchmark -- --ignored --nocapture

OX_BENCH_CLIENT=claude CODEOX_BENCH_BIN="$ox_release_copy/ox" \
  cargo test --release --locked --offline --test relay_benchmark -- --ignored --nocapture
```

The two runs take about six minutes in total. For each API the harness samples
60 seconds of warmed idle memory and CPU, 40 paired first-event times against the same
loopback mock, and peak memory with eight concurrent streams. It measures the relay
process alone and excludes real model and network latency.

For a quicker pass of about one minute per client, shorten the idle window and
the stream hold. Use the full run for numbers you intend to publish:

```sh
CODEOX_BENCH_BIN="$ox_release_copy/ox" CODEOX_BENCH_IDLE_SECS=10 CODEOX_BENCH_STREAM_HOLD_MS=500 \
  cargo test --release --locked --offline --test relay_benchmark -- --ignored --nocapture
```

Each run ends with `RESULT: PASS (16 checks)` when every target is met.

### Current measurement

Measured on 2026-11-23 on a release build of this branch, after the latency
changes described above. The host was a 4-vCPU Intel Xeon 2.1 GHz Firecracker
virtual machine running Linux 6.18 and Ubuntu 24.04. Everything ran on
loopback against a mock provider. System calls are slow on this kind of
virtual machine, so the CPU figures are pessimistic for a laptop.

The first four rows come from the quick form of the benchmark above, for all
six client and API combinations. Both clients ended with
`RESULT: PASS (16 checks)`.

| Measure | Result |
| --- | --- |
| Added time to first event, p95 | 0.32 to 0.58 ms |
| Added time to first event, p50 | 0.10 to 0.30 ms |
| Warm idle memory (RSS) | 5.8 to 6.3 MiB |
| Idle CPU | 0.0000% of one core over 10 seconds |
| Release executable | about 5.6 MB (8.4 MB before the release profile) |

The rows below come from a separate loopback stream harness that is not part
of this repository. It streamed tokens through ox and directly, and compared
the two. They cannot be reproduced with the commands on this page. The
regression tests in [How ox stays fast](#how-ox-stays-fast) guard the same
behaviour.

| Measure | Before | After |
| --- | ---: | ---: |
| Delay added per token at 200 tokens/s, p50 | 0.06 to 0.12 ms | 0.06 to 0.12 ms |
| Delay added per token at 200 tokens/s, p95 | | at most 0.15 ms |
| Total stream time | | +0.0% |
| Delay added per token with a client busy 3 ms per read (Codex on Chat Completions), p95 | 38.7 ms | under 0.1 ms |
| New provider connections in 20 requests, each of the four translated paths | 20 | 0 |
| New provider connections in 20 requests, each of the two pass-through paths | 0 | 0 |
| Launch with a 150 ms catalog command and a 300 ms model list | 461 ms | 309 ms |
| CPU per token | | 63 to 101 µs, or 1.3 to 2.0% of one core at 200 tokens/s |
| Added time to first byte for a 1 MB request | | 2.7 to 4.6 ms |

“Before” is commit `fcf77de`. An empty cell was not measured separately.

### Earlier full run

A release build of commit `22dfc62`, from before the latency changes above, met
its memory, CPU, and added-latency targets for all six client and protocol combinations. The test ran on a Qualcomm
Oryon host with 12 logical CPUs running Linux 7.3.0-77-omarchy (aarch64) on 2026-11-23.
SHA-256 of the local executable used in the test
`ee83bf98511205f872191c2c0a5084d042eb2c883a24e610e1ca3f051dfa00eb`.

| Client | Upstream API | Added p95 first event | Warm idle RSS | Peak RSS, eight streams |
| --- | --- | ---: | ---: | ---: |
| Codex | Responses | 0.749 ms | 5.586 MiB | 5.875 MiB |
| Codex | Chat Completions | 0.765 ms | 5.672 MiB | 5.961 MiB |
| Codex | Anthropic Messages | 1.181 ms | 5.680 MiB | 5.996 MiB |
| Claude Code | Responses | 0.554 ms | 5.625 MiB | 7.750 MiB |
| Claude Code | Chat Completions | 1.478 ms | 5.664 MiB | 8.074 MiB |
| Claude Code | Anthropic Messages | 0.768 ms | 5.613 MiB | 5.926 MiB |

Idle CPU recorded zero ticks in every 60-second window. The Codex and Claude Code
request fixtures were 2,248 B and 2,469 B. Direct Chat Completions and Anthropic Messages
requests were 1,974 B and 1,692 B. The Responses, Chat Completions and Anthropic SSE
fixtures were 3,184 B, 1,695 B and 1,433 B. The mock held the eight streams for 1,500 ms.
[Benchmark fixtures](../tests/fixtures/benchmark)
