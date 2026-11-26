//! Spawns the example plugins' executables and talks the protocol to them,
//! as the agent does.

use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::ChildStdin;
use std::process::ChildStdout;
use std::process::Command;
use std::process::Stdio;

use remora_qa_plugin_sdk::protocol::AgentMessage;
use remora_qa_plugin_sdk::protocol::PluginKind;
use remora_qa_plugin_sdk::protocol::PluginMessage;
use remora_qa_plugin_sdk::protocol::PluginSpec;
use remora_qa_plugin_sdk::protocol::ENV_STATE_DIR;

const LOADAVG: &str = env!("CARGO_BIN_EXE_example-loadavg");
const MARKER: &str = env!("CARGO_BIN_EXE_example-marker");

fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("remora-qa-example-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn describe(exe: &str) -> PluginSpec {
    let out = Command::new(exe).arg("describe").output().unwrap();
    assert!(out.status.success());
    serde_json::from_slice(&out.stdout).unwrap()
}

struct Plugin {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Plugin {
    fn run(exe: &str, state_dir: &Path) -> Self {
        let mut child = Command::new(exe)
            .arg("run")
            .env(ENV_STATE_DIR, state_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
        }
    }

    fn send(&mut self, msg: &AgentMessage) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{}", serde_json::to_string(msg).unwrap()).unwrap();
        stdin.flush().unwrap();
    }

    /// The next message, skipping logs.
    fn recv(&mut self) -> PluginMessage {
        loop {
            let mut line = String::new();
            let n = self.stdout.read_line(&mut line).unwrap();
            assert!(n > 0, "the plugin closed its stdout");
            let msg: PluginMessage = serde_json::from_str(&line).unwrap();
            if !matches!(msg, PluginMessage::Log { .. }) {
                return msg;
            }
        }
    }

    fn close_stdin(&mut self) {
        self.stdin = None;
    }

    fn wait(mut self) -> i32 {
        self.child.wait().unwrap().code().unwrap()
    }
}

#[test]
fn loadavg_samples_until_stopped() {
    let spec = describe(LOADAVG);
    assert_eq!(spec.kind, PluginKind::Probe);
    assert_eq!(spec.name, "example-loadavg");

    let dir = test_dir("loadavg");
    let mut p = Plugin::run(LOADAVG, &dir);
    p.send(&AgentMessage::Start {
        instance: "p1".into(),
        params: serde_json::json!({ "interval-ms": 20 }),
    });
    assert_eq!(p.recv(), PluginMessage::Ready);
    for _ in 0..2 {
        match p.recv() {
            PluginMessage::Event { kind, event } => {
                assert_eq!(kind, "sample");
                assert!(event["load1"].as_f64().unwrap() >= 0.0, "{event}");
                assert!(event["load15"].is_number(), "{event}");
            }
            other => panic!("expected an event, got {other:?}"),
        }
    }
    p.send(&AgentMessage::Stop);
    loop {
        let msg = p.recv();
        if msg == PluginMessage::Done {
            break;
        }
        assert!(matches!(msg, PluginMessage::Event { .. }), "{msg:?}");
    }
    assert_eq!(p.wait(), 0);
}

#[test]
fn loadavg_rejects_invalid_params() {
    let dir = test_dir("loadavg-params");
    let mut p = Plugin::run(LOADAVG, &dir);
    p.send(&AgentMessage::Start {
        instance: "p1".into(),
        params: serde_json::json!({ "interval": 20 }),
    });
    match p.recv() {
        PluginMessage::Failed { error } => assert!(error.contains("interval"), "{error}"),
        other => panic!("expected failed, got {other:?}"),
    }
    assert_eq!(p.wait(), 1);
}

#[test]
fn marker_reverts_when_the_agent_goes_away() {
    let spec = describe(MARKER);
    assert_eq!(spec.kind, PluginKind::Disruptor);
    assert!(spec.reversible);

    let dir = test_dir("marker-eof");
    let marker = dir.join("marker");
    let mut p = Plugin::run(MARKER, &dir);
    p.send(&AgentMessage::Apply {
        instance: "d1".into(),
        params: serde_json::json!({ "path": marker }),
    });
    assert_eq!(p.recv(), PluginMessage::Ready);
    assert!(marker.exists());
    assert!(dir.join("example-marker/d1.json").exists());
    p.close_stdin();
    assert_eq!(p.recv(), PluginMessage::Done);
    assert_eq!(p.wait(), 0);
    assert!(!marker.exists());
    assert!(!dir.join("example-marker/d1.json").exists());
}

#[test]
fn marker_cold_revert_after_a_kill() {
    let dir = test_dir("marker-cold");
    let marker = dir.join("marker");
    let mut p = Plugin::run(MARKER, &dir);
    p.send(&AgentMessage::Apply {
        instance: "d1".into(),
        params: serde_json::json!({ "path": marker }),
    });
    assert_eq!(p.recv(), PluginMessage::Ready);
    // The process dies with its change in place.
    p.child.kill().unwrap();
    p.child.wait().unwrap();
    assert!(marker.exists());

    let status = Command::new(MARKER)
        .args(["revert", "d1"])
        .env(ENV_STATE_DIR, &dir)
        .status()
        .unwrap();
    assert!(status.success());
    assert!(!marker.exists());
    assert!(!dir.join("example-marker/d1.json").exists());

    // Again: nothing left to revert.
    let status = Command::new(MARKER)
        .args(["revert", "d1"])
        .env(ENV_STATE_DIR, &dir)
        .status()
        .unwrap();
    assert!(status.success());
}
