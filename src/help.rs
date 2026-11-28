use std::ffi::OsString;

use crate::provider::RelayError;

const OVERVIEW: &str = "\
ox runs Codex and Claude Code on any model API. One command, nothing to edit by hand.

Get started
  ox                 First run: guided setup, then your client opens
  ox codex           Open Codex and pick a model
  ox claude          Open Claude Code and pick a model
  ox opencode        Open stock OpenCode with your OX API models
  ox desktop [dir]   Open the project in OX Desktop
  ox host <action>   Install, inspect or uninstall the per-user session host
  ox help browser    Verify and watch a local app

If something is off
  ox doctor          Check everything. The last line is the next command to run
  ox doctor --fix    Repair what ox can repair itself (files are backed up first)
  ox setup           Add a provider, paste a key, sign in, or enable web search
  ox help settings   Find providers, accounts and remote access in the apps
  ox ps              List the live sessions and workers the OX host runs
  ox end <id>        End a session (or --idle, --all); ox ps shows the ids
  ox weather [--json]  Weather for your timezone's city (MET Norway; OX_WEATHER=0 turns it off)
  ox memory <cmd>    Inspect, search, pin, or edit persistent OX memories

More
  ox codex --model provider/model-id   Skip the picker (same for ox claude)
  ox doctor --live --provider NAME     Send one real test request
  ox provider add [PROVIDER] [opts]    Add a provider from a script
  ox login chatgpt|claude              Sign in with a ChatGPT or Claude plan
  ox serve [--port PORT]               Share one gateway with desktop apps and devices
  ox update [--check]                  Install or check for the newest ox release
  ox help [browser|install|setup|settings|desktop|share|t3|subscriptions|opencode]
  ox --version

Providers live in codeox.toml in your Codex home; ox doctor prints the path.
API sessions use a separate ox client profile, so your own Codex and Claude Code
settings and history stay untouched.";

const OPENCODE: &str = "\
Usage: ox opencode [OpenCode arguments]

Run stock OpenCode with the providers you configured in OX.

  ox opencode                                              Open the terminal UI
  ox opencode models                                       List OX model IDs
  ox opencode run --model ox/provider/model-id \"Your task\"   Run a single task
  ox opencode run --continue \"Continue from here\"           Resume the last session

Install stock OpenCode 1.x from https://opencode.ai/download and put opencode
on PATH. Tested with 1.19.6. Run ox setup to configure an API provider. Use the
ox/ prefix shown by models. OpenCode stores these sessions in its OX profile.

For scripted runs, add --format json to the run command.
Run opencode --help for the stock command reference.";

const DESKTOP: &str = "\
OX Desktop
  ox desktop [dir] opens the native workbench. Use --desktop with the OX installer.

Connect a desktop app to ox

1. On the machine that will host ox, run ox setup and ox doctor.
   Save the upstream API keys in setup or load them on the host. Run ox help share to create a separate
   OX_GATEWAY_KEY and start ox serve. Use a Tailscale HTTPS URL for another device.
2. On the client, use the same gateway key and a model alias from /v1/models.

Codex Desktop
  If your app allows adding gateways, open Settings > Model Providers & Gateways
  and choose Add > Gateway or custom provider. Name it ox. Set the base URL to
  http://127.0.0.1:8787/v1 on the ox host, or your Tailscale HTTPS URL plus /v1.
  Choose Environment variable auth and enter OX_GATEWAY_KEY for the gateway key.
  Load that variable into the app process. A terminal export reaches programs
  launched from that terminal.
  Add ox as a separate provider in the app. For manual setup, use the Codex
  example in docs/shared-gateway.md with a separate CODEX_HOME. It uses
  /v1/catalog for model discovery.

Claude Desktop on 3P
  Open Developer > Configure Third-Party Inference. Choose Gateway,
  Static API key, and Bearer auth.
  Use the HTTPS gateway origin with no /v1 suffix. Enter the gateway key.
  In Models, add the exact ox alias, such as alibaba-messages/qwen3.8-flash.
  Use a display name for the picker. The optional Claude model behind this ID
  field changes the effort and context profile for a compatible model.
  Apply Changes and choose Save & Restart. The shared gateway guide shows
  how to check caching, compaction, and Auto mode on the Mac.

