use std::io::Write;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::Duration;
use std::time::Instant;

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::protocol::LogLevel;
use crate::protocol::PluginMessage;
use crate::runtime::Control;
use crate::Result;
use crate::StateStore;

/// The handle a plugin instance gets: emit events and logs, know when to
/// stop, run worker threads, keep its state.
///
/// It is cheap to clone and `Send + Sync`: give a clone to every thread
/// that emits. Once the instance is over (`done` or `failed` written),
/// whatever is still emitted is dropped.
#[derive(Clone)]
pub struct Ctx {
    shared: Arc<Shared>,
}

struct Shared {
    plugin: String,
    instance: String,
    out: Arc<Output>,
    stop: Mutex<bool>,
    stop_cv: Condvar,
    workers: Mutex<Vec<JoinHandle<()>>>,
    control: Sender<Control>,
    state: StateStore,
}

/// The plugin's stdout, shared by every thread: one message per line,
/// never interleaved.
pub(crate) struct Output {
    writer: Mutex<Option<Box<dyn Write + Send>>>,
}

impl Output {
    pub(crate) fn new(writer: Box<dyn Write + Send>) -> Self {
        Self {
            writer: Mutex::new(Some(writer)),
        }
    }

    pub(crate) fn send(&self, msg: &PluginMessage) {
        let mut guard = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let Some(w) = guard.as_mut() else {
            return;
        };
        // A message always serializes; a write error means the agent is
        // gone, which the end of stdin reports too.
        let Ok(mut line) = serde_json::to_vec(msg) else {
            return;
        };
        line.push(b'\n');
        let _ = w.write_all(&line).and_then(|()| w.flush());
    }

    /// Writes the last message; later ones are dropped.
    pub(crate) fn close(&self, last: &PluginMessage) {
        self.send(last);
        let mut guard = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        *guard = None;
    }
}

impl Ctx {
    pub(crate) fn new(
        plugin: String,
        instance: String,
        out: Arc<Output>,
        control: Sender<Control>,
        state: StateStore,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                plugin,
                instance,
                out,
                stop: Mutex::new(false),
                stop_cv: Condvar::new(),
                workers: Mutex::new(Vec::new()),
                control,
                state,
            }),
        }
    }

    /// The plugin's name, from its spec.
    pub fn plugin(&self) -> &str {
        &self.shared.plugin
    }

    /// This instance's name, given by the agent.
    pub fn instance(&self) -> &str {
        &self.shared.instance
    }

    /// Emits one event of the plugin's own `kind`. The agent dates and
    /// numbers it.
    pub fn event(&self, kind: &str, event: impl Serialize) {
        match serde_json::to_value(event) {
            Ok(event) => self.shared.out.send(&PluginMessage::Event {
                kind: kind.to_owned(),
                event,
            }),
            Err(e) => self.error(format!("event {kind:?} does not serialize: {e}")),
        }
    }

    /// A line for the agent's log.
    pub fn log(&self, level: LogLevel, message: impl Into<String>) {
        self.shared.out.send(&PluginMessage::Log {
            level,
            message: message.into(),
        });
    }

    pub fn debug(&self, message: impl Into<String>) {
        self.log(LogLevel::Debug, message);
    }

    pub fn info(&self, message: impl Into<String>) {
        self.log(LogLevel::Info, message);
    }

    pub fn warn(&self, message: impl Into<String>) {
        self.log(LogLevel::Warn, message);
    }

    pub fn error(&self, message: impl Into<String>) {
        self.log(LogLevel::Error, message);
    }

    /// Whether the instance is asked to end (`stop`, `revert`, the end of
    /// stdin, or a worker that failed).
    pub fn is_stopping(&self) -> bool {
        *self.shared.stop.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Waits up to `timeout` for the instance to be asked to end; returns
    /// whether it is. The loop of a periodic worker:
    ///
    /// ```no_run
    /// # fn f(ctx: remora_qa_plugin_sdk::Ctx) {
    /// # let interval = std::time::Duration::from_secs(1);
    /// while !ctx.wait_stop(interval) {
    ///     ctx.event("tick", ());
    /// }
    /// # }
    /// ```
    pub fn wait_stop(&self, timeout: Duration) -> bool {
        let deadline = Instant::now().checked_add(timeout);
        let mut stop = self.shared.stop.lock().unwrap_or_else(|e| e.into_inner());
        while !*stop {
            let left = match deadline {
                Some(d) => d.saturating_duration_since(Instant::now()),
                None => Duration::from_secs(3600),
            };
            if left.is_zero() {
                break;
            }
            stop = self
                .shared
                .stop_cv
                .wait_timeout(stop, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        *stop
    }

    /// Runs `work` on a thread of its own, with a clone of this context.
    ///
    /// The runtime asks the workers to end and joins them before it calls
    /// `stop`/`revert` and writes `done`: a worker polls
    /// [`is_stopping`](Self::is_stopping) or waits with
    /// [`wait_stop`](Self::wait_stop), and returns. A worker returning an
    /// error (or panicking) while the instance runs ends the instance with
    /// `failed`.
    pub fn spawn<F>(&self, name: &str, work: F) -> Result
    where
        F: FnOnce(Ctx) -> Result + Send + 'static,
    {
        let ctx = self.clone();
        let label = name.to_owned();
        let handle = std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                let result = crate::runtime::catch(|| work(ctx.clone()));
                if let Err(e) = result {
                    let e = e.context(format!("worker {label}"));
                    if ctx.is_stopping() {
                        ctx.warn(e.to_string());
                    } else {
                        let _ = ctx.shared.control.send(Control::WorkerFailed(e));
                    }
                }
            })?;
        self.shared
            .workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(handle);
        Ok(())
    }

    /// Saves this instance's state, for a revert after the process is gone
    /// (`<plugin> revert <instance>`). Save it **before** changing the
    /// device, so even a crash in the middle can be reverted.
    pub fn save_state<T: Serialize + ?Sized>(&self, state: &T) -> Result {
        self.shared.state.save(&self.shared.instance, state)
    }

    /// This instance's saved state, if any.
    pub fn load_state<T: DeserializeOwned>(&self) -> Result<Option<T>> {
        self.shared.state.load(&self.shared.instance)
    }

    /// Removes this instance's saved state. The runtime does it after a
    /// successful revert.
    pub fn clear_state(&self) -> Result {
        self.shared.state.clear(&self.shared.instance)
    }

    /// The state store of this plugin, for another instance's state.
    pub fn state_store(&self) -> &StateStore {
        &self.shared.state
    }

    /// Asks the workers to end and waits for them.
    pub(crate) fn stop_workers(&self) {
        {
            let mut stop = self.shared.stop.lock().unwrap_or_else(|e| e.into_inner());
            *stop = true;
        }
        self.shared.stop_cv.notify_all();
        loop {
            let handle = self
                .shared
                .workers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop();
            match handle {
                // A worker's panic is already caught in its thread.
                Some(h) => {
                    let _ = h.join();
                }
                None => break,
            }
        }
    }
}
