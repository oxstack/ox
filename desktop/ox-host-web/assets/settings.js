// The local gateway owns validation, revisions and secrets. Keep unfinished edits in this page.
const connection = [
  ["name", "Display name"],
  ["base_url", "API endpoint", "url"],
  [
    "wire_api",
    "Protocol",
    ["responses", "chat_completions", "anthropic_messages"],
  ],
  ["env_key", "Key environment variable"],
  ["discovery", "Discover models", "bool"],
  ["models_url", "Model catalog URL", "url"],
  ["max_output_tokens", "Maximum output tokens", "integer"],
];
const adapters = {
  chat_completions: [
    [
      "max_tokens_field",
      "Output token field",
      ["max_tokens", "max_completion_tokens"],
    ],
    ["chat_reasoning_effort", "Reasoning effort", ["drop", "forward"]],
    ["developer_role", "Developer role", ["system", "developer"]],
    ["include_stream_usage", "Include stream usage", "bool"],
  ],
  anthropic_messages: [
    ["anthropic_version", "Anthropic API version"],
    ["anthropic_thinking", "Thinking mode", ["off", "adaptive", "manual"]],
    ["anthropic_thinking_budget_tokens", "Thinking token budget", "integer"],
    ["output_config_effort", "Send output effort", "bool"],
  ],
};
const modelFields = [
  ["id", "Upstream model ID"],
  ["name", "Display name"],
  ["alias", "Model alias"],
  ["context_window", "Context window", "integer"],
  ["input_modalities", "Input types (comma-separated)", "list"],
  ["reasoning_efforts", "Reasoning levels (comma-separated)", "list"],
];
const searchFields = [
  ["enabled", "Enable Brave Search", "bool"],
  ["env_key", "Key environment variable"],
  ["max_results", "Maximum results", "integer"],
  ["timeout_seconds", "Timeout in seconds", "integer"],
];
const legacyFields = [
  ["name", "Display name"],
  ["model", "Model"],
  ["base_url", "API endpoint", "url"],
  ["model_catalog_url", "Model catalog URL", "url"],
  ["env_key", "Key environment variable"],
  ["wire_api", "Protocol", ["responses"]],
];

function node(tag, text, css) {
  const el = document.createElement(tag);
  if (text !== undefined) el.textContent = text;
  if (css) el.className = css;
  return el;
}
function button(label, handler, css) {
  const el = node("button", label, css);
  el.type = "button";
  el.addEventListener("click", handler);
  return el;
}
function details(label, ...children) {
  const el = node("details");
  el.append(node("summary", label), ...children);
  return el;
}
function textField(parent, label, name, type = "text") {
  const wrap = node("label", label),
    input = node("input");
  input.name = name;
  input.type = type;
  wrap.append(input);
  parent.append(wrap);
  return input;
}
function fields(parent, specs, base = {}, isNew = false) {
  const inputs = {};
  for (const [key, label, type] of specs) {
    const cell = base[key];
    const wrap = node("label", label);
    const options = type === "bool" ? ["true", "false"] : type;
    const control = Array.isArray(options) ? node("select") : node("input");
    const initial =
      cell?.type === "private"
        ? ""
        : Array.isArray(cell?.value)
          ? cell.value.join(", ")
          : String(cell?.value ?? "");
    if (Array.isArray(options)) {
      control.append(new Option("Use default", ""));
      for (const entry of new Set([...options, ...(initial ? [initial] : [])]))
        control.append(
          new Option(
            type === "bool" ? (entry === "true" ? "On" : "Off") : entry,
            entry,
          ),
        );
    } else {
      control.type =
        type === "integer" ? "number" : type === "url" ? "url" : "text";
      if (type === "integer") {
        control.min = "1";
        control.step = "1";
      }
    }
    control.name = key;
    control.value = initial;
    if (cell?.type === "private") {
      control.disabled = true;
      wrap.append(node("small", "Private value retained on the host."));
    }
    wrap.append(control);
    parent.append(wrap);
    inputs[key] = { control, initial, type };
  }
  function changes() {
    const patch = {};
    for (const [key, { control, initial, type }] of Object.entries(inputs)) {
      const value = control.value.trim();
      if (
        control.disabled ||
        (!isNew && initial === value) ||
        (isNew && !value)
      )
        continue;
      if (!value) patch[key] = null;
      else if (type === "integer") {
        if (!Number.isSafeInteger(+value) || +value < 1)
          throw Error("Enter a positive whole number.");
        patch[key] = { type: "integer", value: Number(value) };
      } else if (type === "bool")
        patch[key] = { type: "bool", value: value === "true" };
      else if (type === "list")
        patch[key] = {
          type: "list",
          value: value
            .split(",")
            .map((v) => v.trim())
            .filter(Boolean),
        };
      else patch[key] = { type: "text", value };
    }
    return patch;
  }
  return { inputs, changes };
}

export function registeredHostOrigin(value) {
  try {
    const url = new URL(value);
    if (
      url.protocol === "https:" &&
      url.hostname.endsWith(".ts.net") &&
      url.origin === value &&
      url.origin !== location.origin
    )
      return url;
  } catch {}
  return null;
}

const knownHostsKey = "ox-phone-known-hosts-v1";
export function forgetKnownHosts() {
  try { localStorage.removeItem(knownHostsKey); } catch {}
}
function knownHostAddresses() {
  try {
    const hosts = JSON.parse(localStorage.getItem(knownHostsKey) || "[]");
    return hosts.filter((host) => typeof host.name === "string" && registeredHostOrigin(host.origin));
  } catch {
    return [];
  }
}

