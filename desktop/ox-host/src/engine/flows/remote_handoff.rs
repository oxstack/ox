//! Operator-brokered conversation transfers between two host engines.
use super::*;

impl Engine {
    pub(in crate::engine) fn inspect_handoff_target(&self, client: u64, key: String, scope: Scope) {
        let owner = format!("handoff:{key}");
        let recovered = if self.ledger.data.handoffs.iter().any(|r| r.id == key) {
            None
        } else {
            self.workers
                .all()
                .find(|(s, _, entry)| *s == &scope && entry.origin.launch == owner)
                .map(|(_, session, _)| session.to_owned())
        };
        let mut scopes = self.scope_projects();
        scopes.extend(self.discovered.keys().map(|(s, _)| s.project.clone()));
        let config = self.config.clone();
        let dir = self.state_dir.clone();
        let host = self.host_id;
        let windows = self.ledger.data.windows.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let directory = crate::projects::directory(&config, &dir, scopes, Some(&scope.project));
            let result = (|| {
                let repository = directory
                    .checkouts
                    .first()
                    .and_then(|p| p.repository.as_ref())
                    .ok_or("This checkout is no longer available on the receiving host.")?;
                if !config.installed_runtimes().contains(&scope.route.runtime) {
                    return Err("Install the selected runtime on the receiving host first.".into());
                }
                let mut catalog_window = None;
                if let ox_desk_core::runtime::Billing::Api(alias) = &scope.route.billing {
                    let (done, recv) = std::sync::mpsc::sync_channel(1);
                    let alias = alias.clone();
                    thread::spawn(move || {
                        let found = ox_desk_core::ports::ox::request::<BTreeMap<String, u64>>(
                            &config.ox,
                            &config.path,
                            ox_desk_core::settings_protocol::Request::ModelContexts,
                        )
                        .ok()
                        .and_then(|m| m.get(&alias).copied());
                        let _ = done.send(found);
                    });
                    catalog_window = recv.recv_timeout(WINDOW_WAIT).ok().flatten();
                }
                if !directory.routes.contains(&scope.route) && catalog_window.is_none() {
                    return Err(
                        "This model or runtime is unavailable on the receiving host.".into(),
                    );
                }
                let window = crate::durable::handoff_window(&windows, &scope.route, catalog_window);
                Ok(
                    json!({"host":host.to_string(),"repository":repository,"window":window,"recovered":recovered}),
                )
            })();
            let _ = tx.send(Message::HandoffTarget(client, key, result));
        });
    }

    pub(in crate::engine) fn export_handoff(
        &mut self,
        client: u64,
        key: &str,
        scope: Scope,
        start: HandoffStart,
        target_host: String,
        window: Option<u64>,
    ) -> io::Result<()> {
        start.validate().map_err(io::Error::other)?;
        if uuid::Uuid::parse_str(&target_host)
            .ok()
            .is_none_or(|id| id == self.host_id)
        {
            return Err(io::Error::other("Choose another OX host for this handoff."));
        }
        if !self.running.contains_key(&scope) {
            return Err(io::Error::other(
                "Open the source session before handing it off.",
            ));
        }
        if let Some(reason) = self.handoff_blocker(&scope, &start.source) {
            return Err(io::Error::other(reason));
        }
        self.preparing.push(Preparing {
            export_to: Some((
                target_host,
                uuid::Uuid::parse_str(key).unwrap_or_else(|_| uuid::Uuid::new_v4()),
            )),
            source: scope,
            start,
            reply: Replier::Client(client, key.into()),
            device: format!("client-{client}"),
            surface: self.clients.get(&client).and_then(|c| c.surface),
            deadline: Instant::now() + LOAD_WAIT,
            window: std::sync::Arc::new(std::sync::Mutex::new(Some(window))),
            window_deadline: Instant::now(),
            mode_approval: None,
        });
        self.advance_flows()
    }

    pub(in crate::engine) fn receive_handoff(
        &mut self,
        client: u64,
        key: String,
        scope: Scope,
        packet: Box<handoff::HandoffTransfer>,
        approval: Option<YoloAllowance>,
    ) -> io::Result<()> {
        let reply = Replier::Client(client, key.clone());
        macro_rules! reject {
            ($message:expr) => {
                return self.answer(reply, Err($message.to_string()))
            };
        }
        let scope = match canonical_scope(scope) {
            Ok(scope) => scope,
            Err(err) => reject!(err),
        };
        if scope.route.runtime == ox_desk_core::runtime::Runtime::OpenCode
            || packet.source.runtime == ox_desk_core::runtime::Runtime::OpenCode
        {
            reject!("Hosted OpenCode does not support session handoff.");
        }
        if packet.target_host != self.host_id.to_string()
            || packet
                .source
                .host
                .as_ref()
                .is_none_or(|h| h.id == packet.target_host || uuid::Uuid::parse_str(&h.id).is_err())
            || uuid::Uuid::parse_str(&packet.id).is_err()
            || packet.start.source != packet.source.session
            || packet.start.route() != scope.route
        {
            reject!("The handoff does not match its source or destination.");
        }
        if packet.start.host.is_some() {
            reject!("Nested handoff destinations are invalid.");
        }
        if let Err(reason) = packet.start.validate() {
            reject!(reason);
        }
        if ox_host_service::checkout::repository_id(&scope.project).as_deref()
            != Some(&packet.repository)
        {
            reject!("The selected checkout has a different Git origin from the source.");
        }
        let owner = format!("handoff:{}", packet.id);
        if let Some(record) = self.ledger.data.handoffs.iter().find(|h| h.id == packet.id) {
            if record.target != scope {
                reject!("This handoff already belongs to another checkout or route.");
            }
            if record.status == "starting"
                && let Some(live) = self.running.get_mut(&scope)
                && let Some(launch) = live
                    .launching
                    .as_mut()
                    .or_else(|| live.confirming.as_mut().map(|c| &mut c.launch))
                && launch.origin.launch == owner
            {
                launch.reply = reply;
                return Ok(());
            }
            let message = record
                .message
                .clone()
                .unwrap_or_else(|| "This handoff is already starting on the destination.".into());
            reject!(message);
        }
        let existing = self
            .workers
            .all()
            .find(|(_, _, e)| e.origin.launch == owner)
            .map(|(s, id, _)| (s.clone(), id.to_owned()));
        if let Some((saved_scope, id)) = existing {
            if saved_scope != scope {
                reject!("This handoff already belongs to another checkout or route.");
            }
            return self.answer(
                reply,
                Ok(json!({"session":id,"scope":scope,"recovered":true})),
            );
        }
        if self.draining.is_some() || self.update_reserved() {
            reject!(crate::handover::REFUSAL);
        }
        let policy = match HostPolicy::read(&self.state_dir) {
            Ok(p) => p,
            Err(e) => reject!(e),
        };
        let resolved = if let Some(fixed) = policy.starting_level() {
            let level = match packet.start.mode {
                modes::ModeRequest::Level(l) => l.min(fixed),
                modes::ModeRequest::Inherit => fixed,
            }
            .min(packet.child_ceiling.unwrap_or(Level::Yolo));
            modes::Resolved::new(level, packet.start.runtime, Decision::HostPolicy)
        } else {
            match modes::resolve_operator(
                packet.source.source_level,
                packet.start.mode,
                packet.start.runtime,
            ) {
                Ok(v) => v,
                Err(e) => reject!(e),
            }
        };
        let yolo = if !resolved.level.needs_person() {
            None
        } else {
            match policy {
                HostPolicy::AlwaysYolo => Some(YoloAllowance::HostPolicy),
                HostPolicy::YoloAsksEveryTime if approval.is_none() => {
                    if self.mode_requests.values().any(|m|matches!(&m.activation,mode_requests::Activation::RemoteHandoff{packet:p,..} if p.id==packet.id)) {
                        reject!("This handoff is awaiting the operator's mode decision.");
                    }
                    if let Err(e) = self.ensure(&scope, None) {
                        reject!(e);
                    }
                    self.answer(
                        Replier::Client(client, key.clone()),
                        Ok(json!({"awaiting_approval": true})),
                    )?;
                    self.ask_activation(
                        &scope,
                        mode_requests::Activation::RemoteHandoff {
                            client,
                            key,
                            packet,
                        },
                    )?;
                    return self.process(&scope, vec![], true);
                }
                HostPolicy::YoloAsksEveryTime => approval,
                _ if packet.start.yolo_confirmed => Some(YoloAllowance::Operator {
                    device: format!("client-{client}"),
                }),
                _ => reject!(
                    "The source host’s YOLO grant applies only there. Choose YOLO for this host or select a lower mode."
                ),
            }
        };
        let handoff::HandoffTransfer {
            mut source,
            carried,
            start,
            id: record,
            ..
        } = *packet;
        source.decision = resolved.decision;
        let selection = handoff::Selection {
            carried,
            refs: source.selection.clone(),
        };
        let surface = self.clients.get(&client).and_then(|c| c.surface);
        if let Err((reply, error)) = self.begin_handoff(
            reply,
            HandoffLaunch {
                source,
                selection,
                start,
                resolved,
                yolo,
                target: scope,
                surface,
                record,
                owner,
            },
        ) {
            self.answer(reply, Err(error))?;
        }
        Ok(())
    }
}
