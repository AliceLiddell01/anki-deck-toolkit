# 磁盘空间维护与清理

[Русский](README.md) · [English](README.en.md) · **简体中文**

<!-- i18n-source-git-blob: 2630521deeff475a4b9958ebae7ebbc40e95ebd7 -->

`repository-maintenance` 是一个独立的 Cargo 软件包，用于监测本地产物，并安全清理能够重新生成的数据。它与 `anki-repo` 分开实现，因为它不处理 CrowdAnki 导出数据，而是负责磁盘空间的生命周期管理。

## 审计结果

在当前 `HEAD` 中，相关数据来源及处理策略如下：

| 范围 | 实际来源 | 处理策略 |
|---|---|---|
| Cargo 工作区 | 只有一个根工作区；常规 `target` 目录通过 `cargo metadata` 确定。构建与测试配置文件（profile）在根目录 `Cargo.toml` 中声明。 | 统计工作区内的 `target` 和已配置的 Cargo `build-dir`；仅通过 `cargo clean` 执行清理。 |
| 嵌套构建 | `anki-repo` 的 `code-review` 检查会执行 Cargo Clippy；合成 Cargo 测试场景会把 `CARGO_TARGET_DIR` 设置在各自的 `TempWorkspace` 内。 | 由临时目录的所有者或现有的孤立目录垃圾回收器负责删除。 |
| 进程、浏览器及测试的临时数据 | `asset-store` 创建带有私有所有权标记的 `TempWorkspace`；浏览器在该目录树内使用独立配置目录和 `TMPDIR`。 | 沿用 `asset-store` 对进程身份、PID 启动时间、进程引用、`NOFOLLOW` 和挂载点边界的处理。此工具调用相同的公共垃圾回收逻辑。 |
| 证据 ZIP 压缩包 | `tools/asset-store/src/bin/common/evidence_zip.rs` 将压缩包写入 `<temp_dir>/anki-decks-evidence`，或写入 `--output` 指定的位置。 | ZIP 保存证据，因此新的垃圾回收器只会将其列入清单。默认不会删除没有可验证所有权标记的目录或文件。已识别、但缺少标记的旧目录，只有在显式执行 `clean --cleanup-legacy --apply` 后才可以删除。如果显式指定的 `--output` 路径不在扫描根目录内，则不纳入清单。 |
| 持久化存储和事务性 `.tmp` 文件 | `asset-store` 在自行管理的目录中保存数据；临时文件名用于原子操作以及未完成状态转换的恢复。 | 这些属于业务领域状态，而不是通用 Cargo 垃圾，磁盘清理不会触及。 |
| 持久化的外部 Cargo 构建目录 | 当前源代码中没有专门长期运行、并在 `~/.cache` 下创建 Cargo 构建目录的进程。某次本地故障中发现的任意目录，并不能证明其归本项目所有。 | 对明确需要复用的缓存，提供了 `cache init` 和 `cache run`：它们会创建标记、更新最后使用时间，并受总容量上限约束。未知目录不会清理。 |

本工具不修改 Cargo 构建配置（profile）。一次性构建始终限制在 `TempWorkspace` 内，不会另外创建持久化的外部测试构建目录。

## 所有权边界

