# Disk Cleanup and Maintenance

[Русский](README.md) · **English** · [简体中文](README.zh-CN.md)

<!-- i18n-source-git-blob: 2630521deeff475a4b9958ebae7ebbc40e95ebd7 -->

`repository-maintenance` is a separate Cargo package for monitoring local artifacts and safely cleaning up reproducible data. It is independent of `anki-repo`: it does not work with CrowdAnki exports and instead manages the lifecycle of disk usage.

## Audit findings

At the current `HEAD`, the relevant data sources are as follows:

| Area | Actual source | Policy |
|---|---|---|
| Cargo workspace | A single root workspace; the normal `target` directory is resolved via `cargo metadata`. Build and test profiles are declared in the root `Cargo.toml`. | Measure both `target` and the configured Cargo `build-dir` within the workspace; perform cleanup only through `cargo clean`. |
| Nested builds | The `code-review` check in `anki-repo` runs Cargo Clippy; synthetic Cargo scenarios set `CARGO_TARGET_DIR` within their own `TempWorkspace`. | The temporary directory is removed by its owner or the existing orphan-directory collector. |
| Process, browser, and test temporary data | `asset-store` creates a `TempWorkspace` with a private ownership marker; the browser gets a separate profile and `TMPDIR` inside that tree. | Apply process identity, PID start time, process references, `NOFOLLOW`, and mount-point boundaries implemented by `asset-store`. This tool invokes the same public garbage-collection paths. |
| Evidence ZIP archives | `tools/asset-store/src/bin/common/evidence_zip.rs` writes archives to `<temp_dir>/anki-decks-evidence` or a path supplied via `--output`. | ZIP files contain evidence, so the new collector only lists them in its inventory. Directories and files without a verifiable ownership marker are not deleted by default. Recognized legacy directories without markers can be deleted only by an explicit `clean --cleanup-legacy --apply`. An explicit `--output` outside the scanned roots is excluded from inventory. |
| Persistent storage and transactional `.tmp` files | `asset-store` keeps its data in a directory that it manages; temporary names are used for atomic operations and recovery from incomplete transitions. | This is application-domain state, not general Cargo waste; disk cleanup does not touch it. |
| Persistent external Cargo build directories | There is currently no dedicated long-running process in the source code creating Cargo build directories under `~/.cache`. Arbitrary directories found during a local incident do not establish ownership. | `cache init` and `cache run` support explicitly reusable caches by creating a marker, updating their last-use time, and applying a shared size limit. Unknown directories are never cleaned. |

This utility does not change Cargo profiles. One-off builds remain confined to `TempWorkspace`; separate persistent external test-build directories are not created.

## Ownership boundaries

