# Linux workbench performance gate

## Restored-thread launch

Use the matching packaged GUI, host, and probe on the same desktop and renderer for each side of a comparison.

```sh
ox-desk-probe gate --app /path/to/ox-desktop --ox /path/to/ox \
  --scenarios restore-thread --restore-samples 10 --out restore.json
```

Add `--wayland` on the private Sway test compositor. The seed thread is selected through the existing layout probe, which also works without a native keyboard driver on macOS.

The probe opens and saves a synthetic Codex thread in each of two private profiles (30 threads with 20 history items, and 5,000 threads with 2,000 items). It then relaunches OX without a project argument, first with the same host running and then with the host stopped before each sample. The stopped-host launches start their own private host through the normal desktop path. The profile is seeded in both cases, and filesystem pages are left to the OS.

The common monotonic clock starts before desktop process spawn. The marks record the first GPUI frame callback after the initial draw, the selected thread's composer built and allowed to take a message, the first transcript row painted inside the visible bounds, and the selected thread ready with verified history from the host. Results contain raw samples, medians and p90s. Missing marks fail the scenario. The marks can arrive in different orders, and the report leaves timing judgments to the comparison on that machine. Measure compositor presentation separately.

## Local session host measurements

The host-aware release probe starts a fresh daemon before each cold launch. It samples the desktop, daemon and their runtime children independently. These November 24 ARM64 measurements used disposable homes on Oscar’s Snapdragon X Elite laptop. The stream fixtures drive the real desktop, host and protocol with repeatable synthetic input.

| Scenario | Result | Conditions |
| --- | --- | --- |
| Cold launch | 150.33 ms p90 for Codex and 191.44 ms for Claude | Ninth-fastest of ten launches per runtime, fresh homes, warm filesystem cache. Each sample includes daemon startup and the first desktop frame. |
| Visible idle | Native Wayland recorded 0 GUI and host CPU ticks and 0 runtime-child ticks over 60 s. Claude drew 0 frames. | Xvfb recorded 0.33% of one core for the GUI and host together, 0% for their runtime child, and 62.57 GUI voluntary context switches/s. |
| Stock Claude awaiting approval | The host used 8.71 MiB RSS and 0 CPU ticks, its OX gateway used 8.08 MiB and 0 ticks, and Claude Code 2.1.331 used 206.52 MiB and 0.53% of one core. | 60 s with two local protocol clients, an outstanding native tool approval and an isolated loopback provider. |
| Hidden idle | 0 GUI draws, 3 GUI voluntary context switches, 0 host voluntary context switches and 0 host CPU ticks in 60 s. | Unmapped X11 window in Xvfb. |
| 30 opened threads | 122.31 MiB RSS and 84.07 MiB PSS for the combined GUI and host. The host contributed 11.26 MiB RSS. | Hardware-rendered Xwayland on headless GL Weston. The existing 150 MiB RSS limit now covers both processes. |
| Four Codex streams | 4.80 ms draw p95, 18.30 ms dirty-to-present p95, 17.86 ms presentation-interval p95. | 303 drains, 327 frames, four simultaneous threads at 200 deltas/s each. User input remained visible during streaming. |
| Four Claude streams | 3.30 ms draw p95, 17.81 ms dirty-to-present p95, 31.77 ms presentation-interval p95. | Native Wayland in a private Sway compositor. 211 drains and 344 frames during four overlapping streams. |
| History and Stop | A 2,000-item Codex history painted its tail in 22.32 ms using 13 of 14 viewport rows. Stop reached the Codex pipe in 0.836 ms and Claude in 0.318 ms. | Clocked at the actual pipe and viewport. The drawing budget is 16 ms, and the Stop budget is one 60 Hz frame. |
| Route reuse | One settings read and one runtime launch to open the first API route. Zero additional processes during 20 new threads, 20 thread selections and 20 warm route changes. | The ChatGPT and OX API sessions used distinct Codex processes. |

Renderer choice matters for memory comparisons. The host and GUI used 286.22 MiB RSS for the same 30 threads in Xvfb with software Vulkan before host allocator tuning. The historical G5 baseline used the hardware Xwayland configuration in the table. On glibc the host respects explicitly configured malloc settings and otherwise uses the desktop’s limit of two arenas.