Try it
  Start a fresh local task using ox. Ask it to read a small file in a test
  project. A streamed reply with a detail from the file checks both inference
  and local file tools.
  The gateway is tested with stock Codex App Server and Claude Code CLI.
  Desktop UI flows still need a smoke test on an installed app.

See docs/shared-gateway.md in the ox repository for the client examples.";

const SHARE: &str = r#"Share one ox gateway across devices

1. On the host, add a provider and its API key with ox setup. Run ox doctor.
2. Generate a gateway key once with openssl rand -hex 32 and save it in your
   password manager. Load the same key as OX_GATEWAY_KEY on each restart.
   ox serve also needs the saved or exported vendor API keys.
   For Bash or Zsh, paste it at this hidden prompt:

     printf 'Gateway key (input hidden): '
     IFS= read -rs OX_GATEWAY_KEY
     printf '\n'
     export OX_GATEWAY_KEY
     ox serve --port 8787

   It listens on http://127.0.0.1:8787. Keep this terminal open, or use a service
   manager for an always-on host.
3. To connect another device on your tailnet, enable Tailscale HTTPS and run
   this on the host in a second terminal:

     tailscale serve --bg http://127.0.0.1:8787
     tailscale serve status

   Copy the HTTPS URL from Serve. Restrict access with your tailnet policy.
   The gateway key grants access to all configured models on this host.
4. From a client device, load the same OX_GATEWAY_KEY and replace the example URL:

     printf 'header = "Authorization: Bearer %s"\n' "$OX_GATEWAY_KEY" |
       curl --fail --silent --show-error --config - 'https://ox.example-tailnet.ts.net/v1/models'

   A list of model aliases confirms the connection. HTTP 401 means the client
   key is missing or differs from the one loaded by the host.
5. Connect your client. Use the gateway URL plus /v1 for Codex, and the origin
   for Claude. Clients use the gateway key. Upstream API keys stay on the host.
   Run ox help desktop for desktop fields. For stock Claude Code, set
   ANTHROPIC_BASE_URL to the origin and ANTHROPIC_AUTH_TOKEN to the gateway key.

See docs/shared-gateway.md in the ox repository for the Linux service example
and T3 Code setup."#;

const T3: &str = "\
Connect T3 Code to ox

Run ox serve on the machine that holds the API keys. See ox help share if the
T3 execution host is on another device.

In T3 Code, open Settings > Providers and add an instance named ox.

Codex
  Use the stock Codex provider with a custom gateway. Use
  http://127.0.0.1:8787/v1 on the ox host, or your Tailscale HTTPS URL plus /v1.
  Use the gateway key (OX_GATEWAY_KEY) and add the full ox alias as a custom model.

Claude
  Use the Claude provider, which runs the Claude Agent SDK. Keep its usual
  runtime. Under Environment variables, set:
    ANTHROPIC_BASE_URL    Gateway origin, without /v1
    ANTHROPIC_AUTH_TOKEN  Same value as OX_GATEWAY_KEY (mark it sensitive)
    ANTHROPIC_API_KEY     Explicitly empty
  Add the ox model alias as a custom model. Use a separate Claude config
  directory if you also use Claude sign-in on this execution host.

Enable the instance for your project. Start a new thread with that instance
and the full provider/model-id alias.
For subscription sessions, sign in with the stock client on the execution
host and select the ordinary T3 provider. See ox help subscriptions.
See docs/shared-gateway.md in the ox repository for the client examples.";

const SUBSCRIPTIONS: &str = "\
Use your own plan in the native client

ChatGPT plan in Codex
  ox login chatgpt                    Open Codex's native sign-in
  ox login chatgpt --device-auth      Use device-code sign-in on a supported account
  ox login chatgpt status             Check for a saved sign-in or configured keyring
  ox codex --chatgpt [args...]        Start a native ChatGPT-backed Codex session

ChatGPT API credits exclude Codex usage authenticated with an API key.
See https://developers.openai.com/api/docs/guides/chatgpt-api-credits.

Claude subscription in Claude Code
  ox login claude [status]            Open Claude Code's native sign-in or check its account
  ox claude --native [args...]         Start Claude Code with its own settings

Subscription sessions use your usual client profile. The installed clients manage
sign-in and settings changes there. API sessions started by ox use separate
profiles. For Claude subscription use, keep API and gateway overrides out of
that instance's configuration and
environment. For T3 Code, select a native provider on the signed-in execution host.