- The main `target` directory is determined from the actual output of `cargo metadata`. It is cleaned only if `target` and every relevant build directory are inside the current workspace, the tree has no errors or mount-point boundaries, the standard `CACHEDIR.TAG` is present, and the process check finds no busy paths. If `target` points to an external shared directory, it remains visible but is not automatically cleaned. When it exceeds the hard limit, the report flags unverified ownership as an error and returns a nonzero status.
- Before deletion, build-directory boundaries are checked: the workspace root, source trees, and overlapping directories are never handed to `cargo clean`, even with a valid `CACHEDIR.TAG`. An absent ordinary `target` does not prevent safe cleanup of a separate verified `build-dir`; if a directory appears after planning, it must be checked again.
- A separately configured `build-dir` is cleaned together with `target` by a single `cargo clean` invocation, so both directories must first be verified using the standard `CACHEDIR.TAG`. If the main `target` does not exist, the existing verified `build-dir` is passed as both paths; any new, unverified `target` is left untouched. A directory without a marker is not cleaned. The marker is checked when cleanup is selected (`ThresholdDecision::Clean`): a missing marker then results in `error` and preserves data. For `keep` or `warn`, no cleanup is planned and no marker is required.
- A reusable external cache is created only by `cache init`. Its marker attests to the canonical workspace root, cache identifier, and relative `target` path. The directory must be owned by the current UID and inaccessible to group and other users. `cache run` directs Cargo's `target` and `build-dir` into this cache and updates the last-use timestamp both before and after the command. The collector considers only such caches.
- `--temp-root` and `std::env::temp_dir()` are scanned in full; identical canonical roots are scanned only once. The report includes the total allocated-block size, number of top-level entries, and up to N largest entries. Sparse files are counted by allocated blocks; hard links are counted once per scanned root. Symbolic links are not followed, and other mount-point boundaries are not crossed.
- The existing collector checks ownership when cleaning `TempWorkspace`. A demonstrably live owner, an unknown PID-to-process reference, a corrupted marker, or a verification failure results in `deferred`/`error`, not attempted deletion.
- Entries owned by other UIDs are marked `foreign`; entries without a proven marker are `unknown`. Such directories, files, and links are reported but never deleted by the project; symlink targets are not followed. Access errors and skipped mount points make the corresponding inventory incomplete, in which case the total is merely a lower bound on visible usage. Failure to enumerate an external cache stops execution rather than allowing the collector to use an incomplete list.
- The owner's UID is used for classification, but its numeric value is not exposed in reports. Temporary path components such as `anki-decks-<UID>` are rendered as `anki-decks-<uid>`.
- `systemd-tmpfiles-clean.timer` is shown for diagnostic purposes. System-wide cleanup of unrelated temporary data remains the system's responsibility.

## Policy

The only tracked source of default values is [`policy.toml`](policy.toml):

| Item | Value | Action |
|---|---:|---|
| Warning threshold for the main Cargo `target` | 12 GiB | Warning only. |
| Hard limit for the main Cargo `target` | 20 GiB | A `dry-run` proposes a full `cargo clean`; `clean --apply` executes it after revalidation. |
| Hard limit for ownership-marked external caches | 10 GiB | Clean the oldest eligible caches until their total size is at most 5 GiB or no candidates remain. |
| Minimum age of an external cache | 7 days | Skip newer caches. |
| Orphaned `TempWorkspace` | Existing owner's policy | Schema 1 requires a TTL of at least 24 hours; schema 2 uses evidence of process identity and current references. |
| Number of largest entries shown per inventory root | 20 | `--detail` shows every top-level entry. |

`--policy FILE` overrides the TOML values. For test scenarios, thresholds can be overridden in bytes and seconds—for example, `target_hard_limit_bytes=2`—so cleanup decisions can be tested on small files without allocating gigabytes.

## Initial setup and self-diagnostics

On the first interactive execution of a suitable command (for example, `anki-repo inspect EXPORT`), the main user-facing CLI recommends enabling automatic maintenance, displays the measured size of *verifiably* managed artifacts, and asks `[y/N]`. Both acceptance and refusal are saved, while the actual activity of the user's systemd timer is checked separately. Noninteractive runs do not prompt.

```bash
anki-repo maintenance status
anki-repo --json maintenance status | jq '.result | {preference, automatic, measurement, diagnostics}'
anki-repo maintenance enable
anki-repo maintenance disable
```

`maintenance status` uses the same diagnostic object for both text and JSON output. GC policy defines the effective boundaries for `target`, registered external caches, and `TempWorkspace`. The overall size of `/tmp` is never misrepresented as the size of the Anki-decks cache. `measurement.complete` applies to the **entire** inventory, including unrelated system directories under `/tmp`. The `measurement.system_tmp` summary separately tracks errors and mount boundaries in this system-wide inventory. For example, an inaccessible `systemd-private-…` directory and a separate mount at `/tmp/.X11-unix` produce `system_tmp_inventory_incomplete` (an `info` diagnostic), **not** `gc_operation_error`: accessible unrelated directories may be scanned but are not deleted, and mount boundaries are not crossed.

