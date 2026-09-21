//! One native Codex app-server/TUI pair, controlled over private Unix sockets.
use crate::{
    auth,
    lam_relay::{
        self, BindingSecret, BindingTable, ErrorCode, RelayControl, Request as RelayRequest,
        Response as RelayResponse, Submission, ThreadState,
    },
    process::ProcessEvidence,
    protocol::{starts_turn, Requests, Turns},
    state::{Account, Paths, Store},
};
use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    ffi::OsString,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    process::{Child, ChildStdin, Command},
    sync::{mpsc, oneshot},
    time::{timeout, Instant},
};
use tokio_tungstenite::{
    tungstenite::{protocol::WebSocketConfig, Message},
    WebSocketStream,
};

const DEADLINE: Duration = Duration::from_secs(30);
type Socket = WebSocketStream<UnixStream>;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionStatus {
    pub pid: u32,
    pub account: Option<String>,
    pub email: Option<String>,
    pub account_id: Option<String>,
    pub pending: Option<String>,
    pub error: Option<String>,
}

struct Control {
    action: String,
    reply: oneshot::Sender<SessionStatus>,
}

fn executor() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?)
}

pub fn run(paths: Paths, codex: OsString, args: Vec<OsString>) -> Result<i32> {
    executor()?.block_on(run_async(paths, codex, args))
}

pub fn sessions(paths: &Paths) -> Result<Vec<SessionStatus>> {
    executor()?.block_on(contact(paths, "status"))
}

pub fn notify(paths: &Paths) -> Result<Vec<SessionStatus>> {
    executor()?.block_on(contact(paths, "switch"))
}

async fn contact(paths: &Paths, action: &str) -> Result<Vec<SessionStatus>> {
    let directory = paths.data.join("runtimes");
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut files: Vec<_> = fs::read_dir(directory)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "sock"))
        .collect();
    files.sort();
    let mut reports = Vec::new();
    for path in files {
        let pid = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let result = timeout(DEADLINE + Duration::from_secs(5), async {
            let mut socket = match UnixStream::connect(&path).await {
                Ok(s) => s,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                    ) =>
                {
                    return Ok(None)
                }
                Err(e) => return Err(e.into()),
            };
            socket.write_all(format!("{action}\n").as_bytes()).await?;
            let mut line = String::new();
            BufReader::new(socket.take(8192))
                .read_line(&mut line)
                .await?;
            Ok::<_, anyhow::Error>(Some(serde_json::from_str::<SessionStatus>(&line)?))
        })
        .await;
        match result {
            Ok(Ok(Some(report))) => reports.push(report),
            Ok(Ok(None)) => { /* A dead socket is ignored; its owner may still be cleaning up. */ }
            _ => reports.push(SessionStatus {
                pid,
                account: None,
                email: None,
                account_id: None,
                pending: None,
                error: Some("session did not acknowledge the request".into()),
            }),
        }
    }
    Ok(reports)
}

async fn controls(listener: UnixListener, tx: mpsc::Sender<Control>) {
    while let Ok((socket, _)) = listener.accept().await {
        let tx = tx.clone();
        tokio::spawn(async move {
            let _ = timeout(DEADLINE + Duration::from_secs(5), async move {
                let (read, mut write) = socket.into_split();
                let mut action = String::new();
                BufReader::new(read.take(32)).read_line(&mut action).await?;
                if !matches!(action.trim(), "status" | "switch") {
                    return Ok::<_, anyhow::Error>(());
                }
                let (reply, result) = oneshot::channel();
                tx.send(Control {
                    action: action.trim().into(),
                    reply,
                })
                .await?;
                let status = result.await?;
                write
                    .write_all(format!("{}\n", serde_json::to_string(&status)?).as_bytes())
                    .await?;
                Ok(())
            })
            .await;
        });
    }
}

async fn send(writer: &mut ChildStdin, value: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    writer
        .write_all(&bytes)
        .await
        .context("writing to Codex app-server")?;
    writer.flush().await?;
    Ok(())
}