- 主 `target` 目录根据 `cargo metadata` 的实际输出确定。只有当 `target` 以及所有相关构建目录都位于当前工作区内、目录树不存在错误或挂载点边界、存在标准的 `CACHEDIR.TAG`、进程检查未发现正在使用的路径时，才允许清理。指向外部共享目录的 `target` 会显示在报告中，但不会自动清理；如果超过硬性容量上限，报告会将所有权未经验证列为错误并返回非零状态码。
- 删除前会检查构建目录的边界。即使存在有效的 `CACHEDIR.TAG`，也不会把工作区根目录、源码目录树或相互重叠的目录交给 `cargo clean`。普通 `target` 不存在时，仍可安全清理单独且已验证的 `build-dir`；如果目录在制定计划后才出现，则必须重新检查。
- 单独配置的 `build-dir` 与 `target` 通过一次 `cargo clean` 调用同时清理，因此两者均需事先通过标准 `CACHEDIR.TAG` 验证。如果主 `target` 不存在，就将现有且已验证的 `build-dir` 同时作为两个路径传入，避免触及新出现但未经验证的 `target`。没有标记的目录不会清理。只有确定需要清理（`ThresholdDecision::Clean`）时才要求验证标记；此时缺少标记会产生 `error` 并保留数据。对于 `keep` 或 `warn`，不计划清理，也不要求存在标记。
- 可复用的外部缓存只能由 `cache init` 创建。其标记证明规范化后的工作区根目录、缓存标识符和相对 `target` 路径。目录必须属于当前 UID，且不允许所属组或其他用户访问。`cache run` 将 Cargo 的 `target` 和 `build-dir` 指向该缓存，并在命令执行前后分别更新最后使用时间。垃圾回收器只考虑这种缓存。
- `--temp-root` 和 `std::env::temp_dir()` 均会完整扫描；规范化后相同的根目录只扫描一次。报告包括已分配磁盘块的总大小、顶层条目数和最多 N 个最大的条目。稀疏文件按实际分配的磁盘块计量；硬链接在每个扫描根目录内只计一次。不会跟随符号链接，也不会跨越其他挂载点边界。
- 清理 `TempWorkspace` 时，现有垃圾回收器会核实所有权。已经确认仍存活的所有者、无法确认的 PID 进程引用、损坏的标记或验证失败，都会导致 `deferred`/`error`，而不是尝试删除。
- 其他 UID 所有的条目标记为 `foreign`，没有可证实所有权标记的条目标记为 `unknown`。这些目录、文件和链接会显示在报告中，但项目绝不会删除它们，也不会跟随符号链接目标。访问错误和被跳过的挂载点会导致对应的盘点不完整，因此总量只代表可见占用的下界。若外部缓存的枚举失败，程序将停止执行，避免垃圾回收器使用不完整的列表。
- 所有者 UID 用于分类，但报告不会暴露其具体数值。形如 `anki-decks-<UID>` 的临时路径片段会显示为 `anki-decks-<uid>`。
- `systemd-tmpfiles-clean.timer` 仅用于诊断展示。与项目无关的临时数据仍由系统本身负责全局清理。

## 清理策略

默认参数的唯一受 Git 跟踪的来源是 [`policy.toml`](policy.toml)：

| 项目 | 数值 | 操作 |
|---|---:|---|
| 主 Cargo `target` 的警告阈值 | 12 GiB | 仅提示警告。 |
| 主 Cargo `target` 的硬性上限 | 20 GiB | `dry-run` 提议完整执行 `cargo clean`；`clean --apply` 重新验证后才实际执行。 |
| 带所有权标记的外部缓存硬性总上限 | 10 GiB | 按时间从最旧的合格缓存开始清理，直到总量不超过 5 GiB，或已无可清理项。 |
| 外部缓存的最低存放时间 | 7 天 | 跳过更新的缓存。 |
| 无主 `TempWorkspace` | 现有所有者的策略 | Schema 1 要求 TTL 至少为 24 小时；schema 2 使用进程身份及当前引用的证据。 |
| 每个盘点根目录展示的最大条目数 | 20 | `--detail` 展示全部顶层条目。 |

`--policy FILE` 可以覆盖 TOML 中的参数。测试场景允许按字节和秒覆盖阈值，例如 `target_hard_limit_bytes=2`；因此无需占用数 GiB 磁盘空间，就能通过小文件验证清理决策。

## 初次设置与自诊断

用户首次在交互式终端执行符合条件的主 CLI 命令（例如 `anki-repo inspect EXPORT`）时，程序会建议启用自动维护，显示经**确切验证为项目管理范围内**的资源所占空间，并询问 `[y/N]`。接受和拒绝都会保存；用户 systemd 定时器的实际运行状态则独立验证。非交互式运行不会询问。

