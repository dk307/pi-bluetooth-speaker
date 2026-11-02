# PR #21 Leftover Findings — Handoff for Next PR

Source: PR-Agent reviewer guides on commit `1465052` (comments 08:05–08:45, fetched via
`gh pr view 21 --json comments`). Triage: Tier 1 = correctness, Tier 2 = gate/hard error,
Tier 3 = advisory/simplification. Per AGENTS.md §4.1, Tier 1/2 must be fixed; Tier 3 only if
it genuinely reduces risk or code volume.

## Tier 1 (Correctness — fix first)

1. **Dropped PCM update on mid-query command race** — `bt_monitor_loop`
   (core_daemon.rs, post-query guard `if !commands.is_empty() { last_pb = None; cover.path = None; continue; }`):
   the guard discards the staged `BtSharedUpdate`, but `last_pcms` was already advanced to the
   fresh value → the PCM delta is never republished (dedup reports "unchanged" on every later
   tick; stale stream pill until the PCM list changes again).
   Fix: reset `last_pcms = None` alongside `last_pb`/`cover.path` (or `continue` before the
   pcm dedup commit). Add a regression test if feasible.

2. **Listener thread leak on drop (blocked `MessageIterator::next`)** —
   `spawn_properties_changed_listener` (connection.rs) blocks in `iter.flatten()`; the `stop`
   flag is only observed after the next message or bus loss. Every dropped `DbusClient`
   (incl. `probe_bluetooth()` in `ui/boot.rs`, one per boot, and one per private-bus test)
   pins a thread + `Connection` clone until some unrelated D-Bus traffic arrives.
   Fix options: bounded receive timeout (`MessageIterator` timeout / poll with
   `Connection::receive_match` + `try_next`-style drain), or hold the connection by an
   owned slot the thread can close. Verify zbus 5.19 blocking API for a timeout-based
   iterator before choosing. This is the 5th time flagged; previous defers cited the
   production daemon living for the process lifetime — address the boot-probe and
   test-accumulation cases at minimum.

3. **Config perms become 0600 after tempfile save** — `CoreConfig::save` (config.rs):
   `NamedTempFile` creates the staged file with mode `0600`; `persist()` renames it over
   `/etc/speaker-core/config.json`. Previous `File::create` path produced `0644 & ~umask`.
   Non-root consumers (diagnostic CLIs, provisioning steps) would start getting `EACCES`.
   Fix: `file.set_permissions(std::fs::Permissions::from_mode(0o644))` before `sync_all`
   (via `std::os::unix::fs::PermissionsExt`), plus a test asserting the installed mode.

4. **Serde borrow bug in `ThemePreset` Deserialize** — theme.rs: the impl uses
   `<&'de str>::deserialize`, which only works for zero-copy deserializers; would appear at
   `core_daemon.rs` lines 163-168 (now at ~235-240). `from_str` works today but
   `from_reader`/`from_value` would fail with "invalid type: string, expected a borrowed
   string". Fix: `String::deserialize` + `Self::parse_slug(&slug)` (main's version of this
   exact impl was reverted during the merge conflict resolution — restore it and add a
   `from_value`/`from_reader` regression test).

   NOTE (pre-commit check): after the `1465052` merge this same finding appeared as
   "Latent Serde Bug" — `theme.rs` currently carries the `String::deserialize` variant per
   the merge resolution; verify which variant is actually on disk before fixing
   (`grep -n 'String::deserialize\|<&.de str>' services/core/src/ui/theme.rs`).

5. **Missing `# Errors` doc sections (M-CANONICAL-DOCS)** — several public fallible /
   effectful fns in core_daemon.rs lack them or are mis-documented:
   `run_splash_on`, `init_shell`, `bt_monitor_loop`, `publish_playback_state`,
   `decode_cover_buffer`, `apply_shared_bt_state`, `run_render_loop`, `run_shell_on`.
   For infallible-but-effectful fns, document observable side effects / early-return
   behavior instead of a bogus `# Errors`.

## Tier 2 (Hard gates / CI)