async fn outgoing(socket: &mut Socket, value: Value) -> Result<()> {
    socket
        .send(Message::Text(serde_json::to_string(&value)?.into()))
        .await
        .context("writing to Codex TUI")
}

async fn credentials(store: &Store, alias: &str, refresh: bool) -> Result<Account> {
    let store = store.clone();
    let alias = alias.to_owned();
    tokio::task::spawn_blocking(move || auth::credentials(&store, &alias, refresh)).await?
}

async fn stop(child: &mut Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

async fn run_async(paths: Paths, codex: OsString, args: Vec<OsString>) -> Result<i32> {
    let mut signals = Signals::new()?;
    let store = Store::new(paths.clone());
    let initial = store.read()?.current.context(
        "No account selected. Run `cx add <alias> --current` or `cx add <alias>` first.",
    )?;
    let account = credentials(&store, &initial, false).await?;
    // The directory's permissions protect both WebSocket and control connections.
    let transport = tempfile::Builder::new().prefix("cx-").tempdir()?;
    fs::set_permissions(transport.path(), fs::Permissions::from_mode(0o700))?;
    let tui_socket = transport.path().join("tui.sock");
    let listener = UnixListener::bind(&tui_socket)?;
    let lam_socket = transport.path().join("lam.sock");
    let lam_listener = UnixListener::bind(&lam_socket).context("binding LAM relay socket")?;
    fs::set_permissions(&lam_socket, fs::Permissions::from_mode(0o600))?;
    let directory = paths.data.join("runtimes");
    fs::create_dir_all(&directory)?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    let control_path = directory.join(format!("{}.sock", std::process::id()));
    let control_listener =
        UnixListener::bind(&control_path).context("binding cx control socket")?;
    fs::set_permissions(&control_path, fs::Permissions::from_mode(0o600))?;
    let _socket_cleanup = SocketCleanup(control_path);
    let mut server = Command::new(&codex)
        .arg("app-server")
        .process_group(0)
        .arg("--listen")
        .arg("stdio://")
        .args(["-c", "cli_auth_credentials_store=\"ephemeral\""])
        .env("CODEX_HOME", &paths.codex_home)
        .env("CX_LAM_RELAY", &lam_socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .context("starting Codex app-server (set CX_CODEX_BIN to select a binary)")?;
    let outcome = async {
        let mut tui = Command::new(&codex).args(&args)
            .arg("--remote").arg(format!("unix://{}", tui_socket.display()))
            .env("CODEX_HOME", &paths.codex_home)
            .stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit()).kill_on_drop(true)
            .spawn().context("starting Codex TUI")?;
        let result = async {
            let connection = tokio::select! {
                connection = timeout(DEADLINE, listener.accept()) => connection.context("Codex did not connect within 30 seconds")??.0,
                _ = signals.terminate.recv() => return Ok(143),
                _ = signals.interrupt.recv() => return Ok(130),
                status = tui.wait() => { bail!("Codex exited before connecting ({}); use a Codex version with remote TUI support", status?); }
            };
            let socket = tokio::select! {
                result = timeout(DEADLINE, tokio_tungstenite::accept_async_with_config(connection,
                    Some(WebSocketConfig::default().max_message_size(Some(64 << 20)).max_frame_size(Some(64 << 20))))) => result.context("Codex WebSocket handshake timed out")??,
                _ = signals.terminate.recv() => return Ok(143),
                _ = signals.interrupt.recv() => return Ok(130),
            };
            let server_pid = server
                .id()
                .context("missing Codex app-server process")?;
            let (tx, rx) = mpsc::channel(16);
            let control_task = tokio::spawn(controls(control_listener, tx));
            let (lam_tx, lam_rx) = mpsc::channel(16);
            let lam_task = tokio::spawn(lam_relay::serve(lam_listener, lam_tx));
            let result = relay(
                &mut server,
                &mut tui,
                socket,
                rx,
                Launch {
                    store,
                    alias: initial,
                    account,
                    lam_controls: lam_rx,
                    server_pid,
                },
                &mut signals,
            ).await;
            control_task.abort();
            lam_task.abort();
            result
        }.await;
        stop(&mut tui).await;
        result
    }.await;
    if let Some(pid) = server.id() {
        // The app-server owns a separate group so terminal Ctrl-C reaches only the TUI.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGTERM);
        }
    }
    stop(&mut server).await;
    outcome
}