The size of verified project-owned artifacts is reported separately; an incomplete inventory of unrelated `/tmp` entries does not prove that `target` was inadequately checked. `measurement.managed_complete` separately confirms the completeness of managed-object checks: no fatal errors, verified ownership of `target`, no unknown entries in external caches, and verified project temporary objects. Deferring cleanup for a live owner does not make a measurement erroneous. `measurement.fatal_errors` counts only GC failures; the system-wide inventory is represented in `measurement.system_tmp`. The final `scan` continues to report all detected errors and `complete=false` rather than hiding them.

With `clean --apply`, an incomplete inventory of unrelated `/tmp` can leave the latest snapshot with outcome `partial`, but does not trigger a graphical cleanup-error notification. Failure notifications are sent when `fatal_errors > 0`. `last_attempt` includes optional `mode`, `fatal_errors`, and `managed_inventory_complete`; `inventory_complete` continues to refer to completeness of the entire inventory. `outcome=partial` indicates an incomplete inventory, including unrelated `/tmp`, and does not in itself mean cleanup failed. Older snapshots are read with the new fields treated as unknown (`null`). The format retains `schema_version=1`. Older binaries that strictly reject unknown fields cannot read the extended snapshot and report `last_attempt_unavailable`; after rolling back the binary, another run of the older GC version can supply a new latest result.

In a failure snapshot, `message` contains only the first 2,000 Unicode characters of the error; the original GC error is returned in full. This bounds the size of the summary record, including parser messages containing long configuration lines.

Unknown or incomplete measurements are explicitly labeled. JSON retains the standard `schema_version, command, result` envelope; inside `result` are `schema_version`, `preference`, `automatic`, `installed_workspace`, `measurement`, `last_attempt`, and `diagnostics` with stable `code` and `severity` values. Automatic startup and current execution are represented separately by `timer_enabled`, `timer_active`, and `service_active`. `automatic=enabled` means the verified timer is enabled for startup, is currently active, or its service is still running. Thus, `timer_enabled=false, timer_active=true` is not presented as disabled. With preference `yes`, both relevant conditions must be `true` for fully enabled maintenance; with `no`, any actual activity leads to `preference_mismatch`. `timer status` and `maintenance status` use the same source of facts. Unknown `systemctl` responses yield `unknown`, and unavailable user-systemd yields `unavailable`; these conditions do not allow a successful refusal (`no`) to be saved.

Installation ownership is established from private configuration, exact managed unit templates on disk, and loaded unit properties: `LoadState`, `FragmentPath`, `ExecStart`, `DropInPaths`, and `NeedDaemonReload`. Replaced or outdated units, unknown drop-ins, and unsafe links or permissions prevent changes to the installation. A missing installed binary can be restored by rerunning `enable` if the remaining evidence matches. An ownership marker alone, without a verified checkout, is not enough to permit `maintenance disable`; the files are preserved for diagnosis.

Both `enable` and `disable` hold the shared lock from verification until the preference is saved. Preference `yes` is deliberately retained if installation fails; the error is returned separately and a subsequent `enable` repairs the installation. Disabling verifies that **both** timer and service have stopped before deleting files; `no` is saved only after verifying that neither autostart nor activity remains. Diagnostic commands include a shell-escaped `--workspace-root` for the current checkout; suggested `clean` commands are previews without `--apply`. Text output shows “да/нет/неизвестно” (yes/no/unknown), the age of the last attempt, and Russian names for the mode and result; JSON retains machine-readable values.

External JSON masks the UID only in path components matching `anki-decks-<uid>`, including checkout paths and the latest-attempt path. The private snapshot retains exact paths for matching to the current checkout. Suggested executable commands include the exact path so they can be run. When a path contains non-UTF-8 bytes, diagnostics use a lossy string representation and no path-bearing suggested command is emitted (`action: null`). Private snapshots do not replace such paths: serialization fails and the prior file is preserved.

