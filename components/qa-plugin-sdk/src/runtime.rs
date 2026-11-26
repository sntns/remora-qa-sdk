use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Write;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::mpsc::Receiver;
use std::sync::Arc;

use serde::de::DeserializeOwned;

use crate::ctx::Output;
use crate::protocol::AgentMessage;
use crate::protocol::PluginKind;
use crate::protocol::PluginMessage;
use crate::protocol::PluginSpec;
use crate::protocol::PROTOCOL_VERSION;
use crate::Ctx;
use crate::Disruptor;
use crate::Error;
use crate::Probe;
use crate::Result;
use crate::StateStore;

/// The process ended as asked: described, stopped, reverted.
pub const EXIT_OK: i32 = 0;
/// The instance failed (`failed` written) or a cold revert failed.
pub const EXIT_FAILED: i32 = 1;
/// A bad command line.
pub const EXIT_USAGE: i32 = 2;

/// What the main loop waits on: the agent's messages and the workers.
pub(crate) enum Control {
    Agent(AgentMessage),
    /// A line that is not an [`AgentMessage`].
    BadLine(String),
    /// The end of stdin: the agent is gone.
    Eof,
    WorkerFailed(Error),
}

/// Where a run reads and writes, for [`run_probe_with`] and
/// [`run_disruptor_with`]: the process's own in [`Io::process`], in-memory
/// buffers in tests.
pub struct Io {
    /// The command line, without the program name (`["run"]`).
    pub args: Vec<String>,
    /// The state directory ([`ENV_STATE_DIR`](crate::protocol::ENV_STATE_DIR)).
    pub state_dir: Option<PathBuf>,
    pub stdin: Box<dyn Read + Send>,
    pub stdout: Box<dyn Write + Send>,
}

impl Io {
    /// The process's command line, environment, stdin and stdout.
    pub fn process() -> Self {
        Self {
            args: std::env::args().skip(1).collect(),
            state_dir: crate::state::state_dir_from_env(),
            stdin: Box::new(std::io::stdin()),
            stdout: Box::new(std::io::stdout()),
        }
    }
}

/// Runs a probe as the process: parses the command line, speaks the
/// protocol on stdin/stdout, then exits with the right code.
pub fn run_probe<P: Probe>(probe: P) -> ! {
    let code = run_probe_with(probe, Io::process());
    std::process::exit(code)
}

/// Runs a disruptor as the process: parses the command line, speaks the
/// protocol on stdin/stdout, then exits with the right code.
pub fn run_disruptor<D: Disruptor>(disruptor: D) -> ! {
    let code = run_disruptor_with(disruptor, Io::process());
    std::process::exit(code)
}

/// [`run_probe`] on the given [`Io`]; returns the exit code.
pub fn run_probe_with<P: Probe>(probe: P, io: Io) -> i32 {
    let mut plugin = ProbeAdapter(probe);
    run(&mut plugin, io)
}

/// [`run_disruptor`] on the given [`Io`]; returns the exit code.
pub fn run_disruptor_with<D: Disruptor>(disruptor: D, io: Io) -> i32 {
    let mut plugin = DisruptorAdapter(disruptor);
    run(&mut plugin, io)
}

/// What the loop needs of a plugin, whatever its kind.
trait Plugin {
    fn kind(&self) -> PluginKind;
    fn spec(&self) -> PluginSpec;
    /// Starts or applies, from the first message's params.
    fn begin(&mut self, ctx: &Ctx, params: serde_json::Value) -> Result;
    /// Stops or reverts.
    fn end(&mut self, ctx: &Ctx) -> Result;
    /// Whether a failed `begin` must be ended too (a disruptor's partial
    /// change).
    fn end_after_failed_begin(&self) -> bool;
    fn revert_cold(&mut self, state: &StateStore, instance: &str) -> Result<bool>;
}

struct ProbeAdapter<P>(P);

