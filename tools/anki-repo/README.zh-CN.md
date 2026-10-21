# anki-repo

[Русский](README.md) · [English](README.en.md) · **简体中文**

<!-- i18n-source-git-blob: cf4c051e39d649969d08fa819a37cdfe7c7332e2 -->

`anki-repo` 是一套用于确定性分析 CrowdAnki 导出数据、并对其内容实施严格受限修改的工具。它使智能体能够快速、可预测地了解 `deck.json` 的结构，而无需人工通读数 MB 的 JSON 文件。

`inspect`、`find`、`stats`、`validate`、`qa`、`review`、`review-check` 和 `models` 只读取导出数据。`visual-report` 读取导出数据，并且只写入新的独立报告目录。这些只读命令都不修改导出数据：交互式磁盘维护提示（参见“磁盘维护”）只会将用户偏好保存到导出目录之外的私有文件。独立的 `maintenance` 命令组完全不访问导出数据，而是查看自动清理状态并管理用户的 `systemd --user` 定时器。会修改导出数据的命令有：

- `edit` 修改已有笔记中已有字段的**值**；
- `create` 使用**已有笔记类型**向**已有牌组**添加笔记；按明确的资源域规则，可以放置经过验证的日语汉字及音高重音图片，并将文件名写入 `media_files`；
- `retire` 给已有笔记**追加停用标签**；
- `migrate-media` 将引用从旧文件名迁移到资源域当前的规范文件名。只有此命令可以在证明引用语义后修改已有笔记的媒体引用。

除了这四条命令，`migrate-layout --apply` 还可以迁移目录布局：将旧布局中的导出目录移动到扁平的 `decks/output/`，清理旧布局的占位文件，并删除内容逐字节相同的源目录。导出内容本身不变：`deck.json`、`media/` 和各项身份标识均逐字节迁移，每份导出在迁移前后都通过 SHA-256 证明内容一致。这是布局迁移，不是修改导出数据。

工具集不提供物理删除笔记的功能：应停用的笔记只会被打标签，实际删除由使用者在 Anki 中完成。任何命令都不会创建或删除牌组、笔记类型、字段及配置，不会在导出数据内部移动实体，也不会修改身份标识（`guid`、`note_model_uuid`、`crowdanki_uuid`、`deck_config_uuid`）。修改命令默认使用 dry-run，只有显式传入 `--apply` 才能写入。`create` 只能放置被特定资源域和字段规则授权的已验证文件。`migrate-media` 仅在迁移得到证实后放置规范文件并删除对应旧文件。工具集没有任意写入 `media/` 的接口；`migrate-layout` 会整体移动 `media/`，不改变其字节。

`qa`、`review`、`review-check` 组成外部卡片内容审阅流程：工具以确定性的方式发现问题，提供简明证据包，接收外部智能体的结构化建议，再生成可交给 `edit` 的请求。工具本身**不调用任何 LLM API**，也不进行网络请求；内容判断由外部智能体承担，`anki-repo` 负责守住仓库的确定性边界。

“创建并验证”流程中，`models` 提供实际字段结构及字段语义的证据，`create` 按该结构添加笔记，`retire` 标记过时笔记，`visual-report` 根据实际笔记类型模板生成两个导出状态之间的静态离线对比报告及卡片预览。对于需要迁移的引用，专用的 `migrate-media` 会先证明身份标识与旧文件名的匹配关系、盘点引用者，再仅修改已证实的引用。

## 用途

- 一次 CLI 调用即可代替手动运行 `jq` 或脚本：`inspect`、`find`、`stats`、`validate`、`qa`、`review`、`review-check`、`models`、`visual-report`、`edit`、`create`、`retire`、`migrate-media`、`migrate-layout`。
- 通过 `note_model_uuid` → `note_models[].flds[].ord` 解析字段名的逻辑集中于一处，智能体无需在读取、编辑或创建笔记时重复实现。
- `models` 不仅返回字段名，还提供证据：实际字段值、空值数量、已解析的模板结构，以及无法静态计算的结构列表。智能体不应仅凭字段名称猜测其语义。
- 定向编辑不会改动文件的无关部分：写入前检查前置条件，默认 dry-run，并对字节级差异进行验证。
- 安全创建笔记：新字段值默认不包含媒体引用；明确配置的资源域处理器仅接受所属域的已验证图片，并在 `--apply` 时放置。牌组和笔记类型通过身份标识寻址，候选数据会重新解析并验证。
- 不物理删除的停用操作：笔记仍然存在且可寻址，只修改标签。
- 离线可视化报告使用真实模板生成卡片预览，不只是用文字描述差异。
- 外部审阅流程无需通读巨大的 `deck.json`：按确定性规则选取、每页 25 条的简明数据包、对照实际导出数据验证智能体建议。
- 稳定的机器可读输出：相同数据产生相同 JSON。
- 默认人类可读输出，并为自动化提供独立的 JSON 模式。

## 运行要求

- **仅支持 Linux。** 经过验证的读取逻辑依赖仅支持 Linux 的 `asset-store` 库，该库无法在其他平台编译。
- Rust 工具链使用 2024 edition。最低支持版本（MSRV）只在根目录 `Cargo.toml` 的 `[workspace.package] rust-version` 声明一次。工作区成员继承设置（`edition.workspace = true`、`rust-version.workspace = true`），因此 `tools/anki-repo/Cargo.toml` 不重复声明版本。已使用稳定版 `rustc`/`cargo` 和声明的 MSRV 验证。
- 唯一的构建入口是根目录 Cargo 工作区（`/Cargo.toml`，包含成员 `tools/anki-repo`）；`Cargo.lock` 也位于仓库根目录。应从根目录构建与测试，而不是从软件包子目录执行。
- 不依赖外部服务和网络：处理导出数据仅使用本地文件，任何命令均不调用 LLM API 或其他外部服务。与系统服务的唯一交互来自 `maintenance` 命令组：用户明确执行命令或同意后，管理 `repository-maintenance` 的用户 `systemd --user` 定时器，并将偏好保存在私有文件中。它既不读取也不修改导出数据。

## 构建与运行

在仓库根目录运行：

```bash
cargo build --release --workspace
./target/release/anki-repo --help

# 或不安装
cargo run --quiet --bin anki-repo -- inspect decks/output/Words__N3
```

导出路径可以相对于当前目录，也可以是绝对路径。

### 磁盘维护

`anki-repo maintenance status` 展示真实的自动清理状态、受管理资源占用、测量完整性、最近一次执行情况及结构化诊断事件。`anki-repo --json maintenance status` 可在 `result` 字段中提供同样的信息，供智能体使用；查看状态不会删除任何内容。

首次在符合条件的交互式 CLI 中执行时，程序可能提示 `[y/N]`，询问是否启用用户定时器。肯定回答包括 `y`、`yes`、`д`、`да`；否定回答包括 `n`、`no`、`н`、`нет` 和空输入（`[y/N]` 默认值）。无法识别的回答不会保存，用户偏好仍未设置，之后会再次询问。程序测量的是导出文件所在的仓库：通过导出路径向上查找最近的 `.git` 来确定根目录，而不是使用进程当前目录，以免误测其他项目的 `target` 并安装每天清理它的定时器。显式设置的 `ANKI_REPOSITORY_ROOT` 优先于导出位置；导出不属于 Cargo 工作区时不会询问。拒绝后后台容量检查每天最多执行一次：间隔按实际检查计算，而非仅按显示警告的时间计算。在 `--json`、重定向标准流、CI（检查 `CI` 环境变量，而不单纯依据终端属性，因为部分 CI 提供伪终端）以及 `--help`/`--version` 模式中均不会弹出提示。若用户 systemd 管理器不可用，或定时器已启用但无法证明属于当前工作副本，也不会提示无法完成的操作。用户可显式启用或拒绝：

```bash
anki-repo maintenance status
anki-repo maintenance enable
anki-repo maintenance disable
```

在确认禁用操作确实成功之前，`maintenance disable` 不会宣称成功。如果不能证实定时器归属于当前工作副本，它会保留 unit 文件并返回 `write_failed`。对于已经验证的工作副本，自动启动、定时器活动状态以及服务停止状态都在同一把锁下检查；失败时不会保存 `no`。因此，不能在自动清理仍然运行时错误地将保存的 `no` 解释为已关闭。

策略和清理操作的权威来源是 `repository-maintenance`。参见[磁盘维护工具文档](../repository-maintenance/README.zh-CN.md)：`/tmp` 和其他用户的数据不计入受管理资源占用量。

## 命令

以下所有示例中的 `EXPORT` 都是导出目录，例如 `decks/output/Words__N3`。必须传入目录，不能传入 `deck.json` 文件路径，否则会收到明确的错误。

示例中的路径和字段名仅用于说明。工具集可处理任意 CrowdAnki 导出，不预先知道特定笔记类型的字段名称：`Слово`、`Часть речи`、`Значение` 及其他字段名，还有牌组级别和路径，都只是当前用户牌组的示例，不属于全局契约。字段名始终由调用方指定，并根据目标导出中实际的 `note_models[].flds[].name` 验证。

### 受限的 `decks/input` 区域

`decks/input/` 是留给未来可信导入器的封闭区域。任何命令都不得读取、遍历或修改该目录。凡是指向 `decks/input` 的路径（导出目录、`--before`/`--after`、`--out`、`--root`）都会以 `invalid_request` 拒绝，且 `details.reason = input_area_forbidden`；错误消息会说明区域及其用途。不存在绕过标志：这是 API 层面的限制，而非文件系统权限限制。检查也被纳入导出目录的契约，因此不仅 CLI，库调用也必须遵守。牌组工作区应使用 `decks/output/`。

### 布局边界

`decks/` 存放本地数据，被 Git 整体忽略，因此新克隆仓库时不存在。`decks/output/` 按需要创建（包括通过 `migrate-layout`）；缺少它属于正常情况。Git 仅跟踪 `decks/` 之外的代码、文档和独立测试夹具。

### `inspect`

对导出结构给出简明概览：根牌组、牌组节点数、笔记总数、按 `ord` 排列字段的笔记类型、牌组配置、媒体汇总和各牌组路径下的笔记分布。

```bash
anki-repo inspect decks/output/Words__N1
anki-repo inspect decks/output/Words__N1 --verbose
```

`--verbose` 额外提供 UUID、从父节点到子节点的牌组树、卡片模板、`guid` 统计以及有限的诊断样本（包括缺失或未声明的媒体文件名）。JSON 模式下，详细信息只在启用 `--verbose` 时出现于 `result.verbose`。

### `find`

按稳定条件查找笔记。必须且只能指定一种条件：`--guid`，或者同时提供 `--field` 和 `--value`。

```bash
# 按 guid 查找：identity lookup
anki-repo find decks/output/Words__N1 --guid 'D!TAYVuHi,'

# 按字段原始值查找：包含子串
anki-repo find decks/output/Words__N1 --field Слово --value 偶然

# 按任意字段名进行通用查找：子串或精确匹配
anki-repo find decks/output/Words__N2 --field 'Часть речи' --value Существительное
anki-repo find decks/output/Words__N1 --field 'Часть речи' --value Существительное --match exact
```

行为：

- `--field` 指定字段名；`--match` 可取 `contains`（默认）或 `exact`，且仅与 `--field` 一起使用。字段名根据实际 `note_models[].flds[].name` 检查；未知字段产生领域错误 `unknown_field`（退出码 3），而不是空结果。
- 命令不存在独立的“单词”实体筛选方式：字段由调用方指定，因此 `--field X --value Y --match contains` 与已移除的 `--word` 标志语义相同（参见“公共契约变更”）。
- `--deck` 将范围限定在指定牌组及其全部子牌组。
- `--limit`（默认 20，最多 500）仅限制输出量，而不限制实际搜索；`matched_total` 统计全部匹配，`returned` 和 `truncated` 表示返回范围。
- 匹配区分大小写，直接比较原始字段值，不进行 HTML 规范化。

### `stats`

汇总结构统计：按牌组和笔记类型统计笔记数量、牌组配置使用情况、空值与非空值字段分布及媒体文件概况。

```bash
anki-repo stats decks/output/Words__N3
anki-repo stats decks/output/Words__N3 --group-by 'Часть речи' --top 10
```

- `--group-by` 对指定字段的原始值进行分组统计；`--top`（默认 20，最多 500）限制输出大小，`distinct_values` 和 `truncated` 仍反映总体分布。
- 分组列先按数量降序，再按字段值排序。

### `validate`

对单个导出执行确定性的结构完整性验证。

```bash
anki-repo validate decks/output/Words__N1
anki-repo --json validate decks/output/Words__N1
```

未发现任何 `ERROR` 级问题时，`valid` 为 `true`。`WARNING` 与 `INFO` 不会令导出无效。路径问题（缺少目录或 `deck.json`）是退出码 3 的领域错误；内容问题（包括无效 JSON）以退出码 6 报告。

问题级别及代码：

| 级别 | 代码 |
|---|---|
| `ERROR` | `invalid_json`, `root_not_deck`, `schema_invalid`, `node_not_deck`, `duplicate_note_model_uuid`, `duplicate_deck_config_uuid`, `note_model_identity_missing`, `deck_config_identity_missing`, `note_model_uuid_missing`, `note_model_unresolved`, `deck_config_unresolved`, `note_fields_count_mismatch`, `note_field_value_not_string`, `note_guid_missing`, `duplicate_note_guid`, `field_name_duplicate`, `field_ord_invalid`, `field_ord_negative`, `field_ord_duplicate`, `field_ord_out_of_range`, `field_ord_gap`, `template_field_unresolved` |
| `WARNING` | `media_reference_undeclared`, `media_files_duplicate`, `media_physical_missing`, `media_dir_missing`, `media_name_not_basename`, `template_construct_unchecked`, `deck_config_uuid_missing`, `deck_name_missing`, `conflicting_note_model_definition`, `conflicting_deck_config_definition`, `template_ord_invalid`, `template_ord_duplicate` |
| `INFO` | `export_summary`, `media_physical_unused`, `note_model_unused`, `deck_config_unused`, `empty_field_values`, `issues_truncated` |

重要的结果解释规则：

- 物理媒体文件缺失是 `WARNING`，不是 `ERROR`：仓库尚未单独定义媒体保存策略。
- 媒体文件名只按不含目录的基本名称比较；`media_files` 中的路径绝不用于访问文件系统。
- 在嵌套 CrowdAnki 节点重复声明完全相同的笔记类型或配置不算错误。同一 UUID 下定义冲突才标记为 `WARNING`。只比较语义相关部分：笔记类型的 `name` 和 `flds`（`name`、`ord`），配置的 `name`；此处不比较 `tmpls`、`css` 或调度器参数。
- 有限模板解析器仅检查 `{{Поле}}` 和 `{{tts ja_JP:Поле}}` 等简单引用。只要结构中至少有一个片段与实际字段名匹配，就视为已识别。未闭合的 `{{`、带过滤器且找不到已知字段名的结构，标记为 `template_construct_unchecked`（`WARNING`），以避免误报。不会验证 `{{#X}}`/`{{/X}}` 的成对关系及其他模板逻辑。
- 每个问题代码的记录数有限；截断时 `issues_truncated` 会给出被省略的数量。
- 空 `guid` 产生 `note_guid_missing`（`ERROR`），不再作为重复 GUID 报告，也不进入 GUID 索引，因此 `find --guid ''` 不会找到任何记录。

