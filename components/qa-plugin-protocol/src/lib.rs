//! The contract between the Remora QA agent (`remora-qad`) and an external
//! plugin: a probe or a disruptor shipped as its own executable, typically
//! by a product pack.
//!
//! # The executable
//!
//! - `<plugin> describe` prints its [`PluginSpec`] as JSON on stdout and
//!   exits 0.
//! - `<plugin> run` runs **one instance**. The agent writes
//!   [`AgentMessage`]s on its stdin and reads [`PluginMessage`]s on its
//!   stdout, one JSON object per line. Anything for humans goes to stderr.
//! - `<plugin> revert <instance>` undoes what a disruptor instance applied,
//!   from the state it saved under [`ENV_STATE_DIR`], when the process that
//!   applied it is gone (the agent restarted). Exit 0 once reverted, or if
//!   there was nothing to revert.
//!
//! # A run
//!
//! A probe: `start` → `ready` (or `failed`) → `event`s → `stop` → `done`,
//! then the process exits. A disruptor: `apply` → `ready` once applied (or
//! `failed`) → `event`s → `revert` → `done`.
//!
//! **End of stdin means the agent is gone**: a probe stops, a disruptor
//! reverts what it applied, then the process exits. A disruptor never
//! outlives its agent with its change in place.
//!
//! # Rules every plugin and agent follow
//!
//! - The first message is `start` (probe) or `apply` (disruptor); any other
//!   first message is answered `failed`. Later unexpected lines are logged
//!   by the plugin and ignored.
//! - `failed` may come at any time, also after `ready` (a worker died): a
//!   disruptor reverts first, then reports it. The process exits next.
//! - `stop` and `revert` both end the instance: a disruptor told `stop`
//!   reverts, a probe told `revert` stops.
//! - Exit status of `run`: 0 after `done`, 1 after `failed`.
//! - The agent waits a bounded time (its own setting, typically 10 s) for
//!   `ready` and for `done`; past it, it sends SIGTERM, then SIGKILL, and
//!   for a disruptor runs `<plugin> revert <instance>`. `revert` must
//!   therefore be idempotent and work from the saved state alone.
//! - An instance name is non-empty, without `/`, `\`, NUL or a leading
//!   `.`, so that it can name a state file.
//!
//! The agent dates, numbers and forwards the events: a plugin only names
//! the kind and the data.

use serde::Deserialize;
use serde::Serialize;

/// The version of this contract. A plugin reports the one it implements
/// in its [`PluginSpec`]; the agent refuses a plugin it does not speak.
pub const PROTOCOL_VERSION: u32 = 1;

/// The directory where a disruptor keeps what it needs to revert, one
/// file per instance. Set by the agent; it survives an agent restart but
/// not necessarily a reboot.
pub const ENV_STATE_DIR: &str = "REMORA_QA_STATE_DIR";
/// The instance's name, as in `start`/`apply` (and the `revert`
/// argument): set by the agent for plugins that need it before reading
/// stdin, e.g. to name a log file. Plugins may ignore it.
pub const ENV_INSTANCE: &str = "REMORA_QA_INSTANCE";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PluginKind {
    /// Observes, never changes the device.
    Probe,
    /// Changes the device's state, then restores it.
    Disruptor,
}

/// How far a disruptor goes. The device's own policy decides which levels
/// it accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RiskLevel {
    /// Process-level, undone by `revert` (kill, CPU or memory load).
    Soft,
    /// Device-level but reversible (network cut, clock jump).
    Medium,
    /// Reboots or installs.
    Hard,
    /// Can damage data or hardware state (power cut).
    Destructive,
}

/// What a plugin says of itself (`<plugin> describe`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct PluginSpec {
    /// The contract version it implements ([`PROTOCOL_VERSION`]).
    pub protocol: u32,
    /// Its name within its pack (`kms-transitions`); the agent prefixes
    /// the pack's name.
    pub name: String,
    pub version: u32,
    pub kind: PluginKind,
    /// A disruptor's level; a probe has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<RiskLevel>,
    /// A disruptor's `revert` restores the state it changed.
    #[serde(default)]
    pub reversible: bool,
    /// Device capabilities it needs (`root`, `systemd`, `journald`,
    /// `tracefs`...).
    #[serde(default)]
    pub needs: Vec<String>,
    /// The JSON Schema of its parameters.
    #[serde(default)]
    pub params: serde_json::Value,
    pub description: String,
}

/// What the agent writes on the plugin's stdin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum AgentMessage {
    /// Start a probe instance.
    Start {
        instance: String,
        #[serde(default)]
        params: serde_json::Value,
    },
    /// Apply a disruptor instance.
    Apply {
        instance: String,
        #[serde(default)]
        params: serde_json::Value,
    },
    /// Stop the probe; answer `done`, then exit.
    Stop,
    /// Revert the disruptor; answer `done` (or `failed`), then exit.
    Revert,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

/// What the plugin writes on its stdout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum PluginMessage {
    /// The probe runs, or the disruptor is applied.
    Ready,
    /// One event, of the plugin's own kind.
    Event {
        kind: String,
        #[serde(default)]
        event: serde_json::Value,
    },
    /// A line for the agent's log.
    Log { level: LogLevel, message: String },
    /// Stopped or reverted: the process exits next.
    Done,
    /// It could not start, apply or revert, or it broke while running;
    /// the process exits next.
    Failed { error: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_message_wire_shape() {
        let msg = AgentMessage::Apply {
            instance: "d1".into(),
            params: serde_json::json!({"hold": "10s"}),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "apply");
        assert_eq!(json["instance"], "d1");
        assert_eq!(serde_json::from_value::<AgentMessage>(json).unwrap(), msg);
        let stop: AgentMessage = serde_json::from_str(r#"{"type":"stop"}"#).unwrap();
        assert_eq!(stop, AgentMessage::Stop);
    }

    #[test]
    fn plugin_message_wire_shape() {
        let msg = PluginMessage::Event {
            kind: "sample".into(),
            event: serde_json::json!({"value": 3}),
        };
        let line = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            line,
            r#"{"type":"event","kind":"sample","event":{"value":3}}"#
        );
        let failed: PluginMessage =
            serde_json::from_str(r#"{"type":"failed","error":"no tracefs"}"#).unwrap();
        assert_eq!(
            failed,
            PluginMessage::Failed {
                error: "no tracefs".into()
            }
        );
    }

    #[test]
    fn spec_defaults() {
        let spec: PluginSpec = serde_json::from_str(
            r#"{"protocol":1,"name":"x","version":1,"kind":"probe","description":"d"}"#,
        )
        .unwrap();
        assert_eq!(spec.level, None);
        assert!(spec.needs.is_empty());
        assert!(!spec.reversible);
    }
}
