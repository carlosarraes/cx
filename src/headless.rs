//! Non-interactive app-server client with explicit exec-style argument support.
use crate::{
    auth,
    routing::{self, RunOptions},
    sessions::{Identity, Mode, Reservation},
    state::{Paths, Store},
};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{ChildStdin, ChildStdout, Command},
    signal::unix::{signal, SignalKind},
    time::timeout,
};

#[derive(Default, Debug)]
pub struct Options {
    pub resume: Option<String>,
    prompt: String,
    model: Option<String>,
    effort: Option<String>,
    config: BTreeMap<String, Value>,
    cwd: Option<PathBuf>,
    sandbox: Option<String>,
    json: bool,
    output: Option<PathBuf>,
    schema: Option<Value>,
    ephemeral: bool,
    skip_git: bool,
    add_dirs: Vec<PathBuf>,
}

pub fn parse(args: &[OsString]) -> Result<Options> {
    let args: Vec<_> = args
        .iter()
        .map(|s| {
            s.to_str()
                .map(str::to_string)
                .ok_or_else(|| routing::failure(2, "headless arguments must be UTF-8"))
        })
        .collect::<Result<_>>()?;
    let mut o = Options::default();
    let mut i = 1;
    if args.get(i).is_some_and(|s| s == "resume") {
        i += 1;
        o.resume = Some(
            args.get(i)
                .filter(|s| !s.starts_with('-'))
                .cloned()
                .ok_or_else(|| {
                    routing::failure(2, "exec resume requires an explicit conversation ID")
                })?,
        );
        i += 1;
    }
    let mut positional = false;
    let mut prompt = None;
    while i < args.len() {
        let arg = &args[i];
        i += 1;
        if arg == "--" && !positional {
            positional = true;
            continue;
        }
        if !positional && arg.starts_with('-') && arg != "-" {
            let (flag, inline) = arg
                .split_once('=')
                .map_or((arg.as_str(), None), |(k, v)| (k, Some(v)));
            let mut value = || -> Result<String> {
                if let Some(v) = inline {
                    Ok(v.into())
                } else {
                    let v = args
                        .get(i)
                        .cloned()
                        .ok_or_else(|| routing::failure(2, format!("{flag} requires a value")))?;
                    i += 1;
                    Ok(v)
                }
            };
            match flag {
                "-m" | "--model" => o.model = Some(value()?),
                "-c" | "--config" => {
                    let v = value()?;
                    let (key, raw) = v
                        .split_once('=')
                        .ok_or_else(|| routing::failure(2, "--config requires key=value"))?;
                    if key.contains("auth") || key.starts_with("model_provider") {
                        return Err(routing::failure(
                            2,
                            "headless account routing does not accept auth or provider overrides",
                        ));
                    }
                    let parsed = toml::from_str::<toml::Table>(&format!("value={raw}"))
                        .ok()
                        .and_then(|mut t| t.remove("value"))
                        .map(|v| serde_json::to_value(v).unwrap_or(Value::Null))
                        .unwrap_or_else(|| json!(raw));
                    o.config.insert(key.into(), parsed);
                }
                "-C" | "--cd" => o.cwd = Some(value()?.into()),
                "-s" | "--sandbox" => {
                    let v = value()?;
                    if !["read-only", "workspace-write", "danger-full-access"].contains(&v.as_str())
                    {
                        return Err(routing::failure(2, "invalid sandbox mode"));
                    }
                    o.sandbox = Some(v);
                }
                "-o" | "--output-last-message" => o.output = Some(value()?.into()),
                "--output-schema" => {
                    let p = value()?;
                    o.schema = Some(
                        serde_json::from_slice(
                            &fs::read(p)
                                .map_err(|_| routing::failure(2, "cannot read output schema"))?,
                        )
                        .map_err(|_| routing::failure(2, "invalid output schema"))?,
                    );
                }
                "--add-dir" => o.add_dirs.push(value()?.into()),
                "--json" if inline.is_none() => o.json = true,
                "--ephemeral" if inline.is_none() => o.ephemeral = true,
                "--skip-git-repo-check" if inline.is_none() => o.skip_git = true,
                "--dangerously-bypass-approvals-and-sandbox" if inline.is_none() => {
                    o.sandbox = Some("danger-full-access".into())
                }
                _ => {
                    return Err(routing::failure(
                        2,
                        format!("unsupported headless argument {flag}; see cx --llm"),
                    ))
                }
            }
        } else if prompt.replace(arg.clone()).is_some() {
            return Err(routing::failure(
                2,
                "headless exec accepts one prompt argument",
            ));
        }
    }
    if o.resume.is_some() && o.ephemeral {
        return Err(routing::failure(
            2,
            "--ephemeral cannot be combined with exec resume",
        ));
    }
    o.model = o.model.or_else(|| {
        o.config
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    o.effort = o
        .config
        .get("model_reasoning_effort")
        .and_then(Value::as_str)
        .map(str::to_string);
    o.prompt = match prompt {
        Some(p) if p != "-" => p,
        _ => {
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s)?;
            s
        }
    };
    if o.prompt.trim().is_empty() {
        return Err(routing::failure(
            2,
            "headless exec requires a prompt or stdin",
        ));
    }
    let cwd = o.cwd.clone().unwrap_or(std::env::current_dir()?);
    o.cwd = Some(
        cwd.canonicalize()
            .map_err(|_| routing::failure(2, "working directory does not exist"))?,
    );
    if !o.skip_git
        && !std::process::Command::new("git")
            .args(["rev-parse", "--is-inside-work-tree"])
            .current_dir(o.cwd.as_ref().unwrap())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    {
        return Err(routing::failure(
            2,
            "not in a Git repository; pass --skip-git-repo-check",
        ));
    }
    if !o.add_dirs.is_empty() {
        let roots = o
            .add_dirs
            .iter()
            .map(|p| {
                p.canonicalize()
                    .map_err(|_| routing::failure(2, "--add-dir path does not exist"))
            })
            .collect::<Result<Vec<_>>>()?;
        o.config.insert(
            "sandbox_workspace_write.writable_roots".into(),
            json!(roots),
        );
    }
    Ok(o)
}

pub fn run(
    paths: Paths,
    binary: OsString,
    args: Vec<OsString>,
    options: RunOptions,
) -> Result<i32> {
    let mut report = json!({"schema_version":1,"started_at":chrono::Utc::now().to_rfc3339(),"finished_at":null,"account":null,"label":options.label,"session_id":null,"requested_model":null,"requested_effort":null,"resolved_model":null,"resolved_effort":null,"model_evidence":"unavailable: no thread response","effort_evidence":"unavailable: no thread response","reroutes":[],"usage":null,"usage_evidence":"unavailable: no token notification","native_exit_status":null,"exit_code":70,"outcome":"launcher_error"});
    let result = (|| -> Result<i32> {
        let parsed = parse(&args)?;
        report["requested_model"] = json!(parsed.model);
        report["requested_effort"] = json!(parsed.effort);
        let reservation =
            routing::reserve(&paths, &options, parsed.resume.as_deref(), Mode::Headless)?;
        let identity = reservation.get()?.account;
        report["account"] = serde_json::to_value(&identity)?;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        rt.block_on(execute(
            &paths,
            &binary,
            &parsed,
            &reservation,
            &identity,
            &mut report,
        ))
    })();
    let code = match result {
        Ok(c) => c,
        Err(e) => {
            let code = e.downcast_ref::<routing::Failure>().map_or(70, |f| f.code);
            eprintln!("cx: {e}");
            code
        }
    };
    report["exit_code"] = json!(code);
    report["outcome"] = json!(outcome(code));
    report["finished_at"] = json!(chrono::Utc::now().to_rfc3339());
    if let Some(path) = options.usage_file {
        atomic_write(&path, &serde_json::to_vec_pretty(&report)?)
            .map_err(|_| routing::failure(70, "cannot write usage record"))?;
    }
    Ok(code)
}
fn outcome(code: i32) -> &'static str {
    match code {
        0 => "success",
        1 => "task_failure",
        2 => "invalid_arguments",
        20 => "quota",
        21 => "authentication",
        22 => "model_mismatch",
        23 => "capacity",
        130 | 143 => "interrupted",
        _ => "launcher_error",
    }
}
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut f = tempfile::NamedTempFile::new_in(parent)?;
    f.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    f.write_all(bytes)?;
    f.as_file().sync_all()?;
    f.persist(path).map_err(|e| e.error)?;
    Ok(())
}

