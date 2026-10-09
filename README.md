# aiClean

Local Claude Code and Codex storage cleanup. No CLI account or login is required.

## Command contract

`aiClean check claude codex` is read-only and shows candidates, skipped sessions, source bytes, and recoverable Trash bytes. `plan` is not an alias.
`run claude codex` performs monthly cleanup on or after the 10th, at most once per successful month/provider. Providers are explicit, order-independent, duplicates rejected.
`enable claude codex`, `stop claude codex`, and `status claude codex` control and inspect per-provider user scheduled tasks. `stop` persistently disables future runs and signals a current cleanup to stop before its next session, while allowing the current durable step to finish. No process is forcibly killed.
`undo claude codex` restores each provider's latest unrestored batch; `--batch ID` selects a specific batch. Interrupted cleanup/undo is recoverable via this same command. Existing unrelated files/rows are never overwritten.
`--version`, `update`, `uninstall` are supported. Uninstall removes only the executable and its owned scheduled tasks, preserving sessions, receipts, and recoverable bundles.

```sh
aiClean check claude codex
aiClean enable claude codex
aiClean status claude codex
aiClean stop claude codex
aiClean run claude codex
aiClean undo claude codex
```

`check` and `status` never create receipt state. `status` reports the actual OS task state, next calendar occurrence, latest run/undo attempt, and interrupted recovery operations. Recovery byte counts are based on metadata; full checksums are verified before undo.

## Retention and capacity

Schedule: day 10 at 03:30 local OS time (this Mac is JST), RunAtLoad=false. OS login means signing into Mac/Windows; the CLI has no login. Windows tasks use the current user's interactive token, without storing a password.
Cutoff: first day of the current month at 00:00 in the system timezone. Genuine user sends and assistant message completion count; creation, archive, filesystem mtime, tool and metadata timestamps do not count. Current month, unknown timestamps, incomplete turns, queued work, active goals, unsupported indexes/relations, changed data, and unsafe paths are skipped.
Conversation applications, CLIs, and their code hosts must be closed for mutation; the guard conservatively checks both providers. check/status remain available while they run. Never stop the user's conversation applications automatically. A scheduled run that finds a running application refuses mutation and records the failure; run manually after closing applications if that month's task was skipped.
Raw logs and exact SQLite rows are captured, checksummed, and durably recorded before originals are removed. The bundle is moved to native OS Trash; the CLI never purges it. OS 30-day automatic purge must be verifiably enabled before cleanup, without changing unrelated OS settings. Trash space remains used until OS purge. SQLite deletion releases pages for reuse but is not presented as physical disk reclamation; no automatic VACUUM with large temporary space demand.
Codex thread/delete is not used. Offline direct row removal avoids its descendant cascade. Spawn-related sessions are conservatively skipped until an entire related unit can be safely handled.

## Durability acceptance

The maintenance lock covers run/undo/scheduler mutations. Immutable per-session bundles and durable operation receipts live independently. All source files and target database rows are verified before mutation. Exact typed rows (including stable row identity) are captured. Separate WAL database commits are explicitly journaled and replayed idempotently: crash after any commit/file operation can be recovered by undo; there is no false claim of a cross-database atomic transaction. Any pending partial operation blocks fresh cleanup. Rows already exactly restored are accepted; conflicts refuse before restore writes. File restore uses exclusive creation and verifies checksums. OS-purged bundles cannot be restored and status states this.

## Scope and release

Claude backend targets local Claude Code projects, not cloud history or account switching. Shared ~/.claude/history.jsonl prompt history is preserved; it is not used as a session registration index. Both providers respect CODEX_HOME / CLAUDE_CONFIG_DIR. Receipts/config default to ~/.aiClean, outside provider data.
Mac native Trash and launchd; Windows native Recycle Bin and per-user Task Scheduler. Linux/WSL are not treated as Windows.
Installer and release automation are implemented locally; public repository creation and releases are separate publishing operations. Until release, install from the local release build. Installer URLs become usable only after publication.

Build locally with `cargo build --release --locked`; the executable is `target/release/aiClean` (`aiClean.exe` on Windows). Install to a stable absolute location before `enable`, because scheduled tasks record that executable path. `--state-dir PATH` / `AICLEAN_STATE_DIR` selects independent recovery storage and must not overlap either provider root. Keep the recovery state when moving or updating the binary.

Windows cleanup requires verifiable documented Storage Sense policy for 30-day Recycle Bin cleanup and recovery storage on the OS drive. Unverified GUI settings and other volumes are refused rather than assumed to be purged. The CLI does not change Storage Sense settings. Native Windows MSVC build, strict Clippy, standard tests, PowerShell 5.1 installer fixtures, readonly Recycle Bin roundtrip, and a GUID-only no-run Task Scheduler register/disable/enable/unregister fixture have been verified over SSH. These use disposable data and do not enable cleanup or change Storage Sense on the Windows host. Intel Mac builds are verified; Intel execution remains untested without Rosetta.