impl<P: Probe> Plugin for ProbeAdapter<P> {
    fn kind(&self) -> PluginKind {
        PluginKind::Probe
    }
    fn spec(&self) -> PluginSpec {
        self.0.spec()
    }
    fn begin(&mut self, ctx: &Ctx, params: serde_json::Value) -> Result {
        let params = parse_params::<P::Params>(params)?;
        self.0.start(ctx, params)
    }
    fn end(&mut self, ctx: &Ctx) -> Result {
        self.0.stop(ctx)
    }
    fn end_after_failed_begin(&self) -> bool {
        false
    }
    fn revert_cold(&mut self, _: &StateStore, _: &str) -> Result<bool> {
        // A probe changes nothing: there is never anything to revert.
        Ok(false)
    }
}

struct DisruptorAdapter<D>(D);

impl<D: Disruptor> Plugin for DisruptorAdapter<D> {
    fn kind(&self) -> PluginKind {
        PluginKind::Disruptor
    }
    fn spec(&self) -> PluginSpec {
        self.0.spec()
    }
    fn begin(&mut self, ctx: &Ctx, params: serde_json::Value) -> Result {
        let params = parse_params::<D::Params>(params)?;
        self.0.apply(ctx, params)
    }
    fn end(&mut self, ctx: &Ctx) -> Result {
        self.0.revert(ctx)?;
        ctx.clear_state()
    }
    fn end_after_failed_begin(&self) -> bool {
        true
    }
    fn revert_cold(&mut self, store: &StateStore, instance: &str) -> Result<bool> {
        let Some(state) = store.load::<D::State>(instance)? else {
            return Ok(false);
        };
        self.0.revert_cold(state)?;
        store.clear(instance)?;
        Ok(true)
    }
}

/// Parses the params; missing ones (`null`) are read as `{}` too, so a
/// params struct whose fields all have defaults accepts them.
fn parse_params<T: DeserializeOwned>(params: serde_json::Value) -> Result<T> {
    if params.is_null() {
        if let Ok(p) = serde_json::from_value(serde_json::Value::Null) {
            return Ok(p);
        }
        return serde_json::from_value(serde_json::json!({}))
            .map_err(|e| Error::msg(format!("invalid params: {e}")));
    }
    serde_json::from_value(params).map_err(|e| Error::msg(format!("invalid params: {e}")))
}

/// Runs `f`, turning a panic into an error.
pub(crate) fn catch<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    match std::panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(payload) => {
            let what = if let Some(s) = payload.downcast_ref::<&str>() {
                (*s).to_owned()
            } else if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic".to_owned()
            };
            Err(Error::msg(format!("panicked: {what}")))
        }
    }
}

fn spec_of(plugin: &dyn Plugin) -> PluginSpec {
    let mut spec = plugin.spec();
    spec.protocol = PROTOCOL_VERSION;
    spec.kind = plugin.kind();
    if spec.kind == PluginKind::Probe {
        spec.level = None;
        spec.reversible = false;
    }
    spec
}

const USAGE: &str = "usage: <plugin> describe | run | revert <instance>";

fn run(plugin: &mut dyn Plugin, mut io: Io) -> i32 {
    let args: Vec<&str> = io.args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["describe"] => {
            let spec = spec_of(plugin);
            let mut line = serde_json::to_vec(&spec).unwrap_or_default();
            line.push(b'\n');
            match io.stdout.write_all(&line).and_then(|()| io.stdout.flush()) {
                Ok(()) => EXIT_OK,
                Err(e) => {
                    eprintln!("describe: {e}");
                    EXIT_FAILED
                }
            }
        }
        ["run"] => run_instance(plugin, io),
        ["revert", instance] => {
            let spec = spec_of(plugin);
            let store = StateStore::new(io.state_dir.clone(), spec.name.clone());
            match catch(|| plugin.revert_cold(&store, instance)) {
                Ok(true) => {
                    eprintln!("{}: reverted {instance}", spec.name);
                    EXIT_OK
                }
                Ok(false) => {
                    eprintln!("{}: nothing to revert for {instance}", spec.name);
                    EXIT_OK
                }
                Err(e) => {
                    eprintln!("{}: reverting {instance}: {e}", spec.name);
                    EXIT_FAILED
                }
            }
        }
        ["--help"] | ["-h"] | ["help"] => {
            eprintln!("{USAGE}");
            EXIT_OK
        }
        _ => {
            eprintln!("{USAGE}");
            EXIT_USAGE
        }
    }
}