```bash
anki-repo maintenance status
anki-repo --json maintenance status | jq '.result | {preference, automatic, measurement, diagnostics}'
anki-repo maintenance enable
anki-repo maintenance disable
```

`maintenance status` 的文本与 JSON 输出使用相同的诊断对象。GC 策略规定了 `target`、已注册外部缓存和 `TempWorkspace` 的有效管理范围。绝不会把整个 `/tmp` 的大小误报为 Anki-decks 缓存大小。`measurement.complete` 反映的是**整个**盘点过程的完整性，包括 `/tmp` 下属于其他程序的系统目录。`measurement.system_tmp` 单独汇总这一系统级盘点中的错误数量和挂载点边界。例如，无法访问的 `systemd-private-…` 目录，以及 `/tmp/.X11-unix` 下的独立挂载点，会产生级别为 `info` 的 `system_tmp_inventory_incomplete` 诊断，而**不会**产生 `gc_operation_error`：项目可以查看可访问的外部目录，但不会删除它们，也不会跨越挂载点边界。

经过验证的项目管理资源大小单独显示；外部 `/tmp` 的不完整盘点不能证明 `target` 的检查不完整。`measurement.managed_complete` 独立表明受管理对象的检查是否完整：不存在致命错误，`target` 所有权已验证，外部缓存不包含未知条目，项目临时对象已核实。由于所有者仍在运行而延迟清理，不意味着测量出错。`measurement.fatal_errors` 仅统计 GC 错误；系统级盘点的状态记录在 `measurement.system_tmp`。最终的 `scan` 仍然会报告所有发现的错误并标记 `complete=false`，而不是将其隐藏。

执行 `clean --apply` 时，如果与项目无关的 `/tmp` 盘点不完整，最新快照的 `outcome` 可能仍为 `partial`，但不会因此发送清理失败的图形通知。只有当 `fatal_errors > 0` 时才发送失败通知。`last_attempt` 新增可选字段 `mode`、`fatal_errors` 和 `managed_inventory_complete`；`inventory_complete` 仍表示整个盘点的完整性。`outcome=partial` 表示包括外部 `/tmp` 在内的盘点不完整，本身并不代表清理失败。读取旧快照时，新增字段按未知值（`null`）处理。文件格式继续使用 `schema_version=1`；严格拒绝未知字段的旧版本二进制程序无法读取扩展后的快照，会显示 `last_attempt_unavailable`。如果回退到旧程序版本，可以再次运行旧版 GC，以生成新的最近一次运行结果。

失败快照的 `message` 仅保存错误消息前 2000 个 Unicode 字符；原始 GC 错误仍会完整返回。这限制了概要记录的大小，包括解析器因配置中存在过长行而生成的错误消息。

未知或不完整的测量结果会明确标记。JSON 保持标准的 `schema_version, command, result` 外层结构；`result` 包含 `schema_version`、`preference`、`automatic`、`installed_workspace`、`measurement`、`last_attempt` 和 `diagnostics`，其中的 `code` 与 `severity` 是稳定值。自动启动和当前运行情况通过 `timer_enabled`、`timer_active` 和 `service_active` 分别表示。`automatic=enabled` 表示经过验证的定时器已启用自动启动、当前处于活动状态，或其服务仍在运行。因此，`timer_enabled=false, timer_active=true` 不会被误判为已禁用。若用户偏好是 `yes`，完整启用维护要求这两项相关事实均为 `true`；若用户偏好为 `no`，任何正在生效的状态都会导致 `preference_mismatch`。`timer status` 与 `maintenance status` 使用同一事实来源。`systemctl` 的未知响应对应 `unknown`；用户 systemd 不可用则对应 `unavailable`。这些状态不允许成功保存拒绝偏好 `no`。