struct SocketCleanup(PathBuf);
impl Drop for SocketCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

struct Login {
    revision: u64,
    id: String,
    alias: String,
    account: Account,
    started: Instant,
}

enum PendingRelayKind {
    Bind {
        thread_id: uuid::Uuid,
        secret: BindingSecret,
        peer_pid: u32,
    },
    Inspect {
        thread_id: uuid::Uuid,
    },
    Queue {
        thread_id: uuid::Uuid,
        attempt_id: uuid::Uuid,
        input: Value,
    },
}

struct PendingRelay {
    kind: PendingRelayKind,
    reply: oneshot::Sender<RelayResponse>,
    deadline: Instant,
}

#[derive(Default)]
struct RelayState {
    bindings: BindingTable,
    serial: u64,
    pending: HashMap<String, PendingRelay>,
}

async fn relay(
    server: &mut Child,
    tui: &mut Child,
    mut socket: Socket,
    mut controls: mpsc::Receiver<Control>,
    launch: Launch,
    signals: &mut Signals,
) -> Result<i32> {
    let Launch {
        store,
        alias: initial,
        account: initial_account,
        mut lam_controls,
        server_pid,
    } = launch;
    let mut writer = server.stdin.take().context("missing app-server stdin")?;
    let mut lines =
        BufReader::new(server.stdout.take().context("missing app-server stdout")?).lines();
    let mut status = SessionStatus {
        pid: std::process::id(),
        account: None,
        email: None,
        account_id: None,
        pending: Some(initial.clone()),
        error: None,
    };
    let mut requests = Requests::default();
    let mut relays = RelayState::default();
    let mut server_evidence = None;
    let mut turns = Turns::default();
    let mut pane_model = PaneModel::default();
    let mut login: Option<Login> = None;
    let mut serial = 0u64;
    let mut desired_revision = 0u64;
    let mut initialization: Option<Value> = None;
    let mut initial_params = Some(auth::login_params(&initial_account)?);
    let mut loaded: Option<Account> = None;
    let mut initialized = false;
    let started = Instant::now();
    let mut waiting: Vec<oneshot::Sender<SessionStatus>> = Vec::new();
    let mut deferred = VecDeque::new();
    let mut notifications = Vec::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    loop {
        tokio::select! {
            result = tui.wait() => return Ok(result?.code().unwrap_or(1)),
            _ = signals.terminate.recv() => return Ok(143),
            _ = signals.interrupt.recv() => {},
            _ = tick.tick() => {
                if !initialized && started.elapsed() > DEADLINE { bail!("Codex initialization timed out; installed protocol may be incompatible"); }
                if login.as_ref().is_some_and(|l| l.started.elapsed() > DEADLINE) {
                    bail!("Codex account change timed out; runtime stopped to avoid using an uncertain account");
                }
                expire_relays(&mut relays.pending, &mut turns);
            },
            Some(control) = controls.recv() => {
                if control.action == "switch" {
                    match store.read().and_then(|s| s.current.context("no selected account")) {
                        Ok(alias) => {
                            desired_revision += 1;
                            status.pending = Some(alias);
                            status.error = None;
                            if turns.busy() || !initialized { let _ = control.reply.send(status.clone()); }
                            else { waiting.push(control.reply); }
                        }
                        Err(_) => { status.error=Some("could not read selected account".into()); let _ = control.reply.send(status.clone()); }
                    }
                } else { let _ = control.reply.send(status.clone()); }
            },
            Some(control) = lam_controls.recv() => {
                if server_evidence.is_none() {
                    match ProcessEvidence::read(server_pid) {
                        Ok(evidence) => server_evidence = Some(evidence),
                        Err(_) => {
                            let _ = control.reply.send(RelayResponse::error(
                                ErrorCode::Unavailable,
                                Submission::NotStarted,
                            ));
                            continue;
                        }
                    }
                }
                start_relay(
                    control,
                    initialized && login.is_none(),
                    server_evidence.as_ref().expect("captured app-server evidence"),
                    &mut relays,
                    &mut turns,
                    &mut writer,
                ).await;
            },
            frame = socket.next() => {
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        let message: Value = serde_json::from_str(&text).context("invalid Codex TUI protocol message")?;
                        if login.is_some() { deferred.push_back(message); }
                        else { forward_client(message, &mut writer, &mut socket, ClientState { requests: &mut requests, turns: &mut turns, pane_model: &mut pane_model, initialized, codex_home: &store.paths.codex_home }).await?; }
                    }
                    Some(Ok(Message::Ping(bytes))) => { socket.send(Message::Pong(bytes)).await?; },
                    Some(Ok(Message::Close(_))) | None => return Ok(timeout(Duration::from_secs(2), tui.wait()).await.ok().and_then(|r| r.ok()).and_then(|s|s.code()).unwrap_or(0)),
                    Some(Err(_)) => bail!("Codex TUI connection closed unexpectedly"),
                    _ => {}
                }
            },
            line = lines.next_line() => {
                let line = line?.context("Codex app-server exited unexpectedly")?;
                let mut message: Value = serde_json::from_str(&line).context("invalid Codex app-server protocol message")?;
                if message["method"] == "account/chatgptAuthTokens/refresh" && message.get("id").is_some() {
                    let id=message["id"].clone();
                    let alias=status.account.as_deref().context("Codex requested credentials before initial login")?;
                    let expected=message.pointer("/params/previousAccountId").and_then(Value::as_str);
                    let account=loaded.as_ref().context("missing loaded identity")?.clone();
                    let refresh = if expected.is_some_and(|id| id != account.account_id) { Err(anyhow::anyhow!("account mismatch")) }
                        else {
                            let store=store.clone(); let alias=alias.to_owned();
                            tokio::task::spawn_blocking(move || auth::credentials_for(&store,&alias,&account.email,&account.account_id,true)).await?
                        };
                    match refresh {
                        Ok(account) => {
                            let mut params=auth::login_params(&account)?;
                            loaded=Some(account);
                            params.as_object_mut().context("invalid credential parameters")?.remove("type");
                            send(&mut writer, &json!({"id":id,"result":params})).await?;
                        }
                        Err(_) => { send(&mut writer, &json!({"id":id,"error":{"code":-32000,"message":"cx could not refresh this account; sign in again with cx add <alias> --force"}})).await?; }
                    }
                } else if message["id"].as_str().is_some_and(|id| id.starts_with("cx.lam.")) {
                    if let Some(id) = message["id"].as_str().map(str::to_owned) {
                        if let Some(pending) = relays.pending.remove(&id) {
                            let was_queue = matches!(&pending.kind, PendingRelayKind::Queue { .. });
                            let response = finish_relay(
                                &message,
                                pending.kind,
                                &mut relays.bindings,
                                server_evidence.as_ref().expect("pending relay has app-server evidence"),
                            );
                            if was_queue {
                                let tracking = if response.accepted() {
                                    message.clone()
                                } else {
                                    json!({"id":id,"error":{"code":-32000}})
                                };
                                turns.start_response(&id, &tracking);
                                if response.accepted() {
                                    debug_assert!(turns.busy(), "accepted queue must reserve the turn gap");
                                }
                            }
                            let _ = pending.reply.send(response);
                        }
                    }
                } else if login.as_ref().is_some_and(|l| message["id"].as_str() == Some(&l.id)) {
                    let completed=login.take().unwrap();
                    if message.get("error").is_some() {
                        status.error=Some("Codex rejected the account change; check protocol compatibility or sign in again".into());
                        status.pending=None;
                        for reply in waiting.drain(..) { let _=reply.send(status.clone()); }
                        bail!("Codex rejected account authentication; cx requires the chatgptAuthTokens app-server protocol");
                    }
                    status.account=Some(completed.alias.clone());
                    status.email=Some(completed.account.email.clone());
                    status.account_id=Some(completed.account.account_id.clone());
                    loaded=Some(completed.account);
                    if status.pending.as_ref()==Some(&completed.alias) && completed.revision == desired_revision { status.pending=None; }
                    status.error=None;
                    if let Some(response)=initialization.take() {
                        outgoing(&mut socket,response).await?;
                        initialized=true;
                        for notification in notifications.drain(..) { outgoing(&mut socket,notification).await?; }
                    }
                    if status.pending.is_none() {
                        for reply in waiting.drain(..) { let _=reply.send(status.clone()); }
                    }
                } else if let Some((id, method)) = requests.restore(&mut message) {
                    pane_model.response(&id, &method, &mut message);
                    if starts_turn(&method) { turns.start_response(&id, &message); }
                    if method=="initialize" {
                        if message.get("error").is_some() { bail!("Codex refused initialization; update Codex or cx"); }
                        initialization=Some(message);
                        send(&mut writer,&json!({"method":"initialized","params":null})).await?;
                        serial+=1;
                        let id=format!("cx.auth.{serial}");
                        send(&mut writer,&json!({"id":id,"method":"account/login/start","params":initial_params.take().context("duplicate initialization")?})).await?;
                        login=Some(Login{revision:0,id,alias:initial.clone(),account:initial_account.clone(),started:Instant::now()});
                    } else { outgoing(&mut socket,message).await?; }
                } else {
                    turns.notification(&message);
                    if initialized { outgoing(&mut socket,message).await?; }
                    else { notifications.push(message); }
                }
            }
        }
        if initialized && login.is_none() && !turns.busy() && relays.pending.is_empty() {
            if let Some(alias) = status.pending.clone() {
                match credentials(&store, &alias, false).await {
                    Ok(account) => {
                        let unchanged = loaded.as_ref().is_some_and(|old| {
                            old.email == account.email
                                && old.account_id == account.account_id
                                && old.data.pointer("/tokens/access_token")
                                    == account.data.pointer("/tokens/access_token")
                        });
                        if unchanged && status.account.as_ref() == Some(&alias) {
                            status.pending = None;
                            for reply in waiting.drain(..) {
                                let _ = reply.send(status.clone());
                            }
                        } else {
                            let params = auth::login_params(&account)?;
                            serial += 1;
                            let id = format!("cx.auth.{serial}");
                            send(
                                &mut writer,
                                &json!({"id":id,"method":"account/login/start","params":params}),
                            )
                            .await?;
                            login = Some(Login {
                                revision: desired_revision,
                                id,
                                alias,
                                account,
                                started: Instant::now(),
                            });
                        }
                    }
                    Err(_) => {
                        status.error =
                            Some(format!("could not load account {alias}; sign in again"));
                        status.pending = None;
                        for reply in waiting.drain(..) {
                            let _ = reply.send(status.clone());
                        }
                    }
                }
            }
        }
        if initialized && login.is_none() {
            while let Some(message) = deferred.pop_front() {
                forward_client(
                    message,
                    &mut writer,
                    &mut socket,
                    ClientState {
                        requests: &mut requests,
                        turns: &mut turns,
                        pane_model: &mut pane_model,
                        initialized,
                        codex_home: &store.paths.codex_home,
                    },
                )
                .await?;
            }
        }
    }
}

