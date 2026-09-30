use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use rmcp::{
    ClientHandler, ErrorData, RoleClient, RoleServer, ServerHandler, ServiceError, ServiceExt,
    model::{
        CallToolRequest, CallToolRequestParams, CallToolResponse, CallToolResult,
        ClientCapabilities, ClientConfig, ClientRequest, ContentBlock, Implementation,
        ListToolsResult, PaginatedRequestParams, ProgressNotificationParam, ProgressToken,
        ServerCapabilities, ServerConfig, ServerResult, Tool,
    },
    service::{NotificationContext, Peer, PeerRequestOptions, RequestContext, RunningService},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    process::{Child, Command},
    signal::unix::{SignalKind, signal},
    sync::OnceCell,
    task::JoinSet,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

// Servers get their stdin closed first; playwright-mcp needs up to 15 s to
// close the browser before its watchdog exits, so the grace period must be
// longer than that.
const CLOSE_GRACE: Duration = Duration::from_secs(20);
const TERM_GRACE: Duration = Duration::from_secs(5);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Parser)]
#[command(
    about = "Stdio MCP proxy that runs one server process per session ID carried in tools/call _meta"
)]
struct Args {
    #[arg(
        long,
        default_value_t = 1800,
        help = "Seconds after a session's last tool call before its server process is stopped; 0 keeps it until shutdown"
    )]
    idle_timeout: u64,
    #[arg(
        long,
        default_value = "ai.opencode/sessionID",
        help = "Key in tools/call _meta that carries the session ID"
    )]
    meta_key: String,
    #[arg(
        long,
        default_value_t = 30,
        help = "Seconds a server process may take to finish the MCP handshake"
    )]
    startup_timeout: u64,
    #[arg(
        long,
        value_name = "FILE",
        conflicts_with = "probe",
        help = "JSON file written by --probe with the server's info and tool list; without it, the server is started once at startup to obtain them"
    )]
    manifest: Option<PathBuf>,
    #[arg(
        long,
        help = "Start the server once, print the manifest JSON to stdout, and exit"
    )]
    probe: bool,
    #[arg(
        last = true,
        num_args = 1..,
        required = true,
        value_name = "COMMAND",
        help = "Server command and its arguments, after --"
    )]
    command: Vec<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    server_info: Option<Implementation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    tools: Vec<Tool>,
}

impl Manifest {
    async fn probe(command: &[String], startup_timeout: Duration) -> Result<Self> {
        let (backend, running) = spawn_backend(command, startup_timeout).await?;
        let probed: Result<Self> = async {
            let info = backend
                .peer
                .peer_info()
                .context("server sent no initialize result")?;
            if info.capabilities.tools.is_none() {
                bail!("server does not advertise the tools capability");
            }
            let tools = timeout(startup_timeout, backend.peer.list_all_tools())
                .await
                .context("tools/list timed out")??;
            Ok(Self {
                server_info: info.server_info.clone(),
                instructions: info.instructions.clone(),
                tools,
            })
        }
        .await;
        backend.stop().await;
        drop(running);
        probed.with_context(|| format!("probing `{}`", command[0]))
    }

    fn load(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path)
            .with_context(|| format!("opening manifest `{}`", path.display()))?;
        serde_json::from_reader(std::io::BufReader::new(file))
            .with_context(|| format!("parsing manifest `{}`", path.display()))
    }

    fn print(&self) -> Result<()> {
        let mut stdout = std::io::stdout().lock();
        serde_json::to_writer_pretty(&mut stdout, self)?;
        stdout.write_all(b"\n")?;
        Ok(())
    }
}

type ProgressRoutes = Arc<Mutex<HashMap<ProgressToken, (Peer<RoleServer>, ProgressToken)>>>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn tool_error(message: String) -> CallToolResponse {
    CallToolResponse::Complete(CallToolResult::error(vec![ContentBlock::text(message)]))
}

fn own_implementation() -> Implementation {
    Implementation::new("mcp-session-mux", env!("CARGO_PKG_VERSION"))
}

async fn reap(child: &mut Child, pid: u32, close_grace: Duration) {
    if timeout(close_grace, child.wait()).await.is_ok() {
        return;
    }
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    if timeout(TERM_GRACE, child.wait()).await.is_ok() {
        return;
    }
    let _ = child.kill().await;
}

struct Backend {
    peer: Peer<RoleClient>,
    ct: CancellationToken,
    pid: u32,
    child: tokio::sync::Mutex<Option<Child>>,
    progress: ProgressRoutes,
}

