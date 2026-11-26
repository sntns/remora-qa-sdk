//! An example probe: emits a `sample` event with the load averages of
//! `/proc/loadavg` every `interval-ms`.

use std::time::Duration;

use remora_qa_plugin_sdk::probe_spec;
use remora_qa_plugin_sdk::Context as _;
use remora_qa_plugin_sdk::Ctx;
use remora_qa_plugin_sdk::PluginSpec;
use remora_qa_plugin_sdk::Probe;
use remora_qa_plugin_sdk::Result;
use serde::Deserialize;
use serde::Serialize;

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct Params {
    #[serde(default = "default_interval_ms")]
    interval_ms: u64,
}

fn default_interval_ms() -> u64 {
    1000
}

#[derive(Serialize)]
struct Sample {
    load1: f64,
    load5: f64,
    load15: f64,
}

fn read_loadavg() -> Result<Sample> {
    let text = std::fs::read_to_string("/proc/loadavg").context("reading /proc/loadavg")?;
    let mut fields = text.split_whitespace().map(str::parse::<f64>);
    let mut next = || -> Result<f64> {
        match fields.next() {
            Some(v) => Ok(v.context("parsing /proc/loadavg")?),
            None => remora_qa_plugin_sdk::bail!("/proc/loadavg is too short"),
        }
    };
    Ok(Sample {
        load1: next()?,
        load5: next()?,
        load15: next()?,
    })
}

struct LoadAvg;

impl Probe for LoadAvg {
    type Params = Params;

    fn spec(&self) -> PluginSpec {
        PluginSpec {
            needs: vec!["procfs".into()],
            params: serde_json::json!({
                "type": "object",
                "properties": {
                    "interval-ms": { "type": "integer", "minimum": 1, "default": 1000 }
                },
                "additionalProperties": false
            }),
            ..probe_spec("example-loadavg", 1, "Samples /proc/loadavg")
        }
    }

    fn start(&mut self, ctx: &Ctx, params: Params) -> Result {
        if params.interval_ms == 0 {
            remora_qa_plugin_sdk::bail!("interval-ms must be at least 1");
        }
        // Fail at once, before `ready`, when there is nothing to sample.
        read_loadavg()?;
        let interval = Duration::from_millis(params.interval_ms);
        ctx.spawn("sampler", move |ctx| {
            while !ctx.wait_stop(interval) {
                ctx.event("sample", read_loadavg()?);
            }
            Ok(())
        })
    }
}

fn main() {
    remora_qa_plugin_sdk::run_probe(LoadAvg)
}