With API providers configured, an ox subscription session adds ox_workers
for that launch. Ask to list your ox API models or start a background worker.
You can ask for a Codex goal on an API alias, or a Claude Code workflow with
an API model or your Claude subscription. Use the job ID to check or cancel it.
Plain codex and claude use their own tools.

For API vendors, desktop gateways, and sharing, add provider API keys with
ox setup and run ox serve on the host. For included API credits, use an API
key from the OpenAI or Claude organization where you claimed those credits.";

const SETTINGS: &str = "\
Settings in OX Desktop and on the phone

Both screens edit the same provider file and saved keys as ox setup. Use
whichever is at hand.

OX Desktop
  Open Settings with Ctrl+, (Cmd+, on macOS) or the sidebar footer.
  Accounts                Sign in with ChatGPT or Claude; Configure provider.
  Providers & models      Add, edit or remove a provider and its models.
  API keys                Save, replace or remove a provider key.
  Web search              Brave Search for both clients.
  Starred models          Order the model lists; any model still runs.
  Appearance & input      Theme, text size and the Enter key.
  Remote access           Enable Tailscale Serve, Pair a device, and allow
                          terminal access for a paired phone.
  Advanced configuration  Edit the provider registry and favorites as text.

Phone (the paired web app)
  Sessions → ••• → Settings
  Accounts                Sign in; Configure API provider.
  Starred models          Browse the model catalog, star models, and order the model lists.
  Providers & models      The same provider editor.
  API keys                Manage provider and Brave Search keys. Copy selected keys
                          from this host to a paired host with a destination review.
  Web search              Brave Search for both clients.
  Remote access           Pair another device and review paired devices.
  Install & notifications Home Screen and alert setup.
  Phone appearance & input
                          Theme, text size and the Enter key on this phone.
  Sign-in buttons need terminal access for that device: OX Desktop →
  Settings → Remote access → Allow terminal access.

Where the files are
  Providers: $CODEX_HOME/codeox.toml (default ~/.codex/codeox.toml).
  Saved keys: $CODEX_HOME/ox-api-keys.json, or the environment variable named
  in the provider entry. An exported variable overrides a saved key.

Checks and next steps
  ox setup                Add a provider, save a key, sign in, enable Brave.
  ox doctor               Check files, providers and saved accounts. It prints
                          home: and a next: line with the command to run.
  ox doctor --fix         Repair what ox can repair; files are backed up first.
  ox host pair [--port N] Pair a desktop or phone over your tailnet (default HTTPS port 8443).
  ox host pair --worker-host  Create an invitation for a worker host.
  ox host pair --terminal Include terminal access for the device that pairs.
  ox host pair --for LOGIN Only that Tailscale login may redeem the invitation.
  ox host pair --window    Wait five minutes for a device on the tailnet to pair, with no code.
  ox host name [NAME]     Show or change the name shown on paired devices.
  ox host policy          Show the host mode policy. Use set POLICY to change it.
  ox host project add DIR  Keep a checkout available on a headless host. Use list or remove DIR to manage it.
  ox host labels set big  Set placement labels used to identify this host.
  ox host update dev      Follow development builds. Use stable for releases.
  ox host devices         List device IDs for ox host revoke <device-id>.
  ox host doctor          Check the host, Tailscale Serve and paired devices.
  ox help share           Serve one gateway to another device.
  ox help subscriptions   Sign in with a ChatGPT or Claude plan.

These screens are OX Desktop and the phone. Stock Codex, Codex Desktop and
Claude Code keep their own settings; use ox help desktop for those fields.";

const INSTALL: &str = "\
Install or update ox on macOS and Linux

  curl -fsSL https://raw.githubusercontent.com/oxstack/ox/main/install.sh | sh

The installer picks the build for this machine, verifies its SHA-256
checksum and installs it as ~/.local/bin/ox. To install the development
build made from every commit on main, end the command with: sh -s -- --dev

Run ox update --check to look for a newer release.
Run ox update to install the matching desktop suite or standalone CLI.
Run ox update --stage to download now and ox update --apply to install later.
Run ox update --dev or ox update --stable to switch channels.

Releases: https://github.com/oxstack/ox/releases
Manual download steps and platform filenames are in docs/configuration.md.";

