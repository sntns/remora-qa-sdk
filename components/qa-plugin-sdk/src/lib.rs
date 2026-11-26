//! Write a Remora QA plugin in Rust: a **probe** that observes the device
//! and emits events, or a **disruptor** that changes the device's state
//! and restores it.
//!
//! The SDK is the whole executable around your code: it parses the
//! command line (`describe`, `run`, `revert <instance>`), speaks the
//! [`protocol`] on stdin/stdout, ends the
//! instance when the agent says so or goes away (end of stdin), turns
//! errors and panics into `failed`, keeps a disruptor's state for a cold
//! revert, and exits with the right code. It is std-only (threads, no
//! async runtime) and depends on serde and serde_json only.
//!
//! # A probe
//!
//! ```no_run
//! use std::time::Duration;
//!
//! use remora_qa_plugin_sdk::{probe_spec, Ctx, PluginSpec, Probe, Result};
//! use serde::Deserialize;
//!
//! #[derive(Deserialize)]
//! #[serde(rename_all = "kebab-case", deny_unknown_fields)]
//! struct Params {
//!     #[serde(default = "default_interval")]
//!     interval_ms: u64,
//! }
//!
//! fn default_interval() -> u64 {
//!     1000
//! }
//!
//! struct Uptime;
//!
//! impl Probe for Uptime {
//!     type Params = Params;
//!
//!     fn spec(&self) -> PluginSpec {
//!         PluginSpec {
//!             params: serde_json::json!({
//!                 "type": "object",
//!                 "properties": { "interval-ms": { "type": "integer", "minimum": 1 } }
//!             }),
//!             ..probe_spec("uptime", 1, "Samples /proc/uptime")
//!         }
//!     }
//!
//!     fn start(&mut self, ctx: &Ctx, params: Params) -> Result {
//!         let interval = Duration::from_millis(params.interval_ms);
//!         // `start` returns at once; the work runs on a worker thread,
//!         // stopped and joined by the SDK before it answers `done`.
//!         ctx.spawn("sampler", move |ctx| {
//!             while !ctx.wait_stop(interval) {
//!                 let uptime = std::fs::read_to_string("/proc/uptime")?;
//!                 ctx.event("sample", serde_json::json!({ "uptime": uptime.trim() }));
//!             }
//!             Ok(())
//!         })
//!     }
//! }
//!
//! fn main() {
//!     remora_qa_plugin_sdk::run_probe(Uptime)
//! }
//! ```
//!
//! # A disruptor
//!
//! A disruptor saves what it needs to revert **before** it changes
//! anything, so the change can be undone even after a crash, by the agent
//! running `<plugin> revert <instance>`. The default
//! [`revert`](Disruptor::revert) loads that state and calls
//! [`revert_cold`](Disruptor::revert_cold), so most disruptors only write
//! `apply` and `revert_cold`.
//!
//! ```no_run
//! use std::path::PathBuf;
//!
//! use remora_qa_plugin_sdk::{disruptor_spec, Ctx, Disruptor, PluginSpec, Result, RiskLevel};
//! use serde::{Deserialize, Serialize};
//!
//! #[derive(Deserialize)]
//! struct Params {
//!     path: PathBuf,
//! }
//!
//! #[derive(Serialize, Deserialize)]
//! struct State {
//!     path: PathBuf,
//! }
//!
//! struct Marker;
//!
//! impl Disruptor for Marker {
//!     type Params = Params;
//!     type State = State;
//!
//!     fn spec(&self) -> PluginSpec {
//!         disruptor_spec("marker", 1, RiskLevel::Soft, "Creates a marker file")
//!     }
//!
//!     fn apply(&mut self, ctx: &Ctx, params: Params) -> Result {
//!         ctx.save_state(&State { path: params.path.clone() })?;
//!         std::fs::write(&params.path, b"remora-qa\n")?;
//!         Ok(())
//!     }
//!
//!     fn revert_cold(&mut self, state: State) -> Result {
//!         match std::fs::remove_file(&state.path) {
//!             Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
//!             _ => Ok(()),
//!         }
//!     }
//! }
//!
//! fn main() {
//!     remora_qa_plugin_sdk::run_disruptor(Marker)
//! }
//! ```
//!
//! # What the SDK does
//!
//! - `describe`: prints [`spec`](Probe::spec) as one JSON line, with
//!   `protocol` and `kind` filled in by the SDK.
//! - `run`: waits for `start` (a probe) or `apply` (a disruptor), parses
//!   its params into `Params` (missing params read as `{}`; invalid ones
//!   answer `failed` with serde's message), calls `start`/`apply`, answers
//!   `ready`. Then, on `stop`, `revert` or the end of stdin, it asks the
//!   workers to end ([`Ctx::wait_stop`]), joins them, calls
//!   `stop`/`revert`, answers `done` and exits 0. A disruptor's state is
//!   cleared once reverted.
//! - An error or a panic in `start`/`apply`/`stop`/`revert`, or in a
//!   worker while the instance runs, answers `failed` and exits 1. When
//!   `apply` fails, `revert` is still called to undo a partial change: it
//!   must cope with one.
//! - `revert <instance>`: loads the instance's saved state and calls
//!   `revert_cold`, then clears it. Exits 0 once reverted or when there
//!   is no state, 1 on failure (the state is kept for another try).
//!
//! # Rules for a plugin
//!
//! - **Never print to stdout**: it carries the protocol. Use
//!   [`Ctx::log`] (forwarded to the agent's log) or stderr.
//! - `start`/`apply` return once started or applied; long work goes to
//!   [`Ctx::spawn`] workers that return soon after the stop signal.
//! - A disruptor's `revert` and `revert_cold` must be idempotent: they
//!   can run after a partial `apply`, or twice.
//!
//! # Testing
//!
//! [`run_probe_with`] and [`run_disruptor_with`] run the same loop on any
//! [`Io`] (command line, state directory, stdin and stdout), so a plugin
//! can be driven in-memory from its own tests.