### `qa`

对卡片内容执行确定性的质量检查。规则只依赖当前导出数据，不依赖仓库当下的其他状态。

```bash
anki-repo qa decks/output/Words__N1
anki-repo qa decks/output/Words__N3 --code empty_field_value --max-per-code 5
anki-repo --json qa decks/output/Words__N5 > findings.json
```

规则注册表（问题按注册顺序输出；JSON 的 `rules` 也保持相同顺序）：

| 代码 | 级别 | 检测内容 |
|---|---|---|
| `empty_field_value` | `warning` | 字符串字段值等于 `""` |
| `leading_whitespace` | `warning` | 值以空白字符开头 |
| `trailing_whitespace` | `warning` | 值以空白字符结尾 |
| `forbidden_white_span` | `error` | `<span>` 的 `style` 中设置了白色文字 |
| `duplicate_note_content` | `warning` | 同一笔记类型的多条笔记具有完全相同的内容 |

行为：

- `--code` 可以重复指定，将输出限制为选定的问题代码。未知代码产生 `unknown_qa_code`（退出码 3），而不是空结果；不能让拼写错误看起来像“没有问题”。
- `--max-per-code`（默认 20，最大 200）仅限制每个代码输出的问题数，不限制真实计数。`findings_total` 和 `by_code` 统计全部选定代码的问题，`truncated` 表示是否有省略。`rules[].findings` 不受筛选参数影响，始终统计整个导出下全部规则。
- `error` 表示对读者而言的缺陷严重程度，而非命令失败；即使发现 `error` 级问题，`qa` 也返回退出码 `0`。
- 纯空白字符字段会同时触发 `leading_whitespace` 和 `trailing_whitespace`。
- 空值与缺失值不同：`""` 和纯空格字符串都会被检查；`null` 或非字符串不算空字段，由 `validate` 处理。
- `duplicate_note_content` 比较相同笔记类型下整个原始字段值数组及其位置，不对 HTML 或空白字符进行规范化。不假设存在“主字段”，因为主字段的语义属于特定笔记类型，不属于通用 CrowdAnki 格式，因此工具集无法为其定义正确的通用语义。调用者可在 `find` 或 `review` 中明确指定 `--field`、`--value` 进行分组。
- 注册表**恰好只有以上五个代码**。`duplicate_primary_field` 及“`Слово` 必然是主字段”的假设被有意移除（参见“公共契约变更”）。请求已删除的规则会得到 `unknown_qa_code`。
- 规则不依赖笔记类型和字段名称：同样的值换成其他字段名或类型后仍得到相同问题。
- QA 问题不会导致导出结构无效，也不会阻止 `edit`。`error | warning | info` 描述内容缺陷，独立于 `validate`；QA `error` 不是结构错误。
- 可能在无法可靠寻址的笔记上发现问题：类型无法解析、缺少 `guid` 或 `guid` 不唯一。这些问题会明确标为 `addressable: false` 并计入 `unaddressable_findings`，而非直接丢弃。`validate` 会指出结构性原因，`qa` 不会把不可执行地址当成普通 `edit` 目标。
- 重复内容问题描述的是**整组**笔记。`group_size` 给出成员数；`related_note_indices` 和 `related_guids` 按原问题的规范顺序列出成员，**包括问题所有者**（而 `review` 的 `related` 排除所有者）。列表最多 20 个成员（`MAX_RELATED_NOTES`），超出时显示 `related_truncated: true`，不会悄然省略。

每个问题记录包含导出中的位置（`note_index`）、`guid`、牌组路径、笔记类型，以及“字段名 + `ord`”。`evidence` 保存长度受限的样本：值长度、截断后的内容和规则特定细节，例如空白规则的 `boundary` 与 `forbidden_white_span` 的 `first_tag`。分组信息不在 `evidence` 中重复，而是由 `group_size`、`related_note_indices`、`related_guids` 提供。

### `review`

为外部审阅生成紧凑且数量受限的笔记数据包。`qa` 主要描述问题；`review` 则提供需要实际阅读和思考的内容：按 `ord` 排序的笔记字段及各笔记已有的 QA 问题。

```bash
# 按已发现的问题筛选笔记
anki-repo review decks/output/Words__N1 --qa-code empty_field_value --limit 25

# 下一页
anki-repo review decks/output/Words__N1 --qa-code empty_field_value --offset 25

# 按内容筛选和精确查找
anki-repo review decks/output/Words__N2 --field 'Часть речи' --value Существительное
anki-repo review decks/output/Words__N1 --guid 'D!TAYVuHi,'
```

必须且只能选择一个筛选条件：

| 条件 | 含义 |
|---|---|
| `--all` | 选定范围内的全部笔记 |
| `--guid` | 通过 `guid` 定位单条笔记 |
| `--field` + `--value` | 按字段名筛选，`--match contains`（默认）或 `exact` |
| `--qa-code` | 具有指定 QA 代码问题的笔记 |

`--deck` 限制为指定牌组及其子牌组，`--offset` 跳过部分已选笔记，`--limit`（默认 25，最大 200）指定页大小。返回结果包含 `total_selected`、`returned`、`truncated` 和 `next_offset`；最后一页的 `next_offset` 为 `null`。筛选和字段名解析与 `find` 共用实现，所以未知字段、牌组或 GUID 产生相同错误和退出码。

`review` 是读取及分页命令，不要求“必须找到内容”。无匹配或 `--offset` 超出末尾时，空页仍返回成功（`returned: 0`、退出码 `0`），否则分页无法正常结束。例外为 `--guid`，因为它明确定位一条笔记：找不到返回 `not_found`（4），重复不唯一返回 `ambiguous`（5）。

数据包内容：

- 排除不具备明确可执行 `guid` 的笔记：未设置、为空或出现多次的 GUID（重复者的**所有**出现位置均排除）。排除数量见 `excluded_unaddressable`；`notes_total` 始终是整个导出的笔记数，而不是当前页的数目。即使因排除而产生空页，计数器也会解释原因。
- 使用 `--qa-code` 选取重复内容问题时，必须包含分组成员，而不仅是问题所有者，否则 `qa → review` 会丢失其他笔记。所有者携带问题本身（`group_size`、不包含自己的 `related`）；其他成员携带指向所有者的 `group_membership`（`code`、`owner_note_index`、`owner_guid`、`group_size`）。成员清单来自领域结果，最多 `MAX_RELATED_NOTES`（20）。若一组有 30 条笔记，数据包只包含所有者和最多 20 位成员；所有者问题中的 `related_truncated: true` 表明清单不完整。超过上限不能承诺包含所有成员，须借助 `review --all` 或 `--field` 另行查找。
- `--all`、`--guid`、`--field`、`--qa-code` 不会另行扩展分组，只选择各自条件明确表示的内容。

### `review-check`

根据导出的**当前**状态验证外部智能体的修改建议。命令只读，不会写入 `deck.json` 或任何辅助文件。

```bash
# 验证建议并生成可直接交给 `edit` 的请求
anki-repo --json review-check decks/output/Words__N1 \
  --proposals proposals.json | jq .result.edit_request > request.json

# 相同操作，改用 stdin
anki-repo review-check decks/output/Words__N1 --proposals - < proposals.json

# 生成的请求走既有的写入边界
anki-repo edit decks/output/Words__N1 --request request.json
anki-repo edit decks/output/Words__N1 --request request.json --apply
anki-repo validate decks/output/Words__N1
```

建议文档使用独立的版本化结构，不等同于 `edit` 请求：

```json
{
  "schema_version": 1,
  "proposals": [
    {
      "proposal_id": "p0",
      "guid": "guid-1",
      "field": "Пример",
      "expected": "",
      "replacement": "пример-1",
      "reason": "в колоде пустой пример"
    }
  ]
}
```

`proposal_id` 和 `reason` 是建议契约的一部分。前者在生成的请求中成为 `edit_id`；后者仅作为审阅元数据保留（报告中安全截断，不会复制到 `edit_request`）。不允许未知键：建议属于独立的公开输入结构，不能接受其他格式的字段；错误会使用建议契约的术语。`review-check` 复用 `edit` 的内部解析和目标定位，因此生成的 `edit_request` 可以直接交给 `edit`。

建议状态：

| 状态 | 条件 | 进入 `edit_request` |
|---|---|---|
| `valid` | 目标有效，当前值等于 `expected` 且不同于 `replacement` | 是 |
| `already_correct` | 当前值同时等于 `expected` 和 `replacement` | 否 |
| `already_applied` | 当前值已等于 `replacement` | 否 |
| `conflict` | 当前值既不等于 `expected`，也不等于 `replacement` | 否 |
| `invalid` | 目标无法定位：笔记/字段不存在，字段不是可编辑字符串，或 `guid` 不唯一 | 否 |

对于 `invalid` 建议，`problem` 会明确记录 `note_not_found`、`unknown_field`、`ambiguous_guid` 或 `field_not_resolvable`。`edit` 对后两种目标会以通用 `internal_error` 终止整个操作；`review-check` 刻意不这样做，因为建议中的不可执行目标不是工具内部故障，其他建议仍需检查。

结果与退出码：

| `outcome` | 条件 | 退出码 |
|---|---|---|
| `ok` | 没有 `invalid` 和 `conflict` | `0` |
| `stale` | 至少一个 `conflict` 表示智能体依据的状态已过期 | `7` |
| `invalid` | 至少一个 `invalid`；优先处理不唯一 GUID（5），其次缺失笔记（4），其他为 3 | `5` / `4` / `3` |

只有同时满足 `outcome: ok`、至少一个 `valid` 建议、原始数据可编辑（`source_editable: true`），才生成 `edit_request`。空 `edits` 违反 `edit` 契约；把部分有误的建议悄悄拼成请求也不允许。新请求里的 `expected` 直接来自智能体建议，这是安全的，因为获得 `valid` 状态前已与真实当前值核对。

因此，`review-check` 还使用与 `edit` 完全相同的共享逻辑检查源数据（不是第二套实现）：

| 阻止条件 | `source_blockers` 中的代码 | 退出码 |
|---|---|---|
| `deck.json` 不符合规范格式 | `source_not_canonical` | `3` |
| 导出含有任何 `ERROR` | `export_invalid` | `6` |
| 笔记类型字段顺序定义存在歧义（`WARNING conflicting_note_model_definition`） | `export_not_mutable` | `6` |

即使源文件不可编辑，建议报告仍会输出：状态、计数及 `proposals` 对智能体有用；`source_editable: false` 和 `source_blockers` 解释为什么没有生成请求。退出码反映源文件阻止条件，不能把表面上的 `ok` 误认为具备写入条件。

`stale` 和 `invalid` 是验证结果，不代表命令无法运行，因此普通和 JSON 模式都产生完整报告（JSON 位于 `result`，不是 `error`），仅由非零退出码表示结果，类似 `validate` 的退出码 6。

报告有数量限制：最多展示 50 条 `proposals` 并提供 `proposals_truncated`，但 `counts` 和 `edit_request` 不截断。限制只作用于显示，人类可读输出会明确说明这一点，不会谎称 `--json` 提供了完整清单。

文档整体层面的错误仍是退出码 3 的领域错误：在建议文档无法解析时根本无从检查。该结构以 `invalid_request` 和如下 `details.reason` 报告错误：`malformed_proposals`（格式错误或其他结构）、`unsupported_schema_version`、`empty_proposals`、`too_many_proposals`（超过 20,000）、`proposals_too_large`（超过 8 MiB）、`duplicate_proposal_id`。与 `edit` 请求共用的 `edit::validate_request` 检查还会产生 `empty_guid`、`empty_field`；重复的（`guid`，字段）组合单独报告 `duplicate_edit_target`，而不是 `invalid_request` 内的原因。

### `models`

显示选定范围内各笔记类型的实际字段结构：字段名称、在笔记 `fields` 中的位置、实际内容，以及预览支持的模板结构。这是创建笔记和生成可视化报告的输入：字段名是具体导出的事实，而非通用契约。

```bash
EXPORT=decks/output/Words__N1

# 导出根牌组的笔记类型（默认选择器：preorder 0）
anki-repo models "$EXPORT"

# 按名称、身份或遍历顺序选择牌组
anki-repo models "$EXPORT" --deck "Words::N1"
anki-repo models "$EXPORT" --deck-uuid 00000000-0000-0000-0000-000000000000
anki-repo models "$EXPORT" --deck-preorder 1

# 每个字段显示多少个示例值（1..10，默认 3）
anki-repo models "$EXPORT" --sample-limit 5
```

筛选条件可以组合，但所有条件必须指向同一个牌组节点；不一致属于 `deck_identity_mismatch`（退出码 3），不能随意选择其中之一。未指定时选中导出根牌组。

对于范围内的每种笔记类型，输出包括：

- 身份标识（`crowdanki_uuid`）、名称、类型（`standard` 或 `cloze`）、模型类别，以及在当前子树中定义还是继承；
- 当前牌组与整个子树中该笔记类型的笔记数；
- **按 `ord` 顺序排列的字段**。`ord` 是笔记 `fields` 数组中的索引，因此即使声明顺序不同，也按实际使用顺序展示；
- 各字段在笔记类型中声明的描述、当前牌组的空值数量，以及最多 `--sample-limit` 个非空值示例和对应来源笔记的 `guid`；
- 各模板的 `ord`、名称、普通和特殊字段，以及无法静态计算的结构清单（`{{cloze:…}}`、其他过滤器、`<script>`、无效模板）；
- `schema_problems`：为何字段结构无法用于构建字段值（`ord` 缺失、重复或非数字）。仍会显示该笔记类型及证据，但 `create` 拒绝使用它。

示例值是证据，而不是格式保证；它取自真实笔记并按长度截断。智能体可据此判断字段含义，例如 `Ударение` 实际包含 `<img>` 而非普通文本。

`models` 没有写入检查，也无需有，因为它从不写入。

### `create`

向**已有牌组**中按**已有笔记类型**添加新笔记。命令不会创建牌组、笔记类型或字段；实际结构完全由导出数据决定。

```bash
EXPORT=decks/output/Words__N1
REQUEST=create.json

# 1. 先 dry-run：查看结果
anki-repo create "$EXPORT" --request "$REQUEST"

# 2. 写入同一条笔记
anki-repo create "$EXPORT" --request "$REQUEST" --apply

# 3. 以可移植形式记录结果
anki-repo create "$EXPORT" --request "$REQUEST" --apply --emit-resolved resolved.json

# 请求也可以从 stdin 读取
cat "$REQUEST" | anki-repo create "$EXPORT" --request -
```

#### 请求格式

