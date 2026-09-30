use std::{
    collections::{BTreeSet, HashMap},
    fs,
    io::{BufRead, BufReader, Write},
    panic::{AssertUnwindSafe, catch_unwind},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        ListToolsResult, PaginatedRequestParams, ProgressNotificationParam, ServerCapabilities,
        ServerConfig, Tool,
    },
    service::RequestContext,
};
use serde_json::{Map, Value, json};

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(20);
const SESSION_KEY: &str = "ai.opencode/sessionID";

struct FakeBackend {
    calls: AtomicU64,
    cancellations: AtomicU64,
}

fn schema(properties: Value, required: &[&str]) -> Map<String, Value> {
    let mut schema = json!({"type": "object", "properties": properties});
    if !required.is_empty() {
        schema["required"] = json!(required);
    }
    schema.as_object().unwrap().clone()
}

fn argument(params: &CallToolRequestParams, name: &str) -> Result<u64, ErrorData> {
    params
        .arguments
        .as_ref()
        .and_then(|arguments| arguments.get(name))
        .and_then(Value::as_u64)
        .ok_or_else(|| ErrorData::invalid_params(format!("missing `{name}`"), None))
}

fn text(value: Value) -> CallToolResponse {
    CallToolResponse::Complete(CallToolResult::success(vec![ContentBlock::text(
        value.to_string(),
    )]))
}

impl ServerHandler for FakeBackend {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("fake-backend", "9.9"))
            .with_instructions("fake backend instructions")
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(vec![
            Tool::new("whoami", "Report pid and counters", schema(json!({}), &[])),
            Tool::new(
                "sleep",
                "Sleep for ms",
                schema(json!({"ms": {"type": "integer"}}), &["ms"]),
            ),
            Tool::new("echo_meta", "Echo request _meta", schema(json!({}), &[])),
            Tool::new(
                "progress",
                "Send progress notifications",
                schema(json!({"steps": {"type": "integer"}}), &["steps"]),
            ),
            Tool::new("crash", "Exit without replying", schema(json!({}), &[])),
            Tool::new("fail", "Return a JSON-RPC error", schema(json!({}), &[])),
        ]))
    }

    async fn call_tool(
        &self,
        params: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let calls = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        match params.name.as_ref() {
            "whoami" => Ok(text(json!({
                "pid": std::process::id(),
                "calls": calls,
                "cancellations": self.cancellations.load(Ordering::SeqCst),
            }))),
            "sleep" => {
                let ms = argument(&params, "ms")?;
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_millis(ms)) => Ok(text(json!({"slept": ms}))),
                    () = context.ct.cancelled() => {
                        self.cancellations.fetch_add(1, Ordering::SeqCst);
                        Err(ErrorData::internal_error("cancelled", None))
                    }
                }
            }
            "echo_meta" => Ok(text(Value::Object(context.meta.0.0.clone()))),
            "progress" => {
                let steps = argument(&params, "steps")?;
                let token = context
                    .meta
                    .get_progress_token()
                    .ok_or_else(|| ErrorData::invalid_params("no progress token", None))?;
                for step in 1..=steps {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    context
                        .peer
                        .notify_progress(
                            ProgressNotificationParam::new(token.clone(), step as f64)
                                .with_total(steps as f64),
                        )
                        .await
                        .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
                Ok(text(json!({"steps": steps})))
            }
            "crash" => std::process::exit(3),
            "fail" => Err(ErrorData::invalid_params("bad params", None)),
            other => Err(ErrorData::invalid_params(
                format!("unknown tool `{other}`"),
                None,
            )),
        }
    }
}

fn run_fake_backend(mode: &str) -> ! {
    if mode == "fail" {
        eprintln!("fake backend refusing to start");
        std::process::exit(7);
    }
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let backend = FakeBackend {
            calls: AtomicU64::new(0),
            cancellations: AtomicU64::new(0),
        };
        backend
            .serve(rmcp::transport::stdio())
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap();
    });
    std::process::exit(0)
}