impl Backend {
    async fn stop(&self) {
        self.ct.cancel();
        let mut slot = self.child.lock().await;
        if let Some(mut child) = slot.take() {
            reap(&mut child, self.pid, CLOSE_GRACE).await;
        }
    }
}

#[derive(Clone)]
struct ChildHandler {
    progress: ProgressRoutes,
}

impl ClientHandler for ChildHandler {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(ClientCapabilities::default(), own_implementation())
    }

    async fn on_progress(
        &self,
        mut params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        let route = lock(&self.progress).get(&params.progress_token).cloned();
        if let Some((peer, token)) = route {
            params.progress_token = token;
            let _ = peer.notify_progress(params).await;
        }
    }
}

struct ProgressRoute {
    routes: ProgressRoutes,
    token: ProgressToken,
}

impl ProgressRoute {
    fn register(
        routes: &ProgressRoutes,
        token: ProgressToken,
        upstream: (Peer<RoleServer>, ProgressToken),
    ) -> Self {
        lock(routes).insert(token.clone(), upstream);
        Self {
            routes: routes.clone(),
            token,
        }
    }
}

impl Drop for ProgressRoute {
    fn drop(&mut self) {
        lock(&self.routes).remove(&self.token);
    }
}

async fn spawn_backend(
    command: &[String],
    startup_timeout: Duration,
) -> Result<(Arc<Backend>, RunningService<RoleClient, ChildHandler>)> {
    let (program, args) = command.split_first().context("empty server command")?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to start `{program}`"))?;
    let pid = child
        .id()
        .context("server process exited before it could be tracked")?;
    let stdout = child.stdout.take().context("server stdout is not piped")?;
    let stdin = child.stdin.take().context("server stdin is not piped")?;
    let progress = ProgressRoutes::default();
    let ct = CancellationToken::new();
    let handshake = ChildHandler {
        progress: progress.clone(),
    }
    .serve_with_ct((stdout, stdin), ct.clone());
    let running = match timeout(startup_timeout, handshake).await {
        Ok(Ok(running)) => running,
        Ok(Err(error)) => {
            reap(&mut child, pid, Duration::ZERO).await;
            return Err(anyhow!(error))
                .with_context(|| format!("MCP handshake with `{program}` (pid {pid}) failed"));
        }
        Err(_) => {
            reap(&mut child, pid, Duration::ZERO).await;
            bail!(
                "`{program}` (pid {pid}) did not finish the MCP handshake within {}s",
                startup_timeout.as_secs()
            );
        }
    };
    let backend = Arc::new(Backend {
        peer: running.peer().clone(),
        ct,
        pid,
        child: tokio::sync::Mutex::new(Some(child)),
        progress,
    });
    Ok((backend, running))
}

#[derive(Default)]
struct Session {
    backend: OnceCell<Arc<Backend>>,
}

struct Entry {
    session: Arc<Session>,
    in_flight: usize,
    last_used: Instant,
}

struct Inner {
    meta_key: String,
    command: Vec<String>,
    startup_timeout: Duration,
    idle_timeout: Option<Duration>,
    server_config: ServerConfig,
    tools: Vec<Tool>,
    sessions: Mutex<HashMap<String, Entry>>,
}

#[derive(Clone)]
struct Mux(Arc<Inner>);

struct Lease {
    mux: Mux,
    id: String,
    session: Arc<Session>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(entry) = lock(&self.mux.0.sessions).get_mut(&self.id)
            && Arc::ptr_eq(&entry.session, &self.session)
        {
            entry.in_flight = entry.in_flight.saturating_sub(1);
            entry.last_used = Instant::now();
        }
    }
}

impl Mux {
    fn new(args: Args, manifest: Manifest) -> Self {
        let mut server_config =
            ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
                .with_server_info(manifest.server_info.unwrap_or_else(own_implementation));
        server_config.instructions = manifest.instructions;
        Self(Arc::new(Inner {
            meta_key: args.meta_key,
            command: args.command,
            startup_timeout: Duration::from_secs(args.startup_timeout),
            idle_timeout: (args.idle_timeout > 0).then(|| Duration::from_secs(args.idle_timeout)),
            server_config,
            tools: manifest.tools,
            sessions: Mutex::default(),
        }))
    }