安装归属需通过私有配置、磁盘上准确匹配的受管理 unit 模板，以及已加载 unit 的属性共同验证：`LoadState`、`FragmentPath`、`ExecStart`、`DropInPaths`、`NeedDaemonReload`。被替换或过期的 unit、未知的 drop-in，以及不安全的链接或权限都会阻止对安装进行修改。如果其余证据均匹配，可以再次执行 `enable` 来恢复缺失的已安装二进制文件。仅凭所有权标记、但没有经过验证的工作副本，不足以允许执行 `maintenance disable`；文件会保留用于诊断。

`enable` 与 `disable` 均在验证开始到偏好保存完毕期间持有同一把共享锁。如果安装失败，程序会有意保留 `yes` 偏好、单独返回错误；再次运行 `enable` 可以修复安装。禁用操作在删除文件前，必须验证定时器**和**服务都已停止；只有确认自动启动和活动状态均不存在，才保存 `no`。诊断命令包含对当前工作副本路径正确进行 shell 转义的 `--workspace-root`；建议的 `clean` 命令均不带 `--apply`，只是预览。文本输出使用俄语的“да/нет/неизвестно”（是/否/未知）、上次运行距今的时间，以及俄语的模式和结果名称；JSON 仍保留机器可读值。

公开的 JSON 仅在形如 `anki-decks-<uid>` 的路径片段中隐藏 UID，包括工作副本路径和最近一次运行记录路径。私有快照保存完整、准确的路径，供与当前工作副本比对。建议执行的命令则包含准确路径，以便直接运行。如果路径包含非 UTF-8 字节，诊断信息会使用替换无效字节后的字符串表示，并且不会生成包含该路径的操作建议（`action: null`）。私有快照不会进行此类替换：序列化失败会向调用方返回错误，并保留原文件。

用户选择通过原子写入保存在 `${XDG_CONFIG_HOME:-~/.config}/anki-decks/gc-preference.json`。上一次后台容量检查和上一次显示警告的时间分别记录：每天的检查间隔以执行检查本身为准。因此，即使用户拒绝启用维护，在容量尚未超出阈值时也不会每次调用都重复完整扫描。最近一次 `clean --apply` 的概要状态保存在 `${XDG_STATE_HOME:-~/.local/state}/anki-decks/gc-last-attempt.json`。该记录绑定工作区：其他工作副本的记录不会被当作当前工作副本的最近一次运行结果。拒绝启用维护不会导致启动提示反复出现。文本版阈值警告每天最多显示一次；`maintenance status` 则不限制查看次数。

当 `systemd --user` 已关闭、不存在或不可用时，不会将状态伪装为已启用。如果定时器已属于另一份工作副本，`enable` 不会自动重新绑定，必须先显式禁用之前的安装。`enable` 要求 `anki-repository-maintenance` 二进制文件位于 `anki-repo` 旁边或 `PATH` 中；常规 `cargo build --release --workspace` 会在同一目录下生成这两个程序。不会将 `anki-repo` 本身作为已安装的 GC 程序使用。

应用清理后，程序保存最近一次尝试的结果，包括错误和不完整的盘点。如果 GC 出现严重故障，会尝试发送**可选**通知：在普通 Linux 中，通过 D-Bus 和 `notify-send`；在 WSL2 中，通过 `powershell.exe` 和用户 Windows 环境中**已经安装**的 `BurntToast` 模块，发送到原生 Windows 通知中心。GC 过程中**不会安装任何模块**。在 WSL2 中，程序首先在 `PATH` 中查找 `powershell.exe`，然后查找 `/etc/wsl.conf` 中 `[automount] root` 对应的路径（默认 `/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe`）。即使 Windows 互操作不可用、未安装 BurntToast、缺少图形会话或通知投递失败，也不会中断清理；结果仍可通过 systemd 日志、`maintenance status` 和快照查看。通知处理程序返回退出码 0，并不证明用户实际看到了弹窗。

**不执行清理**、仅测试一次通知发送：

```bash
anki-repository-maintenance notify-test
```