```json
{
  "schema_version": 1,
  "notes": [
    {
      "note_id": "новая-заметка-1",
      "guid": "w0Wc3Ip@N#",
      "deck": { "path": "Words::N1", "crowdanki_uuid": "…" },
      "model": { "mode": "auto" },
      "fields": {
        "Слово": "новое",
        "Часть речи": "",
        "Значение": "значение",
        "Ударение": "…",
        "Пример": "…",
        "Похожие слова": "…"
      },
      "tags": ["черновик"]
    }
  ]
}
```

- `note_id` 是调用方指定的标签，在报告和错误中返回，方便智能体把结果与请求对应。它不会写入导出数据。
- `deck` 通过 `path`、`crowdanki_uuid` 和/或 `preorder` 选择牌组，所有指定条件必须指向同一节点。目标牌组没有 `crowdanki_uuid` 时拒绝写入（`unresolved_deck_identity`）。
- `model` 可设为 `{"mode": "auto"}`（默认），或者 `{"mode": "explicit", "crowdanki_uuid": "…"}` / `{"mode": "explicit", "name": "…"}`。在 `auto` 模式下，根据请求中**完整的字段名集合**寻找兼容笔记类型。因此，字段名拼错可能产生 `unknown_model`，错误会列出实际请求的名称。存在歧义（`ambiguous_model`）时返回退出码 5，绝不随便选择第一个。完整的 `auto` 选择规则如下，不进行猜测或按使用频率排序：

  1. 候选必须拥有完全可用的字段结构（无效结构按 `model_schema_unusable` 排除），且字段名集合与请求**完全一致**，不多不少。首先排除无效结构。如果某类型仅因结构无效而被排除，最终仍返回 `unknown_model`，但 `details.rejected` 会列出各被排除类型及 `schema_problems`；必须区分“没有该类型”和“有但不可用”。
  2. 如果剩余类型中存在在目标牌组子树中实际有笔记的类型，就只保留这些类型。
  3. 如果子树内没有兼容类型的笔记（例如空牌组），保留导出中全部兼容类型。这是“唯一兼容结构”规则，`model_evidence` 会明确说明目标子树中没有兼容类型的笔记。如果兼容类型不止一个，必须拒绝；邻近牌组的结构不能作为选择依据。
  4. 恰好一个候选就选择它；零个返回 `unknown_model`；多个返回 `ambiguous_model`，附候选清单及其子树笔记数。

  每个结果的 `model_evidence` 都解释实际使用了哪条规则，以及目标牌组已有笔记的影响。如果需要确定的笔记类型，使用 `mode = explicit`，这是多个兼容结构下保证选中指定类型的唯一办法。
- `fields` 按**笔记类型中准确的字段名**提供值。必须包含全部字段，空字符串是合法值。请求对象中的键顺序无关紧要；笔记的 `fields` 按 `ord` 顺序写入。
- `guid` 可选。未提供时自动生成（报告中 `guid_generated: true`），并保存到 `--emit-resolved`，以便重试保持幂等。

成功执行并指定 `--emit-resolved` 时会产生**两个结果**：指定文件中的已解析请求，以及写入 `stdout` 的报告。JSON 模式不会再把完整请求复制进输出封套，因为 20,000 条笔记的请求可能过大。因此，可移植的产物是文件，而非命令输出。

写入顺序属于公开契约，而不是偶然的实现细节：

1. 先完整解析并规划请求中的全部笔记、`guid` 和类型。
2. 将产物路径与规范化的 `deck.json` 路径比较。如果 `--emit-resolved` 指向 `deck.json`、其符号链接，或只是写法不同（`./`、`..`、不区分大小写文件系统上的字母大小写），立即返回 `invalid_request` 和 `reason = emit_resolved_aliases_source`，**不得先修改任何内容**。
3. 将完整产物原子写入同目录临时文件，再用 `rename` 发布。
4. 只有之后才发布 `deck.json`。
5. 若发布失败，新笔记没有创建，但包含各笔记 `guid` 的产物已存在。使用它重试会恰好创建一条笔记或得到 `already_applied`，不会悄悄生成重复笔记。

不采用相反顺序（先改 `deck.json`）的原因是：如果导出已修改成功、但产物写入失败，调用方会得到错误并丢失生成的 GUID。也不在 `apply` 后执行回滚，因为部分故障下的回滚可能破坏其他写入者的修改。

即使在 dry-run 模式下，`--emit-resolved` 也会写入产物，但**不会**修改 `deck.json`；它是命令独立输出，而不是修改导出的副作用。产物的父目录必须已存在，否则返回 `write_failed`，`create` 不会在任意路径创建目录。不指定 `--emit-resolved` 时行为不变，每个 GUID 仍会出现在报告中。
- `tags` 是可选的新笔记标签列表。采用与 `retire` 相同的规则，并按照 Anki 的行为忽略大小写判重。`["Дом", "дом"]` 是同一标签的两种写法，会因 `duplicate_tag` 被拒绝。

#### 每条笔记的三种结果

| 状态 | 含义 |
|---|---|
| `created` | 原来没有该 `guid`，已创建或将在 dry-run 计划中创建 |
| `dry_run` | 未要求写入，报告计划写入的结果 |
| `already_applied` | 已存在同一 `guid` 且内容与请求一致，不重写文件 |

如果相同 GUID 已存在但**内容不同**，返回 `guid_conflict`（退出码 6），绝不悄然更新。`create` 不编辑已有笔记，应使用 `edit`。

#### 保证

- 新字段值默认禁止媒体引用（`media_forbidden`，退出码 3）。唯一例外是下文明确配置的资源域处理器，只允许经过验证的本域图片 `img/src` 引用。

  以下内容都属于媒体引用：

  - 各种形式的 `[sound:名称]`，包括 `[sound: 名称]` 和 `[sound:路径/名称]`；
  - 共用 `ADDRESS_ATTRIBUTES` 表中携带媒体地址的 HTML 属性：`img`、`audio`、`video`、`source`、`track`、`input`、`embed`、`iframe`、`frame`、`script` 的 `src`；`img` 和 `source` 的 `srcset`；`video` 的 `poster`；`object` 的 `data`；SVG `image` 和 `feImage` 的 `href`/`xlink:href`；以及 `body`、`table`、`thead`、`tbody`、`tfoot`、`tr`、`td`、`th` 已废弃的 `background` 属性；
  - 未闭合标签中的媒体属性，例如缺少 `>` 的 `<img src="a.png"`。字段不是完整 HTML 文档，Anki 会把它与模板组合，未闭合标签可能在模板中闭合，进而变成真实文件请求。因此，无法理解的结构必须拒绝，不能跳过。`href` 等导航属性不用于请求媒体文件，因此未闭合的普通导航链接不按媒体引用处理；报告的信任边界仍会根据另一条规则拒绝不完整结构；
  - 字段值 CSS 中的地址：`style` 属性或 `<style>` 中的 `url(…)`、`image()`/`image-set()`、`@font-face` 的 `src`，以及 `@import`。CSS 地址同样会请求文件，`@import` 还可能带入外部样式。地址识别由 `report::css` 统一负责，创建限制不会另写解析器。解析 CSS 之前会解码 HTML 属性实体：浏览器把 `url&#40;a.png&#41;` 视为地址。未知或无法解释的地址结构必须拒绝。

  标签大小写、属性大小写以及空格写法没有影响：`<IMG SRC = a.png>`、`<img srcset="…">` 和 `<img src="a.png">` 都是媒体引用。`<img alt="a.png">` 和 `<функция src=x>` 不是；真实媒体属性中的未闭合引号仍被识别为地址。

  `<div src="не-адрес">` 也不是媒体引用，因为 `div` 没有需要加载文件的 `src`。因此地址识别按“**元素 + 属性**”进行。导航属性（`href`、`xlink:href`、`action`、`formaction`、`ping`、`manifest`、`cite`、`longdesc`、`usemap`、`archive`、`classid`、`codebase`）则在所有元素上识别。导航不一定请求本地媒体，但可能跳出离线报告，因此不能漏掉外部地址。SVG `use` 的 `href` 有意归为导航，因为它也能指向文档内片段（如 `#иконка`）。创建限制与 `visual-report` 共用 `htmlscan` 和同一张地址表；CSS 同样由 `report::css` 统一判断。