struct Client {
    writer: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    serial: u64,
    store: Store,
    identity: Identity,
}
impl Client {
    async fn send(&mut self, value: Value) -> Result<()> {
        let mut b = serde_json::to_vec(&value)?;
        b.push(b'\n');
        self.writer.write_all(&b).await?;
        self.writer.flush().await?;
        Ok(())
    }
    async fn next(&mut self) -> Result<Value> {
        loop {
            let line = self
                .lines
                .next_line()
                .await?
                .context("Codex app-server closed unexpectedly")?;
            let m: Value = serde_json::from_str(&line)
                .map_err(|_| routing::failure(70, "invalid app-server response"))?;
            if m.get("method").is_some() && m.get("id").is_some() {
                if m["method"] == "account/chatgptAuthTokens/refresh" {
                    let store = self.store.clone();
                    let identity = self.identity.clone();
                    if m.pointer("/params/previousAccountId")
                        .and_then(Value::as_str)
                        .is_some_and(|s| s != identity.account_id)
                    {
                        return Err(routing::failure(
                            21,
                            "refresh requested for a different account",
                        ));
                    }
                    let a = tokio::task::spawn_blocking(move || {
                        auth::credentials_for(
                            &store,
                            &identity.alias,
                            &identity.email,
                            &identity.account_id,
                            true,
                        )
                    })
                    .await?
                    .map_err(|_| routing::failure(21, "account refresh failed; sign in again"))?;
                    let mut params = auth::login_params(&a)?;
                    params.as_object_mut().unwrap().remove("type");
                    self.send(json!({"id":m["id"],"result":params})).await?;
                } else {
                    self.send(json!({"id":m["id"],"error":{"code":-32000,"message":"cx headless cannot request interactive approval or input"}})).await?;
                    return Err(routing::failure(
                        1,
                        "task requested interactive approval or input",
                    ));
                }
                continue;
            }
            return Ok(m);
        }
    }
    async fn request(
        &mut self,
        method: &str,
        params: Value,
        events: &mut Events<'_>,
    ) -> Result<Value> {
        self.serial += 1;
        let id = format!("cx.headless.{}", self.serial);
        self.send(json!({"id":id,"method":method,"params":params}))
            .await?;
        timeout(Duration::from_secs(30), async {
            loop {
                let m = self.next().await?;
                if m["id"] == id {
                    if let Some(error) = m.get("error") {
                        return Err(routing::failure(
                            classify(error),
                            format!("Codex rejected {method}"),
                        ));
                    }
                    return Ok(m["result"].clone());
                }
                events.observe(&m)?;
            }
        })
        .await
        .map_err(|_| routing::failure(70, format!("Codex {method} timed out")))?
    }
}

