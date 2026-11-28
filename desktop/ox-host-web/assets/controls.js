// Render only confirmed host projections. An accepted request may still be awaiting its native runtime.
const $ = (id) => document.getElementById(id);
const modes = {
  read_only: "Read only",
  workspace: "Workspace",
  auto_review: "Auto-review",
  yolo: "YOLO",
  configured: "Configured",
  default: "Default",
  auto: "Auto",
  accept_edits: "Accept edits",
  plan: "Plan mode",
  bypass_permissions: "YOLO · bypass permissions",
};
const bypass = (value) => value === "yolo" || value === "bypass_permissions";
const button = (label, click) => {
  const b = document.createElement("button");
  b.type = "button";
  b.textContent = label;
  b.onclick = click;
  return b;
};
function choices(select, rows, value) {
  const signature = JSON.stringify([rows, value]);
  if (select.dataset.selection === signature) return;
  select.dataset.selection = signature;
  select.replaceChildren(...rows.map(([v, l]) => new Option(l, v)));
  if (rows.some(([v]) => v === value)) select.value = value;
}
function field(parent, id, label, rows, value, enabled, change) {
  let box = $(id + "-field");
  if (!box) {
    box = document.createElement("label");
    box.id = id + "-field";
    box.append(label);
    const select = document.createElement("select");
    select.id = id;
    box.htmlFor = id;
    box.append(select);
    parent.append(box);
  }
  box.hidden = !rows.length;
  const select = $(id);
  choices(select, rows, value);
  select.disabled = !enabled;
  select.onchange = () => {
    const target = select.value;
    select.value = value;
    change(target);
  };
}
function nativeChoices(schema) {
  const rows = new Map(Object.entries(schema.session_commands ?? {}));
  rows.set("/status", "Session status");
  if (schema.methods?.includes("thread/compact/start"))
    rows.set("/compact", "Compact context");
  if (schema.goals && schema.methods?.includes("thread/goal/get"))
    rows.set("/goal", "Session goal");
  for (const cmd of ["/terminal", "/new", "/resume", "/speed", "/rename"])
    rows.delete(cmd);
  return [...rows]
    .map(([cmd, hint]) => [cmd, hint ? cmd + " · " + hint : cmd])
    .sort(([a], [b]) => a.localeCompare(b));
}
function usage(state, timeline, act, enabled) {
  const usage = timeline?.detail?.usage;
  const host = state?.detail?.usage;
  const entries = [];
  if (usage?.total) {
    const t = usage.total;
    entries.push(
      (t.totalTokens ?? 0).toLocaleString() +
        " tokens · " +
        (t.inputTokens ?? 0).toLocaleString() +
        " input · " +
        (t.outputTokens ?? 0).toLocaleString() +
        " output",
    );
    if (usage.modelContextWindow)
      entries.push(
        Math.round(
          (100 * (usage.last?.totalTokens ?? 0)) / usage.modelContextWindow,
        ) + "% context used",
      );
  }
  for (const [key, bucket] of Object.entries(host?.limits ?? {})) {
    for (const window of [bucket.primary, bucket.secondary].filter(Boolean)) {
      entries.push(
        (bucket.limitName || key) +
          " · " +
          window.usedPercent +
          "% used" +
          (window.resetsAt
            ? " · resets " +
              new Date(window.resetsAt * 1000).toLocaleTimeString()
            : ""),
      );
    }
  }
  if (host?.error) entries.push(host.error);
  const box = $("session-usage");
  const stamp = JSON.stringify([entries, enabled, host?.loading]);
  if (box.dataset.state === stamp) return;
  box.dataset.state = stamp;
  box.replaceChildren();
  for (const text of entries) {
    const p = document.createElement("p");
    p.textContent = text;
    box.append(p);
  }
  if (enabled) {
    const refresh = button("Refresh account limits", () =>
      act("read_account_limits"),
    );
    refresh.disabled = !!host?.loading;
    box.append(refresh);
  }
}
export class SessionControls {
  constructor() {
    this.reset();
    $("access-mode").onclick = () => {
      this.show(true);
      $("permission").focus();
    };
    $("open-settings").onclick = () => this.show($("session-settings").hidden);
    $("permissions-form").onsubmit = (e) => {
      e.preventDefault();
      const { id } = this.ctx;
      const value = $("permission").value;
      if ($("apply-permission").disabled || !this.allowed.includes(value))
        return;
      // Choosing YOLO and applying it is the confirmation. The grant is offered with it.
      const grant = id !== null && bypass(value) && $("yolo-grant").checked;
      this.apply(
        id === null ? { set_next_access: value } : { set_access: [id, value] },
        "permission",
        grant ? () => this.ctx.act({ yolo_grant: [id, true] }, () => {}) : undefined,
      );
    };
    $("permission").onchange = () => this.render();
    $("typed-native-command").oninput = () => $("typed-native-command").setCustomValidity("");
    $("native-command").onchange = () => {
      this.render();
      if (!$("typed-native-command").hidden) $("typed-native-command").focus();
    };
    $("native-command-form").onsubmit = (e) => {
      e.preventDefault();
      const cmd = $("native-command").value;
      if (
        !this.commandRows.some(([v]) => v === cmd) ||
        !this.enabled("native_commands")
      )
        return;
      const args = $("command-args").value.trim();
      const text = cmd === "custom" ? $("typed-native-command").value.trim() :
        (args ? cmd + " " + args : cmd);
      if (!/^\/[^\s/]/.test(text)) {
        $("typed-native-command").setCustomValidity("Enter a slash followed by the command name.");
        $("typed-native-command").reportValidity();
        return;
      }
      this.apply({ native_command: [this.ctx.id, text] }, "command");
    };
  }
  reset() {
    this.ctx = null;
    this.caps = [];
    this.schema = {};
    $("composer-models").hidden = true;
    $("composer-status").hidden = true;
    this.pending = new Set();
    this.error = "";
    this.actionStamp = null;
    $("session-controls").hidden = true;
    this.show(false);
    $("command-args").value = "";
    $("typed-native-command").value = "";
    $("typed-native-command").setCustomValidity("");
    $("native-command").replaceChildren();
    delete $("native-command").dataset.selection;
    $("permission").removeAttribute("data-selection");
  }
  show(open) {
    $("session-settings").hidden = !open;
    $("open-settings").setAttribute("aria-expanded", String(open));
    $("access-mode").setAttribute("aria-expanded", String(open));
    this.render();
  }
  enabled(cap) {
    return !!this.ctx?.ready && this.caps.includes(cap);
  }
  update(ctx) {
    this.ctx = ctx;
    this.render();
  }
  apply(action, kind, after) {
    const ctx = this.ctx,
      pending = this.pending;
    if (!ctx?.ready || pending.has(kind)) return;
    this.error = "";
    ctx.notice();
    pending.add(kind);
    this.render();
    const finish = (ok, msg) => {
      if (this.pending !== pending) return;
      pending.delete(kind);
      if (!ok)
        this.error =
          ctx.routesLabel(ctx.scope.route) +
          " · " +
          (msg || "Disconnected before confirmation.");
      if (ok && after) after();
      this.render();
    };
    if (!ctx.act(action, finish)) finish(false);
  }
  render() {
    if (!this.ctx) return;
    const { state, id, row, scope, ready } = this.ctx,
      draft = id === null;
    const detail = state?.detail ?? {},
      own = row?.detail ?? {};
    const present = !!state && (draft || !!row);
    $("session-controls").hidden = !present;
    if (!present) {
      $("composer-models").hidden = true;
      $("composer-status").hidden = true;
      this.schema = {};
      this.caps = [];
      return;
    }
    this.caps = (draft ? state.capabilities : row.capabilities) ?? [];
    const confirmed = draft ? detail.applied_draft_access : own.access;
    const planned = draft ? detail.next_access : own.resume_access;
    const current = confirmed ?? planned;
    const badge = $("access-mode");
    badge.textContent = confirmed
      ? (modes[confirmed] ?? confirmed)
      : planned
        ? (modes[planned] ?? planned) + (draft ? " on start" : " on resume")
        : "Permission mode";
    badge.dataset.bypass = String(bypass(confirmed));
    // An active YOLO grant is part of the chip, so its state is visible without opening settings.
    const grant = own.yolo_grant === true && bypass(confirmed);
    badge.dataset.grant = String(grant);
    if (bypass(confirmed)) {
      badge.textContent = grant ? "YOLO + workers" : "YOLO";
      badge.setAttribute(
        "aria-label",
        modes[confirmed] + ", change permission mode",
      );
    } else {
      badge.removeAttribute("aria-label");
    }
    // A worker's mode stays within its recorded ceiling. The host enforces this too.
    // A handoff target belongs to the operator and has no such bound.
    const ceiling =
      own.origin?.relation === "handoff" ? null : own.origin?.ceiling;
    const order = ["read", "write", "classifier", "yolo"];
    const levelOf = (x) =>
      order.indexOf(
        {
          read_only: "read",
          default: "read",
          plan: "read",
          workspace: "write",
          accept_edits: "write",
          auto_review: "classifier",
          auto: "classifier",
          yolo: "yolo",
          bypass_permissions: "yolo",
        }[x],
      );
    const withinCeiling = (x) =>
      !ceiling || (levelOf(x) >= 0 && levelOf(x) <= order.indexOf(ceiling));
    $("yolo-grant-row").hidden = draft || !bypass($("permission").value);
    this.allowed = (own.access_choices ?? detail.access_choices ?? []).filter(
      (x) => Object.hasOwn(modes, x) && withinCeiling(x),
    );
    // Thread actions live in the conversation menu, so they render whether or not settings are open.
    this.threadActions();
    const settings = (draft ? detail.default_options : own.options) ?? {};
    const schema = (draft ? null : own.command_caps) ?? detail.commands ?? {};
    this.schema = schema;
    const model = settings.model || schema.model || "";
    const models = schema.models ?? {};
    const route = own.route ?? scope.route,
      billing = route.billing;
    const compatible = (id) =>
      typeof billing === "string" ||
      billing.api === id ||
      (scope.route.runtime === "codex" && detail.provider !== "codeox") ||
      (id.includes("/") && billing.api.includes("/") && id.split("/")[0] === billing.api.split("/")[0]);
    const modelRows = Object.entries(models)
      .filter(([k]) => compatible(k))
      .sort(([a, x], [b, y]) => (x.rank ?? Infinity) - (y.rank ?? Infinity) || a.localeCompare(b))
      .map(([k, v]) => [
        k,
        // The default entry's label already names the model it resolves to.
        v.label && v.label !== k
          ? k === "default"
            ? v.label
            : v.label + " · " + k
          : k,
      ]);
    const edit =
      modelRows.length > 0 &&
      this.enabled("model_options") &&
      !own.changing &&
      !this.pending.has("options");
    const change = (choice) =>
      this.apply({ model_options: [id, choice] }, "options");
    $("composer-models").hidden = !modelRows.length && !model;
    // Keep the confirmed model visible even if discovery hasn't returned its row yet.
    if ((model || modelRows.length) && !modelRows.some(([id]) => id === model))
      modelRows.unshift([model, model || "Choose model"]);
    const selectModel = (v) => { if (v) change({ model: v }); };
    field($("composer-models"), "composer-model", "Model", modelRows, model, edit, selectModel);
    const timeline = state.transcripts[id];
    const status =
      this.error ||
      timeline?.detail?.command_notice ||
      detail.notice ||
      detail.error;
    $("control-status").hidden = !status;
    $("control-status").textContent = typeof status === "string" ? status : "";
    $("composer-status").hidden = !status || !$("session-settings").hidden;
    if ($("composer-status").textContent !== (status || ""))
      $("composer-status").textContent = status || "";
    if ($("session-settings").hidden) return;
    const options = this.allowed.map((v) => [v, modes[v]]);
    if (!this.allowed.includes(current))
      options.unshift([
        "",
        current ? (modes[current] ?? current) : "Choose a mode",
      ]);
    choices($("permission"), options, current ?? "");
    $("permission").disabled =
      !this.enabled("access") || this.pending.has("permission");
    $("apply-permission").disabled =
      $("permission").disabled ||
      !this.allowed.includes($("permission").value) ||
      $("permission").value === current;
    const parent = $("model-fields"),
      activeModel = models[model];
    field(parent, "session-model", "Model", modelRows, model, edit, selectModel);
    const efforts = (activeModel?.efforts ?? []).map((v) => [
      v.reasoningEffort,
      v.reasoningEffort,
    ]);
    field(
      parent,
      "session-effort",
      "Reasoning effort",
      efforts,
      settings.effort ?? activeModel?.default_effort ?? "",
      edit,
      (v) => change({ effort: v }),
    );
    const speeds = schema.speed
      ? [
          ["", "Default"],
          ...(schema.catalog?.[model] ?? []).map((v) => [v.id, v.name]),
        ]
      : [];
    field(
      parent,
      "session-speed",
      "Speed",
      speeds,
      settings.tier ?? "",
      edit,
      (v) => change({ speed: v || null }),
    );
    this.commandRows = nativeChoices(schema);
    if (scope.route.runtime === "claude")
      this.commandRows.push(["custom", "Enter a Claude command…"]);
    $("native-command-form").hidden =
      !this.caps.includes("native_commands") || !this.commandRows.length;
    const commandMenu = $("native-command");
    const previous = commandMenu.value;
    choices(commandMenu, this.commandRows, undefined);
    if (this.commandRows.some(([value]) => value === previous)) commandMenu.value = previous;
    const custom = commandMenu.value === "custom";
    $("typed-native-command").hidden = !custom;
    $("typed-native-command").disabled = !custom;
    $("command-args").hidden = custom;
    $("native-command-form").querySelector("button").disabled =
      !ready || this.pending.has("command");
    const readLimits =
      scope.route.runtime === "codex" &&
      billing === "subscription" &&
      schema.methods?.includes("account/rateLimits/read");
    usage(
      state,
      timeline,
      (action) => this.apply(action, "usage"),
      !!readLimits && ready,
    );
  }
  threadActions() {
    const { id, ready, archived, pinned, color, togglePin, cycleColor } =
      this.ctx;
    const stamp = JSON.stringify([
      id,
      ready,
      this.caps,
      this.pending.has("thread"),
      pinned,
      color,
    ]);
    if (this.actionStamp === stamp) return;
    this.actionStamp = stamp;
    const container = $("thread-actions");
    container.replaceChildren();
    if (!id) return;
    if (togglePin) {
      container.append(
        button(pinned ? "Unpin thread" : "Pin thread", togglePin),
      );
    }
    if (cycleColor) {
      const colorName = color
        ? color.charAt(0).toUpperCase() + color.slice(1)
        : "None";
      container.append(button(`Colour · ${colorName}`, cycleColor));
    }
    for (const [cap, label] of [
      ["end", "End session"],
      ["rename", "Rename"],
      ["archive", "Archive"],
      ["fork", "Fork"],
      ["generate_title", "Generate title"],
    ]) {
      if (!this.caps.includes(cap)) continue;
      const b = button(label, () => {
        if (cap === "rename") {
          const value = window
            .prompt("New session name", this.ctx.row.title)
            ?.trim();
          if (!value) return;
          this.apply({ rename_thread: [id, value] }, "thread");
        } else if (cap === "archive") {
          this.ctx.confirm("Archive this session?", "Archive", () =>
            this.apply({ archive_thread: id }, "thread", archived),
          );
        } else if (cap === "end") {
          this.endSession();
        } else
          this.apply(
            cap === "fork" ? { fork_thread: id } : { generate_title: id },
            "thread",
          );
      });
      b.disabled = !ready || this.pending.has("thread");
      container.append(b);
    }
  }
  // Ending a turn or a worker that is running, or an approval that waits, asks first.
  endSession() {
    const { id, state, row } = this.ctx;
    const live = (x) => ["running", "needs_input"].includes(x.status);
    const workers = (state?.sessions ?? []).filter(
      (x) => x.parent === id && live(x),
    ).length;
    const busy =
      workers > 0 ||
      live(row ?? {}) ||
      !!state?.asks?.some((a) => a.thread === id);
    const run = (force) =>
      this.apply({ [force ? "force_end_session" : "end_session"]: id }, "thread");
    if (!busy) return run();
    const who = workers
      ? ` and ${workers} worker${workers === 1 ? "" : "s"}`
      : "";
    this.ctx.confirm(
      `End session${who}? A running turn stops. History is kept.`,
      "End session",
      () => run(true),
    );
  }
}