async fn start_relay(
    control: RelayControl,
    ready: bool,
    tui: &ProcessEvidence,
    relays: &mut RelayState,
    turns: &mut Turns,
    writer: &mut ChildStdin,
) {
    let RelayControl {
        request,
        peer_pid,
        deadline,
        reply,
    } = control;
    if !ready {
        let _ = reply.send(RelayResponse::error(
            ErrorCode::Unavailable,
            Submission::NotStarted,
        ));
        return;
    }

    let (kind, request, queue) = match request {
        RelayRequest::Bind {
            thread_id, binding, ..
        } => {
            if tui.validate_descendant(peer_pid).is_err() {
                let _ = reply.send(RelayResponse::error(
                    ErrorCode::Unauthorized,
                    Submission::NotStarted,
                ));
                return;
            }
            (
                PendingRelayKind::Bind {
                    thread_id,
                    secret: binding,
                    peer_pid,
                },
                json!({"method":"thread/read","params":{"threadId":thread_id,"includeTurns":false}}),
                false,
            )
        }
        RelayRequest::Inspect {
            thread_id, binding, ..
        } => {
            if relays.bindings.authenticate(thread_id, &binding).is_err() {
                let _ = reply.send(RelayResponse::error(
                    ErrorCode::Unauthorized,
                    Submission::NotStarted,
                ));
                return;
            }
            (
                PendingRelayKind::Inspect { thread_id },
                json!({"method":"thread/read","params":{"threadId":thread_id,"includeTurns":false}}),
                false,
            )
        }
        RelayRequest::Queue {
            thread_id,
            binding,
            attempt_id,
            text,
            ..
        } => {
            if relays.bindings.authenticate(thread_id, &binding).is_err() {
                let _ = reply.send(RelayResponse::error(
                    ErrorCode::Unauthorized,
                    Submission::NotStarted,
                ));
                return;
            }
            let input = json!([{"type":"text","text":text,"text_elements":[]}]);
            (
                PendingRelayKind::Queue {
                    thread_id,
                    attempt_id,
                    input: input.clone(),
                },
                json!({"method":"thread/queue/add","params":{
                    "threadId":thread_id,
                    "clientUserMessageId":attempt_id,
                    "input":input
                }}),
                true,
            )
        }
    };

    relays.serial = relays.serial.saturating_add(1);
    let id = format!("cx.lam.{}", relays.serial);
    let mut request = request;
    request["id"] = json!(id);
    if queue {
        if let PendingRelayKind::Queue { thread_id, .. } = &kind {
            turns.starting(&id, &thread_id.to_string());
        }
    }
    relays.pending.insert(
        id.clone(),
        PendingRelay {
            kind,
            reply,
            deadline,
        },
    );

    if !matches!(
        tokio::time::timeout_at(deadline, send(writer, &request)).await,
        Ok(Ok(()))
    ) {
        if let Some(failed) = relays.pending.remove(&id) {
            if queue {
                turns.start_response(&id, &json!({"id":id,"error":{"code":-32000}}));
            }
            let _ = failed.reply.send(RelayResponse::error(
                ErrorCode::UpstreamError,
                if queue {
                    Submission::Uncertain
                } else {
                    Submission::NotStarted
                },
            ));
        }
    }
}