Linux workflow artifacts contain the candidate Git SHA and SHA-256 of the packaged GUI, host, gateway and probe in the gate JSON. Cross-run package comparisons can use those hashes even when the release version is shared. [Session host protocol and recovery](host-notes.md)

### Paired phone and desktop

The November 24 Linux ARM64 stock-Claude run created a session on the paired phone in another project. Its row appeared on the desktop without a search or refresh. The desktop approval wrote only in that project. Both clients observed a partial reply before the provider released the final chunk. The phone selected YOLO, the desktop restored Default, and Stop on the phone cancelled the active operation.

On a separate split-response run the navigation catalog delivered four frames totaling 3,142 bytes. The subscribed thread delivered seven frames totaling 63,888 bytes, including both text chunks. The catalog contained neither text chunk. Both sockets sent zero frames during a two-second idle observation. The initial catalog and selected-scope snapshots measured 1,063 bytes and 31,857 bytes respectively.

A Wayland release probe during this change recorded 74.62 ms to paint the tail of 2,000 history items, 5.73 ms p95 draw time across four streaming threads and 0.205 ms for Stop to reach the native pipe. Presentation interval p95 was 20.63 ms and dirty-to-present p95 was 65.70 ms. The 5,000-row sidebar laid out at most 14 rows. Twenty thread starts, selections and warm route changes spawned zero additional helper processes. Packaged CI reports identify the exact binaries for each target.

## Earlier desktop-owned runtime measurements

Development measurements on Oscar’s Snapdragon X Elite laptop, November 24. These used a local release build, an isolated profile, the synthetic app-server and the Adreno X1-85 renderer. Stock Codex boundary tests are recorded separately in [codex-notes.md](codex-notes.md). The release gate remains open until the packaged candidate has run every applicable scenario.

| Scenario | Observed | Test conditions |
| --- | --- | --- |
| G1 · Cold launch | 194.9 ms, ninth of ten | Fresh homes, 30-thread fixture, warm filesystem cache, Xwayland on headless Weston. The clock starts before process launch and stops when the first-frame callback reports. |
| G2–3 · Focused idle | 0.033% of one core, 4.07 voluntary context switches/s | Native Wayland, headless Sway with the hardware renderer, 60 s. The wakeup target is 2/s. Xwayland measured 1.08% and 64.1/s with GPUI’s periodic X11 refresh timer. |
| G4 · Hidden idle | 0 draws, 246 voluntary switches over 60 s | Measured both an unmapped X11 window and a Wayland window on another workspace. Sway kept reporting the surface visible to GPUI. |
| G5 · 30 opened threads | 112,992,256 bytes GUI RSS and 77,233,152 bytes PSS | Xwayland. Server RSS was 4,296 KiB. With the previous glibc arena policy the GUI used 197,136,384 bytes. |
| G6 · Scroll 5,000 threads | Reached row 5,000, maximum 10 rows in one layout, draw p95 2.27 ms | Server-paged list driven by wheel events on Xwayland. |
| G7 · Four streams | 2.80 ms draw p95, 301 drains, 327 draws | Each thread received 200,000 bytes at 200 deltas/s. The composer retained the text typed during streaming. Dirty-to-present p95 was 18.56 ms and presentation interval p95 was 18.99 ms. |
| G8 · Paint history tail | 30.94 ms and 15 distinct history rows for a viewport plus overscan budget of 16 | Xwayland. The clock starts in the server at the reply and ends in the last row’s GPUI paint callback. |
| G9 · Process reuse | 20 new threads, 20 selections, zero spawns. 20 route switches, two processes on first API use, zero on the remaining switches. | First API use reads the configured provider with the OX helper and starts its isolated app-server. The subscription server stays connected. |
| G10 · Stop during listing | 0.135 ms from button action to server receipt | Xwayland. A pending 5,000-row reply runs concurrently. |
| G11 · Stripped executable | 42,164,616 bytes | Intermediate ARM64 release build, before the long-document rendering change. |
| G12 · Secure entry | Saved canary in the real OX key store, mode 0600, zero hits outside it | Native settings field, matching OX helper, isolated profile. The user journal returned no entries during the sample. |
| G13 · Hyprland controls | Packaged window passed at 1.25× and 1.5×. Width changed from 1,270 to 632 logical pixels with a second tile and returned to 1,270. | Workspace and output power cycles restored the session. English → Norwegian → English keymap changes succeeded. Oscar reserved physical sleep/wake for his final hardware check. |
| G14 · macOS | Recorded by the final candidate workflow on a graphical Apple Silicon runner | The macOS job archives its window result. When WindowServer, Metal and keyboard-event access are available it records startup, focused idle and memory after 30 threads. |

