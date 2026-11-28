//! Separate authenticated operator transport. Runtime and agent links cannot receive PTY input.
use super::{App, DENY, ErrCode, Proof, device, remote_settings};
use crate::bridge;
use axum::{
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode},
    response::Response,
};
use ox_desk_core::host_protocol::{Client, Server, VERSION};
use ox_pty::{Terminal, TerminalCommand};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{io::BufReader, time::timeout};

pub(super) async fn upgrade(
    State(app): State<App>,
    h: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, ErrCode> {
    let id = device(&app, &h)?;
    if app.state.lock().unwrap().terminal_generation(&id).is_none() {
        return Err(DENY);
    }
    let slot = app
        .slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| StatusCode::TOO_MANY_REQUESTS)?;
    Ok(ws
        .max_message_size(32768)
        .max_frame_size(32768)
        .on_upgrade(move |socket| async move {
            let _slot = slot;
            operator(app, id, socket).await;
        }))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Open {
    proof: Proof,
    request: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    preset: Preset,
    project: Option<PathBuf>,
    scope: Option<ox_desk_core::host_protocol::Scope>,
    thread: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Preset {
    Shell,
    CodexLogin,
    ClaudeLogin,
    AntigravityLogin,
    NativeSession,
}
use crate::terminal_protocol::Input;
async fn send(ws: &mut WebSocket, v: Value) -> bool {
    matches!(
        timeout(
            Duration::from_secs(3),
            ws.send(Message::Text(v.to_string().into()))
        )
        .await,
        Ok(Ok(()))
    )
}
fn allowed(app: &App, id: &str, generation: u64) -> bool {
    app.state.lock().unwrap().terminal_generation(id) == Some(generation)
}

fn google_login_url(line: &str) -> bool {
    line.bytes().all(|byte| byte.is_ascii_graphic())
        && url::Url::parse(line).is_ok_and(|url| {
            url.scheme() == "https"
                && url.host_str() == Some("accounts.google.com")
                && url.port().is_none()
                && matches!(url.path(), "/o/oauth2/auth" | "/o/oauth2/v2/auth")
                && url.query().is_some_and(|query| !query.is_empty())
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none()
        })
}

async fn project(app: &App, requested: Option<&Path>) -> Option<PathBuf> {
    let Some(path) = requested else {
        return std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute() && p.is_dir());
    };
    if !path.is_absolute() {
        return None;
    }
    let path = path.canonicalize().ok()?;
    if !path.is_dir() {
        return None;
    }
    let mut conn = bridge::connect(&app.host).await.ok()?;
    bridge::write(&mut conn, &Client::Hello { version: VERSION })
        .await
        .ok()?;
    let line = bridge::line(&mut BufReader::new(conn), &mut vec![])
        .await
        .ok()?;
    let Server::Hello { projects, .. } = serde_json::from_slice(&line).ok()? else {
        return None;
    };
    (projects.contains(&path)
        || app
            .discovered
            .lock()
            .unwrap()
            .iter()
            .any(|s| s.project == path))
    .then_some(path)
}
async fn operator(app: App, id: String, mut ws: WebSocket) {
    let Ok(Some(Ok(Message::Text(raw)))) = timeout(Duration::from_secs(5), ws.recv()).await else {
        return;
    };
    let Ok(open) = serde_json::from_str::<Open>(&raw) else {
        return;
    };
    let Ok(request) = serde_json::from_str::<Request>(&open.request) else {
        return;
    };
    let (generation, mut revoked) = {
        let mut state = app.state.lock().unwrap();
        let Some(g) =
            state.prove_terminal(&id, &open.proof.nonce, &open.proof.signature, &open.request)
        else {
            return;
        };
        (g, state.changed.subscribe())
    };
    let Ok(_activity) = crate::host_updates::TERMINALS.try_read() else {
        send(&mut ws,json!({"error":"The host is restarting to update. Reopen the terminal after it reconnects."})).await;
        return;
    };
    let mut lease = None;
    let login_matcher = matches!(request.preset, Preset::AntigravityLogin)
        .then(|| Box::new(google_login_url) as ox_pty::LineMatcher);
    let (cwd, mut cmd) = match request.preset {
        Preset::NativeSession => {
            let (Some(scope), Some(thread), None) =
                (request.scope, request.thread, request.project)
            else {
                return;
            };
            let cwd = scope.project.clone();
            match timeout(
                Duration::from_secs(8),
                super::operator_handoff::Lease::preview(&app, scope, thread),
            )
            .await
            {
                Ok(Ok((session, cmd))) => {
                    lease = Some(session);
                    (cwd, cmd)
                }
                _ => {
                    send(&mut ws,json!({"error":"Open an idle parent session before handing it to the terminal."})).await;
                    return;
                }
            }
        }
        mode => {
            let shell = matches!(mode, Preset::Shell);
            if request.scope.is_some()
                || request.thread.is_some()
                || (!shell && request.project.is_some())
            {
                return;
            }
            let Ok(Some(cwd)) = timeout(
                Duration::from_secs(4),
                project(&app, request.project.as_deref()),
            )
            .await
            else {
                send(
                    &mut ws,
                    json!({"error":"Choose an available project on the host."}),
                )
                .await;
                return;
            };
            let cmd = if shell {
                TerminalCommand::line_input_shell()
            } else if matches!(mode, Preset::AntigravityLogin) {
                TerminalCommand {
                    program: "agy".into(),
                    args: vec!["acp".into(), "--login".into()],
                }
            } else {
                let Ok(binary) = remote_settings::binary() else {
                    return;
                };
                TerminalCommand {
                    program: binary,
                    args: if matches!(mode, Preset::CodexLogin) {
                        vec!["login", "chatgpt", "--device-auth"]
                    } else {
                        vec!["login", "claude"]
                    }
                    .into_iter()
                    .map(Into::into)
                    .collect(),
                }
            };
            (cwd, cmd)
        }
    };
    if cmd.resolve().is_err() {
        send(
            &mut ws,
            json!({"error":"The terminal program could not be found on this host."}),
        )
        .await;
        return;
    }
    let Ok(preview) = cmd.preview() else {
        send(
            &mut ws,
            json!({"error":"The host could not prepare a terminal preview."}),
        )
        .await;
        return;
    };
    if !allowed(&app, &id, generation) || !send(&mut ws, json!({"review":preview,"cwd":cwd})).await
    {
        return;
    }
    let review_timeout = tokio::time::sleep(Duration::from_secs(120));
    tokio::pin!(review_timeout);
    loop {
        if !allowed(&app, &id, generation) {
            return;
        }
        tokio::select! {biased;
            _=revoked.changed()=>continue,
            _=&mut review_timeout=>{send(&mut ws,json!({"error":"Terminal review expired. Open the terminal again."})).await;return;},
            msg=ws.recv()=>match msg{
                Some(Ok(Message::Ping(_) | Message::Pong(_)))=>continue,
                Some(Ok(Message::Text(s))) if matches!(serde_json::from_str::<Input>(&s),Ok(Input::Run))=>break,
                _=>return
            }
        }
    }
    if !allowed(&app, &id, generation) {
        return;
    }
    if let Some(session) = &mut lease {
        // If authorization or the connection disappears while native releases the thread,
        // the private lease returns it to the host without starting a terminal process.
        let acquire = timeout(Duration::from_secs(20), session.acquire());
        tokio::pin!(acquire);
        loop {
            if !allowed(&app, &id, generation) {
                return;
            }
            tokio::select! {biased;
                _=revoked.changed()=>continue,
                msg=ws.recv()=>match msg {
                    Some(Ok(Message::Ping(_) | Message::Pong(_)))=>continue,
                    _=>return,
                },
                result=&mut acquire=>{
                    if !matches!(result,Ok(Ok(()))){send(&mut ws,json!({"error":"Native session handoff was refused."})).await;return;}
                    break;
                }
            }
        }
    }
    let check = app.clone();
    let owner = id.clone();
    let Ok((pty, screen, events)) = Terminal::start_with_line_capture(
        cwd,
        cmd,
        Box::new(move || {
            allowed(&check, &owner, generation)
                .then_some(())
                .ok_or_else(|| "Terminal access was disabled.".into())
        }),
        (24, 80),
        login_matcher,
    ) else {
        send(
            &mut ws,
            json!({"error":"The host could not start the terminal process."}),
        )
        .await;
        return;
    };
    pty.observe_echo();
    if let Some(session) = &mut lease {
        session.process(screen.clone(), events.clone());
    }
    if !send(&mut ws, json!({"running":true})).await {
        return;
    }
    loop {
        if !allowed(&app, &id, generation) {
            break;
        }
        tokio::select! {biased;
            _=revoked.changed()=>continue,
            msg=ws.recv()=>{
                let Some(Ok(msg))=msg else{break};
                if !allowed(&app,&id,generation){break;}
                match msg {
                    Message::Text(s)=>match serde_json::from_str::<Input>(&s){
                        Ok(Input::Keys{text}) if text.len()<=8192=>{if !pty.send(text.into_bytes()){break;}},
                        Ok(Input::Resize{rows,cols}) if (8..=60).contains(&rows)&&(20..=180).contains(&cols)=>pty.resize((rows,cols)),
                        _=>break
                    },
                    Message::Ping(_) | Message::Pong(_)=>(),
                    _=>break
                }
            },
            update=events.recv_async()=>{
                if update.is_err() || !allowed(&app,&id,generation){break;}
                let (frame, outcome) = {
                    let mut view = screen.lock().unwrap();
                    let mut frame = json!({"screen":view.parser.screen().contents(),"echo":view.echo});
                    if let Some(url) = view.take_captured_line() {
                        frame["login_url"] = url.into();
                    }
                    (frame, view.outcome.clone())
                };
                if !send(&mut ws, frame).await { break; }
                if let Some(result)=outcome{
                    match result{
                        Ok(code)=>{send(&mut ws,json!({"exit":code})).await;}
                        Err(_)=>{send(&mut ws,json!({"error":"The host could not start or continue this terminal."})).await;}
                    }
                    break;
                }
            }
        }
    }
    // Closing the last input handle wakes the supervisor; it kills and reaps the operator session.
}
