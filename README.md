# remora-qa-sdk

The public part of Remora QA, the test platform for Remora devices: what
a **product pack** needs to extend it without access to Remora's private
code.

- `components/qa-plugin-protocol`: the contract between the Remora QA
  agent (`remora-qad`) and an external plugin, a probe or a disruptor
  shipped as its own executable. See the crate's documentation.
- `components/qa-plugin-sdk` (`remora-qa-plugin-sdk`): write a plugin in
  Rust. It is the whole executable around your code: the command line,
  the run loop, events and logs, ending on `stop`/`revert` or when the
  agent goes away, errors and panics reported as `failed`, a disruptor's
  state kept for a cold revert. Std-only, depends on serde and
  serde_json.
- `components/qa-plugin-example` (not published): an example probe
  (`example-loadavg`) and an example disruptor (`example-marker`), with
  a test that runs their executables and talks the protocol to them.
- Planned: `qa-pack` (the schemas of a pack's manifest, scenarios and log
  markers, with a validator for a pack's CI).

## Writing a plugin for a pack

A plugin is a standalone crate in the pack, one executable per plugin,
depending on the SDK by git URL and revision:

```toml
[package]
name = "my-probe"
version = "0.1.0"
edition = "2021"

[dependencies]
remora-qa-plugin-sdk = { git = "https://github.com/sntns/remora-qa-sdk", rev = "<commit>" }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

Its `main` hands a type implementing `Probe` or `Disruptor` to the SDK:

```rust
fn main() {
    remora_qa_plugin_sdk::run_probe(MyProbe)
}
```

A probe implements `spec()` and `start(ctx, params)`, where it spawns
workers (`ctx.spawn`) that emit events (`ctx.event("sample", ...)`) until
`ctx.wait_stop(interval)` says to end. A disruptor implements `spec()`,
`apply(ctx, params)`, which saves its state (`ctx.save_state(...)`)
before it changes anything, and `revert_cold(state)`, which undoes the
change from that state alone. See the SDK's crate documentation for
complete examples, and `components/qa-plugin-example` for runnable ones.

Never print to stdout in a plugin: it carries the protocol. Log with
`ctx.info(...)` and friends, or on stderr.

Try it by hand:

```sh
cargo build -p remora-qa-plugin-example
./target/debug/example-loadavg describe
# Samples until the end of stdin, then answers `done`.
(echo '{"type":"start","instance":"p1","params":{"interval-ms":500}}'; sleep 2) \
  | ./target/debug/example-loadavg run
```

The pack ships the plugin's sources; the Remora QA controller
cross-builds them for the device's architecture (armv7, aarch64), so a
pack needs no cross toolchain of its own. Keep dependencies few and
small: a plugin runs on small ARM devices.

## Development

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Licensed under the Apache License, Version 2.0.