struct Events<'a> {
    report: &'a mut Value,
    json: bool,
    resumed: bool,
    thread: Option<String>,
    turn: Option<String>,
    baseline: Option<Value>,
    answer: String,
    code: Option<i32>,
    started: bool,
}
impl Events<'_> {
    fn emit(&self, event: Value) -> Result<()> {
        if self.json {
            let mut out = std::io::stdout().lock();
            serde_json::to_writer(&mut out, &event)?;
            writeln!(out)?;
        }
        Ok(())
    }
    fn observe(&mut self, m: &Value) -> Result<()> {
        let p = &m["params"];
        if let Some(thread) = p.get("threadId").and_then(Value::as_str) {
            if self.thread.as_deref().is_some_and(|t| t != thread) {
                return Ok(());
            }
        }
        match m["method"].as_str().unwrap_or("") {
            "turn/started" => {
                self.started = true;
                self.turn = p
                    .pointer("/turn/id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                self.emit(json!({"type":"turn.started"}))?;
            }
            "thread/tokenUsage/updated" => {
                let total = &p["tokenUsage"]["total"];
                if !self.started {
                    self.baseline = Some(total.clone());
                } else if !self.resumed || self.baseline.is_some() {
                    let mut usage = serde_json::Map::new();
                    for (native, key) in [
                        ("inputTokens", "input_tokens"),
                        ("cachedInputTokens", "cached_input_tokens"),
                        ("outputTokens", "output_tokens"),
                        ("reasoningOutputTokens", "reasoning_output_tokens"),
                        ("cacheWriteInputTokens", "cache_write_input_tokens"),
                    ] {
                        if let Some(n) = total[native].as_u64() {
                            let base = self
                                .baseline
                                .as_ref()
                                .and_then(|b| b[native].as_u64())
                                .unwrap_or(0);
                            if n < base {
                                return Err(routing::failure(70, "token counters decreased"));
                            }
                            usage.insert(key.into(), json!(n - base));
                        }
                    }
                    self.report["usage"] = Value::Object(usage);
                    self.report["usage_evidence"] =
                        json!("app-server cumulative counters minus pre-turn baseline");
                } else {
                    self.report["usage_evidence"] =
                        json!("unavailable: resumed thread has no pre-turn baseline");
                }
            }
            "model/rerouted" => {
                self.report["reroutes"].as_array_mut().unwrap().push(json!({"from_model":p["fromModel"],"to_model":p["toModel"],"reason":p["reason"]}));
                if p["toModel"] != self.report["resolved_model"] {
                    self.code = Some(22);
                }
            }
            "item/started" | "item/completed" => {
                let item = &p["item"];
                let kind = match item["type"].as_str().unwrap_or("") {
                    "agentMessage" => "agent_message",
                    "reasoning" => "reasoning",
                    "commandExecution" => "command_execution",
                    "fileChange" => "file_change",
                    "mcpToolCall" => "mcp_tool_call",
                    "webSearch" => "web_search",
                    _ => "app_server_item",
                };
                let mut out = json!({"id":item["id"],"type":kind});
                if let Some(text) = item.get("text") {
                    out["text"] = text.clone();
                    if kind == "agent_message" && m["method"] == "item/completed" {
                        self.answer = text.as_str().unwrap_or("").into();
                    }
                }
                for (source, target) in [
                    ("command", "command"),
                    ("aggregatedOutput", "aggregated_output"),
                    ("exitCode", "exit_code"),
                    ("status", "status"),
                    ("changes", "changes"),
                    ("server", "server"),
                    ("tool", "tool"),
                ] {
                    if let Some(v) = item.get(source) {
                        out[target] = v.clone();
                    }
                }
                self.emit(json!({"type":if m["method"]=="item/started" {"item.started"}else{"item.completed"},"item":out}))?;
            }
            "turn/completed" => {
                let status = p
                    .pointer("/turn/status")
                    .and_then(Value::as_str)
                    .unwrap_or("failed");
                let code = match status {
                    "completed" => 0,
                    "interrupted" => 130,
                    _ => classify(&p["turn"]["error"]),
                };
                self.code = Some(self.code.filter(|c| *c != 0).unwrap_or(code));
            }
            _ => {}
        }
        Ok(())
    }
}
fn classify(error: &Value) -> i32 {
    let info = error
        .get("codexErrorInfo")
        .or_else(|| error.pointer("/data/codexErrorInfo"));
    match info.and_then(Value::as_str) {
        Some("usageLimitExceeded" | "rateLimitExceeded") => 20,
        Some("unauthorized") => 21,
        _ => {
            if info
                .and_then(Value::as_object)
                .is_some_and(|o| o.values().any(|v| v["httpStatusCode"] == 401))
            {
                21
            } else {
                1
            }
        }
    }
}

