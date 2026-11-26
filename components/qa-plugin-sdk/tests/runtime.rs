//! Drives probes and disruptors through in-memory stdin/stdout.

use std::io::Read;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::Duration;
use std::time::Instant;

use remora_qa_plugin_sdk::disruptor_spec;
use remora_qa_plugin_sdk::probe_spec;
use remora_qa_plugin_sdk::protocol::AgentMessage;
use remora_qa_plugin_sdk::protocol::PluginKind;
use remora_qa_plugin_sdk::protocol::PluginMessage;
use remora_qa_plugin_sdk::protocol::PluginSpec;
use remora_qa_plugin_sdk::run_disruptor_with;
use remora_qa_plugin_sdk::run_probe_with;
use remora_qa_plugin_sdk::Ctx;
use remora_qa_plugin_sdk::Disruptor;
use remora_qa_plugin_sdk::Io;
use remora_qa_plugin_sdk::Probe;
use remora_qa_plugin_sdk::Result;
use remora_qa_plugin_sdk::RiskLevel;
use remora_qa_plugin_sdk::StateStore;
use serde::Deserialize;
use serde::Serialize;

// ---- in-memory pipes ----

/// The agent's end of the plugin's stdin: dropping it is the end of stdin.
struct Stdin(mpsc::Sender<Vec<u8>>);

impl Stdin {
    fn send(&self, msg: &AgentMessage) {
        let mut line = serde_json::to_vec(msg).unwrap();
        line.push(b'\n');
        self.0.send(line).unwrap();
    }

    fn raw(&self, line: &str) {
        self.0.send(format!("{line}\n").into_bytes()).unwrap();
    }
}

struct PipeReader {
    rx: mpsc::Receiver<Vec<u8>>,
    pending: Vec<u8>,
}

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pending.is_empty() {
            match self.rx.recv() {
                Ok(bytes) => self.pending = bytes,
                Err(_) => return Ok(0),
            }
        }
        let n = buf.len().min(self.pending.len());
        buf[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
        Ok(n)
    }
}

#[derive(Clone, Default)]
struct Stdout(Arc<Mutex<Vec<u8>>>);

impl Write for Stdout {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Stdout {
    fn messages(&self) -> Vec<PluginMessage> {
        let bytes = self.0.lock().unwrap().clone();
        String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l}")))
            .collect()
    }

    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }

    /// Waits until a message matches.
    fn wait_for(&self, what: &str, f: impl Fn(&PluginMessage) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.messages().iter().any(&f) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("no {what} in {:?}", self.messages());
    }
}

struct Running {
    stdin: Stdin,
    stdout: Stdout,
    thread: JoinHandle<i32>,
}

impl Running {
    fn exit(self) -> (i32, Vec<PluginMessage>) {
        let Running {
            stdin,
            stdout,
            thread,
        } = self;
        drop(stdin);
        (thread.join().unwrap(), stdout.messages())
    }
}

fn io(args: &[&str], state_dir: Option<PathBuf>) -> (Io, Stdin, Stdout) {
    let (tx, rx) = mpsc::channel();
    let stdout = Stdout::default();
    let io = Io {
        args: args.iter().map(|s| s.to_string()).collect(),
        state_dir,
        stdin: Box::new(PipeReader {
            rx,
            pending: Vec::new(),
        }),
        stdout: Box::new(stdout.clone()),
    };
    (io, Stdin(tx), stdout)
}

fn spawn_probe<P: Probe>(probe: P) -> Running {
    let (io, stdin, stdout) = io(&["run"], None);
    let thread = std::thread::spawn(move || run_probe_with(probe, io));
    Running {
        stdin,
        stdout,
        thread,
    }
}

fn spawn_disruptor<D: Disruptor>(d: D, state_dir: PathBuf) -> Running {
    let (io, stdin, stdout) = io(&["run"], Some(state_dir));
    let thread = std::thread::spawn(move || run_disruptor_with(d, io));
    Running {
        stdin,
        stdout,
        thread,
    }
}

fn test_dir(name: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "remora-qa-sdk-it-{}-{}-{name}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn start(params: serde_json::Value) -> AgentMessage {
    AgentMessage::Start {
        instance: "p1".into(),
        params,
    }
}