fn finish_relay(
    message: &Value,
    kind: PendingRelayKind,
    bindings: &mut BindingTable,
    tui: &ProcessEvidence,
) -> RelayResponse {
    let queue = matches!(&kind, PendingRelayKind::Queue { .. });
    let submission = if queue {
        Submission::Uncertain
    } else {
        Submission::NotStarted
    };
    if message.get("error").is_some() {
        return RelayResponse::error(ErrorCode::UpstreamError, submission);
    }

    match kind {
        PendingRelayKind::Bind {
            thread_id,
            secret,
            peer_pid,
        } => {
            let expected = thread_id.to_string();
            if message.pointer("/result/thread/id").and_then(Value::as_str)
                != Some(expected.as_str())
            {
                return RelayResponse::error(ErrorCode::ThreadMismatch, submission);
            }
            match bindings.bind(thread_id, secret, peer_pid, tui) {
                Ok(()) => RelayResponse::bound(),
                Err(_) => RelayResponse::error(ErrorCode::Conflict, submission),
            }
        }
        PendingRelayKind::Inspect { thread_id } => {
            let expected = thread_id.to_string();
            if message.pointer("/result/thread/id").and_then(Value::as_str)
                != Some(expected.as_str())
            {
                return RelayResponse::error(ErrorCode::ThreadMismatch, submission);
            }
            if message.pointer("/result/thread/canAcceptDirectInput") == Some(&Value::Bool(false)) {
                return RelayResponse::error(ErrorCode::Unavailable, submission);
            }
            match message
                .pointer("/result/thread/status/type")
                .and_then(Value::as_str)
            {
                Some("idle") => RelayResponse::inspected(ThreadState::Idle),
                Some("active") => RelayResponse::inspected(ThreadState::Active),
                _ => RelayResponse::error(ErrorCode::UpstreamError, submission),
            }
        }
        PendingRelayKind::Queue {
            thread_id: _,
            attempt_id,
            input,
        } => {
            let queued = &message["result"]["queuedSubmission"];
            let receipt = queued["id"]
                .as_str()
                .and_then(|id| uuid::Uuid::parse_str(id).ok());
            let expected = attempt_id.to_string();
            if queued["clientUserMessageId"].as_str() != Some(expected.as_str())
                || queued["input"] != input
                || receipt.is_none()
            {
                return RelayResponse::error(ErrorCode::UpstreamError, submission);
            }
            RelayResponse::queued(receipt.expect("checked receipt"))
        }
    }
}