如需专门验证用户 systemd 上下文中的运行情况，请在一次性 user unit 中执行构建好的程序。在工作区根目录执行 `cargo build --locked -p repository-maintenance` 之后，可运行：

```bash
systemd-run --user --wait --collect --pipe \
  "$(pwd)/target/debug/anki-repository-maintenance" notify-test
```

如果用户设置了 `CARGO_TARGET_DIR`，需要根据实际构建配置确定二进制文件路径。此命令不会激活 GC、删除数据、安装定时器或修改 systemd 配置。在 WSL 的用户服务中，可能没有 `WSL_INTEROP` 环境变量；识别平台并不依赖它，但必须通过实际测试才能确认 user unit 是否能够启动 Windows 进程。Toast 投递受 Windows 通知策略及用户会话可用性的影响；无需在 WSL 中自动启动 Dunst。

这不是持续监控服务，也不是独立数据库。

## 命令

从工作区根目录构建二进制文件。手动执行 `clean --apply` 时，应将程序放在待清理的构建目录之外：程序会根据 PID 和启动时间识别自身，不会因为自身进程而阻止清理；但如果可执行文件位于要清理的 `target` 内，`cargo clean` 会将其删除。下面的示例将程序构建到单独的目录：

```bash
MANUAL_BUILD_ROOT="${XDG_CACHE_HOME:-$HOME/.cache}/repository-maintenance-manual-build"
CARGO_TARGET_DIR="$MANUAL_BUILD_ROOT/target" CARGO_BUILD_BUILD_DIR="$MANUAL_BUILD_ROOT/build" cargo build --locked --release -p repository-maintenance
MAINT="$MANUAL_BUILD_ROOT/target/release/anki-repository-maintenance"
```

这些环境变量仅在构建命令执行时设置。如果它们已经在当前 shell 中导出，调用维护工具时应从环境中移除，以便程序通过 Cargo 正确确定工作区目录：

```bash
env -u CARGO_TARGET_DIR -u CARGO_BUILD_TARGET_DIR -u CARGO_BUILD_BUILD_DIR "$MAINT" clean --apply --json
```

只读盘点及稳定的 JSON 输出：

```bash
"$MAINT" scan
"$MAINT" scan --json
"$MAINT" scan --detail --json
```

预览清理计划并显式执行：

```bash
"$MAINT" clean --json
"$MAINT" clean --apply --json
"$MAINT" clean --cleanup-legacy --json
"$MAINT" clean --cleanup-legacy --apply --json
```

未传入 `--apply` 时，`clean` 处于 `dry-run` 模式：报告会展示针对已测量 `target` 目录计划执行的操作，但不会运行 Cargo 清理。这保证了严格的无写入行为：即使执行 `cargo clean --dry-run`，Cargo 本身也会在 target 目录中创建 `.rustc_info.json`。默认保留已经识别、但缺少所有权标记的旧目录，并将其标记为 `ownership: unknown`。传入 `--cleanup-legacy` 才会将这些目录纳入清理计划；实际删除必须同时使用 `--cleanup-legacy --apply`。用户 systemd 定时器不会传入此选项。

执行清理时，使用准确确定的路径调用常规 `cargo clean`。`clean --apply` 会以非阻塞方式获取共享维护锁，并在实际清理前重新核实大小、inode、挂载点、盘点错误、所有权标记及进程。如果已有受管理的构建或其他清理任务占用锁，命令会立即报错退出，不会等待锁释放。锁目录位于 `<XDG_CACHE_HOME>/anki-decks/maintenance-lock` 或 `~/.cache/anki-decks/maintenance-lock`，不属于外部构建缓存目录树；运行 `clean --apply` 要求至少设置 `XDG_CACHE_HOME` 或 `HOME`。如果重新测量后发现占用低于硬性上限，就不会执行清理。`target` 不存在时重新运行，会显示释放了 0 字节。

创建和使用可复用的外部缓存：

