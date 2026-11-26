//! An example disruptor: creates a marker file, removes it on revert. Its
//! state (the file's path) lets `example-marker revert <instance>` remove
//! it after the process that created it is gone.

use std::path::PathBuf;

use remora_qa_plugin_sdk::disruptor_spec;
use remora_qa_plugin_sdk::Context as _;
use remora_qa_plugin_sdk::Ctx;
use remora_qa_plugin_sdk::Disruptor;
use remora_qa_plugin_sdk::PluginSpec;
use remora_qa_plugin_sdk::Result;
use remora_qa_plugin_sdk::RiskLevel;
use serde::Deserialize;
use serde::Serialize;

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct Params {
    path: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct State {
    path: PathBuf,
}

struct Marker;

impl Disruptor for Marker {
    type Params = Params;
    type State = State;

    fn spec(&self) -> PluginSpec {
        PluginSpec {
            params: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
                "additionalProperties": false
            }),
            ..disruptor_spec(
                "example-marker",
                1,
                RiskLevel::Soft,
                "Creates a marker file, removes it on revert",
            )
        }
    }

    fn apply(&mut self, ctx: &Ctx, params: Params) -> Result {
        if params.path.exists() {
            // Not ours: reverting would remove it. Fail before saving any
            // state, so the revert that follows a failed apply is a no-op.
            remora_qa_plugin_sdk::bail!("{} already exists", params.path.display());
        }
        // The state first: a crash right after the write is still undone.
        ctx.save_state(&State {
            path: params.path.clone(),
        })?;
        std::fs::write(&params.path, format!("remora-qa {}\n", ctx.instance()))
            .with_context(|| format!("creating {}", params.path.display()))?;
        ctx.info(format!("created {}", params.path.display()));
        Ok(())
    }

    fn revert_cold(&mut self, state: State) -> Result {
        match std::fs::remove_file(&state.path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("removing {}", state.path.display()))
            }
            _ => Ok(()),
        }
    }
}

fn main() {
    remora_qa_plugin_sdk::run_disruptor(Marker)
}