fn apply(params: serde_json::Value) -> AgentMessage {
    AgentMessage::Apply {
        instance: "d1".into(),
        params,
    }
}

fn failed_error(msgs: &[PluginMessage]) -> String {
    match msgs.last() {
        Some(PluginMessage::Failed { error }) => error.clone(),
        other => panic!("expected failed last, got {other:?} in {msgs:?}"),
    }
}

// ---- a test probe ----

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct TickParams {
    #[serde(default = "default_interval")]
    interval_ms: u64,
    #[serde(default)]
    panic_in_start: bool,
    #[serde(default)]
    fail_worker: bool,
}

fn default_interval() -> u64 {
    5
}

#[derive(Default)]
struct Ticker {
    stopped: Arc<Mutex<bool>>,
}

impl Probe for Ticker {
    type Params = TickParams;

    fn spec(&self) -> PluginSpec {
        PluginSpec {
            needs: vec!["procfs".into()],
            ..probe_spec("ticker", 3, "Ticks")
        }
    }

    fn start(&mut self, ctx: &Ctx, p: TickParams) -> Result {
        if p.panic_in_start {
            panic!("boom in start");
        }
        let interval = Duration::from_millis(p.interval_ms);
        let fail = p.fail_worker;
        ctx.spawn("ticker", move |ctx| {
            let mut n = 0u32;
            while !ctx.wait_stop(interval) {
                n += 1;
                ctx.event("tick", serde_json::json!({ "n": n }));
                if fail && n == 2 {
                    remora_qa_plugin_sdk::bail!("sensor lost");
                }
            }
            Ok(())
        })
    }

    fn stop(&mut self, ctx: &Ctx) -> Result {
        *self.stopped.lock().unwrap() = true;
        Ok(())
    }
}

#[test]
fn describe_prints_the_spec() {
    let (io, _stdin, stdout) = io(&["describe"], None);
    assert_eq!(run_probe_with(Ticker::default(), io), 0);
    let text = stdout.text();
    assert_eq!(text.lines().count(), 1);
    let spec: PluginSpec = serde_json::from_str(&text).unwrap();
    assert_eq!(spec.kind, PluginKind::Probe);
    assert_eq!(spec.name, "ticker");
    assert_eq!(spec.version, 3);
    assert_eq!(spec.needs, ["procfs"]);
}

#[test]
fn bad_command_line_is_a_usage_error() {
    for args in [&[][..], &["nope"], &["revert"], &["run", "x"]] {
        let (io, _stdin, _stdout) = io(args, None);
        assert_eq!(run_probe_with(Ticker::default(), io), 2, "{args:?}");
    }
}

#[test]
fn probe_runs_until_stop() {
    let stopped = Arc::new(Mutex::new(false));
    let run = spawn_probe(Ticker {
        stopped: stopped.clone(),
    });
    run.stdin
        .send(&start(serde_json::json!({ "interval-ms": 2 })));
    run.stdout.wait_for(
        "second tick",
        |m| matches!(m, PluginMessage::Event { kind, event } if kind == "tick" && event["n"] == 2),
    );
    run.stdin.send(&AgentMessage::Stop);
    let (code, msgs) = run.exit();
    assert_eq!(code, 0);
    assert_eq!(msgs[0], PluginMessage::Ready);
    assert_eq!(msgs.last(), Some(&PluginMessage::Done));
    assert!(*stopped.lock().unwrap());
}

#[test]
fn probe_stops_at_end_of_stdin() {
    let run = spawn_probe(Ticker::default());
    run.stdin.send(&start(serde_json::Value::Null));
    run.stdout
        .wait_for("ready", |m| matches!(m, PluginMessage::Ready));
    let (code, msgs) = run.exit();
    assert_eq!(code, 0);
    assert_eq!(msgs.last(), Some(&PluginMessage::Done));
}

#[test]
fn end_of_stdin_before_start_exits_quietly() {
    let run = spawn_probe(Ticker::default());
    let (code, msgs) = run.exit();
    assert_eq!(code, 0);
    assert!(msgs.is_empty(), "{msgs:?}");
}