```bash
"$MAINT" cache init --id local-check
"$MAINT" cache run --id local-check -- cargo test --workspace --locked
"$MAINT" cache run --id local-check --json -- cargo test --workspace --locked
```

`cache run` 只接受直接调用 Cargo，不支持 shell 包装程序或 `env` 包装程序。可以指定名称为 `cargo` 的可执行文件，包括 `cargo +stable`。只支持内置的构建相关子命令 `build`、`check`、`test`、`bench`、`doc` 和 `run`。Cargo 别名以及外部子命令会被拒绝，避免其配置暗中重新指定构建目录。命令禁止使用 `--target-dir`、`--target-dir=...` 以及会改变当前目录和 Cargo 配置搜索路径的 `-C`。如果 `--config` 的值或其指向的 TOML 文件会修改 `build.target-dir` 或 `build.build-dir`，也会被拒绝；允许子命令的其他参数仍然可用。Cargo 分隔符 `--` 之后的参数将传给被执行的程序，不会被当作 Cargo 设置检查。此命令通过环境变量将两个构建目录指向已注册的 `target`，并在用户参数之后追加具有最终优先级的 Cargo 配置。因此，即使嵌套的 `include` 文件也无法将构建重定向到受管理目录之外。

启动子命令之前，`cache run` 会验证缓存标记并更新最后使用时间。Cargo 结束后会再次更新标记。子进程运行的整个期间，程序持续持有与 `clean --apply` 相同的独占维护锁。如果锁已被占用，这些命令会立即失败：新命令返回错误及退出码 2，不会等待。因此，受管理的 `cache run` 仍在执行时，`clean --apply` 不会开始清理。

指定 `--json` 时，包装程序在 stdout 上只输出一个 JSON 文档。子 Cargo 进程的 stdout 会以流式方式重定向到 stderr，原本的 stderr 仍写入 stderr；不会将构建输出全部缓存在内存中。非 JSON 模式下，Cargo 的标准流照常继承。Cargo 已成功启动时，`exit_code` 字段及包装程序的返回码保留 Cargo 的返回码；若 Cargo 因信号终止，则返回 `128 + 信号编号`。校验、启动或等待 Cargo 失败，以及 Cargo 成功后更新标记失败，均返回退出码 2。Cargo 自身返回非零退出码时，包装程序返回其退出码。

只有在外部缓存总容量超过硬性上限、且该缓存已达到七天的最低存放时间时，它才会成为垃圾回收候选项。

可选安装的每日用户 systemd 定时器：

```bash
"$MAINT" timer install
"$MAINT" timer status
"$MAINT" timer uninstall
```

安装程序从当前安装进程的 `PATH` 中查找 Cargo 可执行文件，通过 `cargo --version` 验证后，将所在目录放在服务 `PATH` 的最前面，再追加标准系统目录。因此，服务不依赖用户交互式 shell 的 `PATH`。安装程序把可执行文件复制到 `~/.local/bin`，编写使用 `%h`（而非机器专属路径）的 unit 文件，并在用户安装配置中保存工作区根目录。定时器每天执行一次 `clean --apply`；这是唯一会自动应用实际更改的路径。`timer uninstall` 会禁用定时器，只删除自己管理的 unit 文件、安装配置和二进制文件。用户 systemd 管理器不可用时，手动命令仍能使用。

所有模式均支持 `--workspace-root`。使用 `XDG_CACHE_HOME`、`--policy` 和 `--temp-root` 可以在测试环境中隔离缓存、阈值以及 `TempWorkspace` 垃圾回收范围。`--temp-root` 同时指定项目临时目录清理根目录和盘点目录。只有当 `std::env::temp_dir()` 的规范化路径与 `--temp-root` 不同时，才单独扫描该系统临时目录。

