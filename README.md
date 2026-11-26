# remora-qa-sdk

The public part of Remora QA, the test platform for Remora devices: what
a **product pack** needs to extend it without access to Remora's private
code.

- `components/qa-plugin-protocol`: the contract between the Remora QA
  agent (`remora-qad`) and an external plugin, a probe or a disruptor
  shipped as its own executable. See the crate's documentation.
- Planned: `qa-plugin-sdk` (writing a plugin in Rust: the run loop,
  events, state kept for a cold revert) and `qa-pack` (the schemas of a
  pack's manifest, scenarios and log markers, with a validator for a
  pack's CI).

Licensed under the Apache License, Version 2.0.