fn alive(pid: u64) -> bool {
    fs::read_to_string(format!("/proc/{pid}/status")).is_ok_and(|status| {
        !status
            .lines()
            .any(|line| line.starts_with("State:") && line.contains('Z'))
    })
}

fn children_of(pid: u32) -> BTreeSet<u32> {
    let mut children = BTreeSet::new();
    for entry in fs::read_dir("/proc").unwrap().flatten() {
        let Ok(child) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(format!("/proc/{child}/stat")) else {
            continue;
        };
        let Some((_, rest)) = stat.rsplit_once(") ") else {
            continue;
        };
        let ppid = rest
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u32>().ok());
        if ppid == Some(pid) {
            children.insert(child);
        }
    }
    children
}

fn wait_until(what: &str, limit: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

struct Mux {
    child: Child,
    stdin: Option<ChildStdin>,
    output: mpsc::Receiver<String>,
    reader: Option<JoinHandle<()>>,
    next_id: u64,
    responses: HashMap<u64, Value>,
    notifications: Vec<Value>,
}

impl Mux {
    fn spawn(extra_args: &[&str], backend_mode: &str, capture_stderr: bool) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mcp-session-mux"));
        command.args(extra_args);
        if !extra_args.contains(&"--") {
            command.arg("--");
            command.arg(std::env::current_exe().unwrap());
        }
        let mut child = command
            .env("FAKE_BACKEND", backend_mode)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if capture_stderr {
                Stdio::piped()
            } else {
                Stdio::inherit()
            })
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, output) = mpsc::channel();
        let reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            stdin: child.stdin.take(),
            child,
            output,
            reader: Some(reader),
            next_id: 1,
            responses: HashMap::new(),
            notifications: Vec::new(),
        }
    }

    fn start(extra_args: &[&str]) -> Self {
        let mut mux = Self::spawn(extra_args, "server", false);
        let response = mux.rpc(
            "initialize",
            json!({
                "protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": {"name": "mux-test", "version": "1"},
            }),
        );
        assert_eq!(response["result"]["protocolVersion"], "2025-11-25");
        mux.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        mux
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn send(&mut self, message: Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    fn pump(&mut self, limit: Duration) -> bool {
        match self.output.recv_timeout(limit) {
            Ok(line) => {
                let message: Value = serde_json::from_str(&line).unwrap();
                if let Some(id) = message.get("id").and_then(Value::as_u64) {
                    self.responses.insert(id, message);
                } else {
                    self.notifications.push(message);
                }
                true
            }
            Err(_) => false,
        }
    }

    fn recv(&mut self, id: u64) -> Value {
        let deadline = Instant::now() + RESPONSE_TIMEOUT;
        loop {
            if let Some(response) = self.responses.remove(&id) {
                assert_eq!(response["jsonrpc"], "2.0");
                return response;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                self.pump(remaining),
                "response {id} timed out or stdout closed"
            );
        }
    }

    fn drain(&mut self, quiet: Duration) {
        while self.pump(quiet) {}
    }

    fn rpc(&mut self, method: &str, params: Value) -> Value {
        let id = self.request(method, params);
        self.recv(id)
    }

    fn call_with_meta(&mut self, name: &str, arguments: Value, meta: Value) -> Value {
        self.rpc(
            "tools/call",
            json!({"name": name, "arguments": arguments, "_meta": meta}),
        )
    }

    fn call(&mut self, session: &str, name: &str, arguments: Value) -> Value {
        let response = self.call_with_meta(name, arguments, json!({SESSION_KEY: session}));
        assert!(response.get("error").is_none(), "{response}");
        response["result"].clone()
    }

    fn call_json(&mut self, session: &str, name: &str, arguments: Value) -> Value {
        let result = self.call(session, name, arguments);
        assert_ne!(result["isError"], true, "{result}");
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    fn whoami(&mut self, session: &str) -> Value {
        self.call_json(session, "whoami", json!({}))
    }

    fn close_stdin(&mut self) {
        drop(self.stdin.take());
    }

    fn wait_exit(&mut self, limit: Duration) -> std::process::ExitStatus {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "mux did not exit in time");
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Mux {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.child.try_wait().unwrap().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn error_text(result: &Value) -> &str {
    assert_eq!(result["isError"], true, "{result}");
    result["content"][0]["text"].as_str().unwrap()
}

fn passes_through_initialize_and_tool_list() {
    let mut mux = Mux::spawn(&[], "server", false);
    let response = mux.rpc(
        "initialize",
        json!({
            "protocolVersion": "2024-11-05", "capabilities": {},
            "clientInfo": {"name": "mux-test", "version": "1"},
        }),
    );
    let result = &response["result"];
    assert_eq!(result["protocolVersion"], "2024-11-05");
    assert_eq!(result["serverInfo"]["name"], "fake-backend");
    assert_eq!(result["serverInfo"]["version"], "9.9");
    assert_eq!(result["instructions"], "fake backend instructions");
    assert!(result["capabilities"]["tools"].is_object());
    mux.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let response = mux.rpc("tools/list", json!({}));
    let tools = response["result"]["tools"].as_array().unwrap();
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["whoami", "sleep", "echo_meta", "progress", "crash", "fail"])
    );
    let sleep = tools.iter().find(|tool| tool["name"] == "sleep").unwrap();
    assert_eq!(sleep["inputSchema"]["properties"]["ms"]["type"], "integer");
    assert_eq!(sleep["inputSchema"]["required"], json!(["ms"]));
    assert!(children_of(mux.pid()).is_empty());
}

fn rejects_calls_without_session_id() {
    let mut mux = Mux::start(&[]);
    for meta in [json!({}), json!({SESSION_KEY: 42}), json!({"other": "x"})] {
        let response = mux.call_with_meta("whoami", json!({}), meta);
        let text = error_text(&response["result"]);
        assert!(text.contains(SESSION_KEY), "{text}");
        assert!(text.contains("whoami"), "{text}");
    }
    let response = mux.rpc("tools/call", json!({"name": "whoami", "arguments": {}}));
    error_text(&response["result"]);
    assert!(children_of(mux.pid()).is_empty());
}

fn isolates_sessions_and_reuses_servers() {
    let mut mux = Mux::start(&[]);
    let a1 = mux.whoami("session-a");
    let b1 = mux.whoami("session-b");
    let a2 = mux.whoami("session-a");
    assert_ne!(a1["pid"], b1["pid"]);
    assert_eq!(a1["pid"], a2["pid"]);
    assert_eq!(a1["calls"], 1);
    assert_eq!(a2["calls"], 2);
    assert_eq!(b1["calls"], 1);
    let children = children_of(mux.pid());
    assert_eq!(children.len(), 2, "{children:?}");
    assert!(children.contains(&(a1["pid"].as_u64().unwrap() as u32)));
    assert!(children.contains(&(b1["pid"].as_u64().unwrap() as u32)));
}

fn honours_custom_meta_key() {
    let mut mux = Mux::start(&["--meta-key", "x-session"]);
    let response = mux.call_with_meta("whoami", json!({}), json!({SESSION_KEY: "s"}));
    let text = error_text(&response["result"]);
    assert!(text.contains("x-session"), "{text}");
    let response = mux.call_with_meta("whoami", json!({}), json!({"x-session": "s"}));
    assert_ne!(response["result"]["isError"], true, "{response}");
}

fn forwards_meta_and_replaces_progress_token() {
    let mut mux = Mux::start(&[]);
    let response = mux.call_with_meta(
        "echo_meta",
        json!({}),
        json!({SESSION_KEY: "s", "progressToken": "upstream-1", "extra": {"k": [1, 2]}}),
    );
    let result = &response["result"];
    assert_ne!(result["isError"], true, "{result}");
    let meta: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(meta[SESSION_KEY], "s");
    assert_eq!(meta["extra"], json!({"k": [1, 2]}));
    assert!(meta["progressToken"].is_number(), "{meta}");
}

fn forwards_progress_notifications() {
    let mut mux = Mux::start(&[]);
    let response = mux.call_with_meta(
        "progress",
        json!({"steps": 3}),
        json!({SESSION_KEY: "s", "progressToken": "tok-42"}),
    );
    assert_ne!(response["result"]["isError"], true, "{response}");
    mux.drain(Duration::from_millis(300));
    let mut progress: Vec<f64> = mux
        .notifications
        .iter()
        .filter(|n| n["method"] == "notifications/progress")
        .map(|n| {
            assert_eq!(n["params"]["progressToken"], "tok-42", "{n}");
            assert_eq!(n["params"]["total"].as_f64(), Some(3.0));
            n["params"]["progress"].as_f64().unwrap()
        })
        .collect();
    progress.sort_by(f64::total_cmp);
    assert_eq!(progress, vec![1.0, 2.0, 3.0]);
    mux.notifications.clear();
    let response = mux.call_with_meta("progress", json!({"steps": 2}), json!({SESSION_KEY: "s"}));
    assert_ne!(response["result"]["isError"], true, "{response}");
    mux.drain(Duration::from_millis(500));
    assert!(
        mux.notifications
            .iter()
            .all(|n| n["method"] != "notifications/progress"),
        "{:?}",
        mux.notifications
    );
}

fn passes_through_json_rpc_errors() {
    let mut mux = Mux::start(&[]);
    let response = mux.call_with_meta("fail", json!({}), json!({SESSION_KEY: "s"}));
    assert_eq!(response["error"]["code"], -32602, "{response}");
    assert_eq!(response["error"]["message"], "bad params");
    let response = mux.call_with_meta("unknown", json!({}), json!({SESSION_KEY: "s"}));
    assert_eq!(response["error"]["code"], -32602, "{response}");
    assert_eq!(mux.whoami("s")["calls"], 3);
}

fn forwards_cancellation() {
    let mut mux = Mux::start(&[]);
    mux.whoami("s");
    let id = mux.request(
        "tools/call",
        json!({"name": "sleep", "arguments": {"ms": 5000}, "_meta": {SESSION_KEY: "s"}}),
    );
    thread::sleep(Duration::from_millis(200));
    mux.send(json!({
        "jsonrpc": "2.0", "method": "notifications/cancelled",
        "params": {"requestId": id, "reason": "test"},
    }));
    wait_until(
        "cancellation to reach the backend",
        Duration::from_secs(5),
        || mux.whoami("s")["cancellations"] == 1,
    );
    mux.drain(Duration::from_millis(300));
    assert!(!mux.responses.contains_key(&id), "{:?}", mux.responses);
}

fn runs_sessions_concurrently() {
    let mut mux = Mux::start(&[]);
    mux.whoami("a");
    mux.whoami("b");
    let started = Instant::now();
    let first = mux.request(
        "tools/call",
        json!({"name": "sleep", "arguments": {"ms": 1500}, "_meta": {SESSION_KEY: "a"}}),
    );
    let second = mux.request(
        "tools/call",
        json!({"name": "sleep", "arguments": {"ms": 1500}, "_meta": {SESSION_KEY: "b"}}),
    );
    let first = mux.recv(first);
    let second = mux.recv(second);
    let elapsed = started.elapsed();
    assert_ne!(first["result"]["isError"], true, "{first}");
    assert_ne!(second["result"]["isError"], true, "{second}");
    assert!(elapsed < Duration::from_millis(2900), "took {elapsed:?}");
}

fn reaps_idle_sessions_but_not_busy_ones() {
    let mut mux = Mux::start(&["--idle-timeout", "1"]);
    let old = mux.whoami("s");
    let old_pid = old["pid"].as_u64().unwrap();
    wait_until("idle server to be reaped", Duration::from_secs(10), || {
        !alive(old_pid)
    });
    let fresh = mux.whoami("s");
    assert_ne!(fresh["pid"], old["pid"]);
    assert_eq!(fresh["calls"], 1);
    let busy_pid = fresh["pid"].as_u64().unwrap();
    let response = mux.call_with_meta("sleep", json!({"ms": 2500}), json!({SESSION_KEY: "s"}));
    assert_ne!(response["result"]["isError"], true, "{response}");
    assert!(alive(busy_pid));
    assert_eq!(mux.whoami("s")["pid"], busy_pid);
}

fn recovers_from_a_crashed_server() {
    let mut mux = Mux::start(&[]);
    let a = mux.whoami("a");
    let b = mux.whoami("b");
    let a_pid = a["pid"].as_u64().unwrap();
    let response = mux.call_with_meta("crash", json!({}), json!({SESSION_KEY: "a"}));
    let text = error_text(&response["result"]);
    assert!(text.contains("exited"), "{text}");
    wait_until(
        "crashed server to be reaped",
        Duration::from_secs(5),
        || !alive(a_pid),
    );
    let fresh = mux.whoami("a");
    assert_ne!(fresh["pid"], a["pid"]);
    assert_eq!(fresh["calls"], 1);
    assert_eq!(mux.whoami("b")["pid"], b["pid"]);
}

fn shuts_down_on_stdin_eof() {
    let mut mux = Mux::start(&[]);
    let pids: Vec<u64> = ["a", "b"]
        .iter()
        .map(|session| mux.whoami(session)["pid"].as_u64().unwrap())
        .collect();
    mux.close_stdin();
    let status = mux.wait_exit(Duration::from_secs(10));
    assert!(status.success(), "{status}");
    for pid in pids {
        wait_until("server to exit", Duration::from_secs(5), || !alive(pid));
    }
}

fn shuts_down_on_sigterm() {
    let mut mux = Mux::start(&[]);
    let pid = mux.whoami("a")["pid"].as_u64().unwrap();
    unsafe { libc::kill(mux.pid() as libc::pid_t, libc::SIGTERM) };
    let status = mux.wait_exit(Duration::from_secs(10));
    assert!(status.success(), "{status}");
    wait_until("server to exit", Duration::from_secs(5), || !alive(pid));
}

fn prints_a_manifest_with_probe() -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_mcp-session-mux"))
        .args(["--probe", "--"])
        .arg(std::env::current_exe().unwrap())
        .env("FAKE_BACKEND", "server")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let manifest: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(manifest["serverInfo"]["name"], "fake-backend");
    assert_eq!(manifest["serverInfo"]["version"], "9.9");
    assert_eq!(manifest["instructions"], "fake backend instructions");
    let tools = manifest["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 6);
    let sleep = tools.iter().find(|tool| tool["name"] == "sleep").unwrap();
    assert_eq!(sleep["description"], "Sleep for ms");
    assert_eq!(sleep["inputSchema"]["required"], json!(["ms"]));
    manifest
}

fn serves_a_manifest_without_probing() {
    let manifest = prints_a_manifest_with_probe();
    let dir = std::env::temp_dir().join(format!("mcp-session-mux-test-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("manifest.json");
    fs::write(&path, manifest.to_string()).unwrap();
    let path = path.to_str().unwrap();

    let mut mux = Mux::spawn(
        &["--manifest", path, "--", "/nonexistent/binary"],
        "server",
        false,
    );
    let response = mux.rpc(
        "initialize",
        json!({
            "protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": {"name": "mux-test", "version": "1"},
        }),
    );
    let result = &response["result"];
    assert_eq!(result["serverInfo"]["name"], "fake-backend");
    assert_eq!(result["instructions"], "fake backend instructions");
    mux.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let response = mux.rpc("tools/list", json!({}));
    assert_eq!(response["result"]["tools"], manifest["tools"]);
    assert!(children_of(mux.pid()).is_empty());
    let response = mux.call_with_meta("whoami", json!({}), json!({SESSION_KEY: "s"}));
    assert!(
        error_text(&response["result"]).contains("/nonexistent/binary"),
        "{response}"
    );
    drop(mux);

    let mut mux = Mux::spawn(&["--manifest", path], "server", false);
    let response = mux.rpc(
        "initialize",
        json!({
            "protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": {"name": "mux-test", "version": "1"},
        }),
    );
    assert_eq!(response["result"]["serverInfo"]["name"], "fake-backend");
    mux.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    assert_eq!(mux.whoami("s")["calls"], 1);
    drop(mux);

    fs::write(path, "{ not json").unwrap();
    let mut mux = Mux::spawn(&["--manifest", path], "server", true);
    let status = mux.wait_exit(Duration::from_secs(10));
    assert!(!status.success(), "{status}");
    let mut stderr = String::new();
    std::io::Read::read_to_string(mux.child.stderr.as_mut().unwrap(), &mut stderr).unwrap();
    assert!(stderr.contains("parsing manifest"), "{stderr}");
    fs::remove_dir_all(&dir).unwrap();
}

fn fails_fast_when_the_probe_fails() {
    let mut mux = Mux::spawn(&[], "fail", true);
    let status = mux.wait_exit(Duration::from_secs(10));
    assert!(!status.success(), "{status}");
    let mut stderr = String::new();
    std::io::Read::read_to_string(mux.child.stderr.as_mut().unwrap(), &mut stderr).unwrap();
    assert!(stderr.contains("refusing"), "{stderr}");
    assert!(stderr.contains("handshake"), "{stderr}");
    let mut mux = Mux::spawn(&["--", "/nonexistent/binary"], "server", true);
    let status = mux.wait_exit(Duration::from_secs(10));
    assert!(!status.success(), "{status}");
}

fn main() {
    if let Ok(mode) = std::env::var("FAKE_BACKEND") {
        run_fake_backend(&mode);
    }
    let tests: [(&str, fn()); 15] = [
        (
            "passes_through_initialize_and_tool_list",
            passes_through_initialize_and_tool_list,
        ),
        (
            "rejects_calls_without_session_id",
            rejects_calls_without_session_id,
        ),
        (
            "isolates_sessions_and_reuses_servers",
            isolates_sessions_and_reuses_servers,
        ),
        ("honours_custom_meta_key", honours_custom_meta_key),
        (
            "forwards_meta_and_replaces_progress_token",
            forwards_meta_and_replaces_progress_token,
        ),
        (
            "forwards_progress_notifications",
            forwards_progress_notifications,
        ),
        (
            "passes_through_json_rpc_errors",
            passes_through_json_rpc_errors,
        ),
        ("forwards_cancellation", forwards_cancellation),
        ("runs_sessions_concurrently", runs_sessions_concurrently),
        (
            "reaps_idle_sessions_but_not_busy_ones",
            reaps_idle_sessions_but_not_busy_ones,
        ),
        (
            "recovers_from_a_crashed_server",
            recovers_from_a_crashed_server,
        ),
        ("shuts_down_on_stdin_eof", shuts_down_on_stdin_eof),
        ("shuts_down_on_sigterm", shuts_down_on_sigterm),
        (
            "serves_a_manifest_without_probing",
            serves_a_manifest_without_probing,
        ),
        (
            "fails_fast_when_the_probe_fails",
            fails_fast_when_the_probe_fails,
        ),
    ];
    let mut failed = 0;
    for (name, test) in tests {
        let started = Instant::now();
        match catch_unwind(AssertUnwindSafe(test)) {
            Ok(()) => println!("PASS {name} ({:.2?})", started.elapsed()),
            Err(_) => {
                failed += 1;
                println!("FAIL {name} ({:.2?})", started.elapsed());
            }
        }
    }
    println!("{} passed, {failed} failed", tests.len() - failed);
    if failed > 0 {
        std::process::exit(101);
    }
}