User preference is atomically stored in `${XDG_CONFIG_HOME:-~/.config}/anki-decks/gc-preference.json`. Separate timestamps track the last background size check and the last displayed warning: the daily check interval is consumed by performing the check itself, so refusal does not cause a full scan on every invocation, even before thresholds are exceeded. A compact snapshot of the latest `clean --apply` attempt is written to `${XDG_STATE_HOME:-~/.local/state}/anki-decks/gc-last-attempt.json`. That result is bound to a workspace: a record from another checkout is not presented as the current checkout's latest attempt. Declining the prompt does not cause the introductory question to be asked again. The threshold warning in text output is limited to once per day; `maintenance status` exposes it without a frequency limit.

When `systemd --user` is disabled, absent, or unavailable, its state is never presented as enabled. If another checkout already owns the timer, `enable` does not reassign it: the previous installation must first be explicitly disabled. `enable` requires the `anki-repository-maintenance` binary alongside `anki-repo` or in `PATH`; a normal `cargo build --release --workspace` creates both binaries side by side. `anki-repo` itself is not used as the installed GC binary.

After cleanup is applied, the latest attempt is saved, including errors and incomplete inventories. On a significant GC failure, the utility attempts an optional notification: on regular Linux, via D-Bus and `notify-send`; on WSL2, via `powershell.exe` and the user's already-installed Windows `BurntToast` module to the native Windows notification center. **GC does not install modules.** On WSL2, it first looks for `powershell.exe` in `PATH`, then under `[automount] root` from `/etc/wsl.conf` (default `/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe`). Unavailable Windows interoperability, missing BurntToast, an absent graphical session, or notification delivery failure do not interrupt cleanup; results remain available in the systemd journal, `maintenance status`, and the snapshot. A handler exit code of 0 does not prove that the user saw the popup.

A one-off test of notification delivery **without running cleanup**:

```bash
anki-repository-maintenance notify-test
```

To test specifically within the user systemd context, run the built binary in a transient user unit. From the workspace root, after `cargo build --locked -p repository-maintenance`:

```bash
systemd-run --user --wait --collect --pipe \
  "$(pwd)/target/debug/anki-repository-maintenance" notify-test
```

If the user has set `CARGO_TARGET_DIR`, resolve the binary's actual path from the build configuration. The command does not activate GC, delete data, install a timer, or modify systemd configuration. `WSL_INTEROP` may be absent from WSL user services; it is not needed to identify the platform, but the ability to launch Windows processes from a user unit must be verified with an actual test. Toast delivery depends on Windows notification policy and the availability of the user's session; WSL does not need to start Dunst automatically.

This is neither a monitoring daemon nor a separate database.

## Commands

Build the binary from the workspace root. For manually running `clean --apply`, use a location outside the build directory being cleaned: the tool recognizes its own process using PID and start time and does not block cleanup, but `cargo clean` will delete the executable file if it resides inside the affected `target`. This example builds the binary in a separate directory:

```bash
MANUAL_BUILD_ROOT="${XDG_CACHE_HOME:-$HOME/.cache}/repository-maintenance-manual-build"
CARGO_TARGET_DIR="$MANUAL_BUILD_ROOT/target" CARGO_BUILD_BUILD_DIR="$MANUAL_BUILD_ROOT/build" cargo build --locked --release -p repository-maintenance
MAINT="$MANUAL_BUILD_ROOT/target/release/anki-repository-maintenance"
```

These environment variables apply only to the build command. If they are already exported in your current shell, unset them for the maintenance invocation so the utility can discover workspace directories through Cargo:

```bash
env -u CARGO_TARGET_DIR -u CARGO_BUILD_TARGET_DIR -u CARGO_BUILD_BUILD_DIR "$MAINT" clean --apply --json
```

Read-only inventory and stable JSON output:

```bash
"$MAINT" scan
"$MAINT" scan --json
"$MAINT" scan --detail --json
```

Cleanup plan and explicit application:

```bash
"$MAINT" clean --json
"$MAINT" clean --apply --json
"$MAINT" clean --cleanup-legacy --json
"$MAINT" clean --cleanup-legacy --apply --json
```