export function setupSettings({ request, open, close, pushStatus, enablePush, login, hostName }) {
  const panel = node("section");
  panel.id = "settings";
  panel.hidden = true;
  document.querySelector("main").append(panel);
  let snapshot,
    pageId = 0,
    accountsPage,
    installPrompt,
    refreshPushPermission,
    inviteTimer,
    pendingTimer = null;
  const pushEdits = new Set();
  const localDefaults = {
    theme: "dark",
    font_size: 16,
    enter_sends: false,
    reduce_motion: false,
  };
  let local = { ...localDefaults };
  try {
    const saved = JSON.parse(localStorage.getItem("ox-phone-appearance"));
    if (
      saved &&
      ["system", "dark", "light"].includes(saved.theme) &&
      Number.isInteger(saved.font_size) &&
      saved.font_size >= 12 &&
      saved.font_size <= 24
    )
      local = {
        theme: saved.theme,
        font_size: saved.font_size,
        enter_sends: saved.enter_sends === true,
        reduce_motion: saved.reduce_motion === true,
      };
  } catch {
    /* Local display preferences are optional in a private browser. */
  }
  const systemLight = matchMedia("(prefers-color-scheme: light)");
  function applyLocal() {
    const css = document.documentElement.dataset;
    css.phoneTheme =
      local.theme === "system"
        ? systemLight.matches
          ? "light"
          : "dark"
        : local.theme;
    // Status bar and native form colours follow the active theme (house nav token).
    const light = css.phoneTheme === "light";
    document.documentElement.style.colorScheme = light ? "light" : "dark";
    document
      .querySelector('meta[name="theme-color"]')
      ?.setAttribute("content", light ? "#ffffff" : "#12131f");
    css.phoneSize = String(local.font_size);
    css.phoneReducedMotion = String(local.reduce_motion);
  }
  applyLocal();
  systemLight.addEventListener("change", applyLocal);
  window.addEventListener("ox-native-login-finished", async () => {
    const id = accountsPage;
    if (panel.hidden || id !== pageId) return;
    try {
      await load(accounts);
    } catch (err) {
      if (id === pageId && !panel.hidden) status.textContent = err.message;
    }
  });
  document.getElementById("draft").addEventListener("keydown", (event) => {
    if (
      event.key !== "Enter" ||
      event.shiftKey ||
      event.altKey ||
      event.isComposing ||
      event.keyCode === 229
    )
      return;
    if (!(local.enter_sends || event.ctrlKey || event.metaKey)) return;
    event.preventDefault();
    if (!event.repeat && !document.getElementById("send").disabled)
      document.getElementById("composer").requestSubmit();
  });
  const status = node("p", "", "phone-settings-status");
  status.setAttribute("role", "status");
  const body = node("div", undefined, "phone-settings-body");
  window.addEventListener("beforeinstallprompt", (e) => {
    e.preventDefault();
    installPrompt = e;
  });
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden) refreshPushPermission?.();
  });
  function screen(title, back = menu, backLabel) {
    pageId++;
    refreshPushPermission = null;
    clearTimeout(inviteTimer);
    clearInterval(pendingTimer);
    pendingTimer = null;
    status.textContent = "";
    body.replaceChildren();
    const heading = node("h1", title);
    heading.tabIndex = -1;
    panel.replaceChildren(
      button(
        backLabel ?? (back === close
          ? "← Sessions"
          : back === remote
            ? "← Remote access"
          : back === credentials
            ? "← API keys"
            : back === keyDestinations
              ? "← Destinations"
            : back === providers
            ? "← Providers"
              : "← Settings"),
        back,
        "back",
      ),
      heading,
      status,
      body,
    );
    heading.focus({ preventScroll: true });
    window.scrollTo(0, 0);
    return pageId;
  }
  async function readPage(id, query) {
    try {
      const value = await request(query);
      return id === pageId && !panel.hidden ? value : null;
    } catch (err) {
      if (id === pageId && !panel.hidden) status.textContent = err.message;
      return null;
    }
  }
  function action(label, work, css) {
    return button(
      label,
      async (e) => {
        const control = e.currentTarget,
          before = pageId;
        control.disabled = true;
        status.textContent = "";
        try {
          await work();
        } catch (err) {
          if (before === pageId)
            status.textContent =
              err.message || "The host could not complete this request.";
        } finally {
          control.disabled = false;
        }
      },
      css,
    );
  }
  function submit(form, label, work) {
    const control = node("button", label, "primary");
    control.type = "submit";
    form.append(control);
    form.onsubmit = async (event) => {
      event.preventDefault();
      if (control.disabled) return;
      const before = pageId;
      control.disabled = true;
      try {
        await work();
      } catch (err) {
        if (before === pageId)
          status.textContent = err.message || "Could not save settings.";
      } finally {
        control.disabled = false;
      }
    };
  }
  async function load(show = menu) {
    const before = pageId;
    const updated = await request({ action: "read_rows" });
    if (before !== pageId || panel.hidden) return;
    snapshot = updated;
    show();
  }
  async function save(requestBody, next, message = "Saved on the host.") {
    const before = pageId;
    const result = await request(requestBody);
    if (before !== pageId || panel.hidden) return;
    if (result.status === "changed_on_disk") {
      status.textContent =
        "These settings changed on the host. Your edits are still here. ";
      status.append(action("Reload host values", () => load(next)));
      return;
    }
    if (result.status !== "applied")
      throw Error("The host did not confirm this change.");
    await load(() => {
      next();
      status.textContent = message;
    });
  }
  const edit = (revision, change, next) =>
    save({ action: "edit", revision, edit: change }, next);
  function confirmAction(label, question, work) {
    const wrap = node("div", undefined, "phone-settings-confirm");
    const first = button(
      label,
      () => {
        const cancel = button("Cancel", () => {
          wrap.replaceChildren(first);
          first.focus();
        });
        wrap.replaceChildren(
          node("p", question),
          cancel,
          action(label, work, "danger"),
        );
        cancel.focus();
      },
      "quiet",
    );
    wrap.append(first);
    return wrap;
  }
  function menu() {
    screen("Settings", close);
    const groups = snapshot
      ? [
          [
            "Models & accounts",
            [
              ["Accounts", "ChatGPT, Claude Code, Antigravity and agent start modes", accounts],
              ["Starred models", "Browse catalog, star & reorder models", favorites],
              ["Providers & models", "API endpoints & static models", providers],
              ["API keys", "Manage & copy provider credentials", credentials],
              ["Web search", "Brave Search for Codex & Claude", search],
              ...(snapshot.legacy
                ? [["Legacy Codex provider", "Edit legacy connection", legacy]]
                : []),
            ],
          ],
        ]
      : [];
    groups.push(
      [
        "This phone",
        [
          [
            "Phone appearance & input",
            "Theme, text size, Enter key & motion",
            () => behaviorForm(true),
          ],
          [
            "Notifications",
            "Choose which session alerts this device receives",
            () => notificationPreferences(),
          ],
          ["Install OX", "Add OX to your Home Screen", phone],
        ],
      ],
      [
        "Host & remote",
        [
          [
            "Remote access",
            "Pair devices, mode policy & worker links",
            remote,
          ],
          [
            "Desktop appearance & input",
            "Host theme, text size & weather",
            () => behaviorForm(false),
          ],
          ["Host updates", "Release channel & automatic updates", hostUpdates],
          ...(snapshot
            ? [
                [
                  "Advanced configuration",
                  "Edit codeox.toml & favorites as text",
                  advanced,
                ],
              ]
            : []),
        ],
      ],
    );
    const nav = node("nav", undefined, "phone-settings-menu");
    nav.setAttribute("aria-label", "Settings sections");
    for (const [heading, items] of groups) {
      const sec = node("div", undefined, "phone-settings-group");
      sec.append(node("h2", heading, "phone-settings-group-title"));
      for (const [label, desc, destination] of items) {
        const btn = action(label, destination);
        btn.dataset.subtitle = desc;
        sec.append(btn);
      }
      nav.append(sec);
    }
    body.append(
      nav,
      action("Refresh host settings", () => load(), "quiet"),
    );
    for (const message of snapshot?.issues || [])
      body.append(node("p", message));
  }
  document.getElementById("open-phone-settings").onclick = async () => {
    open();
    const id = screen("Settings", close);
    status.textContent = "Loading host settings…";
    try {
      await load();
    } catch (err) {
      if (id === pageId) {
        snapshot = null;
        menu();
        status.textContent = err.message;
      }
    }
  };
  async function accounts() {
    const id = screen("Accounts");
    accountsPage = id;
    const deviceStatus = await readPage(id, { action: "devices" });
    if (!deviceStatus) return;
    const granted = deviceStatus.devices.some(
      (d) => d.id === deviceStatus.self && d.terminal === true,
    );
    if (!granted)
      body.append(
        node(
          "p",
          "To sign in from this phone, enable terminal access for this device in OX Desktop → Settings → Remote access.",
        ),
      );
    for (const [runtime, name, identity, account, cmd] of [
      [
        "codex",
        "Codex · ChatGPT",
        "ChatGPT",
        snapshot.chatgpt_account,
        "ox login chatgpt --device-auth",
      ],
      ["claude", "Claude Code", "Claude", snapshot.claude_account, "ox login claude"],
      ["antigravity", "Antigravity", "Google", "Uses the Antigravity CLI’s Google sign-in.", "agy acp --login"],
    ]) {
      const detail = node("div", undefined, "phone-settings-card");
      detail.append(
        node("h2", name),
        node(
          "p",
          typeof account === "string"
            ? account
            : account
              ? "Signed in"
              : "Signed out",
        ),
      );
      const launch = action(
        "Sign in with " + identity,
        () => login(runtime),
      );
      launch.disabled = !granted;
      detail.append(launch);
      detail.append(
        details(
          "Host terminal command",
          node(
            "p",
            "Run this command in the host terminal to use the native sign-in flow.",
          ),
          node("code", cmd),
          action("Copy sign-in command", () =>
            navigator.clipboard.writeText(cmd),
          ),
        ),
      );
      body.append(detail);
    }
    await agentsCard(id);
    if (id !== pageId || panel.hidden) return;
    body.append(
      action("Refresh accounts", () => load(accounts)),
      button("Configure API provider", providers),
    );
  }
  // How agents start and who answers their workers: host preferences shared with OX Desktop.
  async function agentsCard(id) {
    const record = await readPage(id, { action: "read_behavior" });
    if (!record) return;
    const card = node("div", undefined, "phone-settings-card");
    card.append(node("h2", "Agents"));
    const form = node("form");
    const fields = [];
    const choice = (label, name, options) => {
      const wrap = node("label", label),
        select = node("select");
      select.name = name;
      for (const [text, value] of options) select.append(new Option(text, value));
      select.value = record.behavior[name] ?? "";
      wrap.append(select);
      form.append(wrap);
      fields.push(select);
    };
    const terminal = ["Same as terminal", ""];
    choice("New Claude sessions", "claude_start", [
      terminal,
      ["Ask before edits", "default"],
      ["Auto", "auto"],
      ["Accept edits", "accept_edits"],
      ["Plan mode", "plan"],
      ["YOLO · bypass permissions", "bypass_permissions"],
    ]);
    choice("New Codex sessions", "codex_start", [
      terminal,
      ["Read only", "read_only"],
      ["Workspace", "workspace"],
      ["Auto-review", "auto_review"],
      ["YOLO", "yolo"],
    ]);
    choice("New Antigravity sessions", "antigravity_start", [
      terminal,
      ["Ask before edits", "default"],
      ["Accept edits", "accept_edits"],
      ["Plan mode", "plan"],
      ["YOLO", "yolo"],
    ]);
    form.append(
      node(
        "p",
        "Same as terminal uses the agent’s own permissions.",
      ),
    );
    choice("Worker requests", "worker_approvals", [
      ["Parent agent answers", "parent"],
      ["I answer", "operator"],
    ]);
    form.append(
      node(
        "p",
        "A parent agent answers its workers' questions, and approves their actions when its own mode runs without asking you (Auto, Auto-review or YOLO). Read-only workers and mode changes always ask you.",
      ),
    );
    submit(form, "Save agent settings", async () => {
      const behavior = { ...record.behavior };
      for (const select of fields)
        behavior[select.name] =
          select.value === "" && select.name.endsWith("_start")
            ? null
            : select.value;
      const result = await request({
        action: "save_behavior",
        revision: record.revision,
        behavior,
      });
      if (id !== pageId || panel.hidden) return;
      status.textContent =
        result.status === "applied"
          ? "Saved on the host."
          : "Settings changed on the host. Refresh accounts and try again.";
      if (result.status === "applied") record.revision = await reread();
    });
    async function reread() {
      const fresh = await request({ action: "read_behavior" });
      record.behavior = fresh.behavior;
      return fresh.revision;
    }
    card.append(form);
    body.append(card);
  }
  function providers() {
    screen("Providers");
    if (!snapshot.registry) {
      body.append(node("p", "The host could not read its provider registry."));
      return;
    }
    const list = node("div", undefined, "phone-settings-menu");
    for (const row of snapshot.registry.providers)
      list.append(
        button(row.fields.name?.value || row.id, () =>
          providerEditor(row, false),
        ),
      );
    const select = node("select");
    select.setAttribute("aria-label", "Provider template");
    select.append(new Option("Custom provider", ""));
    snapshot.presets.forEach(([name], i) =>
      select.append(new Option(name, String(i))),
    );
    const add = button("Add provider", () => {
      const row =
        select.value === ""
          ? { id: "", fields: {}, models: [] }
          : snapshot.presets[Number(select.value)][1];
      providerEditor(row, true);
    });
    const models = node("ul");
    body.append(
      list,
      select,
      add,
      action("Discover available models", async () => {
        const result = await request({ action: "models" });
        models.replaceChildren(...result.map((m) => node("li", m)));
        if (!result.length) models.append(node("li", "No models reported."));
      }),
      models,
    );
  }
  function providerEditor(row, isNew) {
    screen(
      isNew ? "Add provider" : row.fields.name?.value || row.id,
      providers,
    );
    const form = node("form");
    const id = textField(form, "Provider ID", "provider_id");
    id.required = true;
    id.value = row.id;
    const base = fields(form, connection, row.fields, isNew);
    const wire = base.inputs.wire_api.control;
    const options = {};
    for (const [protocol, specs] of Object.entries(adapters)) {
      const box = details("Protocol options");
      options[protocol] = { box, group: fields(box, specs, row.fields, isNew) };
      form.append(box);
    }
    const showProtocol = () => {
      for (const [type, { box }] of Object.entries(options))
        box.hidden = type !== wire.value;
    };
    wire.onchange = showProtocol;
    showProtocol();
    const list = details("Static models");
    const rows = [];
    function addModel(values, original, expand) {
      const box = details(values.id?.value || "New model");
      box.open = expand;
      const group = fields(box, modelFields, values, !original);
      group.inputs.id.control.required = true;
      const state = { original, group, removed: false };
      rows.push(state);
      box.append(
        button(
          "Remove model",
          () => {
            state.removed = true;
            box.remove();
          },
          "quiet",
        ),
      );
      list.insertBefore(box, add);
    }
    const add = button("Add static model", () => addModel({}, null, true));
    list.append(add);
    for (const model of row.models)
      addModel(model, isNew ? null : model.id?.value, false);
    form.append(list);
    function mutation(removed = false) {
      const patch = removed ? {} : base.changes();
      for (const [protocol, { group }] of Object.entries(options)) {
        if (protocol === wire.value) Object.assign(patch, group.changes());
        else if (!isNew && wire.value !== (row.fields.wire_api?.value || ""))
          for (const [key] of adapters[protocol])
            if (row.fields[key]) patch[key] = null;
      }
      const models = removed
        ? []
        : rows.flatMap(({ original, removed, group }) => {
            if (removed)
              return original ? [{ original, id: null, fields: {} }] : [];
            const modelId = group.inputs.id.control.value.trim(),
              changes = group.changes();
            if (!modelId) throw Error("Enter an upstream model ID.");
            return !original ||
              original !== modelId ||
              Object.keys(changes).length
              ? [{ original, id: modelId, fields: changes }]
              : [];
          });
      return {
        section: "providers",
        changes: [
          {
            row: {
              original: isNew ? null : row.id,
              id: removed ? null : id.value.trim(),
              fields: patch,
            },
            models,
          },
        ],
      };
    }
    const revision = snapshot.revisions.registry;
    submit(form, "Save provider", () => edit(revision, mutation(), providers));
    body.append(form);
    if (!isNew)
      body.append(
        action("Test saved connection", async () => {
          const message = await request({
            action: "test_provider",
            id: row.id,
            revision,
          });
          status.textContent = `Connection verified. ${message} models discovered.`;
        }),
        confirmAction(
          "Remove provider",
          "Remove this provider and its static models from OX?",
          () => edit(revision, mutation(true), providers),
        ),
      );
  }
  async function keyDestinations() {
    const id = screen("Copy API keys", credentials);
    const data = await readPage(id, { action: "key_destinations" });
    if (!data) return;
    if (data.hosts.length) {
      body.append(node("h2", "Destination host"));
      for (const host of data.hosts)
        body.append(
          action(host.name + " · " + host.origin, () =>
            reviewTransfer(host.id),
          ),
        );
    }
    const form = node("form", undefined, "phone-settings-card");
    form.autocomplete = "off";
    form.append(
      node("h2", "Pair a destination"),
      node(
        "p",
        "On the receiving host, run ox host pair or create an invitation in Remote access. This host will remember the destination for future transfers.",
      ),
    );
    const address = textField(
      form,
      "Invitation link or host URL",
      "destination",
    );
    const code = textField(
      form,
      "Pairing code (for a host URL)",
      "code",
      "password",
    );
    address.required = true;
    address.spellcheck = false;
    address.setAttribute("autocapitalize", "none");
    code.autocomplete = "off";
    submit(form, "Pair destination", async () => {
      const host = await request({
        action: "pair_key_destination",
        address: address.value.trim(),
        code: code.value.trim(),
      });
      address.value = code.value = "";
      if (id === pageId && !panel.hidden) await reviewTransfer(host.id);
    });
    body.append(form);
  }
  async function reviewTransfer(host) {
    const id = screen("Review API keys", keyDestinations);
    const review = await readPage(id, { action: "review_key_transfer", host });
    if (!review) return;
    body.append(node("h2", review.host.name), node("p", review.host.origin));
    if (!review.rows.length) {
      body.append(node("p", "This host has no configured API keys to copy."));
      return;
    }
    body.append(
      node(
        "p",
        "Anyone with access to the destination’s OX key store can use these keys and incur charges. The originals stay on this host. Revoking the pairing later leaves the copies on the destination.",
        "warning",
      ),
    );
    const inputs = [];
    const form = node("form", undefined, "phone-settings-card");
    for (const row of review.rows) {
      const label = node("label");
      const box = node("input");
      box.type = "checkbox";
      box.checked = row.selected;
      box.disabled = !row.available;
      const name =
        row.key.scope === "https://api.search.brave.com/"
          ? "Brave Search · " + row.key.name
          : row.key.name;
      label.append(
        box,
        document.createTextNode(name),
        node("small", row.key.scope + " · " + row.status),
      );
      form.append(label);
      inputs.push([box, row.key]);
    }
    const chosen = () =>
      inputs
        .filter(([box]) => !box.disabled && box.checked)
        .map(([, key]) => key);
    submit(form, "Send selected keys", async () => {
      const result = await request({
        action: "send_key_transfer",
        host,
        pairing: review.pairing,
        source: review.source,
        target: review.target,
        selected: chosen(),
      });
      if (id !== pageId || panel.hidden) return;
      credentials();
      status.textContent =
        result.message ??
        `Sent ${result.sent} ${result.sent === 1 ? "key" : "keys"} to ${review.host.name}.`;
    });
    const send = form.querySelector('button[type="submit"]');
    const selection = () => {
      send.disabled = !chosen().length;
    };
    form.addEventListener("change", selection);
    selection();
    body.append(
      form,
      action("Refresh review", () => reviewTransfer(host), "quiet"),
    );
  }
  function credentials() {
    screen("API keys");
    body.append(action("Copy keys to another host…", keyDestinations));
    if (!snapshot.credentials.length)
      body.append(
        node("p", "Configure a provider or Brave Search to add a credential."),
      );
    const revision = snapshot.revisions.keys;
    for (const key of snapshot.credentials) {
      const form = node("form", undefined, "phone-settings-card");
      form.autocomplete = "off";
      form.append(
        node("h2", key.name),
        node("p", key.scope),
        node(
          "p",
          !key.configured && key.saved
            ? "Saved · configure this endpoint to use the key"
            : key.environment
              ? "Environment key active"
              : key.saved
                ? "Saved key configured"
                : "No key configured",
        ),
      );
      const req = (value) => ({
        action: "edit_key",
        revision,
        name: key.name,
        scope: key.scope,
        value,
      });
      if (key.configured) {
        const field = textField(
          form,
          key.saved ? "Replacement key" : "New key",
          "secret",
          "password",
        );
        field.autocomplete = "new-password";
        field.maxLength = 8192;
        field.required = true;
        field.spellcheck = false;
        field.setAttribute("autocapitalize", "none");
        submit(form, key.saved ? "Replace key" : "Save key", async () => {
          await save(req(field.value), credentials);
        });
      }
      if (key.saved)
        form.append(
          confirmAction(
            "Remove saved key",
            "Remove the saved key for this destination?",
            () => save(req(null), credentials),
          ),
        );
      body.append(form);
    }
  }
  function favorites() {
    const id = screen("Starred models");
    body.append(
      node(
        "p",
        "Starred models appear first in model pickers; any available model can still be used.",
      ),
    );
    const ordered = [...snapshot.favorites];
    let catalogModels = [
      ...new Set([...snapshot.favorites, ...snapshot.models]),
    ];
    const list = node("ol", undefined, "phone-favorites");
    const catalogBox = node("div", undefined, "phone-available-models");
    const datalist = node("datalist");
    datalist.id = "phone-known-models";
    const form = node("form");
    const input = textField(
      form,
      "Filter catalog or enter model alias",
      "alias",
    );
    input.placeholder = "Search provider/model…";
    input.setAttribute("list", datalist.id);
    input.required = true;
    function draw() {
      datalist.replaceChildren(
        ...catalogModels.map((m) => new Option(m, m)),
      );
      list.replaceChildren(
        ...ordered.map((model, i) => {
          const li = node("li"),
            controls = node("div", undefined, "phone-settings-actions");
          const shift = (offset) => {
            [ordered[i], ordered[i + offset]] = [
              ordered[i + offset],
              ordered[i],
            ];
            draw();
          };
          const up = button("Move up", () => shift(-1)),
            down = button("Move down", () => shift(1));
          up.disabled = i === 0;
          down.disabled = i + 1 === ordered.length;
          controls.append(
            up,
            down,
            button("Remove", () => {
              ordered.splice(i, 1);
              draw();
            }),
          );
          li.append(node("span", model), controls);
          return li;
        }),
      );
      drawCatalog();
    }
    function drawCatalog() {
      const needle = input.value.trim().toLowerCase();
      const unstarred = catalogModels.filter(
        (m) =>
          !ordered.includes(m) &&
          (!needle || m.toLowerCase().includes(needle)),
      );
      if (!unstarred.length) {
        catalogBox.replaceChildren(
          node(
            "p",
            catalogModels.length
              ? needle
                ? "No unstarred models match your filter. Tap Add to favorites to star this custom alias."
                : "All discovered models are starred."
              : "No models discovered yet. Tap Refresh catalog or type an alias above.",
          ),
        );
        return;
      }
      const byProvider = new Map();
      for (const m of unstarred) {
        const prov = m.includes("/") ? m.split("/")[0] : "Models";
        const group = byProvider.get(prov) ?? [];
        group.push(m);
        byProvider.set(prov, group);
      }
      const frag = document.createDocumentFragment();
      for (const [prov, models] of byProvider) {
        const groupEl = node("div", undefined, "phone-model-provider-group");
        groupEl.append(
          node("h3", `${prov} (${models.length})`, "phone-model-provider-title"),
        );
        for (const alias of models.slice(0, 60)) {
          const btn = button(`＋  ${alias}`, () => {
            if (!ordered.includes(alias)) ordered.push(alias);
            draw();
          });
          btn.className = "phone-star-candidate";
          btn.dataset.modelAlias = alias;
          groupEl.append(btn);
        }
        frag.append(groupEl);
      }
      catalogBox.replaceChildren(frag);
    }
    input.addEventListener("input", drawCatalog);
    draw();
    submit(form, "Add to favorites", () => {
      const alias = input.value.trim();
      if (!alias) throw Error("Enter a model alias.");
      if (!ordered.includes(alias)) ordered.push(alias);
      if (!catalogModels.includes(alias)) catalogModels.push(alias);
      input.value = "";
      draw();
    });
    async function discoverCatalog() {
      const found = await readPage(id, { action: "models" });
      if (!found || !Array.isArray(found)) return;
      catalogModels = [
        ...new Set([...catalogModels, ...found.filter(Boolean)]),
      ];
      snapshot.models = catalogModels;
      draw();
    }
    const catalogHeader = node("div", undefined, "phone-catalog-header");
    catalogHeader.append(
      node("h2", "Available models"),
      action("Refresh catalog", discoverCatalog, "quiet"),
    );
    body.append(
      list,
      datalist,
      form,
      action(
        "Save favorites",
        () =>
          edit(
            snapshot.revisions.favorites,
            { section: "favorites", models: ordered },
            favorites,
          ),
        "primary",
      ),
      catalogHeader,
      catalogBox,
    );
    void discoverCatalog();
  }
  function search() {
    screen("Web search");
    if (!snapshot.registry) {
      body.append(node("p", "The provider registry is unavailable."));
      return;
    }
    const form = node("form");
    const data = fields(form, searchFields, snapshot.registry.search);
    submit(form, "Save search settings", () =>
      edit(
        snapshot.revisions.registry,
        { section: "search", fields: data.changes() },
        search,
      ),
    );
    body.append(form, button("Search credential", credentials));
  }
  function legacy() {
    const current = snapshot.legacy;
    screen("Legacy");
    const form = node("form");
    const data = fields(form, legacyFields, {
      ...current.fields,
      model: current.model,
    });
    submit(form, "Save legacy provider", () =>
      edit(
        snapshot.revisions.legacy,
        { section: "legacy", id: current.id, fields: data.changes() },
        legacy,
      ),
    );
    body.append(node("p", current.id), form);
  }
  async function advanced() {
    const id = screen("Advanced");
    const docs = await readPage(id, { action: "read_advanced" });
    if (!docs) return;
    for (const doc of docs) {
      const form = node("form"),
        area = node("textarea");
      area.value = doc.text;
      area.spellcheck = false;
      area.autocomplete = "off";
      area.setAttribute("aria-label", doc.title);
      form.append(area);
      submit(form, "Save configuration", async () => {
        const result = await request({
          action: "save_advanced",
          id: doc.id,
          revision: doc.revision,
          original: doc.text,
          text: area.value,
        });
        if (pageId !== id || panel.hidden) return;
        if (result.status !== "applied")
          throw Error(
            "The host configuration changed. Reopen this page to load it.",
          );
        await advanced();
        status.textContent = "Saved on the host.";
      });
      body.append(details(doc.title, form));
    }
  }
  async function hostModePolicy() {
    const id = screen("Host mode policy", remote);
    const current = await readPage(id, { action: "host_mode_policy" });
    if (!current) return;
    const form = node("form", undefined, "phone-settings-card");
    const label = node("label", "Starting mode on this host");
    const select = node("select");
    select.name = "host_mode_policy";
    for (const [value, name] of [
      ["follow_interface", "Follow interface"],
      ["always_read", "Always read"],
      ["always_write", "Always write"],
      ["always_classifier", "Always auto-review"],
      ["always_yolo", "Always YOLO"],
      ["yolo_asks_every_time", "YOLO asks every time"],
    ]) select.append(new Option(name, value));
    select.value = current.policy;
    label.append(select);
    form.append(label, node("p",
      "Applies to sessions opened from any paired device. Always YOLO starts sessions without permission prompts. YOLO asks every time lets any paired device approve each new entry into YOLO.",
    ));
    submit(form, "Save host policy", async () => {
      await request({ action: "host_mode_policy", set: select.value });
      if (id === pageId && !panel.hidden) status.textContent = "Saved on the host.";
    });
    body.append(form);
  }
  async function hostUpdates() {
    const id = screen("Host updates");
    const initial = await readPage(id, {action: "host_update", command: {type: "status"}});
    if (!initial) return;
    function show(state) {
      body.replaceChildren(node("p", "Installed on this host: " + state.installed), node("p", state.message));
      if (!state.managed) return;
      const card = node("div", undefined, "phone-settings-card");
      async function change(command) {
        for (const control of card.querySelectorAll("select,button")) control.disabled = true;
        try {
          const next = await request({action: "host_update", command});
          if (pageId === id && !panel.hidden) show(next);
        } catch (err) {
          if (pageId === id && !panel.hidden) {
            show(state);
            status.textContent = err.message;
          }
        }
      }
      function choice(name, legend, options, selected) {
        const label = node("label", legend);
        const select = node("select");
        select.name = name;
        for (const [value, title] of options) select.append(new Option(title, value));
        select.value = selected;
        select.onchange = () => change({type: "set_" + name, [name]: select.value});
        label.append(select);
        card.append(label);
      }
      choice("mode", "Update mode", [["automatic","Automatic when idle"], ["download","Download only"], ["manual","Manual"]], state.mode);
      choice("channel", "Release channel", [["stable","Stable"], ["dev","Dev"]], state.channel);
      card.append(
        action(state.scheduled ? "Cancel scheduled update" : "Update when idle", () => change({type: state.scheduled ? "cancel" : "update_when_idle"})),
        action("Refresh status", () => change({type: "status"})),
      );
      body.append(card);
    }
    show(initial);
  }
  async function hosts(back = close, backLabel) {
    const show = () => hosts(back, backLabel);
    const id = screen("Hosts", back, backLabel);
    const current = node("div", undefined, "phone-settings-card");
    current.append(node("h2", hostName()), node("p", "Current host"));
    body.append(current);
    status.textContent = "Loading linked hosts…";
    let peers;
    let offline = false;
    try {
      const links = await request({ action: "worker_links" });
      if (id !== pageId || panel.hidden) return;
      peers = [
        ...(links.hosts || []).map((host) => [host, "Linked for workers"]),
        ...(links.available || []).map((host) => [host, "Worker link not enabled"]),
      ];
      try {
        localStorage.setItem(knownHostsKey, JSON.stringify(peers
          .map(([{ name, origin }]) => ({ name, origin }))
          .filter((host) => registeredHostOrigin(host.origin))));
      } catch {}
    } catch (error) {
      offline = true;
      if (error.status === 403) forgetKnownHosts();
      peers = knownHostAddresses().map((host) => [host, "Saved address"]);
    }
    if (id !== pageId || panel.hidden) return;
    status.textContent = offline ? "Couldn’t refresh the host list." : "";
    if (peers.length) body.append(node("p", "Open another host to view its sessions. Pair this phone there if asked."));
    else if (!offline) body.append(node("p", "No worker destinations are linked."));
    for (const [host, state] of peers) {
      const card = node("div", undefined, "phone-settings-card");
      card.append(node("h2", host.name), node("p", state));
      const origin = registeredHostOrigin(host.origin);
      if (origin) {
        const address = node("p", origin.host, "phone-settings-host-address");
        const link = node("a", `Open ${host.name}`, "phone-settings-host-open");
        link.href = origin.href;
        link.target = "_blank";
        link.rel = "noopener noreferrer";
        card.append(address, link);
      }
      body.append(card);
    }
    body.append(offline
      ? action("Retry host list", show, "quiet")
      : action("Manage worker hosts", () => workerLinks(show, "← Hosts")));
  }
  async function workerLinks(back = remote, backLabel) {
    const show = () => workerLinks(back, backLabel);
    const id = screen("Worker host links", back, backLabel);
    const outbound = await readPage(id, {action: "worker_links"});
    if (!outbound) return;
    const form = node("form", undefined, "phone-settings-card");
    form.autocomplete = "off";
    form.append(node("h2", "Send workers to another host"));
    for (const host of outbound.available || []) {
      form.append(action("Link " + host.name, async () => {
        await request({action: "link_paired_host", host: host.id});
        if (pageId === id && !panel.hidden) await show();
      }));
    }
    const address = textField(form, "Worker invitation or host URL", "worker_address");
    const code = textField(form, "Code (leave empty while that host is waiting)", "worker_code", "password");
    address.required = true;
    address.spellcheck = false;
    address.setAttribute("autocapitalize", "none");
    submit(form, "Link worker host", async () => {
      await request({action: "add_worker_link", address: address.value.trim(), code: code.value.trim()});
      address.value = code.value = "";
      if (pageId === id && !panel.hidden) await show();
    });
    body.append(form);
    const waiting = node("div", undefined, "phone-settings-card");
    waiting.append(
      node("h2", "Hosts waiting to pair"),
      node("p", "A host whose operator opened a pairing window appears here. Linking it needs no code."),
      action("Look for hosts waiting to pair", async () => {
        const found = await readPage(id, { action: "nearby_window_hosts" });
        if (!found) return;
        waiting.replaceChildren(
          node("h2", "Hosts waiting to pair"),
          node("p", "A host whose operator opened a pairing window appears here. Linking it needs no code."),
        );
        if (!(found.hosts || []).length) {
          waiting.append(node("p", "Nothing is waiting on this tailnet right now."));
          return;
        }
        for (const host of found.hosts) {
          waiting.append(
            action(`${host.name} · ${host.origin}`, async () => {
              await request({ action: "add_worker_link_window", origin: host.origin });
              if (pageId === id && !panel.hidden) await show();
            }, "primary"),
          );
        }
      }),
    );
    body.append(waiting);
    for (const host of outbound.hosts) {
      const card = node("div", undefined, "phone-settings-card");
      card.append(node("h2", host.name), node("p", host.origin), action("Remove outgoing link", async () => {
        const result = await request({action: "remove_worker_link", host: host.id});
        if (pageId !== id || panel.hidden) return;
        await show();
        if (!result.revoked) status.textContent = "Removed here. Revoke the incoming link on the other host when it is reachable.";
      }));
      body.append(card);
    }
    const receive = node("div", undefined, "phone-settings-card");
    receive.append(node("h2", "Accept workers on this host"), node("p", "A new link allows launches up to YOLO, subject to this host’s mode policy. Paste the invitation in Worker host links on the sending host."));
    const invitation = node("div");
    receive.append(action("Create worker invitation", async () => {
      const pair = await readPage(id, {action: "invite_worker_host"});
      if (!pair) return;
      clearTimeout(inviteTimer);
      invitation.replaceChildren(node("p", location.origin), node("p", pair.short_code), action("Copy worker invitation", () => navigator.clipboard.writeText(pair.url)));
      inviteTimer = setTimeout(() => invitation.replaceChildren(node("p", "Invitation expired.")), Math.max(0, pair.expires_at * 1000 - Date.now()));
    }), invitation);
    body.append(receive);
    for (const device of outbound.incoming) {
      const card = node("form", undefined, "phone-settings-card");
      card.append(node("h2", device.label));
      const allow = node("input"); allow.type = "checkbox"; allow.checked = device.worker.may_launch;
      const allowed = node("label"); allowed.append(allow, document.createTextNode(" Allow worker launches"));
      const ceiling = node("select");
      for (const [value, label] of [["read","Read"], ["write","Write"], ["classifier","Auto-review"], ["yolo","YOLO"]])
        ceiling.append(new Option(label, value));
      ceiling.value = device.worker.ceiling;
      const limit = node("label", "Maximum worker mode"); limit.append(ceiling);
      const sync = node("input"); sync.type = "checkbox"; sync.checked = device.worker.may_sync_memory === true;
      const synced = node("label"); synced.append(sync, document.createTextNode(" Allow memory sync"));
      card.append(allowed, limit, synced);
      const moves = node("input"); moves.type = "checkbox"; moves.checked = device.worker.may_accept_moves === true;
      if (outbound.supports_session_moves) {
        const label = node("label"); label.append(moves, document.createTextNode(" Accept session moves"));
        card.append(label, node("p", "Let this host send a conversation and its working files here when you move a session."));
      }
      submit(card, "Save worker access", async () => {
        await request({action: "edit_worker_link", id: device.id, before: device.worker, may_launch: allow.checked, ceiling: ceiling.value, may_sync_memory: sync.checked,
          ...(outbound.supports_session_moves ? {may_accept_moves: moves.checked} : {})});
        if (pageId === id && !panel.hidden) { await show(); status.textContent = "Saved on this host."; }
      });
      body.append(card, action("Revoke " + device.label, async () => {
        await request({action: "revoke_device", id: device.id});
        if (pageId === id && !panel.hidden) await show();
      }));
    }
  }
  async function remote() {
    const id = screen("Remote access");
    const result = await readPage(id, { action: "devices" });
    if (!result) return;
    body.append(
      node("p", result.origin || location.origin),
      action("Host mode policy", hostModePolicy),
      action("Worker host links", workerLinks),
    );
    const left = result.window
      ? Math.max(0, Math.ceil((result.window.expires_at * 1000 - Date.now()) / 60000))
      : 0;
    const pendingBox = node("div");
    const renderPending = (rows, paired) => {
      pendingBox.replaceChildren();
      if (paired !== undefined)
        pendingBox.append(
          node("p", `Waiting for a device to pair · ${paired} paired`),
          node("p", "On the new device, open Add host and choose this host. Its request appears here for you to accept."),
        );
      for (const pending of rows) {
        const row = node("div", undefined, "phone-settings-card");
        row.append(
          node("h2", pending.label),
          node("p", `${pending.login} · ${pending.approved ? "accepted, waiting for the device" : "wants to pair"}`),
        );
        if (!pending.approved) {
          row.append(
            action("Accept " + pending.label, async () => {
              await readPage(id, { action: "approve_pending", id: pending.id });
              if (pageId === id && !panel.hidden) await remote();
            }, "primary"),
          );
          row.append(
            action("Decline", async () => {
              await readPage(id, { action: "deny_pending", id: pending.id });
              if (pageId === id && !panel.hidden) await remote();
            }),
          );
        }
        pendingBox.append(row);
      }
    };
    renderPending(result.pending || [], left > 0 ? result.window.paired : undefined);
    body.append(pendingBox);
    if (left > 0) {
      body.append(
        action("Stop waiting", async () => {
          await readPage(id, { action: "close_pairing_window" });
          if (pageId === id && !panel.hidden) await remote();
        }),
      );
      // The operator is told to stay here, so this screen has to keep showing
      // requests as they arrive.
      if (!pendingTimer)
        pendingTimer = setInterval(async () => {
          if (pageId !== id || panel.hidden) {
            clearInterval(pendingTimer);
            pendingTimer = null;
            return;
          }
          const fresh = await request({ action: "devices" }).catch(() => null);
          if (!fresh || pageId !== id || panel.hidden) return;
          const minutes = fresh.window
            ? Math.max(0, Math.ceil((fresh.window.expires_at * 1000 - Date.now()) / 60000))
            : 0;
          renderPending(fresh.pending || [], minutes > 0 ? fresh.window.paired : undefined);
        }, 3000);
    } else {
      body.append(
        action("Wait for a device to pair", async () => {
          await readPage(id, { action: "open_pairing_window" });
          if (pageId === id && !panel.hidden) await remote();
        }),
        node("p", "A device on this tailnet can pair from its own Add host within five minutes, with no code."),
      );
    }
    const invite = node("div", undefined, "phone-settings-invite");
    body.append(
      action(
        "Pair another device",
        async () => {
          const pair = await readPage(id, { action: "pair_device" });
          if (!pair) return;
          clearTimeout(inviteTimer);
          const qr = new DOMParser().parseFromString(
            pair.qr_svg,
            "image/svg+xml",
          );
          const size = Number(qr.documentElement.getAttribute("width"));
          const path = qr.querySelector("path")?.getAttribute("d");
          if (!Number.isInteger(size) || size < 1 || size > 1024 || !path)
            throw Error("Could not read the pairing QR.");
          const code = node("canvas");
          code.width = code.height = size;
          code.setAttribute("role", "img");
          code.setAttribute(
            "aria-label",
            "One-time QR for pairing another device",
          );
          const ctx = code.getContext("2d");
          ctx.fillStyle = "white";
          ctx.fillRect(0, 0, size, size);
          ctx.fillStyle = "black";
          ctx.fill(new Path2D(path));
          invite.replaceChildren(
            code,
            node(
              "p",
              "On the new phone, connect to the same tailnet, open this OX address and choose Scan pairing QR.",
            ),
            action("Copy pairing link", () =>
              navigator.clipboard.writeText(pair.url),
            ),
          );
          inviteTimer = setTimeout(
            () =>
              invite.replaceChildren(
                node("p", "Pairing code expired. Create a fresh code."),
              ),
            Math.max(0, pair.expires_at * 1000 - Date.now()),
          );
        },
        "primary",
      ),
      invite,
      action("Copy OX address", () =>
        navigator.clipboard.writeText(result.origin || location.origin),
      ),
      details(
        "Pair from OX Desktop",
        node(
          "p",
          "In OX Desktop on the host, choose Settings → Remote access → Pair a device. Open OX on the new phone and scan the code.",
        ),
      ),
    );
    for (const device of result.devices.filter(d => d.class !== "host")) {
      const card = node("div", undefined, "phone-settings-card");
      card.append(
        node("h2", device.label),
        node(
          "p",
          device.login + (device.id === result.self ? " · This device" : ""),
        ),
        node(
          "p",
          "Terminal access " +
            (device.terminal ? "enabled" : "off") +
            " · Managed in OX Desktop",
        ),
        node(
          "p",
          "Last used " + new Date(device.last_used * 1000).toLocaleString(),
        ),
        confirmAction(
          "Revoke device",
          "Remove this device’s access to OX?",
          async () => {
            await request({ action: "revoke_device", id: device.id });
            await remote();
          },
        ),
      );
      body.append(card);
    }
  }
  async function behaviorForm(onPhone) {
    const id = screen("Appearance & input");
    const record = onPhone
      ? { behavior: local }
      : await readPage(id, { action: "read_behavior" });
    if (!record) return;
    const prefs = record.behavior;
    const form = node("form");
    const themeLabel = node("label", "Theme"),
      theme = node("select");
    theme.name = "theme";
    for (const mode of ["system", "dark", "light"])
      theme.append(new Option(mode[0].toUpperCase() + mode.slice(1), mode));
    theme.value = prefs.theme;
    themeLabel.append(theme);
    form.append(themeLabel);
    const font = textField(form, "Text size (12–24 px)", "font_size", "number");
    font.min = "12";
    font.max = "24";
    font.step = "1";
    font.required = true;
    font.value = prefs.font_size;
    const entry = node("label", "Return key"),
      enter = node("select");
    enter.name = "enter_sends";
    enter.append(
      new Option("Insert a new line", "newline"),
      new Option("Send the message", "send"),
    );
    enter.value = prefs.enter_sends ? "send" : "newline";
    entry.append(enter);
    form.append(entry);
    const motion = textField(
      form,
      "Reduce motion",
      "reduce_motion",
      "checkbox",
    );
    motion.checked = prefs.reduce_motion;
    // Only a city-level place derived from the laptop's timezone reaches api.met.no.
    const weatherBox = onPhone
      ? null
      : textField(form, "Live weather (MET Norway)", "live_weather", "checkbox");
    if (weatherBox) weatherBox.checked = prefs.live_weather !== false;
    submit(
      form,
      onPhone ? "Save phone preferences" : "Save desktop preferences",
      async () => {
        // Keep the host preferences this form does not show, such as agent settings.
        const behavior = {
          ...(onPhone ? {} : prefs),
          theme: theme.value,
          font_size: Number(font.value),
          enter_sends: enter.value === "send",
          reduce_motion: motion.checked,
        };
        if (weatherBox) behavior.live_weather = weatherBox.checked;
        if (onPhone) {
          local = behavior;
          applyLocal();
          try {
            localStorage.setItem(
              "ox-phone-appearance",
              JSON.stringify(behavior),
            );
            status.textContent = "Saved on this phone.";
          } catch {
            status.textContent =
              "Applied for this visit. Browser storage is unavailable.";
          }
        } else {
          const result = await request({
            action: "save_behavior",
            revision: record.revision,
            behavior,
          });
          if (id !== pageId || panel.hidden) return;
          if (result.status !== "applied") {
            status.textContent = "Desktop preferences changed on the host. ";
            status.append(
              action("Reload desktop preferences", () => behaviorForm(false)),
            );
          } else {
            await behaviorForm(false);
            status.textContent = "Saved on the host.";
          }
        }
      },
    );
    body.append(form);
  }
  function phone() {
    screen("Install OX");
    const installed =
      navigator.standalone === true ||
      matchMedia("(display-mode: standalone)").matches;
    const ios =
      /iPhone|iPad|iPod/.test(navigator.userAgent) ||
      (navigator.platform === "MacIntel" && navigator.maxTouchPoints > 1);
    if (installed) body.append(node("p", "OX is open as an installed app."));
    else if (installPrompt)
      body.append(
        action(
          "Install OX",
          async () => {
            const prompt = installPrompt;
            installPrompt = null;
            await prompt.prompt();
            await prompt.userChoice;
            phone();
          },
          "primary",
        ),
      );
    else
      body.append(
        details(
          ios ? "Install OX on iPhone" : "Install OX",
          node(
            "p",
            ios
              ? "In Safari, open Share → Add to Home Screen. Turn on Open as Web App if shown. Open OX from its icon, then scan a fresh pairing QR."
              : "Open your browser menu and choose Install app or Add to Home screen. Open OX from its icon.",
          ),
        ),
      );
    body.append(action("Pair another device", remote));
  }
  async function notificationPreferences(back = menu) {
    const id = screen("Notifications", back);
    const current = () => id === pageId && !panel.hidden;
    status.textContent = "Loading notification settings…";
    // A quick return to this page still reads the result of a prior edit.
    await Promise.allSettled(pushEdits);
    if (!current()) return;
    const record = await readPage(id, { action: "push_preferences" });
    if (!current()) return;
    if (!record) {
      body.append(button("Try again", () => notificationPreferences(back)));
      return;
    }
    status.textContent = "";
    const prefs = record.preferences;
    const permission = node("section", undefined, "phone-settings-card phone-push-permission");
    const events = node("fieldset", undefined, "phone-notification-events");
    const feedback = node("p", "", "phone-notification-feedback");
    feedback.setAttribute("role", "status");
    events.append(node("legend", "Notify me when"));
    let pending = 0;
    let saveError = "";
    let enrolling = false;
    function toggle(name, label) {
      const control = button(label, async () => {
        if (control.getAttribute("aria-disabled") === "true") return;
        const value = !prefs[name];
        control.setAttribute("aria-checked", String(value));
        control.setAttribute("aria-disabled", "true");
        pending++;
        saveError = "";
        feedback.textContent = "Saving…";
        const edit = request({
          action: "push_preferences",
          change: { event: name, value },
        });
        pushEdits.add(edit);
        try {
          const updated = await edit;
          prefs[name] = updated.preferences[name];
        } catch {
          saveError = `Could not save “${label}”. Try again.`;
        } finally {
          pushEdits.delete(edit);
          pending--;
          if (current()) {
            control.removeAttribute("aria-disabled");
            control.setAttribute("aria-checked", String(prefs[name]));
            feedback.textContent = saveError || (pending ? "Saving…" : "Saved.");
          }
        }
      }, "phone-notification-switch");
      control.setAttribute("role", "switch");
      control.setAttribute("aria-label", label);
      control.setAttribute("aria-checked", String(prefs[name]));
      return control;
    }
    const master = toggle("enabled", "Push notifications");
    for (const [name, label] of [
      ["approval", "Approval needed"],
      ["question", "Question asked"],
      ["finished", "Turn done"],
      ["goal_reached", "Goal reached"],
      ["failed", "Failed"],
    ])
      events.append(toggle(name, label));
    function showPermission() {
      if (enrolling) return;
      const focused = document.activeElement === permission.querySelector("button");
      const { blocked, granted } = pushStatus();
      const available = granted && record.subscribed;
      permission.replaceChildren(
        node("h2", "Browser permission"),
        node("p", blocked || (available
          ? "Allowed on this device."
          : granted
            ? "The browser allows notifications. Finish setting up push delivery."
            : "Allow notifications in this browser to receive your selected alerts.")),
      );
      if (blocked || available) {
        if (focused) master.focus({ preventScroll: true });
        return;
      }
      const enable = button(granted ? "Set up push delivery" : "Enable on this device", async () => {
        if (enrolling) return;
        enrolling = true;
        enable.setAttribute("aria-disabled", "true");
        status.textContent = "Setting up push delivery…";
        try {
          record.subscribed = await enablePush();
          if (current()) status.textContent = "";
        } catch (err) {
          if (current()) status.textContent = pushStatus().blocked ? "" : err.message;
        } finally {
          enrolling = false;
          if (current()) showPermission();
        }
      }, "primary");
      permission.append(enable);
      if (focused) enable.focus({ preventScroll: true });
    }
    refreshPushPermission = showPermission;
    showPermission();
    body.append(
      permission,
      master,
      events,
      node("p", "When Codex completes a goal and a turn together, OX sends one alert."),
      feedback,
    );
  }
  async function memory(project, back) {
    const id = screen("OX Memory", back);
    const name = project.split("/").filter(Boolean).pop() || project;
    body.append(
      node("p", `Saved preferences for ${name} on ${location.hostname}.`),
      node("p", "Local-only entries can be viewed on the host."),
    );
    const list = node("div");
    function render(value) {
      list.replaceChildren();
      if (!value.entries.length)
        list.append(node("p", "No saved memories here yet."));
      for (const entry of value.entries) {
        const card = node("div", undefined, "phone-settings-card");
        const scope = {project: "This project", global: "All projects", host: "This host"}[entry.scope] || entry.scope;
        card.append(
          node("p", entry.text),
          node("p", [scope, entry.sensitivity, entry.pinned ? "Pinned" : null, entry.saved_here ? "Saved on this host" : null].filter(Boolean).join(" · ")),
        );
        list.append(card);
      }
    }
    const form = node("form");
    const input = textField(form, "Add a project preference", "memory");
    input.maxLength = 500;
    input.required = true;
    submit(form, "Save for this project", async () => {
      const value = await request({action: "add_project_memory", project, text: input.value});
      if (id !== pageId || panel.hidden) return;
      input.value = "";
      render(value);
    });
    const initial = await readPage(id, {action: "read_project_memory", project});
    if (!initial) return;
    render(initial);
    body.append(form, list);
  }
  return {
    memory,
    openNotifications() {
      open();
      return notificationPreferences(close);
    },
    openHosts(back, backLabel) {
      open();
      return hosts(back, backLabel);
    },
    reset() {
      pageId++;
      refreshPushPermission = null;
      clearTimeout(inviteTimer);
      snapshot = null;
      status.textContent = "";
      body.replaceChildren();
      panel.replaceChildren();
    },
  };
}