如果 `fatal_errors` 为零，已完成的报告返回退出码 0；若存在致命的维护操作错误，则返回退出码 1。`errors` 既包含致命错误，也包含临时目录盘点中的错误。对于 `std::env::temp_dir()` 条目，`complete: false` 表示盘点不完整；`errors` 统计读取错误，挂载点边界则单独显示。不完整盘点及其错误不会增加 `fatal_errors`；如果没有其他致命错误，命令仍返回 0。若错误导致报告无法生成，则返回退出码 2；使用 `--json` 时，stdout 包含带有 `schema_version` 和 `error` 字段的 JSON 对象。

CLI 使用 `run_and_record`，在同一把共享锁下完成清理、重新测量和最近一次尝试的记录。最近一次尝试是最后一个被这把锁允许进入 GC、且已经执行完毕的操作；竞争者如果获取锁失败，不会覆盖该快照。通知发送之前会释放锁。由于锁被占用而延迟执行（`RunFailure::Deferred`）不会触发失败通知；其他获取锁或运行错误（`RunFailure::Failed`）仍属于错误。JSON 中的错误仍表示为字符串，拒绝执行的退出码保持为 2。公开的 `run`、`record_report`、`record_failure` API 仍然保留，但分别调用它们无法保证上述原子性；需要原子性时必须使用 `run_and_record`。

如果保存最近一次 `clean --apply` 结果失败，不会改变返回码：此时清理已经完成，保存失败会在 stderr 和定时器日志中显示。`maintenance status` 在这种情况下显示之前保存的结果，而不是当前运行的结果。如果 Cargo `target` 或其 `build-dir` 的测量不完整（例如读取错误或存在嵌套挂载点），无论可见占用多小，都视为致命错误：不完整的数据无法证明未超过硬性上限。

## `busy`、`deferred` 及限制

`busy` 表示检测到相关构建/测试进程，或 `TempWorkspace` 的所有者仍然存活；对于 `target`，这意味着安全地延后清理，而非致命错误。`deferred` 表示最低存放时间尚未到期，或再次检查时发现路径仍被占用。`error` 表示所有权未得到验证、相关进程状态未知，或出现标记、读取、清理错误。超过硬性上限时，这些错误会导致非零状态码并禁止删除。无论出现哪种情况，都不能因此继续删除。

扫描 `/proc` 时，如果某进程恰好消失（读取身份时返回 `ENOENT` 或 `ESRCH`），就按进程不存在处理。访问错误、I/O 故障和损坏数据会保留未知状态并阻止清理；仍必须重新验证 PID 和启动时间。

进程检查使用 `/proc` 中可用的信息：当前工作目录、可执行文件、进程身份、父进程链，以及工作区进程已打开的文件描述符。在最终检查和调用 `cargo clean` 之间，未通过本工具协调的 Cargo 进程仍存在短暂的竞态窗口：Cargo 没有提供能够保护整个 `target` 目录、防止同时开始新构建的锁。发现已在运行的相关进程时，清理会延后；定时器本身不会启动构建。

完整扫描某个盘点根目录时，可能遇到 root 或 systemd 保护而无法访问的目录。报告会将其标记为 `foreign` 或 `error`；由于内容可能无法读取，JSON 将盘点标记为 `complete: false` 并增加 `errors`。本项目不会替代系统的 `systemd-tmpfiles`，也不会删除未知的证据 ZIP，即使文件名中包含 `anki-decks`。

两个可选通知处理程序的超时均为五秒。超时后，子进程会终止，并通过 `wait` 回收，同时关闭其标准流。处理程序失败或超时不会改变 GC 退出状态或已经保存的快照；`notify-test` 会返回可诊断的错误。Windows Toast 通知继续使用 PowerShell/BurntToast：首先在 `PATH` 中查找 `powershell.exe`，随后查找 `/etc/wsl.conf` 的 `[automount] root` 下的路径（默认 `/mnt/c/Windows/...`）。如果 Windows 系统盘不是默认盘符或已禁用 automount，应设置服务的 `PATH`。通知处理程序返回 0 仅表示请求已被接收；是否实际显示弹窗需要另外验证。
