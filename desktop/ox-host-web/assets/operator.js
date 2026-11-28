// This channel belongs to a paired operator. Its screen and input live only in this dialog.
import { deviceKey, signedOperation } from "./device.js";
const el = (id) => document.getElementById(id);
let active = null;
export async function openPhoneTerminal(
  preset,
  project = null,
  session = null,
  initialInput = "",
) {
  if (active) {
    el("operator-close").focus();
    return;
  }
  const dialog = el("operator-panel"),
    screen = el("operator-screen"),
    input = el("operator-input");
  let ws = null,
    running = false,
    finished = false,
    approved = false;
  const pending = {};
  active = pending;
  const title = {
    "native-session": "Native session terminal",
    shell: "Host terminal",
    "codex-login": "Sign in to ChatGPT",
    "claude-login": "Sign in to Claude",
    "antigravity-login": "Sign in to Antigravity",
  };
  const nativeLogin = ["codex-login", "claude-login", "antigravity-login"].includes(preset);
  const antigravityLogin = preset === "antigravity-login";
  el("operator-title").textContent = title[preset] || "Terminal";
  el("operator-state").textContent = "Verifying terminal access…";
  const visibility = el("operator-visibility");
  let echo = null;
  let visibleOverride = null;
  const showInput = (visible) => {
    input.type = visible ? "text" : "password";
    visibility.textContent = visible ? "Hide" : "Show";
    visibility.setAttribute(
      "aria-label",
      visible ? "Hide input" : "Show input",
    );
  };
  const nextPrompt = () => {
    echo = null;
    visibleOverride = null;
    showInput(false);
  };
  visibility.onclick = () => {
    visibleOverride = input.type !== "text";
    showInput(visibleOverride);
  };
  input.oninput = () => {
    if (!input.value && visibleOverride === null) showInput(echo === true);
  };
  const reset = () => {
    input.value = "";
    nextPrompt();
    screen.textContent = "";
    el("operator-review").hidden = true;
    el("operator-command").textContent = "";
    el("operator-directory").textContent = "";
    el("operator-tools").hidden = true;
    el("operator-link").hidden = true;
    el("operator-link").removeAttribute("href");
    el("operator-run").hidden = true;
  };
  reset();
  const viewport = window.visualViewport;
  const fitSheet = () => {
    dialog.style.setProperty(
      "--terminal-top",
      (viewport?.offsetTop ?? 0) + "px",
    );
    dialog.style.setProperty(
      "--terminal-height",
      (viewport?.height ?? innerHeight) + "px",
    );
  };
  fitSheet();
  viewport?.addEventListener("resize", fitSheet);
  viewport?.addEventListener("scroll", fitSheet);
  dialog.showModal();
  const send = (message) => {
    if (ws?.readyState === WebSocket.OPEN) ws.send(JSON.stringify(message));
  };
  // Measure a single fixed glyph without copying any terminal contents.
  const measure = document.createElement("canvas").getContext("2d");
  let lastRows = 0,
    lastCols = 0;
  const fitPty = () => {
    if (!running || !measure) return;
    const css = getComputedStyle(screen);
    measure.font = css.fontSize + " " + css.fontFamily;
    const width =
      screen.clientWidth -
      parseFloat(css.paddingLeft) -
      parseFloat(css.paddingRight);
    const height =
      screen.clientHeight -
      parseFloat(css.paddingTop) -
      parseFloat(css.paddingBottom);
    const cols = Math.max(
      20,
      Math.min(180, Math.floor(width / measure.measureText("M").width)),
    );
    const rows = Math.max(
      8,
      Math.min(60, Math.floor(height / parseFloat(css.lineHeight))),
    );
    if (cols !== lastCols || rows !== lastRows) {
      lastCols = cols;
      lastRows = rows;
      send({ type: "resize", cols, rows });
    }
  };
  const resize = new ResizeObserver(fitPty);
  resize.observe(screen);
  const cleanup = () => {
    if (active !== pending) return;
    active = null;
    resize.disconnect();
    viewport?.removeEventListener("resize", fitSheet);
    viewport?.removeEventListener("scroll", fitSheet);
    ws?.close();
    reset();
    el("operator-state").textContent = "";
  };
  const close = () => {
    cleanup();
    dialog.close();
  };
  el("operator-close").onclick = close;
  dialog.onclose = () => {
    if (!dialog.open) cleanup();
  };
  const unload = () => {
    ws?.close();
    if (dialog.open) close();
  };
  window.addEventListener("pagehide", unload, { once: true });
  dialog.addEventListener(
    "close",
    () => window.removeEventListener("pagehide", unload),
    { once: true },
  );
  el("operator-run").onclick = () => {
    if (approved) return;
    approved = true;
    el("operator-run").hidden = true;
    el("operator-state").textContent = "Starting…";
    send({ type: "run" });
  };
  const keys = (text) => {
    if (running) send({ type: "keys", text });
  };
  const line = () => {
    const value = input.value;
    input.value = "";
    nextPrompt();
    keys(value + "\r");
    input.focus({ preventScroll: true });
  };
  // Keep the software keyboard open when its user taps a terminal control.
  for (const button of dialog.querySelectorAll(".operator-controls button")) {
    button.onpointerdown = (event) => {
      if (document.activeElement === input) event.preventDefault();
    };
  }
  el("operator-send").onclick = line;
  input.onkeydown = (e) => {
    if (e.key === "Enter" && !e.isComposing) {
      e.preventDefault();
      line();
    }
  };
  for (const b of dialog.querySelectorAll("button[data-key]")) {
    const chars = {
      interrupt: "\x03",
      eof: "\x04",
      tab: "\t",
      escape: "\x1b",
      up: "\x1b[A",
      down: "\x1b[B",
    };
    b.onclick = () => {
      if (["interrupt", "eof"].includes(b.dataset.key)) nextPrompt();
      keys(chars[b.dataset.key]);
      if (matchMedia("(pointer: fine)").matches)
        input.focus({ preventScroll: true });
    };
  }
  try {
    const key = await deviceKey();
    if (!key) throw new Error();
    const request = JSON.stringify({ preset, project, ...(session ?? {}) });
    const auth = await signedOperation(key, "terminal", request);
    if (active !== pending) return;
    ws = new WebSocket("wss://" + location.host + "/api/operator");
    ws.onopen = () => send({ proof: auth, request });
    ws.onmessage = (e) => {
      if (active !== pending) return;
      const f = JSON.parse(e.data);
      if (f.review) {
        el("operator-directory").textContent = f.cwd;
        el("operator-command").textContent = f.review;
        el("operator-review").hidden = false;
        el("operator-state").textContent = "Review before running.";
        el("operator-run").hidden = false;
      }
      if (f.running) {
        if (!running && preset === "native-session") input.value = initialInput;
        running = true;
        el("operator-review").hidden = true;
        el("operator-tools").hidden = false;
        el("operator-state").textContent = "Connected";
        fitPty();
        const focus = matchMedia("(pointer: fine)").matches ? input : screen;
        focus.focus({ preventScroll: true });
      }
      if (typeof f.screen === "string") {
        const currentEcho = f.echo === true;
        if (currentEcho !== echo) {
          visibleOverride = null;
          echo = currentEcho;
          // Never reveal a partially entered secret when the program restores ECHO.
          showInput(currentEcho && !input.value);
        }
        screen.textContent = f.screen;
        screen.scrollTop = screen.scrollHeight;
        if (nativeLogin) {
          // Antigravity prints a long OAuth URL. The host supplies the complete
          // line separately from the terminal's wrapped screen.
          const candidates = antigravityLogin
            ? [f.login_url]
            : f.screen.match(/https:\/\/[^\s<>"']+/g) ?? [];
          for (const candidate of candidates) {
            if (typeof candidate !== "string") continue;
            let url;
            try {
              url = new URL(candidate);
            } catch {
              continue;
            }
            const native =
              preset === "codex-login"
                ? url.origin === "https://auth.openai.com" &&
                  url.pathname === "/codex/device"
                : antigravityLogin
                  ? url.origin === "https://accounts.google.com" &&
                    ["/o/oauth2/v2/auth", "/o/oauth2/auth"].includes(url.pathname) &&
                    url.search && !url.hash
                  : [
                    "https://claude.ai",
                    "https://platform.claude.com",
                    "https://console.anthropic.com",
                  ].includes(url.origin) && url.pathname.includes("/oauth/");
            if (native && !url.username && !url.password) {
              el("operator-link").href = antigravityLogin ? candidate : url.href;
              el("operator-link").hidden = false;
              break;
            }
          }
        }
      }
      if ("exit" in f || f.error) {
        running = false;
        finished = true;
        reset();
        el("operator-state").textContent =
          f.error ||
          "Process exited" +
            (f.exit === null ? "" : " with status " + f.exit) +
            ".";
        if (f.exit === 0 && nativeLogin)
          window.dispatchEvent(new CustomEvent("ox-native-login-finished", { detail: { preset } }));
      }
    };
    ws.onclose = () => {
      if (active !== pending) return;
      running = false;
      reset();
      if (!finished)
        el("operator-state").textContent =
          "Terminal disconnected. Check terminal access for this device in OX Desktop.";
    };
  } catch {
    if (active === pending)
      el("operator-state").textContent =
        "Terminal access must be enabled for this device in OX Desktop → Settings → Remote access.";
  }
}