- 新笔记对象**恰好**包含 `__type__`、`fields`、`guid`、`note_model_uuid`、`tags` 五个键，不会添加其他键。
- 新笔记只追加到目标节点 `notes` 的**末尾**，原有笔记不移动、不改写。对于整体“已有笔记不修改”的规则，唯一例外是通过独立的 [`migrate-media`](#migrate-media) 对旧媒体名称进行有证据的迁移；`create` 本身不碰已有笔记。
- 写入前重新解析并验证候选（`candidate_reparsed`、`appended_notes_verified`、`only_notes_and_media_files_appended`、`guids_resolved_without_conflict`）。`validation` 对比修改前后的问题代码；如果引入新错误就不写入。
- 要求 `deck.json` 为规范格式；写入原子化，并像 `edit` 一样防止并发编辑覆盖。

#### 明确的媒体授权规则

`create` 从导出目录向上查找最近包含 `.git`（目录或 Git worktree 标记文件）的仓库，然后读取其中的 `.anki-repo/create.yaml`。进程当前目录不影响规则选择。若规则文件不存在，保持默认不允许媒体引用的行为。`--create-config PATH`、`--asset-store PATH`、`--pitch-asset-store PATH` 可以分别指定规则文件和资源域存储；显式指定但不存在的规则文件会报错。默认使用同一仓库中的 `.asset-store/kanji` 和 `.asset-store/pitch-accent`。如果没有媒体引用，就不打开任何资源存储；请求未涉及的资源域也无需存在存储。

```yaml
schema_version: 1
note_models:
  - crowdanki_uuid: "identity-from-models"
    fields:
      ExactFieldName:
        processors:
          - type: kanji_assets
      OtherExactFieldName:
        processors:
          - type: pitch_accent
```

笔记类型由实际 `crowdanki_uuid` 标识，字段通过 `models` 返回的准确名称标识；笔记类型的显示名称不参与处理器选择。规则可以包含其他笔记类型；但若选中的类型下规则引用了不存在的字段，就会产生 `config_stale_field`。不支持的版本、未知键/类型、重复键、笔记类型 UUID、字段名或处理器，以及有歧义的 YAML 结构，都会导致整个配置被拒绝。配置仅选择处理器，不记录具体有哪些图片。其中的 `note_models[].crowdanki_uuid` 描述当前导出中实际使用的特定笔记类型，属于当前数据，而不是代码永久契约。

`processors` 是非空、有顺序的处理器链，按 YAML 列表顺序执行。当前支持 `kanji_assets` 和 `pitch_accent` 两种类型。未知类型属于配置错误；同一列表中重复相同类型会在执行前拒绝，因为每项只有 `type`，没有可区分多个实例的独立配置。每个处理器只能声明自己识别的引用。任何引用必须**恰好有一个**所有者：多个处理器声称拥有它产生 `media_reference_conflict`，无人认领产生 `media_reference_unclaimed`。存在处理器链并不意味着字段内其他引用也被放行；未配置的类型或字段仍没有处理器。

字段 HTML 由请求直接提供。处理器只接受共用 HTML 解析器识别的**完整** `img/src` 元素。`kanji_assets` 接受仅包含一个支持的 Han/CJK 字符及精确 `.gif` 或 `.png` 扩展名的基本文件名，例如 `<IMG SRC = '漢.gif'>`。`pitch_accent` 只接受 `<surface>.pitch.png`，与所属资源域为该词形生成的规范文件名完全一致。URL、目录路径、查询参数、锚点、`[sound:...]`、CSS、`srcset` 和其他引用仍然禁止。处理器不会生成 HTML 或更改扩展名；文件名不符时会说明可用的规范名称。

`asset-store::AssetStore::read_verified_with_policy` 返回经资源域验证的字节和记录。创建需要规范状态 `VERIFIED`，并由资源所有者确认最终可信状态：当前域验证器的决定，或（在域策略允许时）对完整正确解码的**精确 SHA-256** 由人工明确批准。`kanji_assets` 允许人工批准并保留自动化证据；人工明确拒绝会取消对当前字节的信任。`pitch_accent` 只信任预期验证器 `jpdb-pitch-accent-render` 对该 SHA 当前给出的自动 `VERIFIED`；人工批准不能替代自动决定，旧版本验证器的证据即使 SHA 相同也不再有效。同时必须核对校验和、大小、格式，使用不跟随符号链接且始终位于存储边界内的文件描述符。不会读取 `.runtime`。解析引用不会下载图片或访问网络；缺少图片会阻止创建，必须另行通过 `kanji-assets` 或 `pitch-assets` 获取。

CrowdAnki 的 `media/` 是所有资源域共享的扁平命名空间。若两个不同身份或处理器把**不同字节**映射到同一文件名，`create` 会以 `media_filename_collision` 拒绝整个操作，不通过处理器顺序择优。同名同 SHA 不构成冲突。被固定在 `media_assets` 中的身份如果具有未知 `namespace`，会产生 `media_domain_unknown`：路由按 `identity.namespace` 而非文件名决定。

已知的一种冲突来自旧流程把音高重音图命名为 `<surface>.png`。对于单字符词及其备用汉字图，这与汉字图规范名称相同。正确做法不是覆盖或手动编辑 `deck.json`，而是通过独立的 [`migrate-media`](#migrate-media) 将引用迁移到现行规范名称并释放旧文件名。这样 `create` 才能把备用汉字图放入已释放的位置。

Dry-run 会运行全部预检，不写入 JSON、`media/`、规则文件或存储。JSON `media.references` 最多展示 50 条（笔记索引、笔记类型 UUID、字段、文件名、资源身份），并提供完整 `references_total` 和截断标记 `references_truncated`。`assets` 包含身份、`canonical_filename`、`sha256`、相对 `destination`、`action: copy|reuse`，另有 `media_files_added`、`mutations_planned` 和 `mutations_applied`。目标被占用时错误包含 `action: conflict`。即使请求没有媒体引用，`outcomes[].processor_fields` 也列出配置的字段；`media_references` 仍有可能非零。文本输出提供媒体计划概要，JSON 提供完整计划。

已解析的 v1 请求额外包含 `media_assets`：按顺序排列、与本机路径无关的资源身份、文件名及 SHA-256 列表。即使列表为空，也会固定资源集合。该文件可重新传入 `--request`；改变、移除或重命名固定资源会产生 `stale_pinned_asset`。旧 v1 请求如果没有 `media_assets`，仍兼容但不固定资源集合。要重新选择资源，使用没有旧固定信息的原始请求。`--emit-resolved` 在修改媒体前完成写入，并且不能覆盖源文件、`media/`、规则文件或资源存储。

指定 `--apply` 时，在第一次改动导出前，会检查请求、规则、资源、所有目标及候选数据。同一把持久的导出目录锁协调 `edit`、`retire` 和 `create`，源文件变化会在复制任何文件前被拒绝。先把已验证字节写入临时文件，再使用硬链接发布，绝不覆盖已有目标。如果另一个进程同时创建了同名字节一致的文件，就直接复用；字节不同则返回 `conflict`。符号链接和非常规文件被拒绝。不会为选择图片而完整扫描用户的 `media/`。

只有在媒体放置完毕后才发布 `deck.json`。若树中没有任何节点声明某文件名，就将其追加到根节点 `media_files`；原有声明及顺序保留。检查先证明笔记恰好追加，再通过对比完整 JSON 证明 `media_files` 只做了必要追加。`only_notes_and_media_files_appended` 与 `media_assets_verified` 记录核验结果；只有未增加声明时 `only_notes_appended` 才为真。`media_references_absent` 表示是否确实没有媒体引用。

若部分失败，`deck.json` 不发布；已经成功复制且正确的媒体文件保留供下次安全复用。重试相同请求可恢复缺失文件或声明而不重复创建笔记。`already_applied` 对应笔记状态，`media.mutations_applied` 对应媒体恢复；完全恢复后再次运行将成为空操作。共享锁负责协调遵守同一协议的写入进程；外部写入者也必须遵循该协议。

新增原因在 `error.details.reason`，相关证据在 `error.details.evidence`；俄语错误消息文本本身不属于 API：

| `reason` | 错误 / 退出码 | 含义 |
|---|---|---|
| `config_absent`, `processor_not_enabled` | `media_forbidden` / 3 | 字段未获得明确授权 |
| `config_invalid`, `config_unsupported`, `config_stale_field`, `config_context` | `invalid_request` / 3 | 规则无效、过期或无法确定导出上下文 |
| `media_reference_unclaimed` | `media_forbidden` / 3 | 没有处理器接受此引用 |
| `media_reference_conflict` | `media_forbidden` / 3 | 多个处理器声明拥有同一引用 |
| `media_domain_unknown` | `media_forbidden` / 3 | 固定资源身份的 `namespace` 不属于配置的处理器 |
| `kanji_asset_missing`, `pitch_asset_missing` | `invalid_request` / 3 | 缺少已验证资源或其存储 |
| `asset_integrity_invalid` | `invalid_request` / 3 | 状态、决定、校验和、格式或边界不符；详情见 `asset_code` |
| `canonical_filename_mismatch`, `stale_pinned_asset` | `expected_mismatch` / 7 | 规范文件名不符，或固定资源发生变化、消失、重命名 |
| `media_filename_collision` | `expected_mismatch` / 7 | 不同身份或域在同一文件名下对应不同字节 |
| `destination_media_conflict` | `expected_mismatch` / 7 | 目标文件名被不同字节或不安全对象占用 |
| `legacy_media_changed` | `expected_mismatch` / 7 | 制定迁移计划后旧文件出现、消失或变化 |
| `asset_store_boundary`, `media_declaration_conflict`, `emit_resolved_protected_path` | `invalid_request` / 3 | 路径越界或媒体声明冲突 |
| `media_materialization_failed`, `media_disappeared`, `media_directory_changed` | `write_failed` / 8 | 放置或最终验证媒体时失败 |
| `source_modified`, `source_missing`, `export_directory_changed` | `source_changed` / 7 | 锁保护期间源文件变化或导出目录被替换 |

若 JSON 发布失败，保留原有 `write_failed`、退出码 8 和 `details.operation`；已验证的附加媒体文件可在重试时安全复用。

#### 报告

| 键 | 含义 |
|---|---|
| `export_dir`, `deck_json` | 导出目录与 `deck.json` 路径 |
| `dry_run`, `applied` | 运行模式及是否替换文件 |
| `source_bytes`, `candidate_bytes`, `byte_delta` | 文件大小及差值 |
| `notes_total`, `notes_created`, `notes_already_applied` | 各状态笔记计数 |
| `outcomes` | 每条笔记的结果（最多 50 条，含 `outcomes[].processor_fields` 和 `outcomes_truncated`） |
| `decks_touched` | 涉及的牌组、原笔记数与新增数 |
| `validation` | 前后问题计数与新增代码 |
| `checks` | JSON、模型、GUID 和媒体验证结果 |
| `media` | 媒体引用和已验证文件计划，参见[明确的媒体授权规则](#明确的媒体授权规则) |
| `resolved_request` | **不包含在** `--json` 中：写入 `--emit-resolved` 指定文件，对每条笔记补充 `guid`、牌组身份和明确的类型选择。要么成功写入文件，要么以 `write_failed` 失败 |

### `migrate-media`

将经过证明的媒体引用从旧文件名迁移到所属资源域当前的规范文件名。这是唯一允许修改已有笔记字段的狭义例外。

```bash
# 1. 先 dry-run：证明引用语义并查看计划
anki-repo migrate-media EXPORT --namespace pitch_accent --key 飴 --from 飴.png

# 2. 写入：重写引用、放置规范字节、释放旧文件名
anki-repo migrate-media EXPORT --namespace pitch_accent --key 飴 --from 飴.png --apply
```

如果计划要释放旧文件名，执行 `--apply` 时会持有 `ExportLock`，将旧文件是否存在、大小和 SHA-256 与制定计划时的快照重新比较。状态不一致会在媒体放置和 `deck.json` 发布前触发 `legacy_media_changed`。真正删除旧文件前还会重复检查。

为什么需要迁移：CrowdAnki 的 `media/` 是跨资源域共享的扁平命名空间，而各域会刻意使用不同的规范命名方式：汉字图片为 `<char>.gif|png`，音高重音图片为 `<surface>.pitch.png`。旧流程使用 `<surface>.png` 保存音高重音图。如果是单字符词并需要备用汉字图，两张不同图片就可能具有同一文件名。`create` 必须拒绝（`destination_media_conflict`），而不是任选一种字节覆盖另一种。迁移把引用改为当前规范，从而释放旧名称，让规范的汉字图可以安全占用该名称。

`--namespace` 指定拥有**规范文件名**的资源域（`kanji` 或 `pitch_accent`），`--key` 是域内身份键（例如词形），`--from` 是准确的旧文件名，必须是安全的基本文件名，不含目录、`..` 或控制字符。规范名称由域所有者通过 `AssetDomainPolicy::canonical_location` 定义，命令通过资源身份取得并核对。旧名称与 `AssetIdentity` 的匹配则由独立方法 `AssetDomainPolicy::proves_legacy_consumer_filename` 验证。`--create-config`、`--asset-store`、`--pitch-asset-store` 与 `create` 中含义相同。

#### 哪些条件构成证据

在打开资源存储之前，域策略必须证明精确的（`AssetIdentity`、`--from`）组合合法。不能根据旧存储路径推断，也不依赖资源是否已经在存储中。只允许准确且安全的名称：`kanji` 接受 `<key>.gif` 或 `<key>.png`，`pitch_accent` 接受 `<key>.png`。通用策略不定义任何旧文件名。

然后命令会盘点导出中该文件名的**全部**引用者：

- 笔记字段值。该字段必须由 `.anki-repo/create.yaml` 中准确的笔记类型 UUID 和字段名授权；每个地址必须由恰好一个处理器声明拥有，并符合其明确识别的形式，即值为 `--from` 的完整 `img/src` 元素。
- `note_models[].tmpls[].qfmt`、`afmt` 和 `note_models[].css`。这些是静态引用者，无法证明属于特定笔记或资源身份，所以**即使位于其他笔记类型**也会阻止迁移。

盘点的是实际地址，而非纯文本中出现相同字符串。识别时考虑 URL 编码、查询参数和片段，因此 `飴.png?cache=1`、`飴.png#front`、`%E9%A3%B4.png` 都指向同一文件。然而 `srcset`、字段值中的 CSS 地址、`[sound:…]`、未闭合结构、带目录的文件名以及不符合处理器准确语法的值，都无法被该处理器认领。

只要有任何地址或有歧义的结构无法证明，整个操作就会拒绝且不修改任何内容，并返回证据，包括笔记的 `guid` 和字段，或静态表面（`qfmt`、`afmt`、`css`）、`model_uuid`、子树路径，以及 `field_not_owned_by_domain`、`reference_shape_not_claimable`、`field_position_not_described_by_model`、`static_model_reference_not_owned_by_domain` 等原因。独立的计数器区分所有找到的媒体地址与处理器成功认领的引用，避免未证明的引用悄悄继续使用旧名。

#### 实际写入内容

- **笔记字段：** 根据原始字段值收集准确的替换范围，检查相互重叠并一致地替换。一个字段有多个引用也不会导致后续范围偏移；其他文字保持不变。
- **`media/<canonical>`：** 通过与 `create` 相同的信任条件从已验证域存储取得字节。对于 `pitch_accent`，只接受预期验证器当前自动给出的 `VERIFIED`。旧文件**不会被重命名**；SHA 相同则 `reuse`，否则复制规范字节；规范名称若被其他字节占用，则返回 `destination_media_conflict`。
- **`media_files`：** 在整个 `children` 树检查 `--from` 和规范名称的声明。旧名或规范名在不同节点重复声明会触发 `media_declaration_conflict`，并包含冲突子树路径。规范声明必须覆盖所有被迁移笔记的子树：已有合适声明则保留，否则放在引用者与已有规范声明的最近公共祖先处。新声明位于旧声明和迁移笔记的公共祖先；若是同一节点，则原位替换旧名。其他声明顺序不变。即使没有旧文件声明，也可以更新为规范名称。
- **`media/<from>`：** 发布 `deck.json` 后删除已证明的旧文件名，即使从未在 `media_files` 中声明过。只有写入前验证过、准确基本名称对应的普通文件可以删除。

处理顺序固定：验证引用者与候选、放置规范字节、发布 `deck.json`、最后删除旧文件。若在发布 JSON 后中断，最多留下一份额外文件，而不会产生断开的引用。报告会明确区分执行前的快照和执行结束后文件、声明的真实状态。重试时不会为了“保险”而擅自删除残留文件。

该命令具有幂等性：没有需要迁移的内容时不改变文档任何字节，并返回 `changed: false`。和 `edit`、`create`、`retire` 一样，写入要求规范的 `deck.json`、相同的前后 `ERROR` 校验及共享导出目录锁。不传 `--apply` 时只显示 dry-run 计划，不写 `deck.json`、`media/` 或 `media_files`。

#### “已有笔记不可修改”规则的唯一例外

普通操作不会改变已有笔记的字段。**唯一**例外是 `migrate-media`，而且只能修改由某域处理器拥有的字段内、且与已证明身份/文件名组合属于同一处理器的引用。只要一个引用者不属于该域、为静态引用者或存在歧义，就必须阻止整个操作。

禁止手工编辑 `deck.json`、直接在 `media/` 中删除/重命名文件等绕过方式。只有该命令能保持语义证据、发布规范字节并在牌组树中协调 `media_files`。

#### 拒绝执行的原因

| `reason` | 错误 / 退出码 | 含义 |
|---|---|---|
| `migration_invalid_request` | `invalid_request` / 3 | `--namespace`、`--key` 或 `--from` 无效，或 `--from` 已经等于规范名称 |
| `media_domain_unknown` | `invalid_request` / 3 | `--namespace` 没有对应的已配置处理器 |
| `legacy_media_identity_unproven` | `invalid_request` / 3 | 资源域无法证明准确的（`AssetIdentity`、`--from`）组合；在读取存储前拒绝 |
| `legacy_media_reference_unproven` | `expected_mismatch` / 7 | 至少一个引用者未得到证明；`error.details.references` 包含未证明及静态引用 |
| `kanji_asset_missing`, `pitch_asset_missing`, `asset_integrity_invalid` | `invalid_request` / 3 | 缺少已验证域资源或其信任条件不满足 |
| `destination_media_conflict` | `expected_mismatch` / 7 | 规范名称被其他字节或不安全对象占用 |
| `legacy_media_changed` | `expected_mismatch` / 7 | 制定计划后旧文件变化；保留该文件并停止写入 |
| `media_declaration_conflict` | `invalid_request` / 3 | 树中存在重复的旧名或规范名声明；证据列出冲突子树 |

#### 报告

| 键 | 含义 |
|---|---|
| `export_dir`, `deck_json` | 导出及 `deck.json` 路径 |
| `dry_run`, `applied`, `changed` | 模式、是否替换文件、是否改变内容 |
| `identity`, `namespace`, `key` | 被迁移资源的身份 |
| `legacy_filename`, `canonical_filename`, `canonical_sha256`, `canonical_action` | 文件名、规范字节 SHA-256 及放置动作（`copy`/`reuse`） |
| `references`, `references_total`, `references_truncated` | 已证明引用（`guid`、模型、字段、`ord`）、总量及最多 50 条的截断标记 |
| `media_files_added`, `media_files_removed` | 声明的变化 |
| `legacy_declared`, `legacy_media` | 写入前快照：旧名是否在树中声明、物理文件名/大小/SHA-256；不存在则 `null` |
| `legacy_declared_after`, `legacy_media_exists_after` | 执行完成后声明与文件的真实状态；dry-run 保留原状 |
| `legacy_released` | 只有成功 `--apply` 并实际删除旧文件时才为 `true` |
| `validation` | 前后计数和新增问题代码 |

### `migrate-layout`

将仓库旧布局中 `decks/` 下的导出目录一次性、安全地迁移到扁平的 `decks/output/<导出根名称>/`。不改变导出内部的 `deck.json`、`media/` 或身份标识。

```bash
# 1. 先查看计划：哪些内容移动到何处
anki-repo migrate-layout --root . --json

# 2. 写入：移动导出目录并移除旧布局的占位文件
anki-repo migrate-layout --root . --apply
```

迁移如何得到证明：写入前，工具枚举每份导出中的**所有**文件，记录大小、SHA-256，并合成为一个内容指纹。计划在获取锁前生成，因此实际迁移前会重新计算源指纹；如果不同，则在修改布局前拒绝。迁移后再次验证目标指纹；不符就返回 `source_changed`，不能宣称成功。在同一文件系统内，整份导出通过一次 `rename` 移动。若无法 `rename`，先复制到目标旁的临时 `.anki-repo-migrate-tmp-*` 目录，验证副本指纹后用 `rename` 发布，最后才删除源目录。

出现以下任一情况就会**拒绝整个迁移，不移动任何导出**：

- 两份导出产生相同扁平目标名称，或与 `decks/output/` 中已有项冲突（比较时不区分大小写和 Unicode 规范化差异）：`target_name_collision`；
- 目标存在但内容与源不同（`target_conflict`），或在制定计划到写入之间突然出现（`target_appeared`）；
- 源或其内部项是符号链接（`source_is_symlink`、`source_contains_symlink`），或目标是符号链接（`target_is_symlink`）；
- 导出根名称或内部文件名不是有效 UTF-8（`source_non_utf8_name`）；
- 源消失或不是目录（`source_missing`、`source_not_directory`），或无法读取（`source_unreadable`）；
- 缺少 `deck.json`（`deck_json_missing`）；
- 目标存在但无法读取（`target_unreadable`）；
- `decks/` 自身包含 `deck.json`（`legacy_area_is_export`）：不能把含 `decks/input`、`decks/output` 的区域根目录整体当成导出移动。

拒绝时返回退出码 3，报告中 `refused: true`，所有导出都不移动。

幂等性：目标已存在且与源**逐字节相同**时不覆盖目标，只返回 `already_present` 并清理重复源。因此，中断的迁移可通过再次 `--apply` 完成；已经完成的迁移再次运行时不做任何事。首次写入前会检查剩余磁盘空间；不足时返回带有 `required_bytes` 和 `available_bytes` 的 `write_failed`。

仅在旧区域没有其他内容时才删除零字节 `.gitkeep` 占位文件和空目录。只要存在一个无关文件，旧区域整体保留，并在 `legacy_area_retained` 诊断中列出路径。

整个写入期间旧区域受 `ExportLock` 保护，防止并发迁移相互交错。

#### 报告

| 键 | 含义 |
|---|---|
| `root` | 仓库绝对根目录；其他路径均相对于该根目录 |
| `legacy_area`, `output_dir` | 旧布局区域和扁平目标 |
| `apply`, `applied`, `refused` | 是否请求写入、实际应用以及是否出现拒绝 |
| `exports[]` | 各导出的 `name`、`source`、`target`（相对根目录）、`action`（`move` / `already_present` / `refused`）、`state`（`planned` / `applied` / `refused`）、`files`、`bytes`、`content_sha256`、`reason`、`message` |
| `pruned` | 被删除的旧占位文件和空目录（相对根目录） |
| `diagnostics` | `symlink_skipped`、`legacy_area_absent`、`nothing_to_migrate`、`legacy_area_retained`；`paths` 相对根目录 |
| `summary` | `exports`、`planned`、`applied`、`already_present`、`refused`、`files`、`bytes` |

#### 如果旧布局已经被 Git 跟踪

`.gitignore` 不会自动取消已进入 Git 索引的文件。运行 `migrate-layout --apply` 后，需要使用 `git rm -r --cached decks/<旧区域>` 将旧文件从索引移除，并提交此次删除。反过来，其他工作副本执行 `git pull` 获取“删除已跟踪文件”的提交时，也会删除其本地对应文件。获取这样的提交之前，应备份 `decks/`。

### `retire`

通过向笔记标签列表末尾追加一个标签将其停用，其他内容均不改变。工具集不提供物理删除笔记，防止笔记悄然丢失；使用者可以在 Anki 中自行删除。

```bash
EXPORT=decks/output/Words__N1

# 单个 guid 的 dry-run
anki-repo retire "$EXPORT" --guid "w0Wc3Ip@N#" --tag "archived::auto"

# 写入
anki-repo retire "$EXPORT" --guid "w0Wc3Ip@N#" --tag "archived::auto" --apply

# 多条笔记，以及来自文件或 stdin 的批量请求
anki-repo retire "$EXPORT" --guid "…" --guid "…" --tag "archived::auto" --apply
anki-repo retire "$EXPORT" --request retire.json --apply
cat retire.json | anki-repo retire "$EXPORT" --request - --apply
```

批量请求文件：

```json
{
  "schema_version": 1,
  "tag": "archived::auto",
  "notes": [
    { "note_id": "н1", "guid": "w0Wc3Ip@N#" }
  ]
}
```

#### 每条笔记的三种结果

| 状态 | 含义 |
|---|---|
| `retired` | 原来没有该标签，已经追加（或 dry-run 计划追加） |
| `dry_run` | 未要求写入 |
| `already_retired` | 标签已存在，不重写文件也不重复添加 |

标签按 Anki 规则验证：不能为空，不能包含空白或控制字符。比较忽略大小写，包括非 ASCII 和西里尔字母，因此 `тэг` 和 `ТЭГ` 是相同标签。`already_retired` 可以识别任意大小写，不会添加第二种写法。导出中重复 `guid` 已属于 `validate` 级别的 `ERROR`，所以写入前被阻止；指定的 `guid` 不存在则产生 `unresolved_guid`（退出码 3）。

#### 保证

- 只修改目标笔记的 `tags`；`fields`、`guid`、`note_model_uuid` 及笔记在 `notes` 中的位置逐字节保持不变。
- 停用后仍可通过 `find --guid` 定位该笔记（报告中的 `retired_notes_still_resolvable`）。
- 保留原标签顺序，新标签只追加到末尾，不会重复。
- 写入前重新解析候选，验证只追加标签（`only_tags_appended`、`tags_appended_verified`），并比较前后的 `validation`。
- 要求规范格式的 `deck.json`，使用原子且防并发覆盖的写入机制。

#### 报告

| 键 | 含义 |
|---|---|
| `dry_run`, `applied`, `tag` | 模式、文件替换状态、停用标签 |
| `source_bytes`, `candidate_bytes`, `byte_delta` | 大小及差值 |
| `notes_total`, `notes_retired`, `notes_already_retired` | 各状态计数 |
| `outcomes` | 每条笔记的 `note_position`、`previous_tags`、`tags`（最多 50 条） |
| `checks` | 五项强制检查的结果 |

`retire` 有意不提供 `--emit-resolved`：请求本身已经明确，不需要额外解析。

### `visual-report`

静态比较两份导出状态的报告，通过 `guid` 将“修改前”和“修改后”目录中的笔记配对。输出是可以在无网络、无服务器环境下用浏览器打开的独立目录，包含 `index.html`、`cards/*.html` 卡片预览、按状态保存的 `media/<state>/*` 媒体，以及所有权清单 `report-manifest.json`。

修改过的笔记显示**两个彼此独立的预览框**：“修改前”和“修改后”，每个框都使用其自身状态中的实际数据生成。新创建的笔记只有“修改后”，在新状态消失的笔记只有“修改前”，不会凭空造出不存在的另一侧。宽屏时并排显示，窄屏时纵向堆叠。

报告的交互功能完全由生成文件内置的代码完成：明暗主题切换、根据内容自动调整框高、本地音频播放。这不意味着支持 Anki 的全部模板行为；下述信任边界由测试严格验证。

```bash
EXPORT=decks/output/Words__N1

# 对比牌组与其修改后的副本
anki-repo visual-report \
  --before "$EXPORT" \
  --after /path/to/после \
  --out /path/to/отчёт \
  --retire-tag "archived::auto"

# 每个分区详细显示多少条笔记（默认 200，最多 1000）
anki-repo visual-report --before … --after … --out … --preview-limit 50
```

每条笔记恰好归类为一种结果：

| 结果 | 条件 |
|---|---|
| `created` | `guid` 只存在于修改后状态 |
| `changed` | 两边都有 `guid`，但字段值不同 |
| `retired` | 两边都有 `guid`，且修改后新增 `--retire-tag` |
| `removed` | `guid` 只存在于修改前状态 |
| `unchanged` | 两边都有 `guid`，字段和标签均相同 |
| `ambiguous` | 任一状态中 `guid` 重复：报告给出 `ambiguous_guid` 诊断，**绝不猜测**对应哪条笔记 |

`removed` 只是观察结果，不是删除指令：CrowdAnki 不会仅因笔记从 JSON 中消失就将其从 Anki 删除，`retire` 也不会物理删除。报告不模拟导入决策，也不声称 Anki 将创建、更新或保留什么。

#### 报告包含哪些内容

- 各结果计数、修改前后笔记数量、实际生成的预览数量。
- `--preview-limit` 对**各个结果分组分别**限制详细展示的笔记数。无论设置多少，最多生成 4000 个预览文件；`--json` 清单最多显示 500 条，`card_files_total` 仍记录真实生成总数。
- 按结果分类的章节：修改前/后字段、按 HTML token 而不只是屏幕可见文字计算的逐行差异，以及标记新增项的标签列表。
- `cards/` 下独立的卡片预览文件，使笔记类型 CSS 与报告自身 CSS 相互隔离。修改的笔记生成两个状态文件，新增/消失的笔记只生成一个。
- 按状态标注的诊断：媒体或音频缺失、包含路径的引用、外部链接、符号链接、过大文件、重复 GUID、不完整预览。
- 所涉及笔记类型中的不受支持模板结构。
- 明确列出报告的局限，避免读者误以为预览与 Anki 实际渲染完全一致。

#### 卡片预览：支持与不支持的内容

模板解析器是静态的，只展示无需启动 Anki 就能计算的内容：

- `{{Поле}}`：替换为字段值；字段名区分大小写。
- `{{#Поле}}…{{/Поле}}` 和 `{{^Поле}}…{{/Поле}}`：针对非空和空值的条件段，包括嵌套。纯空白内容、`<br>` 和 `<div>` 视为空。
- 特殊字段 `Tags`、`Type`、`Deck`、`Subdeck`、`Card`、`FrontSide`。`FrontSide` 分两次渲染，将正面内容嵌入背面。
- 包装类 `card card{N}`，其中 `N` 为模板 `ord` 加一；笔记类型 CSS 在基础样式之后加载。

此外，报告只模拟可视化验证真正依赖的 Anki 行为：

- **夜间模式：** 切换主题时为文档根节点、`body` 和卡片本身添加 `nightMode`，让笔记类型 CSS 中的 `.card.nightMode` 与 `.nightMode …` 选择器按 Anki 方式生效。
- **音频：** `[sound:名称.mp3]` 不会作为普通文字保留，而会转换成基于已复制音频、使用 `<audio preload="none">` 的本地重播按钮。
- **预览框高度：** 等待媒体加载后测量内容高度并汇报，而不是写死一个高度常量。

有意**不支持**：执行 Anki 模板或字段内的 JavaScript（在 Anki 中 `<script>` 会执行，但静态报告不会执行；该面标记为未渲染，并把标签显示为可见文本）、计算 `cloze`、`type`、`tts`、`furigana`、`hint` 等过滤器、运行时字段 `CardID` 和 `CardFlag`，以及通过笔记类型 CSS 引用的媒体（不复制 `url(…)`）。这些结构在预览中按原始文字显示，并在独立的限制章节列出。

#### 媒体文件

只复制**实际显示的**预览所引用、且确实存在于对应状态 `media/` 的文件。两个状态分别存放在 `media/before/<名称>` 和 `media/after/<名称>`。同名文件在修改前后可能是不同的字节；共用路径会导致“修改前”误显示“修改后”的内容。因此，修改前缺少文件时，也不能拿修改后同名文件代替。只有文件复制成功后才将引用改写为 `../media/<state>/<name>`。音频 `[sound:<name>]` 遵守相同的状态隔离规则；未能解析的音频会保留原始可见文本，同时显示警告和诊断。

外部地址（`http://`、`https://`、`//`、`data:`）不会被下载或替换。包含路径的引用（`../../секрет`）绝不直接读取；仅允许复制基本文件名，同时把原始引用记录为诊断。不适合作为 URL 的文件名不会被复制：路径分隔符（`/`、`\`）、URL 特殊字符（`:`、`?`、`#`、`%`）、HTML 特殊字符（`"`、`&`、`<`、`>`）和控制字符使其无法安全访问。此类名称会出现在 `media.traversal` 和 `media_name_unusable` 诊断中，不会悄悄省略。绝不读取符号链接；超过 64 MiB 的文件不复制，最多复制 4096 个文件。`media_files` 只是一份引用声明，并不能证明实际文件存在；缺少的文件会报告诊断，而不是凭空补全。

#### 生成结果中的信任边界

报告要在浏览器中打开，因此必须区分其自有程序代码与不可信数据：

- **允许报告自有运行时代码：** 主题切换、动态高度和音频播放需要它。这些代码直接写入生成文件，行为确定且无需网络。
- **没有外部依赖：** 报告代码与数据都不允许 `<script src=…>`、CDN、外部字体或网络引用。
- **绝不执行 Anki 模板代码：** 模板或字段值中的 `<script>` 属于不支持的结构，相关一面视为未渲染，标签只显示为文字，不作为可执行标记。
- **按结构验证地址：** 将地址解析为相对于报告根目录的路径，只有路径确实属于生成报告的文件集才允许。`http:`、`https:`、`data:`、`javascript:` 等协议、绝对路径、反斜线、`?`、`#`、`%` 以及规范化后逃出报告根目录的路径都会被拒绝，不能只检查看起来像本地路径的前缀。`srcset` 可能包含多个地址，必须逐个候选检查。
- **把用户数据作为文字而非可执行 HTML 渲染：** `<script>`、`on*` 事件处理器、禁止的元素（`iframe`、`object`、`embed`、`link`、`meta`、`base`、`form`、`applet`、`frame`、`frameset`）及外部地址都会失去执行能力。拒绝内容不会悄然消失；预览显示“未展示……”标记及 `preview_construct_blocked` 诊断。不为事件处理器设置例外；即使字段值含有 `data-report-runtime`、`data-report-state` 或 `data-report-theme-value`，也不能因此获得权限，因为任何人都可以伪造报告运行时读取的属性。
- **大小写或写法不影响判断：** `<SCRIPT>`、`OnErRoR`、`SRC = "…"` 和未闭合引号都按其标准语义检查。
- **遵循浏览器的 HTML 解析语义，而不只是原始字符串：** HTML5 忽略非空元素的自闭合 `/`，所以 `<script/>` 实际打开脚本元素，后续内容必须安全地显示为文字。注释按浏览器规则结束（`-->`、`--!>`、空注释 `<!-->`、`<!--->`），避免错误解析注释后执行本应被检查的标签。属性中的 HTML 实体会在地址检查前解码，例如 `url&#40;a.png&#41;` 在浏览器看来是 URL。未知或含糊的实体（如 `&lpar;`、缺少分号的 `&amp`）会直接拒绝，而非忽略。
- **笔记类型 CSS 仅用于样式：** 不加载 `@import`，非本地 `url(…)` 被替换为无效值；CSS 或字段值中的 `</style>` 不能关闭样式元素并引入新标签，因为 `<` 已被转义。`style` 属性先进行实体解码再解析 CSS，输出时对引号转义，避免属性提前结束。
- **CSS 使用词法分析器而不是简单查找子串：** 无论写法如何，都识别 `url(…)` 和 `@import`，包括转义函数名（`u\72l(`）、转义 URL 字符（`url(https\3a //…)`）以及未闭合的 `url(`。不能无歧义解析的内容按地址处理并阻止，而不是当作普通文本。
- **可验证性：** 单一函数 `report::sanitize::inspect` 判断生成文件中是否遗留外来可执行内容或网络地址。**每个** `index.html` 和 `cards/*.html` 文件都必须先通过内存字符串检查，写出后从磁盘重新读取再检查。验证的是用户实际打开的文件，不只是内存中间结果。自有运行时代码通过最小 DOM 测试环境执行验证。失败时返回 `preview_not_offline` 或 `index_not_offline`，不是警告。

有意不使用 CSP 哈希及 iframe `sandbox`：若不设置 `allow-same-origin`，沙箱预览获得不透明 origin，将无法读取自己复制的媒体文件，直接破坏报告的目的。严格净化用户数据比沙箱隔离更适合本地离线预览。

#### 报告目录的边界及所有权

`--out` 必须位于所有 `decks/**` 及被比较的导出目录之外。报告可能写入数百个文件；将其放入被 Git 忽略的用户数据目录，相当于在用户不知情时修改牌组区域。

仅当非空目录能通过 `report-manifest.json` **证明属于本报告**时，才允许继续写入。该清单记录前一次生成的文件。存在普通 `index.html` 不足以证明所有权，因为其他应用也可能拥有同名文件。

规则如下：

- 不存在或为空的输出目录可以创建或填充。
- 有有效清单的目录允许重新生成：本次报告文件更新到各自位置，前次报告中本次不再需要的文件会删除，并由 `out_dir_stale_removed` 逐一列出。
- 没有清单的非空目录返回 `out_dir_not_owned`，**不修改任何内容**，即使其中有 `index.html`。
- 清单损坏、属于其他来源或指向目录外部时返回 `out_dir_manifest_invalid`，不执行修改。
- `--out` 是符号链接时返回 `out_dir_is_symlink`，**不修改**；否则可能通过链接控制并未选择的目录。
- 报告目录**内部**出现符号链接时返回 `out_dir_entry_conflict`，不修改，也不会跟随目录链接遍历。否则所谓“报告内部”可能实际上指向外部。目录所有权只允许按实际名称替换自己的条目，不能通过别名修改其他位置。同理，清单标为文件的路径不能实际是目录，报告结构要求目录的位置不能是文件。
- 只允许覆盖有证据证明属于报告的文件：前次清单中的文件，或在报告控制的 `cards/`、`media/` 内且名称由报告生成的文件。此例外不包括输出根目录中的普通 `index.html`，那里可能存在其他内容，必须通过清单证明所有权。其他对象占用目标路径时返回 `out_dir_entry_conflict`，不修改。
- 清单和报告结构中均不存在的文件或目录属于外部内容，绝不删除或覆盖，只在 `out_dir_foreign_files` 中报告。“文件消失”和“文件不属于报告”是不同情况。

写入顺序用于保证可处理的故障不会损坏旧结果：

1. 在 `--out` 旁创建临时构建目录（`.<名称>.staging-<pid>`），**完整**构建新报告。读取导出、规划和复制媒体、信任检查都不会影响旧报告。
2. **首次修改输出目录前**验证整个新文件集合，包括所有目标路径、上级目录及过期名称；不能先部分覆盖，再把拒绝视为无害。
3. 通过 `rename` 将完整暂存文件移动到目标位置。
4. 删除清单中已过期的自有文件，并移除报告结构中的空子目录。`remove_dir` 不删除非空目录，因此外部内容仍会保留。
5. **最后**写入清单；此前目录仍属于前一版报告。
6. 完成后按各文件**真正的规范化路径**（`canonicalize`），而不是按路径字符串验证位置；文件不在输出目录内时返回 `report_file_not_confined`。
7. 成功或失败后都会删除暂存目录。

如果进程恰好在步骤 3 和 5 **之间**中断，目录可能保留新旧混合文件和旧清单，但仍可读取。下一次成功执行会收敛到当前文件集合：按报告生成的名称替换 `cards/`、`media/` 内的文件，并根据旧清单删除过期文件。不符合所有权规则的外来条目原样保留并记录诊断。所有工具可以自行处理的**生成阶段**失败都发生在步骤 3 之前，不会修改原有输出目录。

`all_files_inside_out_dir` 验证最终所有报告文件都实际存在于输出目录内；`media_confined_to_out_dir` 验证复制的媒体处于对应状态的子目录。

#### 报告

| 键 | 含义 |
|---|---|
| `before`, `after` | 各状态的目录、`deck.json`、牌组名称、`crowdanki_uuid` 及笔记、牌组、笔记类型数量 |
| `out_dir`, `index_html` | 报告目录及入口文件 |
| `card_files`, `card_files_total` | 预览相对路径及实际总数 |
| `preview_files`, `preview_files_truncated` | 各预览的文件、状态（`before`/`after`）、`guid`、类型、模板、提示、已解析媒体及缺失音频 |
| `retire_tag` | 使用的停用标签 |
| `counts` | 结果、预览及修改前后笔记计数 |
| `outcomes` | 各笔记的 `kind`、牌组、类型、前后标签、变化字段、GUID 出现次数（最多 500 条） |
| `diagnostics` | 诊断代码、级别、消息和对象 |
| `unsupported_constructs` | 预览无法计算的结构及其原因 |
| `media` | 总体及按状态（`states`）列出的媒体复制情况与问题 |
| `limitations` | 与 `index.html` 相同的限制列表 |
| `checks` | 八项强制检查结果 |

报告具有确定性：相同的两个输入状态会生成**逐字节相同**的文件，包括 `report-manifest.json`。清单不包含时间戳、报告目录名或调用路径，文件列表排序固定。报告也不输出时间或版本，因此不会仅因时间经过而自行“过期”。

### 典型工作流程

```bash
EXPORT=decks/output/Words__N1

# 1. 导出中究竟有什么
anki-repo qa "$EXPORT"

# 2. 针对已发现问题的紧凑批次
anki-repo --json review "$EXPORT" --qa-code empty_field_value --limit 25 > batch.json

# 3. 外部智能体读取 batch.json 并返回建议文档
#    (schema_version, proposals: proposal_id, guid, field, expected, replacement)

# 4. 按导出当前状态验证建议
anki-repo --json review-check "$EXPORT" --proposals proposals.json \
  | jq .result.edit_request > request.json

# 5. 用现有命令完成 dry-run、写入和结果验证
anki-repo edit "$EXPORT" --request request.json
anki-repo edit "$EXPORT" --request request.json --apply
anki-repo validate "$EXPORT"
```

添加新笔记与停用旧笔记也采用类似流程：

```bash
EXPORT=decks/output/Words__N1

# 1. 实际字段结构：字段名称及其中内容
anki-repo models "$EXPORT"

# 2. 默认情况下新值不含媒体引用；
#    例外是显式配置的资源域处理器
anki-repo create "$EXPORT" --request create.json            # dry-run
anki-repo create "$EXPORT" --request create.json --apply --emit-resolved create.resolved.json
anki-repo validate "$EXPORT"

# 3. 停用的笔记不会被删除，只会打上标签
anki-repo retire "$EXPORT" --guid "…" --tag "archived::auto" --apply

# 4. 用两个状态的静态报告说明具体改了什么
anki-repo visual-report --before "$ЭТАЛОН" --after "$EXPORT" --out /tmp/отчёт \
  --retire-tag "archived::auto"
```

职责被刻意分离：`qa` 和 `review` 以确定性方式选择材料，外部智能体提出内容修改，`review-check` 根据事实验证建议，只有 `edit`、`create`、`retire`、`migrate-media` 可以通过明确的 `--apply` 写入 `deck.json`。判断需要哪些笔记、哪些笔记该停用由人或智能体决定；工具集只确认操作可执行，并准确执行请求的内容。

#### JSON 模式的报告

| 命令 | `result` 中的键 |
|---|---|
| `qa` | `export_dir`, `notes_total`, `findings_total`, `findings_returned`, `unaddressable_findings`, `truncated`, `max_per_code`, `codes`, `by_code`, `rules`, `findings` |
| `review` | `export_dir`, `selection`, `notes_total`, `total_selected`, `excluded_unaddressable`, `offset`, `limit`, `returned`, `truncated`, `next_offset`, `items` |
| `review-check` | `export_dir`, `deck_json`, `outcome`, `proposals_total`, `counts`, `effective_proposals`, `proposals_truncated`, `source_editable`, `source_blockers`, `proposals`, `edit_request` |
| `models` | `export_dir`, `deck`（`path`、`crowdanki_uuid`、`preorder`、`notes_in_deck`）, `sample_limit`, `models` |
| `create` | `export_dir`, `deck_json`, `dry_run`, `applied`, `source_bytes`, `candidate_bytes`, `byte_delta`, `notes_total`, `notes_created`, `notes_already_applied`, `outcomes`, `outcomes_truncated`, `decks_touched`, `validation`, `checks` |
| `retire` | `export_dir`, `deck_json`, `dry_run`, `applied`, `tag`, `source_bytes`, `candidate_bytes`, `byte_delta`, `notes_total`, `notes_retired`, `notes_already_retired`, `outcomes`, `outcomes_truncated`, `validation`, `checks` |
| `migrate-media` | `export_dir`, `deck_json`, `dry_run`, `applied`, `changed`, `identity`, `namespace`, `key`, `canonical_filename`, `canonical_sha256`, `canonical_action`, `legacy_filename`, `legacy_declared`, `legacy_media`, `legacy_media_exists_after`, `legacy_declared_after`, `legacy_released`, `references_total`, `references_truncated`, `references`, `media_files_added`, `media_files_removed`, `validation` |
| `visual-report` | `before`, `after`, `out_dir`, `index_html`, `card_files`, `card_files_total`, `preview_files`, `preview_files_truncated`, `retire_tag`, `counts`, `outcomes`, `outcomes_truncated`, `diagnostics`, `unsupported_constructs`, `media`, `limitations`, `checks` |

对于 `migrate-media`，`identity` 包含 `namespace` 和 `key`。文件不存在时 `legacy_media` 为 `null`；存在时包含 `filename`、`byte_length` 和 `sha256`。每个 `references` 对象包含 `guid`、`model_uuid`、`model_name`、`field` 和 `field_ord`。`validation` 包含 `before`、`after`、`new_error_codes` 和 `new_warning_codes`，其中 `before` 和 `after` 又分别包含 `errors`、`warnings`、`info`。

在 `review.items` 中，笔记字段按类型的 `ord` 顺序存放在 `fields` 对象内，与 `find` 相同。`qa.findings` 中不绑定特定字段位置的规则，其 `field_ord` 为 `null`。可寻址状态（`qa` 的 `addressable`、`review` 的 `excluded_unaddressable`）及重复分组信息（`qa` 的 `group_size`、`related_note_indices`/`related_guids`；`review` 的 `group_membership`、`related`、`group_size`、`related_truncated`）始终存在，即使没有找到分组、计数为零。

### `edit`

用于修改**已有笔记字段值**的命令。每次编辑通过（笔记 `guid`、字段名）定位一条已有笔记的一个字段。先在导出中查找 GUID，再根据该笔记的 `note_model_uuid` 和 `note_models[].flds[].ord` 解析字段名，最后将新值写入 `fields[ord]`。

不带 `--apply` 时执行完整的 **dry-run**：计算候选文件并执行全部验证，但不写入。带 `--apply` 时仅在所有检查通过后原子替换文件。

```bash
# 单条编辑：dry-run
anki-repo edit decks/output/Words__N1 \
  --guid 'D!TAYVuHi,' --field 'Значение' --expect 'было' --set 'стало'

# 相同编辑并写入
anki-repo edit decks/output/Words__N1 \
  --guid 'D!TAYVuHi,' --field 'Значение' --expect 'было' --set 'стало' --apply

# 从请求文件批量编辑
anki-repo edit decks/output/Words__N1 --request edits.json --apply

# 从 stdin 批量编辑
anki-repo edit decks/output/Words__N1 --request - --apply < edits.json
```

单条编辑必须同时指定 `--guid`、`--field`、`--expect`、`--set` 四个参数。批量编辑使用一个 `--request`，值为 JSON 文件路径或代表 stdin 的 `-`。不能同时使用 `--request` 和 `--guid`。

上述 GUID、字段名和值都只是示例。`--expect` 是**前置条件**，不是提示，必须与真实的当前字段值相同。如果不一致，整个冲突批次**不执行**，从而避免基于过期信息修改。

#### 请求格式

```json
{
  "schema_version": 1,
  "edits": [
    {
      "edit_id": "необязательный идентификатор для отчёта",
      "guid": "D!TAYVuHi,",
      "field": "Значение",
      "expected": "было",
      "replacement": "стало"
    }
  ]
}
```

- 必须提供 `schema_version`，且值只能为 `1`；不支持的版本被拒绝。
- `edits` 包含 1–20,000 条编辑，请求文件最大为 8 MiB。
- `edit_id` 可选，提供时必须唯一。
- 同一个（`guid`、`field`）组合不能在请求中重复，否则编辑顺序和结果会产生歧义。
- 不允许未知键，避免请求键拼写错误被悄然忽略。

#### 每条编辑的四种结果

| 状态 | 条件 | 写入 |
|---|---|---|
| `noop_identical` | 当前值同时等于 `expected` 和 `replacement` | 无需写入 |
| `applied` | 当前值等于 `expected`，但不同于 `replacement` | 已执行 |
| `dry_run` | 相同条件，但没有 `--apply` | 未执行 |
| `already_applied` | 当前值已经等于 `replacement` | 无需写入 |

`already_applied` 和 `noop_identical` 是成功结果。重复运行同一条 `--apply` 命令仍以退出码 `0` 完成，并且**不会**重写文件或改变其 `mtime`。如果所有编辑都无需修改，报告中的 `applied` 为 `false`。

如果当前值既不等于 `expected`，也不等于 `replacement`，则是前置条件冲突：`expected_mismatch`，退出码 7。批量请求中的任何编辑都不会执行，报告会列出全部冲突及预期值、实际值的样本。

#### 保证

只有符合仓库**严格规范格式**的 `deck.json` 才可编辑：LF 换行、四空格缩进、键按字母排序、非 ASCII 字符不使用 `\u` 转义、文件末尾**没有换行符**。工具会逐字节比较源文件与自己通过 `serde_json` 的 `PrettyFormatter`（四空格）生成的结果，因此还要遵守相应的数值格式（`1e3` 写成 `1000.0`）及控制字符转义规则：`\b`、`\f`、`\n`、`\r`、`\t`，其他 U+0000–U+001F 使用 `\uXXXX`。这并非纯粹的格式要求：规范序列化保证除目标字符串外，文件其他字节不会被重新格式化。压缩 JSON、两空格缩进、CRLF 或末尾换行都会以 `source_not_canonical`（退出码 3）拒绝，绝不自动规范化。

写入前检查：

- 源文件确实规范，否则 `source_not_canonical`；
- 导出中没有 `ERROR`（`export_invalid`），也没有 `conflicting_note_model_definition`（`export_not_mutable`）；类型定义冲突会使字段名寻址产生歧义；
- 未类型化 JSON 树与类型化树中的笔记位置、`guid`、`note_model_uuid` 完全对应，确保不会写到错误的 `fields[ord]`；
- 所有目标都可明确定位，GUID 唯一，字段属于相应笔记类型；
- 重新解析候选后得到的值与计划完全一致；
- 字节差异**恰好**对应所请求的编辑：变化行数等于有效编辑数，每个不同的行恰好只有一个字符串 token 变化，“旧值 → 新值”的多重集合与请求一致；
- 文件大小的变化可由替换 token 的长度（包括转义）解释；
- 候选验证没有增加任何 `ERROR`；新 `WARNING`、`INFO` 可以出现，但必须在报告中展示；
- 文件替换具备原子性：先在同目录写入临时文件并同步到磁盘，再保留权限地 `rename` 覆盖 `deck.json`。发生错误时移除临时文件，原 `deck.json` 保持不变。

上述检查是工具的内部不变量；如果违反，说明工具本身存在缺陷，返回 `internal_error`（70）且不写入。

#### 并发执行 `--apply`

源文件比较和候选发布必须处于同一个临界区。检查 `deck.json` 字节之前，对稳定的导出目录取得独占协作锁，一直持有到 `rename` 完成。如果只执行独立的 `read → compare → rename`，另一个写入进程可能在比较之后发布新文件，随后被当前操作悄悄覆盖。

实际含义：

- 同一份导出的 `edit`、`retire` 和 `create --apply` 共享目录锁。落后的写入者在持锁状态下重新检查源文件；快照过期时得到 `source_changed`（退出码 7）。替换 `deck.json` 的 inode 不影响被锁定的目录，因此发布顺序得以保证。
- 进程退出后由内核释放锁；`anki-repo` 崩溃不会留下永久卡住的锁。
- 获取锁会**等待**，而不是立刻失败。这样等待之后会产生具体的 `source_changed` 领域结果，而不是随意的锁获取错误。
- 该保证适用于遵守协议的 `anki-repo` 进程。忽略锁的外部写入者只能在持锁期间通过字节比较发现：比较前发生的变化产生 `source_changed`；比较与 `rename` 之间的外部写入无法仅靠可移植的 `std` API 阻止，因此不承诺更强保证。
- 只支持 Linux；其他平台无法编译该工具，所以 `lock_unsupported_platform` 备用分支不会成为实际运行结果。

#### 编辑限制

- 只修改字段**值**；`fields` 数组不能增减元素。
- 不能通过此命令修改 `guid`、`note_model_uuid`、`tags`、`media_files`、笔记类型、配置或牌组树。
- `edit` 不读取也不修改 `media/`。需要有证明的媒体引用迁移时，必须使用单独的 `migrate-media`。
- 每次调用只处理一份导出。

#### 报告

人类可读报告包含执行模式、修改前后大小、变化行数、各编辑状态及新旧值片段、检查结果与前后验证对比。JSON 模式下 `result` 包含：

| 键 | 含义 |
|---|---|
| `export_dir`, `deck_json` | 导出路径及 `deck.json` |
| `dry_run`, `applied` | 模式及文件是否替换 |
| `source_bytes`, `candidate_bytes`, `byte_delta` | 文件大小及差值 |
| `changed_lines`, `first_changed_line` | 变化行数与第一处变化行 |
| `edits_total`, `effective_edits` | 请求的编辑数量与实际改变值的数量 |
| `summaries` | 各状态计数 |
| `outcomes` | 最多 50 条编辑结果及 `outcomes_truncated` |
| `validation` | 前后计数及新增问题代码 |
| `checks` | 五项强制检查结果 |

## 公共契约变更

### 新增的命令和保证

- 新增 `models`、`create`、`retire`、`migrate-media`、`migrate-layout` 和 `visual-report`。修改导出内容的命令共有四条（`edit`、`create`、`retire`、`migrate-media`）；第五条写入命令 `migrate-layout` 修改的是目录布局，而**不是**导出内容。新增命令都不会创建牌组或笔记类型。`create` 只能在已有类型和牌组下新增笔记，并按明确规则放置已验证媒体；`retire` 只追加标签。
- `visual-report` 在独立目录中生成报告，**绝不**写入导出或 `decks/**`；首次写入前进行检查，完成后按实际生成路径复核。
- `migrate-layout` 是独立的布局迁移，区别于四条修改导出内容的命令。`--apply` 将旧布局导出目录移动到扁平 `decks/output/`、删除占位文件和逐字节一致的源目录，并用 SHA-256 验证内容。不会写入 `deck.json` 或改变身份。
- `decks/input` 是明确的 API 受限区域；任何指向该目录的路径都会返回 `invalid_request` 和 `details.reason = input_area_forbidden`，不存在绕过选项。
- 不提供物理删除笔记的功能。笔记在 JSON 中消失仅作为报告结果 `removed` 展示，所以停用操作选择追加标签。

### 修正过的契约

- `models` 按 `ord` 排序字段并从对应 `ord` 获取示例值。旧实现按 `flds` 声明顺序取得样本，可能把值错误地对应到其他字段，使字段语义证据不可信。
- `models` 的牌组筛选条件（`--deck`、`--deck-uuid`、`--deck-preorder`）可以组合使用。发生不一致时会返回带有差异详情的 `deck_identity_mismatch`，而不是参数解析错误。`create` JSON 请求中的条件也复用该验证逻辑。
- `create --apply` 要求目标牌组声明 `crowdanki_uuid`（`unresolved_deck_identity`），而**不是**要求调用方必须通过 UUID 选择牌组。按名称选择仍然合法，但写入后的目标必须具有稳定身份。
- `visual-report` 中不完整的卡片预览（如模板含 `<script>`）现在除了 HTML 提示外，也会在 JSON 中报告 `preview_incomplete`。
- `visual-report` 的 `counts.previews` 是实际写出的预览文件数量，不再是写入前估计。修改笔记生成前后两个文件；`--preview-limit` 限制详细展示的笔记数，不拆分状态配对。
- 预览媒体只从各自状态导出的 `media/` 查找，只有实际复制成功才重写为 `../media/<state>/<name>`。
- 只被修改前状态引用的文件只从该状态的媒体目录复制，绝不从修改后状态补全。
- 模板或字段中的 `<script>` 显示为文字并标记为不支持，绝不执行。

### 已移除的内容

两项功能被有意移除，因为它们把某个特定笔记类型的语义变成了整个工具集的永久契约，而本工具支持任意 CrowdAnki 导出，不应假设具体字段名。

- 从 `find` 和 `review` 中移除 `--word`。它只是针对特定字段 `Слово` 原始值的子串查找快捷方式。现在通过 `--field <字段名> --value <值> --match contains` 明确表达相同语义。
- 移除 QA 规则 `duplicate_primary_field`。所谓“主字段/词条字段”的概念属于特定笔记类型，并不是 CrowdAnki 统一定义的。规则表恰好保留五项：`empty_field_value`、`leading_whitespace`、`trailing_whitespace`、`forbidden_white_span`、`duplicate_note_content`；分组元数据 `related`、`group_size`、`related_truncated` 归 `duplicate_note_content` 管理。
- 同时从 `selection` 模块移除公开常量 `PRIMARY_FIELD`。调用者自行提供字段名，由 `ensure_known_field` 对照实际 `note_models[].flds[].name` 验证，未知字段返回 `unknown_field`（退出码 3）。

旧契约不会被悄然重新接受：`qa --code duplicate_primary_field` 和 `find`/`review --word` 会因未知代码或参数失败，并由 `tests/cli_contract.rs` 和 `tests/qa_contract.rs` 的负向测试保证。

## 输出模式

- 默认输出：`stdout` 上的俄语人类可读文字。长字段值会压成一行并按长度截断。
- `--json`（可放在子命令前或后）：`stdout` 上**恰好一个** JSON 文档，`stderr` 无输出。
- 例外：由 clap 自身检测的参数解析错误（未知参数、组合冲突、超范围值）会将文字写入 `stderr` 并以退出码 2 结束；此时命令尚未识别，因此不生成 JSON 封套。

成功响应：

```json
{
  "schema_version": 1,
  "command": "inspect",
  "result": { }
}
```

错误响应：

```json
{
  "schema_version": 1,
  "command": "find",
  "error": {
    "code": "not_found",
    "message": "по заданному критерию не найдено ни одной заметки",
    "details": { }
  }
}
```

`schema_version` 表示输出契约版本；`code` 是独立于 Rust 内部类型名称的稳定 snake_case 标识符。

JSON 模式下，笔记字段以对象表示，键按类型的 `ord` 顺序排列。如果笔记类型本身有重复字段名，后续重复键会附加 `#N` 后缀，使对象保持可区分。

## 退出码

| 代码 | 含义 |
|---|---|
| `0` | 成功 |
| `1` | 保留 |
| `2` | CLI 使用错误：未知参数、不兼容的组合或参数超出范围 |
| `3` | 输入错误导致命令无法正常开始：目录不存在、传入的不是目录、缺少或无法读取 `deck.json`。对需要类型化导出的命令（`inspect`、`find`、`stats`、`qa`、`review`、`review-check`、`edit`）还包括 `invalid_json`、`root_not_deck`、`schema_invalid`。`find`、`review`、`qa` 中未知的字段、牌组或 QA 代码也属于此类；`review-check` 的建议文档不可解析或 `deck.json` 非规范（`source_not_canonical`）同样如此。`maintenance` 无法确定 Cargo 工作区（包括无效的 `--workspace-root`）时返回 `input_unreadable`。`validate` 不同，它会把无效 JSON 当作内容问题报告并返回 `6`。`migrate-layout` 在至少一份导出被拒绝时返回 `3`、报告中 `refused: true`；这不是成功，但也不属于普通领域错误。 |
| `4` | `find`、`review --guid`、`edit` 或 `review-check` 建议未找到目标 |
| `5` | `find --guid` 或 `review --guid` 遇到重复 GUID，或 `review-check` 的 `invalid` 建议中存在不唯一 GUID |
| `6` | `validate` 发现至少一个 `ERROR`，包括 `invalid_json`、`root_not_deck`、`schema_invalid`；`edit` 因导出含 `ERROR` 或类型定义歧义而拒绝。重复 GUID（`duplicate_note_guid`）属于 `ERROR`，所以 `edit` 返回 6 而不是 5，因为先验证源文件，再定位目标。`review-check` 会标记 `export_invalid` / `export_not_mutable` 并返回 6，不生成请求。 |
| `7` | 前置条件冲突：`edit` 的 `expected` 或 `deck.json` 版本已变，或 `review-check` 发现过期建议 |
| `8` | 操作结果写入或发布失败（`write_failed`） |
| `70` | 意外的内部错误，包括编辑不变量失败或写入命令输出失败（磁盘空间不足、文件描述符异常） |

读取端提前关闭管道（例如 `anki-repo … | head`）不算内部错误：命令已经计算完结果，因此保留命令自身的退出码。

下表列出领域 `error.code` 及其退出码。`validate` 中的内容问题（`invalid_json`、`root_not_deck`、`schema_invalid`）在该命令下不属于领域错误，具体见验证章节。

| 代码 | 退出码 | 发生场景 |
|---|---|---|
| `usage` | 2 | clap 之外检测到不兼容参数 |
| `input_unreadable` | 3 | 无法读取导出目录、`edit` 请求或 `review-check` 文档；`migrate-layout` 无法遍历旧区域或计算源指纹；`maintenance` 无法根据不存在/不可解析的 `--workspace-root` 找到 Cargo 工作区 |
| `deck_json_missing` | 3 | 导出目录没有 `deck.json` |
| `invalid_json` | 3 | `deck.json` 不是有效 JSON |
| `root_not_deck` | 3 | 根对象不是 CrowdAnki `Deck` |
| `schema_invalid` | 3 | JSON 有效，但不符合类型化核心结构 |
| `unknown_field` | 3 | 导出中的任何笔记类型都没有该字段；对 `edit` 也表示目标笔记的类型没有该字段 |
| `unknown_deck` | 3 | 找不到指定牌组 |
| `unknown_qa_code` | 3 | 指定 QA 代码不在规则表中（`qa --code`、`review --qa-code`） |
| `invalid_request` | 3 | `edit` 请求或 `review-check` 建议文档的结构、版本、字段、身份或容量限制不合法；任何命令的路径指向受限 `decks/input` 时也返回它，且 `details.reason = input_area_forbidden` |
| `duplicate_edit_target` | 3 | `edit`、`review-check` 中重复指定同一（`guid`、字段）目标；这是独立错误，而不是 `invalid_request` 内的 `details.reason` |
| `proposal_not_executable`（`details.reason`） | — | 不是独立的错误代码；`review-check` 在报告中通过建议状态 `invalid` 描述不可执行的建议 |
| `source_not_canonical` | 3 | `edit` 的 `deck.json` 非规范；`review-check` 在 `source_blockers` 中指出同一问题，不生成请求 |
| `export_invalid` | 6 | `edit` 源导出有 `ERROR`（包括 `duplicate_note_guid`），或候选增加新的 `ERROR`；`review-check` 在 `source_blockers` 中标注并返回 6 |
| `export_not_mutable` | 6 | `edit` 的导出存在 `conflicting_note_model_definition`；`review-check` 同样在 `source_blockers` 中报告 |
| `expected_mismatch` | 7 | `edit` 当前值不等于 `expected` |
| `source_changed` | 7 | `edit` 的 `deck.json` 在检查与替换之间变化或消失；`migrate-layout` 迁移后内容指纹与源不一致，包括跨文件系统复制 |
| `write_failed` | 8 | 无法写入/发布结果（包括 `deck.json`）或设置系统维护 |
| `not_found` | 4 | `find`、`review --guid` 没有匹配 |
| `note_not_found` | 4 | `edit` 中指定的 GUID 不存在；对于 `review-check` 则只是建议问题代码，不是领域错误 |
| `ambiguous` | 5 | `find --guid`、`review --guid` 中 GUID 不唯一。对 `edit`、`review-check` 来说，因重复 GUID 会先以 `export_invalid`（6）拒绝，这不可能成为其直接**退出码**；报告中仍可能出现 `invalid`/`ambiguous_guid` |
| `unknown_model` | 3 | `create` 找不到显式指定或按字段集合兼容的 `auto` 类型；错误会列出实际请求的字段 |
| `media_forbidden` | 3 | `create` 的新字段值含媒体引用（`[sound:…]`、`ADDRESS_ATTRIBUTES` 中的媒体属性或 CSS 地址）。大小写无关，未闭合结构也拒绝；明确配置、已验证的 `img/src` 域图片是唯一例外 |
| `unresolved_guid` | 3 | `retire` 的目标 GUID 不存在。对 `create` 显式 GUID，如果已有内容相同则是 `already_applied`，不同则是 `guid_conflict`（6） |
| `unresolved_deck_identity` | 3 | `create --apply` 目标牌组未声明 `crowdanki_uuid`，写入后无法稳定定位 |
| `deck_identity_mismatch` | 3 | `models` / `create` 的多个牌组筛选条件指向不同节点 |
| `model_schema_unusable` | 6 | `create` 的类型字段结构不可用（`ord` 缺失、重复或非数字）；`models` 仍会展示问题和 `schema_problems` |
| `guid_collision` | 6 | `create` 的新 GUID 在请求内重复，或自动生成的 GUID 与已有 GUID 冲突，规定的重试次数内未找到空闲值 |
| `guid_conflict` | 6 | `create` 中 GUID 已存在但内容不同；更新已有笔记属于 `edit` |
| `ambiguous_model` | 5 | `create` 在 `auto` 模式有多个候选，或按显式名称选择的类型不唯一 |
| `ambiguous_deck` | 5 | `models` / `create` 筛选条件匹配多个牌组节点 |
| `missing_field_value` | 3 | `models` / `create` 的请求缺少某个必需字段 |
| `internal_error` | 70 | 内部错误，包括字节级编辑不变量被破坏 |

对于 `visual-report`，输出目录检查会产生 `invalid_request` 及如下 `details.reason`：`out_dir_inside_decks`（`--out` 位于 `decks/**`）、`out_dir_overlaps_export`（位于或等于被比较的导出）、`out_dir_not_owned`（非空但没有有效报告清单）、`out_dir_manifest_invalid`（清单损坏、不是本报告、指向目录外或引用自身）、`out_dir_not_a_directory`（`--out` 已存在但不是目录）、`out_dir_is_symlink`（`--out` 是符号链接）、`out_dir_entry_conflict`（报告目录内含有符号链接，或存在与所有权证明相矛盾的名称）、`before_equals_after`（两个状态是同一目录）、`invalid_tag`（`--retire-tag` 不合法，`retire` 也会使用该原因）。

目录检查后如果生成报告失败，则是 `write_failed` 及以下 `reason`：`report_write_failed`、`media_copy_failed`、`preview_not_offline`、`index_not_offline`、`report_file_not_moved`、`stale_file_not_removed`、`staging_not_created`、`staging_destination_not_created`、`report_file_outside_out_dir`、`out_dir_entry_conflict`（预检后才出现外来条目）、`report_file_not_confined`、`manifest_not_serializable`。

## 确定性

相同输入产生**逐字节相同**的输出：

- 牌组节点按父节点优先遍历。
- 笔记保持导出中原有顺序。
- 笔记类型字段按 `ord` 顺序输出。
- 分布先按数量降序，再按值排序。
- `validate` 问题按级别、代码、牌组路径、位置及文本排序。
- `edit` 按请求顺序解析目标，并通过与规范格式检查相同的代码序列化候选，因此输入相同会得到相同字节。
- `qa` 和 `review` 的问题先按规则在注册表中的位置、笔记在导出中的位置，再按字段 `ord` 及名称排序，不依赖问题被发现的顺序。
- `models` 按笔记类型名称排序，同名再按 `crowdanki_uuid`（导出遍历顺序不是此结果的一部分）；字段和模板按 `ord`，示例值按笔记在导出中的顺序。
- `create` 和 `retire` 按请求顺序解析笔记和牌组，并用同一规范化机制生成候选。
- `visual-report` 按导出顺序处理笔记，诊断及媒体引用清单会排序，预览文件名取决于编号而不是内容。相同输入生成逐字节相同的目录，不包含时间戳、版本或随机值。
- `review-check` 保持建议文档的顺序；`counts` 不受报告截断影响。
- 编辑被拒绝时，`deck.json` 不改变任何字节。

有且只有一个明确例外：没有提供 `guid` 的 `create`。自动生成的 GUID 本来就需要随机，否则两条相同内容的笔记会获得相同 GUID。因此，重复相同请求的结果和候选字节会不同。使用 `--emit-resolved` 可以恢复可复现性：已解析请求包含具体 GUID，重试时返回 `already_applied` 或 `guid_conflict`，而不会创建第二条笔记。

## 限制

- 每次调用只处理一份导出，这是有意的设计。
- `edit` 只修改已有笔记的字段值，不能更改结构、身份（`guid`、`crowdanki_uuid`、`note_model_uuid`、`deck_config_uuid`）、`tags`、`media_files` 或 `media/`。
- `edit` 要求规范格式的 `deck.json`；其他格式须在单独的任务中先规范化。
- `create` 只向已有牌组和已有笔记类型添加笔记，不会创建或更改牌组、类型、字段、模板、配置，也不会在牌组间移动笔记。
- `create` 不编辑已有笔记；同 GUID 但内容不同会产生 `guid_conflict`。
- `create` 默认禁止媒体引用。明确配置的域策略只能授权对应规范存储中的已验证图片 `img/src`；`--apply` 只能把这些字节放入导出的 `media/` 并在 `media_files` 中声明。音频、CSS 媒体地址、`srcset`、任意图片、未配置资源域和字段仍被禁止。
- `migrate-media` 是修改已有笔记媒体引用的唯一方式。要求证明身份与旧文件名组合，并完整盘点字段、`qfmt`、`afmt`、CSS 中的引用者。执行 `--apply` 时先放置已验证规范字节，通过 `children` 树发布引用和声明，最后只删除经过证明的旧文件。
- `retire` 只能追加停用标签，不能删除标签或物理删除笔记。实际删除由用户在 Anki 中完成。
- `visual-report` 只按有限的模板结构生成静态预览，**不是 Anki 原生渲染**；不计算过滤器、不执行模板 JavaScript。夜间模式、音频和动态高度也仅模拟文中明确列出的子集，不等于完整的 Anki WebView。
- `visual-report` 不模拟 CrowdAnki 导入行为，也不能预测 Anki 将如何处理导出；它只是按 GUID 比较两个状态。
- 报告目录属于独立产物，必须在 `decks/**` 之外，不包含运行标识，也不会自动更新。
- `visual-report` 与 `create`、`migrate-media` 不同：它只将实际预览需要的媒体复制到报告目录，而不修改源导出。后两者仅在受限、已证明的条件下把已验证字节放入导出 `media/`；`migrate-media` 可能在发布后删除准确的旧文件名。
- 搜索不支持正则表达式，也不规范化 HTML。
- 工具集不硬编码特定笔记类型的字段名：调用者使用 `--field` 提供字段，并对照实际 `note_models[].flds[].name` 验证。已删除的 `--word` 与 `duplicate_primary_field` 不会恢复。
- `--deck` 要求使用由各牌组节点 `name` 构成的准确路径。
- 不检查日语内容的语义，不比较不同牌组。`qa` 只识别列出的结构与格式问题，语言内容由外部智能体判断。
- 不调用 LLM 或网络。`qa`/`review`/`review-check` 在仓库边界停止：外部智能体提供建议，`anki-repo` 验证并提供可供 `edit` 使用的请求。
- 建议只允许表达 `edit` 支持的操作：修改已有笔记已有字段的值。`qa`/`review` 不提出实体创建、删除或重命名，`review-check` 也不接受这些操作。
- 报告截断（各代码的问题上限、`review` 页大小、编辑或笔记限制、结果项与预览文件上限）始终明确标识：计数器涵盖完整数据，文字会说明人类可读模式和 JSON 模式中省略的内容。
- 修改命令中没有“删除笔记”操作：`create` 只能添加，`retire` 只能追加标签。JSON 中消失的笔记在 `visual-report` 中被标为 `removed`，并不要求物理删除。
- `review-check` 根据当前导出验证建议，但不为建议预留状态。检查与 `edit --apply` 之间内容可能变化，届时由 `edit` 捕获前置条件冲突。
- 正常流程中几乎不会遇到建议目标无法解析（`field_not_resolvable`、`ambiguous_guid`）：非字符串字段值、字段缺失或重复 GUID 已是 `validate` 的 `ERROR`，因此源导出会先因 `export_invalid` 而阻止生成请求。仍保留明确的状态分类和契约测试，以描述已通过预检的输入可能发生的情况。
- QA 规则级别及代码不可配置；注册表固定，只能通过代码和测试共同扩展。

## 验证

所有检查都从包含根 Cargo 工作区（`/Cargo.toml`）的仓库根目录执行：

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

`--locked` 适用于该项目，因为 `Cargo.lock` 在根目录并由全部工作区成员共享。

CI（`.github/workflows/ci.yml`）在 `pull_request`、推送到默认分支及手动触发时独立执行相同检查。另会验证用户数据边界：Git 不得跟踪被整体忽略的 `decks/` 或 `.asset-store/` 下任何路径，且忽略规则必须存在，不能通过再次纳入跟踪来削弱（`.github/scripts/check_local_data_untracked.py`）。检查读取真实 Git 索引，因为 `.gitignore` 不能自动取消已跟踪文件，也可能被 `git add -f` 绕过。

CI 有意不验证用户自身的牌组和资源：这些区域在干净克隆中不存在，把“没有私人文件”当作测试成功并没有意义。应在本地通过 `anki-repo validate decks/output/<导出名称>` 检查具体导出，或通过 `kanji-corpus-gate`、`pitch-corpus-gate` 检查本地资源库（参见 [`asset-store` 文档](../asset-store/README.zh-CN.md)）。CI 只读取仓库，改变用户牌组的组成不要求修改 CI 流程。

常规工具测试自行构造合成导出，绝不读取 `decks/**`。

独立 MSRV 检查通过 `cargo metadata` 读取根目录 `[workspace.package] rust-version`，验证各工作区成员版本一致，并按该版本构建（`cargo +<msrv> check --workspace --all-targets --locked`）。CI 和本文件都不重复存储版本。可在本地复现：

```bash
MSRV=$(cargo metadata --no-deps --format-version 1 | jq -r '[.packages[].rust_version] | unique | .[]')
rustup toolchain install "$MSRV" --profile minimal
cargo "+$MSRV" check --workspace --all-targets --locked
```

如果安装了 nextest，本地可以使用 `cargo nextest run` 代替 `cargo test`；CI 实际执行的是 `cargo test --workspace --locked`。

测试集包括：

- 领域逻辑单元测试：按 `ord` 解析字段、遍历牌组树、`find`/`stats`、模板扫描器及退出码契约。
- QA 单元测试：逐项验证规则边界（空值、开头/末尾/内部空白、白色 `<span>`、重复内容分组）、结果排序与截断。专门测试将五项代码及严重程度确立为公开契约，并确保结果不依赖字段名或笔记类型名。
- `tests/synthetic.rs`：针对各验证分支和各领域错误的合成场景。
- `tests/export_contract.rs`：任何有效 CrowdAnki 导出都应满足的性质。笔记数、字段顺序和媒体情况都根据原始 JSON 及目录内容独立重新计算。测试夹具刻意采用与真实用户牌组不同的结构，不需要存在 `decks/`。
- `tests/cli_contract.rs`：通过真实二进制程序验证 CLI 参数、退出码、`stdout`/`stderr` 分离，包括已移除 `--word` 必须拒绝的负向检查。
- `tests/local_data_contract.rs`：验证本地数据边界：所有命令对 `decks/input` 返回 `input_area_forbidden`，`decks/output` 下的导出仍可读取；`migrate-layout` 验证幂等性、名称冲突拒绝及字节保持。
- `tests/qa_contract.rs`：保证 QA 注册表恰有五项（无 `duplicate_primary_field`）、`error` 级别不导致命令失败或影响 `validate`/`edit`、证据样本限制、每代码上限与完整计数、确定性、明确标记不可寻址问题、完整重复分组信息、通过合成原始 `deck.json` 独立复核数量，而不是在测试中硬编码预期数值。
- `tests/review_contract.rs`：验证页大小、分页全遍历不丢失不重复、越界空页、数据包精简程度、单条笔记内部问题数限制、`--guid` 错误与 `find` 一致、重复分组成员最多 `MAX_RELATED_NOTES`、不可寻址排除计数 `excluded_unaddressable`，以及已移除 `--word` 的拒绝行为。
- `tests/review_check_contract.rs`：五种建议状态，`stale`/`invalid` 退出码优先级（5 → 4 → 3），拒绝外来结构 `edits` 和旧参数 `--request`，覆盖全部建议 `invalid_request` 原因（`malformed_proposals`、`unsupported_schema_version`、`empty_proposals`、`too_many_proposals`、`proposals_too_large`、`duplicate_proposal_id`），元数据映射 `proposal_id` → `edit_id` 且 `reason` 只进入报告，不写文件或临时文件；报告截断但计数完整；stdin；在临时导出副本上进行 `review → 建议 → review-check → edit（dry-run/--apply）→ validate` 端到端流程。
- `tests/edit_contract.rs`：dry-run 和应用、四种编辑结果、前置条件冲突、批量目标解析错误、非规范或无效导出的拒绝、JSON 响应结构、退出码，以及 `--request FILE` 与 `--request -` 的一致性。
- `tests/note_lifecycle_contract.rs`：通过真实 CLI 和合成导出验证“结构 → 创建 → 停用”。包括 `models` 各字段的证据与声明顺序不同的 `ord`、规范的新增笔记和结构校验、通过字节对比证明只追加了笔记、`--emit-resolved` 幂等性、批量原子性、全部四种 `auto` 类型选择规则（目标子树唯一兼容类型、空牌组回退到整个导出的唯一兼容类型并记录 `model_evidence`、两个兼容类型及同名类型必须拒绝），以及 `media_forbidden`、`unknown_model`、`ambiguous_model`、`guid_conflict`、`unresolved_deck_identity`、`source_not_canonical`、`export_not_mutable` 等失败场景；还验证只追加标签、不重复添加、停用后仍可寻址和 stdin 读取。
- `tests/visual_report_contract.rs`：根据 GUID 分类、重复 GUID 的独立诊断且不猜测预览、所有生成文件离线安全（只有报告自身运行时、无外部网络地址）、两个状态预览框使用各自实际值、只为显示的预览复制媒体和音频、同名媒体按状态隔离并禁止跨状态替补、现有音频的本地播放与缺失音频的可见标记、含媒体的两次生成逐字节一致、比较过的导出不变、卡片预览无 diff 高亮、笔记类型 CSS 只存在于本状态卡片而不污染报告页面、每种模板独立生成卡片文件、输出目录拒绝条件、`<script>` 不执行等不支持结构、预览数量限制，以及运行时行为：`nightMode` 同时传递到根节点、`body`、卡片，全部预览框同步主题，高度由内容测量并在媒体加载后更新。运行时代码在 Node 的最小 DOM 测试环境中执行验证；Node 不可用时跳过该测试。
- `tests/edit_source_preservation.rs`：每条编辑恰好改变一行，其他字节不变，覆盖转义、换行、Unicode、幂等不重写、非规范拒绝、权限保持、无临时文件。测试自行构造包含六字段类型及嵌套牌组内单字段类型的完整合成导出，不读取 `decks/`。
- 并发发布测试：单元测试确定性地重现两个写入进程从同一快照比较到 `rename` 的窗口（只有一个发布，另一个得到 `source_changed`），对照测试证明缺少临界区会丢失更新，锁测试确认发布确实等待解锁；`tests/edit_source_preservation.rs` 还并行运行两个真实的 `edit --apply` 进程，确认最后仍是规范导出且没有丢失修改。

针对最初竞态问题，还通过三个可观察性质进行回归保护：`replace_atomically` 保留胜出者的更新并让落后者收到 `source_changed`；无锁对照证明会丢失更新；锁测试确认发布必须等待。由于没有引入测试专用接口就无法从外部直接触发 `write_and_publish` 内部的准确竞态窗口，所以测试验证这些真实性质，而不是伪造“马上发布”的信号。