/// Reads the agent's messages on a thread of its own, so the main loop
/// also hears from the workers.
fn spawn_reader(stdin: Box<dyn Read + Send>, tx: mpsc::Sender<Control>) {
    let spawned = std::thread::Builder::new().name("stdin".into()).spawn({
        let tx = tx.clone();
        move || {
            let mut lines = BufReader::new(stdin).lines();
            loop {
                let control = match lines.next() {
                    None | Some(Err(_)) => Control::Eof,
                    Some(Ok(line)) if line.trim().is_empty() => continue,
                    Some(Ok(line)) => match serde_json::from_str::<AgentMessage>(&line) {
                        Ok(msg) => Control::Agent(msg),
                        Err(e) => Control::BadLine(format!("{e}: {line}")),
                    },
                };
                let eof = matches!(control, Control::Eof);
                if tx.send(control).is_err() || eof {
                    return;
                }
            }
        }
    });
    if spawned.is_err() {
        let _ = tx.send(Control::Eof);
    }
}

fn run_instance(plugin: &mut dyn Plugin, io: Io) -> i32 {
    let spec = spec_of(plugin);
    let out = Arc::new(Output::new(io.stdout));
    let (tx, rx) = mpsc::channel();
    spawn_reader(io.stdin, tx.clone());

    let fail = |error: String| {
        out.close(&PluginMessage::Failed { error });
        EXIT_FAILED
    };

    // The first message names the instance.
    let (instance, params) = match rx.recv() {
        Ok(Control::Agent(AgentMessage::Start { instance, params }))
            if spec.kind == PluginKind::Probe =>
        {
            (instance, params)
        }
        Ok(Control::Agent(AgentMessage::Apply { instance, params }))
            if spec.kind == PluginKind::Disruptor =>
        {
            (instance, params)
        }
        // Gone before anything started: nothing to undo.
        Ok(Control::Eof) | Err(_) => return EXIT_OK,
        Ok(Control::BadLine(line)) => return fail(format!("invalid message: {line}")),
        Ok(Control::Agent(msg)) => {
            let expected = match spec.kind {
                PluginKind::Probe => "start",
                PluginKind::Disruptor => "apply",
            };
            return fail(format!("expected `{expected}` first, got {msg:?}"));
        }
        // No worker runs before the instance starts.
        Ok(Control::WorkerFailed(e)) => return fail(e.to_string()),
    };

    let store = StateStore::new(io.state_dir, spec.name.clone());
    let ctx = Ctx::new(spec.name.clone(), instance, out.clone(), tx, store);

    if let Err(e) = catch(|| plugin.begin(&ctx, params)) {
        ctx.stop_workers();
        if plugin.end_after_failed_begin() {
            if let Err(re) = catch(|| plugin.end(&ctx)) {
                return fail(format!("{e}; then reverting: {re}"));
            }
        }
        return fail(e.to_string());
    }
    out.send(&PluginMessage::Ready);

    let failure = wait_end(&ctx, &rx);
    ctx.stop_workers();
    let ended = catch(|| plugin.end(&ctx));
    match (failure, ended) {
        (None, Ok(())) => {
            out.close(&PluginMessage::Done);
            EXIT_OK
        }
        (None, Err(e)) => fail(e.to_string()),
        (Some(f), Ok(())) => fail(f.to_string()),
        (Some(f), Err(e)) => fail(format!("{f}; then ending: {e}")),
    }
}

/// Waits for the end of the instance; returns the failure that ended it,
/// if any.
fn wait_end(ctx: &Ctx, rx: &Receiver<Control>) -> Option<Error> {
    loop {
        match rx.recv() {
            Ok(Control::Agent(AgentMessage::Stop | AgentMessage::Revert)) => return None,
            Ok(Control::Eof) | Err(_) => {
                ctx.warn("stdin closed: the agent is gone, ending the instance");
                return None;
            }
            Ok(Control::WorkerFailed(e)) => return Some(e),
            Ok(Control::Agent(msg)) => ctx.warn(format!("ignored while running: {msg:?}")),
            Ok(Control::BadLine(line)) => ctx.warn(format!("ignored invalid message: {line}")),
        }
    }
}