Without `--apply`, `clean` is a `dry-run`: the report lists the proposed action for the measured `target` directory without running Cargo cleanup. This preserves a strict no-write guarantee: even `cargo clean --dry-run` creates `.rustc_info.json` in the target directory. Recognized legacy directories without an ownership marker are preserved by default and marked `ownership: unknown`. The `--cleanup-legacy` flag includes them in the plan; they can be removed only with `--cleanup-legacy --apply`. The user systemd timer does not supply this option.

When cleanup is applied, a normal `cargo clean` invocation uses the precisely determined paths. `clean --apply` acquires the shared maintenance lock without waiting and, immediately before cleaning, revalidates size, inode, mount points, inventory errors, ownership markers, and processes. If a managed build or another cleanup holds the lock, the command fails immediately rather than waiting. The lock directory is `<XDG_CACHE_HOME>/anki-decks/maintenance-lock` or `~/.cache/anki-decks/maintenance-lock`, outside the external build-cache tree; `clean --apply` requires either `XDG_CACHE_HOME` or `HOME`. If remeasurement places usage below the hard limit, no cleanup is performed. Rerunning when `target` is absent reports 0 bytes freed.

Create and use a reusable external cache:

```bash
"$MAINT" cache init --id local-check
"$MAINT" cache run --id local-check -- cargo test --workspace --locked
"$MAINT" cache run --id local-check --json -- cargo test --workspace --locked
```

`cache run` requires a direct Cargo invocation; shell wrappers and `env` wrappers are unsupported. An executable named `cargo` is allowed, including `cargo +stable`. Only the built-in build-related subcommands `build`, `check`, `test`, `bench`, `doc`, and `run` are supported. Cargo aliases and external subcommands are rejected so their configuration cannot silently redirect build output. The command disallows `--target-dir` and `--target-dir=...`, as well as `-C`, which changes the current directory and Cargo configuration lookup. `--config` is disallowed when its value or TOML file changes `build.target-dir` or `build.build-dir`; other arguments to the allowed subcommands remain available. Arguments after Cargo's `--` separator are passed to the invoked program and are not inspected as Cargo settings. The command sets both build directories to the registered `target` through the environment and appends overriding Cargo configuration entries after user options, so even nested `include` files cannot redirect the build outside the managed directory.

Before starting the child command, `cache run` validates the cache marker and updates its last-use time. It updates the marker again after Cargo finishes. It holds the same exclusive maintenance lock as `clean --apply` for the entire lifetime of the child process. These commands fail immediately if the lock is taken: the newly invoked command reports an error and exits with code 2 instead of waiting. As a result, `clean --apply` cannot begin cleanup while a managed `cache run` is active.

With `--json`, the wrapper writes exactly one JSON document to stdout. The child Cargo process's stdout is streamed into stderr, while its stderr remains on stderr; build output is not buffered in memory. Outside JSON mode, Cargo streams are inherited normally. When Cargo starts successfully, the `exit_code` field and the wrapper's exit status preserve Cargo's exit code; if Cargo is terminated by a signal, the result is `128 + signal number`. Validation, launch, or wait failures, and a failed marker update after Cargo succeeds, return code 2. If Cargo itself exits nonzero, the wrapper returns its code.

A cache becomes eligible for collection only when the shared hard limit is exceeded and it is at least seven days old.

Optional daily user systemd timer:

```bash
"$MAINT" timer install
"$MAINT" timer status
"$MAINT" timer uninstall
```

The installer locates the Cargo executable in the installation process's `PATH`, verifies it with `cargo --version`, and prepends its directory to the service's `PATH`, followed by standard system directories. Thus the service does not depend on the user's interactive-shell `PATH`. The installer copies the executable to `~/.local/bin`, writes unit files using `%h` instead of a machine-specific path, and records the workspace root in user installation configuration. The timer runs `clean --apply` once per day; this is the only automatic way to apply changes. `timer uninstall` stops the timer and removes only its own unit files, installation configuration, and executable. Manual commands continue to work when the user systemd manager is unavailable.

