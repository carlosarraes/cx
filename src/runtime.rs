//! One native Codex app-server/TUI pair, controlled over private Unix sockets.
use crate::{
    auth,
    protocol::{starts_turn, Requests, Turns},
    state::{Account, Paths, Store},
};
use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::VecDeque, ffi::OsString, fs, os::unix::fs::PermissionsExt, path::PathBuf,
    process::Stdio, time::Duration,
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
    let tui_socket = transport.path().join("tui.sock");
    let listener = UnixListener::bind(&tui_socket)?;
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
            let (tx, rx) = mpsc::channel(16);
            let control_task = tokio::spawn(controls(control_listener, tx));
            let result = relay(&mut server, &mut tui, socket, rx, Launch { store, alias: initial, account }, &mut signals).await;
            control_task.abort();
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
    let mut turns = Turns::default();
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
            frame = socket.next() => {
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        let message: Value = serde_json::from_str(&text).context("invalid Codex TUI protocol message")?;
                        if login.is_some() { deferred.push_back(message); }
                        else { forward_client(message, &mut writer, &mut socket, &mut requests, &mut turns, initialized).await?; }
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
        if initialized && login.is_none() && !turns.busy() {
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
                    &mut requests,
                    &mut turns,
                    initialized,
                )
                .await?;
            }
        }
    }
}

async fn forward_client(
    mut message: Value,
    writer: &mut ChildStdin,
    socket: &mut Socket,
    requests: &mut Requests,
    turns: &mut Turns,
    initialized: bool,
) -> Result<()> {
    let method = message["method"].as_str().unwrap_or("").to_owned();
    if method == "initialized" && initialized {
        return Ok(());
    }
    if matches!(method.as_str(), "account/login/start" | "account/logout") {
        outgoing(socket,json!({"id":message["id"],"error":{"code":-32601,"message":"Use cx add / cx switch from another terminal to manage this session's account"}})).await?;
        return Ok(());
    }
    if method == "initialize" {
        message["params"]["capabilities"]["experimentalApi"] = json!(true);
    }
    if let Some(id) = requests.forward(&mut message) {
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