6. **Lint gate gap: `ui` feature never clippy-checked in CI** — `.github/workflows/core-ci.yml`
   `lint-x86-gated` job runs plain `cargo clippy --target x86_64-unknown-linux-gnu`; aarch64
   job lints the ARM default set. Confirm whether `ui` is a default feature in
   `services/core/Cargo.toml`: if it is default, nothing to do (document in the job comment);
   if it is not, add `--features ui` to the x86 job so ~1500 lines of ui-gated code are gated
   by `-D warnings`.

7. **Dual export path for `boot` module** — `services/core/src/ui/mod.rs`:
   `pub mod boot;` + re-export `pub use boot::{...}` gives two canonical paths
   (`ui::boot::BootProbe` and `ui::BootProbe`), violating M-SINGLE-ITEM-PATH (siblings use
   `pub(crate) mod`). Fix: `pub(crate) mod boot;` and keep the explicit `#[doc(inline)] pub use`.

8. **Misleading test name** — `core_daemon.rs` test
   `run_splash_on_mocked_render_error_still_joins_check_thread` never observes a join: the
   render-error path exits with `checks_done == false` → "abandoning worker" → JoinHandle
   dropped without `join()`. Either rename the test to what it actually asserts
   (e.g. `..._abandons_check_thread`) or fix the production path to join/abort the worker
   on the render-error branch before deciding.

9. **`tempfile` dep approval trail** — reviewer flags that adding `tempfile = "=3.17.1"`
   bypassed the AGENTS.md §5.2 user-approval protocol. It *was* user-directed ("Can we use a
   crate for atomic save logic") — record that approval in the next PR description so the
   audit trail is explicit; no code change.

10. **Vendored agent skill files contain an authenticated `gh` call** —
    `.agents/skills/slint/SKILL.md` (+ `skills-lock.json`) includes a "Show your support"
    section instructing agents to run `gh api --method PUT /user/starred/slint-ui/slint`.
    Untrusted content steering future agent sessions into credentialed actions; out of PR
    scope. Fix: strip the credential-using section from the vendored skill file (or remove
    the vendored skills from the repo entirely if they don't belong in the appliance repo).

## Tier 3 (Advisory — defer or take if trivial)

11. **`SettleWindow::suppresses` double expiry check** — `publish_playback_state` already
    checks `BT_COMMAND_SETTLE` expiry immediately before calling `suppresses(pb)`; a single
    check suffices. Take only if it simplifies without touching tests.

12. **Post-query race resets `cover.path = None`, forcing Lanczos3 re-decode** of an
    unchanged cover; resetting only `last_pb` (and `last_pcms`, per Tier 1 #1) would avoid
    the full re-decode. Take together with #1.

13. **`decode_cover_buffer` skips caching when source ≥ TARGET_COVER_SIZE** — every
    path change or retry re-decodes the full-size original. Cache (copy) in that case too,
    or lean harder on the `CoverState` dedup.

14. **`MAX_SPLASH_WAIT = 30s` magic constant** — add a comment referencing probe bounds
    (D-Bus/I2C timeouts) or a `const _: () = { assert!(...) }` proof; datasheet-bound proof
    is not available, so a documented bound is acceptable.

15. **`publish_playback_state` decomposition** (5th time deferred) — split into
    `retry_cover_decode` / `expire_settle_window` helpers for isolated unit testing. Only
    with the Tier 1 #1/#12 cover/pending-state changes, since they touch the same code.

16. **`Option<Option<CoverPixelBuffer>>` → `CoverDelta` enum** (4th time deferred) —
    M-MAKE-INVALID-STATES-UNREPRESENTABLE. Take only as part of the same worker-publish
    refactor as #15; conversion churn alone is not justified.

17. **Simplifications bundle** (take opportunistically):
    - Move `DEFAULT_CONFIG_PATH` to `crate::diag` (already there post-merge? verify) and
      delete the `#[cfg(not(feature = "ui"))]` literal restatement in `print_usage`.
    - `BluetoothDevice::execute_transport_command(cmd)` shared helper so
      `tests/ui_shell_integration.rs::drain_worker_commands` and `bt_monitor_loop` share
      the dispatch match.
    - Consolidate the double `!commands.is_empty()` re-check into one drain point.

## Suggested split

- **Next PR (correctness):** #1, #2, #3, #4, #8, #10 (+ #6 verification).
- **Follow-up refactor PR:** #5, #7, #11–#17.
