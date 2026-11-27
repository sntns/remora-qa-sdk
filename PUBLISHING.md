# Publishing

`remora-qa-plugin-protocol` and `remora-qa-plugin-sdk` are published to
crates.io with one shared version; `remora-qa-plugin-example` is not.

1. Make sure `main` is green.
2. In GitHub: Actions, "Release and Publish", Run workflow, pick the level:
   - `patch`, `minor` or `major` bumps the version first;
   - `current` publishes the version in `Cargo.toml` as it is (used for
     the first release).
3. cargo-release publishes the protocol, then the SDK, commits
   `chore: release vX.Y.Z`, tags `vX.Y.Z` and pushes.

The workflow uses the organization's `CARGO_REGISTRY_TOKEN` secret.

Changes to `remora-qa-plugin-protocol` are a contract change for every
plugin and for the Remora QA agent: a breaking change needs a new
`PROTOCOL_VERSION` and a major (or, before 1.0, minor) release.