fn expire_relays(pending: &mut HashMap<String, PendingRelay>, turns: &mut Turns) {
    let now = Instant::now();
    let expired: Vec<_> = pending
        .iter()
        .filter_map(|(id, relay)| (relay.deadline <= now).then_some(id.clone()))
        .collect();
    for id in expired {
        let Some(relay) = pending.remove(&id) else {
            continue;
        };
        let queue = matches!(&relay.kind, PendingRelayKind::Queue { .. });
        if queue {
            turns.start_response(&id, &json!({"id":id,"error":{"code":-32000}}));
        }
        let _ = relay.reply.send(RelayResponse::error(
            ErrorCode::Timeout,
            if queue {
                Submission::Uncertain
            } else {
                Submission::NotStarted
            },
        ));
    }
}

async fn forward_client(
    mut message: Value,
    writer: &mut ChildStdin,
    socket: &mut Socket,
    state: ClientState<'_>,
) -> Result<()> {
    let ClientState {
        requests,
        turns,
        pane_model,
        initialized,
        codex_home,
    } = state;
    let method = message["method"].as_str().unwrap_or("").to_owned();
    if method == "initialized" && initialized {
        return Ok(());
    }
    if matches!(method.as_str(), "account/login/start" | "account/logout") {
        outgoing(socket,json!({"id":message["id"],"error":{"code":-32601,"message":"Use cx add / cx switch from another terminal to manage this session's account"}})).await?;
        return Ok(());
    }
    // The TUI updates its active thread separately. Saving this default would
    // change the model seen by every cx pane sharing CODEX_HOME.
    if method == "config/batchWrite" {
        if let Some(edits) = message
            .pointer_mut("/params/edits")
            .and_then(Value::as_array_mut)
        {
            let original_len = edits.len();
            edits.retain(|edit| !is_model_config_key(edit["keyPath"].as_str()));
            if original_len > 0 && edits.is_empty() {
                acknowledge_local_model_selection(socket, &message, codex_home).await?;
                return Ok(());
            }
        }
    } else if method == "config/value/write"
        && is_model_config_key(message.pointer("/params/keyPath").and_then(Value::as_str))
    {
        acknowledge_local_model_selection(socket, &message, codex_home).await?;
        return Ok(());
    }
    if method == "initialize" {
        message["params"]["capabilities"]["experimentalApi"] = json!(true);
    }
    if let Some(id) = requests.forward(&mut message) {
        pane_model.request(&id, &method, &message);
        if starts_turn(&method) {
            let thread = message
                .pointer("/params/threadId")
                .and_then(Value::as_str)
                .context("turn-creating request lacks threadId")?;
            turns.starting(&id, thread);
        }
    }
    send(writer, &message).await
}