mod ctx;
mod error;
mod runtime;
mod state;

pub use remora_qa_plugin_protocol as protocol;
pub use remora_qa_plugin_protocol::LogLevel;
pub use remora_qa_plugin_protocol::PluginSpec;
pub use remora_qa_plugin_protocol::RiskLevel;
use serde::de::DeserializeOwned;
use serde::Serialize;

pub use crate::ctx::Ctx;
pub use crate::error::Context;
pub use crate::error::Error;
pub use crate::error::Result;
pub use crate::runtime::run_disruptor;
pub use crate::runtime::run_disruptor_with;
pub use crate::runtime::run_probe;
pub use crate::runtime::run_probe_with;
pub use crate::runtime::Io;
pub use crate::runtime::EXIT_FAILED;
pub use crate::runtime::EXIT_OK;
pub use crate::runtime::EXIT_USAGE;
pub use crate::state::StateStore;

/// A plugin that observes the device and emits events.
pub trait Probe: Send + 'static {
    /// Its parameters, deserialized from `start`'s `params`.
    type Params: DeserializeOwned;

    /// What it says of itself; start from [`probe_spec`].
    fn spec(&self) -> PluginSpec;

    /// Starts observing and returns: emit from [`Ctx::spawn`] workers.
    /// An error answers `failed`.
    fn start(&mut self, ctx: &Ctx, params: Self::Params) -> Result;

    /// Called once the workers have ended, before `done`: release what
    /// `start` took. Nothing by default.
    fn stop(&mut self, ctx: &Ctx) -> Result {
        Ok(())
    }
}

/// A plugin that changes the device's state, then restores it.
pub trait Disruptor: Send + 'static {
    /// Its parameters, deserialized from `apply`'s `params`.
    type Params: DeserializeOwned;
    /// What it saves to revert without its process
    /// ([`Ctx::save_state`]).
    type State: Serialize + DeserializeOwned;

    /// What it says of itself; start from [`disruptor_spec`].
    fn spec(&self) -> PluginSpec;

    /// Applies the change and returns once it is in place. Save the
    /// state first. An error answers `failed`, after a call to
    /// [`revert`](Self::revert) to undo a partial change.
    fn apply(&mut self, ctx: &Ctx, params: Self::Params) -> Result;

    /// Undoes the change, the workers having ended. By default: loads the
    /// saved state and calls [`revert_cold`](Self::revert_cold) (nothing
    /// to do without a state). The SDK clears the state afterwards.
    fn revert(&mut self, ctx: &Ctx) -> Result {
        match ctx.load_state::<Self::State>()? {
            Some(state) => self.revert_cold(state),
            None => Ok(()),
        }
    }

    /// Undoes the change from the saved state alone, the process that
    /// applied it being gone (`<plugin> revert <instance>`).
    fn revert_cold(&mut self, state: Self::State) -> Result;
}

/// A probe's spec: fill in `needs` and `params` with struct update syntax
/// (`PluginSpec { needs: ..., ..probe_spec(...) }`).
pub fn probe_spec(name: &str, version: u32, description: &str) -> PluginSpec {
    PluginSpec {
        protocol: protocol::PROTOCOL_VERSION,
        name: name.to_owned(),
        version,
        kind: protocol::PluginKind::Probe,
        level: None,
        reversible: false,
        needs: Vec::new(),
        params: serde_json::Value::Null,
        description: description.to_owned(),
    }
}

/// A reversible disruptor's spec: fill in `needs` and `params` with struct
/// update syntax.
pub fn disruptor_spec(name: &str, version: u32, level: RiskLevel, description: &str) -> PluginSpec {
    PluginSpec {
        kind: protocol::PluginKind::Disruptor,
        level: Some(level),
        reversible: true,
        ..probe_spec(name, version, description)
    }
}

/// A fresh, empty directory for a test.
#[cfg(test)]
pub(crate) fn test_dir(name: &str) -> std::path::PathBuf {
    use std::sync::atomic::AtomicU32;
    use std::sync::atomic::Ordering;
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "remora-qa-sdk-{}-{}-{name}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