#[test]
fn invalid_params_fail() {
    let run = spawn_probe(Ticker::default());
    run.stdin
        .send(&start(serde_json::json!({ "interval-ms": "fast" })));
    let (code, msgs) = run.exit();
    assert_eq!(code, 1);
    assert_eq!(msgs.len(), 1, "{msgs:?}");
    let error = failed_error(&msgs);
    assert!(error.starts_with("invalid params: "), "{error}");
    assert!(
        error.contains("interval-ms") || error.contains("invalid type"),
        "{error}"
    );

    let run = spawn_probe(Ticker::default());
    run.stdin.send(&start(serde_json::json!({ "bogus": 1 })));
    let (code, msgs) = run.exit();
    assert_eq!(code, 1);
    assert!(failed_error(&msgs).contains("bogus"));
}

#[test]
fn panic_in_start_fails() {
    let run = spawn_probe(Ticker::default());
    run.stdin
        .send(&start(serde_json::json!({ "panic-in-start": true })));
    let (code, msgs) = run.exit();
    assert_eq!(code, 1);
    assert_eq!(failed_error(&msgs), "panicked: boom in start");
}

#[test]
fn failing_worker_fails_the_instance() {
    let stopped = Arc::new(Mutex::new(false));
    let run = spawn_probe(Ticker {
        stopped: stopped.clone(),
    });
    run.stdin.send(&start(
        serde_json::json!({ "interval-ms": 1, "fail-worker": true }),
    ));
    run.stdout
        .wait_for("failed", |m| matches!(m, PluginMessage::Failed { .. }));
    // The process ends without waiting for the agent.
    let code = run.thread.join().unwrap();
    assert_eq!(code, 1);
    assert_eq!(
        failed_error(&run.stdout.messages()),
        "worker ticker: sensor lost"
    );
    assert!(*stopped.lock().unwrap());
}

#[test]
fn wrong_first_message_fails() {
    let run = spawn_probe(Ticker::default());
    run.stdin.send(&apply(serde_json::Value::Null));
    let (code, msgs) = run.exit();
    assert_eq!(code, 1);
    assert!(failed_error(&msgs).contains("expected `start`"));

    let run = spawn_probe(Ticker::default());
    run.stdin.raw("not json");
    let (code, msgs) = run.exit();
    assert_eq!(code, 1);
    assert!(failed_error(&msgs).starts_with("invalid message"));
}

#[test]
fn invalid_lines_while_running_are_logged() {
    let run = spawn_probe(Ticker::default());
    run.stdin
        .send(&start(serde_json::json!({ "interval-ms": 60000 })));
    run.stdin.raw("{\"type\":\"bogus\"}");
    run.stdout
        .wait_for("warn", |m| matches!(m, PluginMessage::Log { .. }));
    run.stdin.send(&AgentMessage::Stop);
    let (code, msgs) = run.exit();
    assert_eq!(code, 0);
    assert_eq!(msgs.last(), Some(&PluginMessage::Done));
}

// ---- a test disruptor: a marker file ----

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct MarkerParams {
    path: PathBuf,
    #[serde(default)]
    fail_after_create: bool,
    #[serde(default)]
    load_worker: bool,
}

#[derive(Serialize, Deserialize)]
struct MarkerState {
    path: PathBuf,
}

struct Marker;

impl Disruptor for Marker {
    type Params = MarkerParams;
    type State = MarkerState;

    fn spec(&self) -> PluginSpec {
        disruptor_spec("marker", 1, RiskLevel::Soft, "A marker file")
    }