Every mode accepts `--workspace-root`. `XDG_CACHE_HOME`, `--policy`, and `--temp-root` allow cache directories, thresholds, and `TempWorkspace` collection to be isolated for testing. `--temp-root` specifies both the root for cleaning project temporary directories and an inventory root. `std::env::temp_dir()` is scanned separately only if its canonical path differs from `--temp-root`.

A completed report returns code 0 when `fatal_errors` is zero, or code 1 when fatal maintenance errors occurred. `errors` includes both fatal errors and temporary-directory inventory errors. For the `std::env::temp_dir()` entry, `complete: false` denotes an incomplete inventory; `errors` counts read errors, while mount-point boundaries are shown separately. Incomplete inventory and its errors do not increase `fatal_errors`; without other fatal errors, the command returns 0. Failures that prevent construction of a report return code 2; with `--json`, stdout contains a JSON object with `schema_version` and `error`.

The CLI uses `run_and_record`: cleanup, repeat measurements, and recording of the latest attempt take place under one shared lock. The latest attempt is the most recent completed run admitted to GC by this lock; a competing invocation that fails to acquire it does not overwrite the snapshot. The lock is released before any notification. Deferral because the lock is busy (`RunFailure::Deferred`) does not trigger a failure notification; other lock-acquisition and execution errors (`RunFailure::Failed`) remain errors. JSON still represents the error as a string, and the refusal exit code remains 2. Public `run`, `record_report`, and `record_failure` APIs remain available, but calling them separately does not provide this atomicity; use `run_and_record` for that.

Failure to save the latest `clean --apply` result does not change the exit code: cleanup has already completed, and the failure is reported in stderr and the timer journal. In that case, `maintenance status` displays the previous saved result rather than the current run. An incomplete measurement of Cargo `target` or its `build-dir` (read errors or nested mount points) is considered a fatal error at any visible size: partial data cannot establish that the hard limit has not been exceeded.

## `busy`, `deferred`, and limitations

`busy` indicates a detected related build/test process or a live `TempWorkspace` owner; for `target`, this results in safe deferral without fatal status. `deferred` means that the minimum age has not elapsed or a repeated check found a busy path. `error` means that ownership is unverified, the state of a related process is unknown, or a marker, read, or cleanup failure occurred. If the hard limit has been exceeded, these errors result in nonzero status and prohibit deletion. None of these reasons permits deletion to proceed.

A process that disappears during a `/proc` scan (`ENOENT` or `ESRCH` while reading its identity) is treated as absent. Permission errors, I/O failures, and corrupt data leave the process state unknown and prevent cleanup. Rechecking PID and start time remains mandatory.

Process detection uses available `/proc` details: current working directory, executable, process identity, parent chain, and open file descriptors belonging to workspace processes. A short race window remains between the final check and invoking `cargo clean` for Cargo processes not using this tool: Cargo does not offer a lock protecting an entire `target` directory from a new concurrent build. If an already running related process is detected, cleanup is deferred; the timer itself never starts a build.

A full scan of each inventory root may encounter root-owned or systemd-protected directories. The report classifies them as `foreign` or `error`. Their contents may be inaccessible, so JSON marks the inventory as `complete: false` and increments `errors`. The project does not replace the system's `systemd-tmpfiles` and never deletes unknown evidence ZIP archives, even if their names contain `anki-decks`.

Both optional notification handlers have a five-second timeout. On timeout, the child process is terminated and reaped using `wait`; its streams are closed. A handler failure or timeout does not alter GC's exit status or its already-saved snapshot; `notify-test` instead returns a diagnosable error. Windows Toast notifications continue to use PowerShell/BurntToast: first search for `powershell.exe` in `PATH`, then under `[automount] root` from `/etc/wsl.conf` (default `/mnt/c/Windows/...`). For another Windows drive or disabled automount, configure the service's `PATH`. Handler exit code 0 confirms acceptance of the request, not that the popup was visible.