    fn acquire(&self, id: &str) -> Lease {
        let mut sessions = lock(&self.0.sessions);
        let entry = sessions.entry(id.to_owned()).or_insert_with(|| Entry {
            session: Arc::default(),
            in_flight: 0,
            last_used: Instant::now(),
        });
        entry.in_flight += 1;
        entry.last_used = Instant::now();
        Lease {
            mux: self.clone(),
            id: id.to_owned(),
            session: entry.session.clone(),
        }
    }

    fn evict(&self, id: &str, session: &Arc<Session>) -> bool {
        let mut sessions = lock(&self.0.sessions);
        if sessions
            .get(id)
            .is_some_and(|entry| Arc::ptr_eq(&entry.session, session))
        {
            sessions.remove(id);
            true
        } else {
            false
        }
    }

    fn retire(&self, id: &str, session: &Arc<Session>, backend: &Arc<Backend>, reason: &str) {
        if self.evict(id, session) {
            eprintln!(
                "mcp-session-mux: session {id}: server (pid {}) {reason}",
                backend.pid
            );
        }
        let backend = backend.clone();
        tokio::spawn(async move { backend.stop().await });
    }

    async fn start(&self, id: &str, session: &Arc<Session>) -> Result<Arc<Backend>> {
        let (backend, running) = spawn_backend(&self.0.command, self.0.startup_timeout).await?;
        eprintln!(
            "mcp-session-mux: session {id}: started server (pid {})",
            backend.pid
        );
        let watched = (
            self.clone(),
            id.to_owned(),
            session.clone(),
            backend.clone(),
        );
        tokio::spawn(async move {
            let (mux, id, session, backend) = watched;
            let _ = running.waiting().await;
            let evicted = mux.evict(&id, &session);
            backend.stop().await;
            if evicted {
                eprintln!(
                    "mcp-session-mux: session {id}: server (pid {}) exited on its own",
                    backend.pid
                );
            }
        });
        Ok(backend)
    }

    async fn shutdown(&self) {
        let sessions: Vec<(String, Arc<Session>)> = lock(&self.0.sessions)
            .drain()
            .map(|(id, entry)| (id, entry.session))
            .collect();
        let mut stops = JoinSet::new();
        for (id, session) in sessions {
            if let Some(backend) = session.backend.get().cloned() {
                eprintln!(
                    "mcp-session-mux: session {id}: shutting down, stopping server (pid {})",
                    backend.pid
                );
                stops.spawn(async move { backend.stop().await });
            }
        }
        if timeout(SHUTDOWN_TIMEOUT, stops.join_all()).await.is_err() {
            eprintln!(
                "mcp-session-mux: some server processes did not exit within {}s",
                SHUTDOWN_TIMEOUT.as_secs()
            );
        }
    }
}

impl ServerHandler for Mux {
    fn get_info(&self) -> ServerConfig {
        self.0.server_config.clone()
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(self.0.tools.clone()))
    }

    async fn call_tool(
        &self,
        params: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let Some(session_id) = context
            .meta
            .get(self.0.meta_key.as_str())
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            return Ok(tool_error(format!(
                "tools/call `{}` carries no string `_meta[\"{}\"]`, so there is no session to route it to. Is opencode patched to send the session ID?",
                params.name, self.0.meta_key
            )));
        };
        for _ in 0..2 {
            let lease = self.acquire(&session_id);
            let backend = match lease
                .session
                .backend
                .get_or_try_init(|| self.start(&session_id, &lease.session))
                .await
            {
                Ok(backend) => backend.clone(),
                Err(error) => return Ok(tool_error(format!("{error:#}"))),
            };
            if backend.peer.is_transport_closed() {
                self.retire(&session_id, &lease.session, &backend, "exited");
                continue;
            }
            let request = ClientRequest::CallToolRequest(CallToolRequest::new(params.clone()));
            let options = PeerRequestOptions::no_options().with_meta(context.meta.clone());
            let mut handle = match backend
                .peer
                .send_cancellable_request(request, options)
                .await
            {
                Ok(handle) => handle,
                Err(ServiceError::TransportClosed) => {
                    self.retire(&session_id, &lease.session, &backend, "exited");
                    continue;
                }
                Err(error) => {
                    return Err(ErrorData::internal_error(
                        format!("forwarding tools/call failed: {error}"),
                        None,
                    ));
                }
            };
            let _route = context.meta.get_progress_token().map(|upstream| {
                ProgressRoute::register(
                    &backend.progress,
                    handle.progress_token.clone(),
                    (context.peer.clone(), upstream),
                )
            });
            let outcome = tokio::select! {
                response = &mut handle.rx => Some(response),
                () = context.ct.cancelled() => None,
            };
            return match outcome {
                None => {
                    let _ = handle
                        .cancel(Some("cancelled by the client".to_owned()))
                        .await;
                    Err(ErrorData::internal_error(
                        "tools/call was cancelled by the client",
                        None,
                    ))
                }
                Some(Ok(Ok(ServerResult::CallToolResult(result)))) => {
                    Ok(CallToolResponse::Complete(result))
                }
                Some(Ok(Ok(ServerResult::InputRequiredResult(result)))) => {
                    Ok(CallToolResponse::InputRequired(result))
                }
                Some(Ok(Ok(ServerResult::CreateTaskResult(result)))) => {
                    Ok(CallToolResponse::Task(result))
                }
                Some(Ok(Ok(_))) => Err(ErrorData::internal_error(
                    "server returned an unexpected result type for tools/call",
                    None,
                )),
                Some(Ok(Err(ServiceError::McpError(error)))) => Err(error),
                Some(Ok(Err(ServiceError::TransportClosed))) | Some(Err(_)) => {
                    self.retire(
                        &session_id,
                        &lease.session,
                        &backend,
                        "exited during a call",
                    );
                    Ok(tool_error(format!(
                        "The server process for session {session_id} exited during this call. Call the tool again to start a fresh one."
                    )))
                }
                Some(Ok(Err(error))) => Err(ErrorData::internal_error(
                    format!("forwarding tools/call failed: {error}"),
                    None,
                )),
            };
        }
        Ok(tool_error(format!(
            "The server process for session {session_id} keeps exiting right after starting. Check the stderr log of mcp-session-mux."
        )))
    }
}