struct ClientState<'a> {
    requests: &'a mut Requests,
    turns: &'a mut Turns,
    pane_model: &'a mut PaneModel,
    initialized: bool,
    codex_home: &'a Path,
}

#[derive(Default)]
struct PaneModel {
    model: Option<String>,
    effort: Option<Option<String>>,
    pending: std::collections::HashMap<String, ModelChange>,
}

#[derive(Default)]
struct ModelChange {
    model: Option<String>,
    effort: Option<Option<String>>,
}

impl PaneModel {
    fn request(&mut self, id: &str, method: &str, message: &Value) {
        let change = match method {
            "thread/start" | "thread/resume" => ModelChange {
                model: message
                    .pointer("/params/model")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                effort: optional_string(message.pointer("/params/config/model_reasoning_effort")),
            },
            "thread/settings/update" => ModelChange {
                model: message
                    .pointer("/params/model")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                effort: optional_string(message.pointer("/params/effort")),
            },
            _ => return,
        };
        if change.model.is_some() || change.effort.is_some() {
            self.pending.insert(id.to_owned(), change);
        }
    }

    fn response(&mut self, id: &str, method: &str, message: &mut Value) {
        if message.get("error").is_none() {
            if let Some(change) = self.pending.remove(id) {
                if let Some(model) = change.model {
                    self.model = Some(model);
                }
                if let Some(effort) = change.effort {
                    self.effort = Some(effort);
                }
            }
            if matches!(method, "thread/start" | "thread/resume") {
                if let Some(model) = message.pointer("/result/model").and_then(Value::as_str) {
                    self.model = Some(model.to_owned());
                }
                if let Some(effort) = optional_string(message.pointer("/result/reasoningEffort")) {
                    self.effort = Some(effort);
                }
            }
            if method == "config/read" {
                if let Some(config) = message.pointer_mut("/result/config") {
                    if let Some(model) = &self.model {
                        config["model"] = json!(model);
                    }
                    if let Some(effort) = &self.effort {
                        config["model_reasoning_effort"] = json!(effort);
                    }
                }
            }
        } else {
            self.pending.remove(id);
        }
    }
}

