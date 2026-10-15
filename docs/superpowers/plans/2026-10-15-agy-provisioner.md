# Plan: the agy provisioner

Spec: `docs/superpowers/specs/2026-10-15-agy-provisioner-design.md`. Each
step is test first, then code, then `make check`.

1. `src/config/agy.rs`: `KIND`, `required_rule`, `covers`, `missing`, with
   the unit tests and proptests of the spec. Wired in `src/config/mod.rs`
   (`pub mod agy;`) and `Agents::agy_profile`.
2. `src/provision/agy.rs`: `Json` (order-preserving parse and pretty print),
   `merge` and its tests and proptests.
3. The shell commands `read_command` and `write_command`, tested under `sh`
   with a temp `HOME`; `Connector::agy_settings` and
   `Connector::write_agy_settings` on `Endpoint` through `probe`, and the
   fake's in-memory file.
4. `provision::agy::reconcile` and the `dispatch_with`/`resume_with` step
   before `Agents::launch`, with the dispatch tests.
5. `MachineSettings::own_deny`, `Fleet::own_deny`, the actor passing it;
   the daemon test that a machine's own profile reaches its actor.
6. Docs: manual (Permission profiles, the Agent kinds table, the error
   codes), `skills/pastor/SKILL.md`, `AGENTS.md` known gaps,
   `changes/feat-agy-provisioner.md`.
7. Push, open the pull request against `main`, noting the dependency on
   `feat/agent-profiles` and the owed `make smoke-profiles` with an agy
   machine.