async fn reap_idle(mux: Mux, idle_timeout: Duration) {
    let mut ticker =
        tokio::time::interval(idle_timeout.clamp(Duration::from_secs(1), Duration::from_secs(10)));
    loop {
        ticker.tick().await;
        let idle: Vec<(String, Arc<Session>)> = lock(&mux.0.sessions)
            .extract_if(|_, entry| entry.in_flight == 0 && entry.last_used.elapsed() > idle_timeout)
            .map(|(id, entry)| (id, entry.session))
            .collect();
        for (id, session) in idle {
            if let Some(backend) = session.backend.get().cloned() {
                eprintln!(
                    "mcp-session-mux: session {id}: idle for {}s, stopping server (pid {})",
                    idle_timeout.as_secs(),
                    backend.pid
                );
                tokio::spawn(async move { backend.stop().await });
            }
        }
    }
}

async fn shutdown_signal() {
    async fn wait(kind: SignalKind) {
        let Ok(mut signal) = signal(kind) else {
            return std::future::pending().await;
        };
        if signal.recv().await.is_none() {
            std::future::pending::<()>().await;
        }
    }
    tokio::select! {
        () = wait(SignalKind::terminate()) => {}
        () = wait(SignalKind::interrupt()) => {}
        () = wait(SignalKind::hangup()) => {}
    }
}

async fn run(args: Args) -> Result<()> {
    let startup_timeout = Duration::from_secs(args.startup_timeout);
    let manifest = match &args.manifest {
        Some(path) => Manifest::load(path)?,
        None => Manifest::probe(&args.command, startup_timeout).await?,
    };
    if args.probe {
        return manifest.print();
    }
    let mux = Mux::new(args, manifest);
    let ct = CancellationToken::new();
    let running = mux
        .clone()
        .serve_with_ct(rmcp::transport::stdio(), ct.clone())
        .await
        .context("MCP handshake with the client failed")?;
    let reaper = mux
        .0
        .idle_timeout
        .map(|idle_timeout| tokio::spawn(reap_idle(mux.clone(), idle_timeout)));
    let serving = running.waiting();
    tokio::pin!(serving);
    tokio::select! {
        result = &mut serving => {
            if let Err(error) = result {
                eprintln!("mcp-session-mux: serving the client failed: {error}");
            }
        }
        () = shutdown_signal() => {
            ct.cancel();
            let _ = timeout(TERM_GRACE, &mut serving).await;
        }
    }
    if let Some(reaper) = reaper {
        reaper.abort();
    }
    mux.shutdown().await;
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    let runtime = tokio::runtime::Runtime::new().context("failed to start the async runtime")?;
    let result = runtime.block_on(run(args));
    // The stdin reader is a blocking-pool thread stuck in read(2) for as long
    // as the client keeps the pipe open; a regular runtime drop would wait for
    // it forever after SIGTERM.
    runtime.shutdown_background();
    result
}