async fn execute(
    paths: &Paths,
    binary: &OsString,
    o: &Options,
    reservation: &Reservation,
    identity: &Identity,
    report: &mut Value,
) -> Result<i32> {
    let store = Store::new(paths.clone());
    let a = {
        let store = store.clone();
        let id = identity.clone();
        tokio::task::spawn_blocking(move || {
            auth::credentials_for(&store, &id.alias, &id.email, &id.account_id, false)
        })
        .await?
        .map_err(|_| routing::failure(21, "selected account credentials unavailable"))?
    };
    let mut server = Command::new(binary)
        .args([
            "app-server",
            "--listen",
            "stdio://",
            "-c",
            "cli_auth_credentials_store=\"ephemeral\"",
        ])
        .env("CODEX_HOME", &paths.codex_home)
        .env_remove("OPENAI_API_KEY")
        .env_remove("CODEX_API_KEY")
        .current_dir(o.cwd.as_ref().unwrap())
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("starting headless Codex app-server")?;
    let mut client = Client {
        writer: server.stdin.take().unwrap(),
        lines: BufReader::new(server.stdout.take().unwrap()).lines(),
        serial: 0,
        store,
        identity: identity.clone(),
    };
    let mut events = Events {
        report,
        json: o.json,
        resumed: o.resume.is_some(),
        thread: None,
        turn: None,
        baseline: None,
        answer: String::new(),
        code: None,
        started: false,
    };
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let work = async {
        client.request("initialize",json!({"clientInfo":{"name":"cx_headless","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}}),&mut events).await?;
        client.send(json!({"method":"initialized"})).await?;
        client
            .request("account/login/start", auth::login_params(&a)?, &mut events)
            .await
            .map_err(|_| routing::failure(21, "Codex rejected pinned account authentication"))?;
        let mut params = json!({"approvalPolicy":"never","cwd":o.cwd,"modelProvider":"openai","config":o.config});
        if let Some(model) = &o.model {
            params["model"] = json!(model);
        }
        if let Some(sandbox) = &o.sandbox {
            params["sandbox"] = json!(sandbox);
        }
        let method = if let Some(root) = &o.resume {
            params["threadId"] = json!(root);
            "thread/resume"
        } else {
            params["ephemeral"] = json!(o.ephemeral);
            "thread/start"
        };
        let response = client.request(method, params, &mut events).await?;
        let thread = response
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .context("missing conversation ID")?
            .to_owned();
        events.thread = Some(thread.clone());
        events.report["session_id"] = json!(thread);
        events.report["resolved_model"] = response["model"].clone();
        events.report["resolved_effort"] = response["reasoningEffort"].clone();
        events.report["model_evidence"] = json!(if response["model"].is_string() {
            "app-server thread response; backend not attested"
        } else {
            "unavailable: thread response omitted model"
        });
        events.report["effort_evidence"] = json!(if response["reasoningEffort"].is_string() {
            "app-server thread response"
        } else {
            "unavailable: thread response omitted reasoning effort"
        });
        if o.model
            .as_ref()
            .is_some_and(|v| response["model"].as_str().is_some_and(|m| m != v))
            || o.effort
                .as_ref()
                .is_some_and(|v| response["reasoningEffort"].as_str().is_some_and(|m| m != v))
        {
            return Err(routing::failure(
                22,
                "resolved model or reasoning effort differs from request",
            ));
        }
        reservation.attach(&thread, true)?;
        events.emit(json!({"type":"thread.started","thread_id":thread}))?;
        let mut params = json!({"threadId":thread,"input":[{"type":"text","text":o.prompt}],"approvalPolicy":"never"});
        if let Some(model) = &o.model {
            params["model"] = json!(model);
        }
        if let Some(effort) = &o.effort {
            params["effort"] = json!(effort);
        }
        if let Some(schema) = &o.schema {
            params["outputSchema"] = schema.clone();
        }
        let response = client.request("turn/start", params, &mut events).await?;
        events.started = true;
        if events.turn.is_none() {
            events.turn = response
                .pointer("/turn/id")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        while events.code.is_none() {
            let m = client.next().await?;
            events.observe(&m)?;
        }
        Ok::<i32, anyhow::Error>(events.code.unwrap())
    };
    let result = tokio::select! {r=work=>r,_=interrupt.recv()=>Ok(130),_=terminate.recv()=>Ok(143)};
    // Reap the leader and kill surviving tools before releasing the reservation.
    let group = server.id();
    if let Some(pid) = group {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGTERM);
        }
    }
    let exited = timeout(Duration::from_secs(2), server.wait()).await;
    if let Some(pid) = group {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    if exited.is_err() {
        let _ = server.kill().await;
        let _ = server.wait().await;
    }
    if let Some(path) = &o.output {
        atomic_write(path, format!("{}\n", events.answer).as_bytes())
            .map_err(|_| routing::failure(70, "cannot write last-message output"))?;
    }
    let code = result
        .as_ref()
        .copied()
        .unwrap_or_else(|e| e.downcast_ref::<routing::Failure>().map_or(70, |e| e.code));
    if o.json {
        events.emit(if code == 0 {
            json!({"type":"turn.completed","usage":events.report["usage"]})
        } else {
            json!({"type":"turn.failed","error":{"message":outcome(code)},"exit_code":code})
        })?;
    } else if !events.answer.is_empty() {
        println!("{}", events.answer);
    }
    result
}