    fn apply(&mut self, ctx: &Ctx, p: MarkerParams) -> Result {
        ctx.save_state(&MarkerState {
            path: p.path.clone(),
        })?;
        std::fs::write(&p.path, "x")?;
        if p.fail_after_create {
            remora_qa_plugin_sdk::bail!("half applied");
        }
        if p.load_worker {
            ctx.spawn("load", |ctx| {
                while !ctx.is_stopping() {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Ok(())
            })?;
        }
        Ok(())
    }

    fn revert_cold(&mut self, state: MarkerState) -> Result {
        match std::fs::remove_file(&state.path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
}

fn state_file(dir: &std::path::Path) -> PathBuf {
    dir.join("marker").join("d1.json")
}

#[test]
fn disruptor_applies_and_reverts() {
    let dir = test_dir("revert");
    let marker = dir.join("marker-file");
    let run = spawn_disruptor(Marker, dir.clone());
    run.stdin.send(&apply(
        serde_json::json!({ "path": marker, "load-worker": true }),
    ));
    run.stdout
        .wait_for("ready", |m| matches!(m, PluginMessage::Ready));
    assert!(marker.exists());
    assert!(state_file(&dir).exists());
    run.stdin.send(&AgentMessage::Revert);
    let (code, msgs) = run.exit();
    assert_eq!(code, 0);
    assert_eq!(msgs, [PluginMessage::Ready, PluginMessage::Done]);
    assert!(!marker.exists());
    assert!(!state_file(&dir).exists());
}

#[test]
fn disruptor_reverts_at_end_of_stdin() {
    let dir = test_dir("eof");
    let marker = dir.join("marker-file");
    let run = spawn_disruptor(Marker, dir.clone());
    run.stdin
        .send(&apply(serde_json::json!({ "path": marker })));
    run.stdout
        .wait_for("ready", |m| matches!(m, PluginMessage::Ready));
    assert!(marker.exists());
    let (code, msgs) = run.exit();
    assert_eq!(code, 0);
    assert_eq!(msgs.last(), Some(&PluginMessage::Done));
    assert!(!marker.exists());
    assert!(!state_file(&dir).exists());
}

#[test]
fn failed_apply_reverts_the_partial_change() {
    let dir = test_dir("partial");
    let marker = dir.join("marker-file");
    let run = spawn_disruptor(Marker, dir.clone());
    run.stdin.send(&apply(
        serde_json::json!({ "path": marker, "fail-after-create": true }),
    ));
    let (code, msgs) = run.exit();
    assert_eq!(code, 1);
    assert_eq!(failed_error(&msgs), "half applied");
    assert!(!marker.exists());
    assert!(!state_file(&dir).exists());
}

#[test]
fn disruptor_without_state_dir_fails_to_apply() {
    let (io, stdin, stdout) = io(&["run"], None);
    let thread = std::thread::spawn(move || run_disruptor_with(Marker, io));
    stdin.send(&apply(serde_json::json!({ "path": "/nonexistent/x" })));
    drop(stdin);
    assert_eq!(thread.join().unwrap(), 1);
    assert!(failed_error(&stdout.messages()).contains("REMORA_QA_STATE_DIR"));
}

#[test]
fn cold_revert_from_saved_state() {
    let dir = test_dir("cold");
    let marker = dir.join("marker-file");
    // What a crashed `run` leaves behind.
    std::fs::write(&marker, "x").unwrap();
    StateStore::new(Some(dir.clone()), "marker")
        .save(
            "d1",
            &MarkerState {
                path: marker.clone(),
            },
        )
        .unwrap();

    let (io_, _stdin, _stdout) = io(&["revert", "d1"], Some(dir.clone()));
    assert_eq!(run_disruptor_with(Marker, io_), 0);
    assert!(!marker.exists());
    assert!(!state_file(&dir).exists());

    // Nothing left to revert: still a success.
    let (io_, _stdin, _stdout) = io(&["revert", "d1"], Some(dir.clone()));
    assert_eq!(run_disruptor_with(Marker, io_), 0);
}

#[test]
fn failed_cold_revert_keeps_the_state() {
    let dir = test_dir("cold-fail");
    // A directory cannot be removed with remove_file.
    let marker = dir.join("a-directory");
    std::fs::create_dir(&marker).unwrap();
    StateStore::new(Some(dir.clone()), "marker")
        .save(
            "d1",
            &MarkerState {
                path: marker.clone(),
            },
        )
        .unwrap();
    let (io_, _stdin, _stdout) = io(&["revert", "d1"], Some(dir.clone()));
    assert_eq!(run_disruptor_with(Marker, io_), 1);
    assert!(state_file(&dir).exists());
}