const HELP_TOPICS: &[&str] = &[
    "browser",
    "install",
    "setup",
    "settings",
    "desktop",
    "share",
    "t3",
    "subscriptions",
    "opencode",
];
const PS_USAGE: &str = "\
Usage: ox ps [--json]
List the sessions and workers that the OX host runs. Run it while OX Desktop
is closed to see what is still alive. If no host runs, it says so and exits 0.
  KIND     claude or codex
  AGE      time since the host started the process
  IDLE     time since its last event or action
  STATE    running while a turn or an approval is in progress
  THREADS  short thread ids. Any unique prefix works with ox end.
--json prints the host's raw process list.";

const END_USAGE: &str = "\
Usage: ox end <thread-id-prefix> | ox end --idle | ox end --all [--yes]
End sessions on the OX host. A thread stays resumable after it ends.
  <thread-id-prefix>  end the one session whose id starts with this text
  --idle              end every session that is not running
  --all               end every session. Asks y/N unless --yes is given.
A running session loses its current turn when it ends. ox ps lists the ids.";

const TOPICS: &str =
    "Usage: ox help [browser|install|setup|settings|desktop|share|t3|subscriptions|opencode]";

pub(crate) fn overview() {
    println!("{OVERVIEW}");
}

pub(crate) fn dispatch(args: &[OsString]) -> Option<Result<u8, RelayError>> {
    match args {
        [flag, ..] if flag == "-h" || flag == "--help" => overview(),
        [command] if command == "help" => overview(),
        [command] if command == "login" => println!("{SUBSCRIPTIONS}"),
        [command, topic] if command == "help" => match topic.to_str() {
            Some("browser") => crate::browser::print_usage(),
            Some("install") => println!("{INSTALL}"),
            Some("setup") => return Some(crate::setup::print_setup_help()),
            Some("settings") => println!("{SETTINGS}"),
            Some("desktop") => println!(
                "{DESKTOP}\n\nCodex manual configuration\n\
                 Replace the example URL and model alias before merging:\n\n{}",
                include_str!("../examples/codex-gateway.toml").trim_end()
            ),
            Some("share") => println!("{SHARE}"),
            Some("t3") => println!("{T3}"),
            Some("subscriptions") => println!("{SUBSCRIPTIONS}"),
            Some("opencode") => println!("{OPENCODE}"),
            _ => {
                let topic = topic.to_string_lossy();
                let fix = crate::ui::closest(&topic, HELP_TOPICS.iter().copied())
                    .map(|topic| format!("ox help {topic}"))
                    .unwrap_or_else(|| "ox help".to_owned());
                return Some(Err(format!(
                    "unknown help topic {topic:?}\n  why: the topics are {}\n  fix: {fix}",
                    HELP_TOPICS.join(", ")
                )
                .into()));
            }
        },
        [command, ..] if command == "help" => {
            return Some(Err(TOPICS.into()));
        }
        [command, flag] if flag == "-h" || flag == "--help" => match command.to_str() {
            Some("setup") => return Some(crate::setup::print_setup_help()),
            Some("login") => println!("{SUBSCRIPTIONS}"),
            Some("ps") => println!("{PS_USAGE}"),
            Some("end") => println!("{END_USAGE}"),
            Some("opencode") => println!("{OPENCODE}"),
            Some(client @ ("codex" | "claude")) => {
                println!(
                    "Usage: ox {client} [client arguments]\n\
                 Run from your project. With API providers configured, the bare command\n\
                 opens the shared model picker. Type to search and press Tab to star models.\n\
                 Without providers it uses your native sign-in.\n\
                 Example: ox {client} --model provider/model-id\n\
                 Run {client} --help for arguments accepted by the stock client."
                );
                if client == "claude" {
                    println!(
                        "--client-classifier auto|ALIAS  Use an Anthropic model for separate Auto mode\n\
                         classifier requests. API usage may be billed."
                    );
                }
            }
            _ => return None,
        },
        [command, account, flag]
            if (flag == "-h" || flag == "--help")
                && ((command == "login" && (account == "chatgpt" || account == "claude"))
                    || (command == "codex" && account == "--chatgpt")
                    || (command == "claude" && account == "--native")) =>
        {
            println!("{SUBSCRIPTIONS}");
        }
        _ => return None,
    }
    Some(Ok(0))
}