fn optional_string(value: Option<&Value>) -> Option<Option<String>> {
    match value {
        Some(Value::String(value)) => Some(Some(value.clone())),
        Some(Value::Null) => Some(None),
        _ => None,
    }
}

fn is_model_config_key(key: Option<&str>) -> bool {
    matches!(key, Some("model" | "model_reasoning_effort"))
}

async fn acknowledge_local_model_selection(
    socket: &mut Socket,
    message: &Value,
    codex_home: &Path,
) -> Result<()> {
    outgoing(
        socket,
        json!({
            "id": message["id"],
            "result": {
                "status": "ok",
                "version": "cx-session",
                "filePath": codex_home.join("config.toml"),
                "overriddenMetadata": null
            }
        }),
    )
    .await
}

pub fn print_status(report: &SessionStatus) {
    let active = format!(
        "{} ({})",
        report.account.as_deref().unwrap_or("unknown"),
        report.email.as_deref().unwrap_or("identity unknown")
    );
    if let Some(error) = &report.error {
        println!("session {}: {active} ({error})", report.pid);
    } else if let Some(pending) = &report.pending {
        println!(
            "session {}: {active} -> {pending} (pending until idle)",
            report.pid
        );
    } else {
        println!("session {}: {active}", report.pid);
    }
}

pub fn codex_binary() -> OsString {
    std::env::var_os("CX_CODEX_BIN").unwrap_or_else(|| OsString::from("codex"))
}

struct Launch {
    store: Store,
    alias: String,
    account: Account,
    lam_controls: mpsc::Receiver<RelayControl>,
    server_pid: u32,
}
struct Signals {
    terminate: tokio::signal::unix::Signal,
    interrupt: tokio::signal::unix::Signal,
}
impl Signals {
    fn new() -> Result<Self> {
        Ok(Self {
            terminate: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
            interrupt: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?,
        })
    }
}