Run the matching probe and fake server from the same directory. Pass the packaged application and OX helper paths. The X11 driver uses Xvfb or a private Xwayland server, xdotool, ImageMagick and Tesseract. The native Wayland driver uses a private single-output Sway compositor, grim, wtype and Tesseract. Both need Python 3.11 or newer. The probe reads its thresholds from the compiled copy of `desktop/gate.toml`.

```sh
ox-desk-probe gate --wayland --app /path/to/ox-desktop --ox /path/to/ox --out report.json
```

### Packaged Linux checkpoint

Both release packages at `6fff6360f4d6b1a29989632d31957fdc3224fab4` passed the stock concurrent-session window test and G8, G9, G10 and G12. Tail paint took 20.46 ms on x86-64 and 38.71 ms on ARM64. Both built 15 distinct history rows with a budget of 16. Stop reached the pipe in 0.076 ms and 0.093 ms respectively. The runner measured zero canary hits outside the key store on both platforms. [Workflow and downloadable reports](https://github.com/oxstack/ox/actions/runs/45410195417)


The packaged ARM64 application and its matching probe at `50d60e64d14ef15ed4c4dee39e34cd5a77e6e4ba` ran G1–G13 on the laptop with native Wayland. Cold launch took 176.25 ms at p90. After 30 opened threads the GUI used 115,113,984 bytes RSS. Four streams measured 3.78 ms draw p95 and 38.73 ms dirty-to-present p95. The 2,000-item tail painted in 74.28 ms with 14 distinct rows and a budget of 15. Stop reached the pipe in 0.182 ms. The hidden window drew zero frames over 60 seconds. The informational wakeup figures remained above target at 4.07 voluntary context switches/s when focused and 246 switches/min when hidden. The restart, navigation, two-project layout and secure-entry checks passed. Hyprland restored its window after scaling, tiling, workspace changes, display power cycling and keyboard layout changes. Physical sleep/wake is reserved for Oscar. [Measured results and binary hashes](gate-arm64.json) · [Measured ARM64 package](https://github.com/oxstack/ox/actions/runs/45430986035/artifacts/14937833562)

### Reading final candidate evidence

The desktop workflow attaches native application archives, matching probes, and gate reports to the workflow run for the candidate commit. Linux CI drives both packaged architectures. The on-device Linux run also covers cold launch, focused idle, memory, and Hyprland behavior. Probe reports identify the build commit and SHA-256 of the exact binaries measured. On macOS, the archived window log records whether LaunchServices mapped a visible project window. A runner without WindowServer or Metal produces an explicit hardware-unavailable record. Physical laptop sleep/wake remains Oscar’s final check.

## Tiled-window and idle refinements

The ARM64 package at `40716e5` painted the 2,000-item tail in 75.17 ms. The hidden window drew zero frames and recorded 126 voluntary context switches in 60 seconds. Per-thread sampling on native Wayland traced 24 switches in a 12-second hidden interval to a single idle worker in `blocking` 1.7.0. The desktop lockfile uses the compatible 1.6.2 pool, which retires idle workers. The probe now reads the kernel process-wide counter so a retiring thread cannot subtract its accumulated switches from the result. Responses and approval requests wake the workbench immediately. Streamed transcript updates still coalesce at display frames.

The Linux window fixture checks plain Enter to send, Shift+Enter for a new line, and Enter during Pinyin composition. On a half-width tile the thread list opens in a drawer. The workbench uses its available pane width for the composer controls, approval sheet and review panel.
