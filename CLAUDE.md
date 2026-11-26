# remora-qa-sdk

- **This repository is public.** It only holds what a product pack needs
  to extend Remora QA: the plugin contract, the plugin SDK, the pack
  schemas. Never put anything from the private repos here (the agent <->
  controller protocol, remote execution, security, controller internals),
  nor anything specific to one product or customer.
- Everything in English, comments included.
- Same conventions as remora-edge: stable toolchain only, `cargo fmt`,
  `cargo clippy --workspace --all-targets -- -D warnings`, crates under
  `components/`, packages named `remora-<name>`.
- Keep dependencies minimal (a plugin is built for small ARM devices).
