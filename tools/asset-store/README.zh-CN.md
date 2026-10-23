# 资源存储与 `kanji-assets`

[Русский](README.md) · [English](README.en.md) · **简体中文**

<!-- i18n-source-git-blob: 4bb9664bedd154f8b56e7a9dce9f1a142f39aca1 -->

`asset-store` 是用于程序管理资源的公共 Rust 库，负责清单、校验和、完整性及生命周期。领域专用 CLI `kanji-assets` 负责查找 Yarxi 条目、验证汉字图片，并把通过验证的文件发布到规范资源库。本工具不读取 `decks/**/media/`，不更改 `media_files`，也不会把图片复制到牌组中。

## 数据存放位置

汉字 CLI 默认使用 `.asset-store/kanji`。独立的音高重音领域使用 `.asset-store/pitch-accent`；两个已发布资源库不共享清单或 `assets/` 目录。

```text
.asset-store/                          # 整个目录被 Git 忽略（/.asset-store/）
├── kanji/
│   ├── .owner.json                 # 所有者标记；不会发布到 Git
│   ├── manifest.json               # 只包含有效的 VERIFIED 记录
│   ├── assets/
│   │   ├── gif/漢.gif
│   │   └── png/饅.png
│   ├── .runtime/                   # 候选、隔离区、批次
│   ├── .journal/events.jsonl       # 操作日志；只追加
│   ├── .tmp/                       # 暂存与事务标记
│   └── .lock
└── pitch-accent/
    ├── .owner.json
    ├── manifest.json
    ├── assets/png/幽霊.png             # consumer_filename:幽霊.pitch.png
    ├── .runtime/
    ├── .journal/events.jsonl
    ├── .tmp/
    └── .lock
```

根目录 `.gitignore` 完整忽略 `/.asset-store/` 和 `/decks/`，且没有白名单例外。因此 `.owner.json`、`manifest.json`、资源字节、`.runtime/` 和操作日志都不会进入 Git。干净克隆中没有资源库是正常状态，而不是发布任何内容的理由。

清单中的内部 `storage_path` 与消费者文件名不同。例如 `assets/gif/漢.gif` 的 `consumer_filename: "漢.gif"`，使用时对应 `media/漢.gif`。`anki-repo` 读取明确声明的消费者文件名，而不是由资源存储布局猜测。

`.runtime/manifest.json` 与主清单使用相同 `store_id`，`verify_integrity` 检查两者。`Pending`、`Quarantined` 保存在 `.runtime`；验证规范资源库时还会确保其身份与 `.runtime` 无重叠。资源库不存在也允许。

## 领域策略与清单

`AssetDomainPolicy` 定义允许的身份标识、规范存储路径、消费者文件名、发布格式及容量限制。`GenericDomainPolicy` 为非发布型通用资源保留按哈希寻址的结构。领域策略拒绝其他命名空间的身份：`KanjiDomainPolicy` 管理 `kanji`，`PitchAccentDomainPolicy` 管理 `pitch_accent`。

迁移旧消费者文件名必须通过 `AssetDomainPolicy::proves_legacy_consumer_filename(identity, filename)` 独立证明。默认及通用策略不提供此证明。汉字领域仅允许准确的 `<key>.gif` 或 `<key>.png`；音高重音仅允许 `<key>.png`；始终要求不含目录的安全文件名。该检查不使用 `legacy_location`，因为它表示旧清单内部位置，而不是 Anki 在 `media/` 中见到的名称。

汉字 GIF 和 PNG 分别存放于 `assets/gif/` 与 `assets/png/`，文件名严格由字符和扩展名组成。音高重音以准确的词形 `surface` 而非读音 `reading` 为键：文件存储为 `assets/png/<surface>.png`，提供给消费者时叫 `<surface>.pitch.png`。例如 `飴` 内部路径为 `assets/png/飴.png`，Anki 获取 `飴.pitch.png`，与汉字媒体名 `飴.png` 相互独立。读音与正整数 JPDB 词条 ID 保存于元数据和证据中。

资源存储负责**规范**消费者文件名，但不重写已有笔记的引用。旧的 `<surface>.png` 音高重音引用由 `anki-repo migrate-media` 迁移（参见 [anki-repo 文档](../anki-repo/README.zh-CN.md)）；存储仍提供已验证字节，不会重命名自身文件。比较读音时将平假名与片假名视为等价，但必须确认准确的词形/读音组合是所选词条的一种形式；`evidence` 保留 JPDB 的原始读音。

仅成功解码 PNG 不足以获得 `VERIFIED`。`PitchAccentImageValidator` 验证词形和读音、与指定 ID 对应的真实词条页面、图形变体清单、恰好选中其中一个变体以及深色主题的证据。

每个 PNG **恰好**包含一个音高图。`evidence.variant_selection` 按 DOM 顺序保存所选词条的全部图形变体（`detected_variant_count`、`variants`）、所选变体的索引（`selected_index`）和选择依据（`basis`）；`evidence.render` 保存始终为 `1` 的 `captured_graph_count`、所选变体的选择器、`padding_css_px` 以及最终的 `capture_rect`。每个变体记录图形文本、其标记的 SHA-256（`markup_sha256`）、选择分数（`selection_score`）以及矩形：节点在视口和文档坐标中的矩形，和 `paint_rect`——节点与其全部后代的并集。JPDB 图形中带 `margin-right: -2px` 偏移的片段会超出节点矩形，因此截图区域以 `paint_rect` 为基础。`capture_rect` 等于所选变体的 `paint_rect` 向四周各扩展 3 CSS px，再向外取整到整数 CSS 像素。在 DSF `3.0` 下，整数区域产生尺寸精确的 PNG：验证器要求 PNG 尺寸**严格等于** `capture_rect × 3`，不允许误差。该区域必须位于文档之内，且不得与其他变体的 `paint_rect` 相交。

深色主题证据关联浏览器配色设置、文档类、计算得到的背景颜色与 PNG 像素。至少一个背景像素与观察到的颜色在**每个 RGB 通道**上相差不超过 **8**；至少八个图形像素在至少一个通道上相差**超过 16**。PNG **所有**像素必须 `alpha=255`。此外，所选图形绘制区域之外的所有像素（允许其边缘有一个物理像素的抗锯齿）都必须在同样的容差 8 内与观察到的背景一致：由此实际字节证明边距干净、相邻变体和按钮未被截入、线条和字符不接触 PNG 边缘。边距是原生截图中页面的真实背景，而不是后期绘制的边框。必须有浏览器来源证据。验证器 v6 实现现行绘制/证据契约；早期版本的证据不会继承可信状态，不完整证据或早期形式的证据（例如带有版本 5 的 `graph_count` 和 `graph_union_rect`）返回 `UNCERTAIN`。

清单版本为 `5`。每条记录包括 SHA-256、字节大小、格式、状态、来源、语义判断及 `consumer_filename`；存储整体包括 `domain_id`。自动 `ValidationRecord` 与可选 `HumanAttestation` 绑定准确身份和 SHA。对于音高重音，人的 `Approve` 不能取代自动 `ValidationRecord` 记录：相同字节仍需要一条状态为 `VERIFIED`、且 `validator.id`/`validator.version` 符合 `PitchAccentImageValidator` v6 预期的记录。人工批准无法使旧版、不完整证据或 `REJECTED`、`CORRUPT` 变得可信；汉字领域原有人工批准规则不变。新 SHA 不继承旧字节的决定；替换记录必须按预期旧 SHA 做 compare-and-swap。

旧版 v3/v4 清单和原先平铺的汉字目录仍可读取。以写入模式打开时会在独占锁内执行版本迁移：通过硬链接保留旧字节、验证 SHA-256、原子更新清单，再删除旧平铺链接。`.tmp/` 中的事务标记允许中断后继续或回滚。未变化的 SHA、`ValidationRecord`、`HumanAttestation` 均保留，无需重新下载。完成后新旧规范路径不会同时存在。CLI JSON 报告 `store.layout_migrated_on_open`。

`AssetStore::read_verified` 仍是兼容读取的汉字 API。其他领域使用 `read_verified_with_policy`；该 API 可通过旧映射读取 v3/v4，并返回明确的消费者文件名，**不修改清单、不创建状态或锁文件、不迁移布局或触发恢复**。只有写入模式打开才会迁移；读取时验证精确字节、校验和、大小、格式、验证器身份及人工决定。

公共批处理运行时负责 `.runtime/batches/<batch-id>` 下安全的文件机制：`NOFOLLOW`、锁、修订号/CAS、原子状态写入、有界读取和按 SHA 寻址的内容。它不依赖 `KanjiBatch`、`KanjiMetrics`、Yarxi 数据或 HTML 报告，这些都由领域适配器负责。纯文本测试状态验证重新打开和读取内容，无需汉字或图片规则。

公共 Chromium 会话、可执行文件定位、启动诊断、CDP 指标和网络技术遥测在 `browser_runtime.rs` 中。Yarxi 保留自身选择器、URL、TLS 策略、过滤器、后备来源、深色主题和截图几何；公共运行时支持独立的 `3.0` 缩放系数。JPDB 使用隔离会话，`pitch-assets` 独立处理批次并发布已验证 PNG。与 `anki-repo` 的集成仍是单独能力。

Chromium 进程由 `browser_process.rs` 拥有，而不是 `chromiumoxide`：浏览器使用与原 `Browser::launch` 相同的参数启动，但位于**自己的进程组**（`process_group(0)`），CDP 连接到其打印的 DevTools 地址。因此终端 `Ctrl+C` 只把 `SIGINT` 发送给 CLI，而不发送给浏览器：取消由协调器执行，正在进行的请求不会因浏览器退出而变成伪造的 `technical_failure`。停止过程区分各阶段：关闭 CDP（`browser_transport_stop`）、进程组首进程在最多 10 秒窗口内自行退出（`browser_process_exited`）或强制结束整个组（`browser_process_killed`），随后回收组内后代直到 `ESRCH`；组内残留的 zombie（例如没有 reaper、CLI 是容器的 PID 1 时）根据 `/proc` 不计为组成员。信号只发送给本次启动创建的组：在首进程被回收（`wait`）之前，组号不会转给其他进程；回收之后，仅当探测确认组内仍有进程时才向后代发送 `SIGKILL`，探测与发送信号之间仍存在一个狭窄窗口。CLI 自身因 `SIGKILL`/`SIGQUIT` 异常退出时不经过此路径：只要残留进程仍引用运行目录树，孤立运行清理就拒绝删除它。

Yarxi 与 JPDB 共用的并行获取层是 `acquisition_pool.rs`：分配账本（每个 worker 至多一项、校验结果属于其分配、重新入队、使 worker 退出池）、以首个原因（`ctrl_c` 或协调器故障）取消整个池、会话恢复预算，以及对 `SIGINT`/`SIGTERM`/`SIGHUP` 的订阅。该层不了解浏览器和提供者：`token`/`CAS` 检查、检查点、验证器和信任规则仍由领域协调器负责，因此也可以接入非浏览器来源。

用户区域不会通过 Git 发布，无论是整体还是部分：根目录 `.gitignore` 完整忽略 `/.asset-store/` 和 `/decks/`，这两个区域内没有任何例外。可以用真实的 `git check-ignore` 通过专门的回归脚本验证这些规则——脚本将根 `.gitignore` 复制到临时仓库，证明两个区域下的全部路径（包括 `.owner.json`、`manifest.json`、`assets/**`、`.runtime/**`、`.journal/events.jsonl`、旧分组目录及嵌套目录）都被忽略，并检查 `git add -A` 之后索引中不再残留任何此类路径：

```bash
bash tools/asset-store/tests/gitignore_regression.sh
```

`kanji-corpus-gate` 和 `pitch-corpus-gate` 检查所属领域的规范资源库是否满足发布要求，拒绝未经确认的生命周期状态、旧版验证器证据、旧平铺布局、带 SHA 后缀路径、孤立文件及未注册的规范路径。从工作区根目录进行只读检查：

```bash
cargo run --quiet --locked -p asset-store --bin kanji-corpus-gate
cargo run --quiet --locked -p asset-store --bin pitch-corpus-gate
```

如果缺少当前有效 `VERIFIED`、使用不支持的领域/布局、证据过期、有孤立字节或运行时损坏，检查会返回非零退出码。它绝不修改数据，也不把临时内容算入规范资源库。

## 操作日志

位于 `<store-root>/.journal/events.jsonl` 的**只追加**日志解释存储如何到达当前状态。它与 `manifest.json` 同级，但不在 `.runtime/batches/**` 内，因此删除某个批次的本地状态不会丢失历史。格式为 NDJSON，每行一个 JSON 对象，随 `.asset-store/` 一起被 Git 忽略。

日志记录的是状态转换，**不是第二份清单**；`manifest.json` 始终是当前 `VERIFIED` 资源库的权威来源。日志可发现已写入清单但缺少结束事件的变更。日志不包含 token、cookie、CDP dump、HTML 报告或图片，仅含身份、操作、SHA-256、生命周期、清单修订号、技术原因及 `baseline` 计数快照。

实际事件示例：

```json
{"schema_version":1,"seq":2,"event_id":"c3dca04948e2cecdef3dda4b70c81f8b2268a2844707d64b197ba7e64bd5a96e","event_key":"a143ed00d781b3439a0f0688125f45e9d34f08aa9e75ac91e5e30c67746e4b68","recorded_at":"2026-10-15T22:08:27.113Z","domain":"kanji","namespace":"kanji","identity":"漢","operation":"candidate_written","outcome":"completed","intent_id":"441132dfc6eff6e5cd75a3d55566026727b115bedf2724beb252106874778a14","manifest_revision":1,"previous_sha256":null,"new_sha256":"693d949d8c3fdc7fd4ace7c340b5f177a9f0c5be7bafee8bc93a7d88b7523d75","previous_lifecycle":null,"new_lifecycle":"pending","verified":false,"run_id":null,"batch_id":null,"provenance":"local_import:han.gif","reason":null,"snapshot":null}
```

完整读取上限为 **256 MiB**。超过时，`journal list`、`journal check`、`journal reconcile` 返回 `journal_too_large`（退出码 4）；追加仍通过流式读取必要事件键和末尾序号进行。没有自动日志轮转：需人工归档 `.journal/events.jsonl` 才能重新读取完整历史，但不会改变清单的权威性或资源库状态。

| 字段 | 含义 |
|---|---|
| `schema_version` | 事件格式版本（`1`） |
| `seq` | 严格逐次 `+1` 的事件序号 |
| `event_id` | 事件标识，供 `intent_id` 引用 |
| `event_key` | 根据事件字段计算的幂等键，由 `journal check` 验证；兼容旧 schema-1 中未把 `intent_id` 纳入哈希的键 |
| `recorded_at` | RFC 3339 UTC 记录时间 |
| `domain`, `namespace`, `identity` | 记录所有者；整个存储级别事件使用 `identity: "*"` |
| `operation` | `candidate_written`, `verified_published`, `verified_reused`, `trust_granted`, `trust_revoked`, `revalidated`, `quarantined`, `removed`, `recovered`, `layout_migrated`, `baseline` |
| `outcome` | `pending`, `completed`, `refused`, `recovered`, `aborted`, `unknown` |
| `intent_id` | 结束事件所引用的意图事件 `event_id` |
| `manifest_revision` | 规范清单与 `.runtime` 清单的合计修订号 |
| `previous_sha256`, `new_sha256`, `previous_lifecycle`, `new_lifecycle` | 事件前后的真实状态 |
| `verified` | 事件后已验证则 `true`，未验证或已删除则 `false`，非单条记录事件为 `null` |
| `run_id`, `batch_id` | 可选的执行上下文 |
| `provenance` | 不含路径的 `source_kind:source_name` |
| `reason` | 拒绝、恢复或删除的技术原因 |
| `snapshot` | `baseline` 的 `assets`/`verified`/`pending`/`quarantined` 计数 |

### 两阶段记录

一次状态变化由两个事件包围。修改前写入 `outcome: pending` 意图，包含当前 `previous_*` 和期望的 `new_*`；其 `event_id` 成为 `intent_id`。成功执行 `save_manifest` 后写入引用相同 `intent_id` 的 `completed` 结束事件，记录真实 `manifest_revision`、`new_sha256`、`new_lifecycle`、`verified`。提交前的正常拒绝以 `outcome: refused` 和 `reason` 关闭意图，避免留下未结束事件。

只有已经持有存储独占锁的代码能写日志，且只在记录真实变化时写入。并行批处理 worker 不写日志，因此不会拖慢获取过程。只有实际复用了相同 SHA、此前已可信的资源时才产生 `verified_reused`；清单中存在 `VERIFIED` 不代表又发生了一次 `verified_published`。

### 对账与恢复

如果进程在 `save_manifest` 后、结束事件前退出，意图会保持打开。`journal reconcile` 持锁对照规范及 `.runtime` 清单；有效未结束意图可以恢复。日志出现无效引用、损坏/截断的行、`domain`/`namespace`/`identity`/`operation` 不匹配、顺序错误、重复结束或 `event_key` 错误时，以 `manifest_corrupt` 拒绝且不写入。

- 实际状态等于预期 `new_*` → `outcome: recovered`。
- 实际状态等于先前 `previous_*` → `outcome: aborted`。
- 两者均不符合 → `outcome: unknown`，附上 `reason`。

验证器决定和人工信任只修改 `validation` 或 `human_attestation`，不改变 SHA/生命周期，因此清单无法区分已应用与未应用的变更。其未结束意图会以原因 `манифест не различает применённое и неприменённое изменение`（即“清单无法区分已应用与未应用的变更”）标为 `unknown`，转换本体的失败绝不会被当作 `refused`：结果保持未知，而不是靠猜测得出。对账是幂等的：重复调用不会追加任何事件。

### 命令

两个 CLI 都通过自己的 `--store` 读取相应日志：

```bash
# 事件按 seq 升序排列；--limit 只保留最后 N 条。
kanji-assets --store .asset-store/kanji journal list --limit 20
pitch-assets --store .asset-store/pitch-accent journal list \
  --identity 幽霊 --operation verified_published

# 完整性：严格的 seq 编号、唯一的 event_id、正确的 event_key；
# 结果必须引用同一区域和操作的先前 Pending；
# 还会检查允许的单阶段事件、被截断的尾部、
# 损坏的行，以及不高于实际值的修订号。
# 发现问题时返回退出码 3；未结束意图会被列出。
# 日志超过完整读取上限时返回 journal_too_large（退出码 4）。
kanji-assets --store .asset-store/kanji journal check

# 对账未结束意图；打印追加的事件。
kanji-assets --store .asset-store/kanji journal reconcile

# 写入一条带当前状态快照的 baseline 事件；重复写入会被拒绝。
kanji-assets --store .asset-store/kanji journal baseline
```

`journal baseline` **恰好**记录一个 `operation: baseline`、`identity: "*"`、真实清单修订号以及 `assets`/`verified`/`pending`/`quarantined` 快照。它标志可信历史的起点，禁止重复创建。

## 从 JPDB 获取音高重音图片

`JpdbPitchProvider` 访问 JPDB 搜索页和词典详情页。通过 `URL` API 设置搜索参数 `q`，并明确指定 `lang=english`。导航后仅允许 `/search`，或直接重定向到合法的 `/vocabulary/<id>/...`。如果直接跳到详情，必须在获取图形前独立核对词条 ID、与请求的对应关系及可选的显式选择。其他路径或 origin 一律拒绝。

搜索须在观察到的词形与读音中找到**成对匹配**，包括已确认的其他书写形式。若存在歧义，不会选择第一个或排名最高的结果，而是返回 `ambiguous_vocabulary`，附全部候选的词条 ID、写法/读音、词性、含义以及实际详情链接。调用方可通过 `JpdbPitchRequest` 提供 `JpdbPitchSelection`；提供者重新搜索，只有链接中的 ID、`surface`、可选 `reading` 与实际新发现的候选一致才接受。URL 查询参数和片段不属于身份。合法链接须使用 `https://jpdb.io`、默认端口，以及严格的三段或四段非空路径 `/vocabulary/<positive-id>/<surface>[/<reading>]`，不允许末尾斜线。不得信任任意输入 URL/ID 或根据输入字符串拼接链接，只能使用搜索结果的实际 `href`。JPDB 在 `/search` 以详情模式显示的、没有其他书写形式的单一词条，其结果行只含 `Used in` 链接（`/vocabulary/<id>/<surface>/used-in`）；该链接不是路由，也不会通过删去该段而变成路由。此时以页面的 `link[rel=canonical]` 作为路由，但仅当同一 DOM 快照中详情模式下恰有一行、一个结果，文档中恰有一个 canonical 链接，且它通过同样的路由检查，该行每个 `Used in` 链接都指向同一 ID 与写法，且 canonical 的写法/读音对在该行标题词中被观察到。没有读音段的 canonical 只接受于读音等于写法的假名词条。否则无链接的行仍是 `page_contract` 失败；沿 canonical 路由跳转后，详情页仍须通过同样的 ID、词形与查询检查。无明确选择时保持歧义；`vocabulary_not_found` 与技术失败分开。

只有已经核对真实词典详情结构且确定没有 `Pitch accent` 区域，才能报告 `no_pitch_accent_on_source`。无法识别/空的图形 DOM、网络或 JS 错误、深色主题未经证实、截图不合法，都必须作为技术错误。在认定没有重音图前，等待关键页面请求结束并再次检查结构；任意睡眠时间不算证据。`page.evaluate()` 执行/解码失败是带当前阶段的 `browser_evaluation`；互相矛盾的 DOM 是 `page_contract`；超时是 `timeout`。

已明确解析的词条中的音高图是该词条内部的变体。`Pitch accent` 区域的每一行必须恰好包含一个图形，以及可选的 `a.vocabulary-audio` 音频按钮（数量不限，不参与选择）；行中任何其他元素（包括可能的变体信息）都会导致 `capture_contract` 技术失败，而不会被忽略。只有一个图形时直接截取它；有多个变体时**恰好**截取一个，不含相邻图形。

JPDB 不提供**单个**音高变体的常见程度或优先级：变体行的标记只包含音频按钮和图形本身，而 `Top N` 与词频属于整个单词。因此采用可复现的伪随机选择（`basis: seeded_hash_without_source_priority`）：为每个变体计算 `selection_score = SHA-256("anki-decks/jpdb-pitch-variant-selection/v1\0<词条 ID>\0<markup_sha256>")`，选择分数最小的变体。选择不取决于页面上图形的顺序、新的运行、并行或 `resume`，也不会退化为“总是第一个”。相同的标记给出相同的分数，此时选择较小的 DOM 索引。新增变体时，选择要么保持不变，要么转向新变体。标记或变体集合的变化会改变 `markup_sha256` 和分数，因此选择与证据保持一致：验证器根据保存的清单重新计算选择。选择图形从不选择词条：多个匹配的 ID 仍返回 `ambiguous_vocabulary`，直到用户明确选择。

所选变体通过 CDP 截取，页面缩放 `1.0`，设备比例 DSF `3.0`；截图后不缩放、不裁剪、不重新编码 PNG。坐标以视口与文档空间保存（包括 `scrollX`/`scrollY`）。截图前，提供者两次核对变体清单与几何，并检查：没有祖先元素通过 `overflow` 或 `clip-path` 裁剪图形的绘制区域；带边距的区域位于文档之内且不触及相邻变体；不透明的深色祖先背景覆盖整个区域。截图后再次检查相同条件、PNG 尺寸和边距像素。任何违反都会产生带诊断信息的有类型技术失败（`capture_contract` 或 `dark_theme_unverified`），而不是 PNG。验证器不会解读音高图本身，也不会通过像素验证语调是否正确。

每次获取返回原始字节和可供 `PitchAccentImageValidator` 使用的 `PitchAccentDomainMetadata`。`JpdbPitchProvider::acquire_requests` 为请求集创建独立会话，`acquire_requests_in_session` 接收调用方会话。两者都顺序处理，每项重新搜索；普通单项错误不会终止整个批次。如果 CDP 观察者丢失数据、会话不再可信，只为**已经开始**的项目生成结果，并另报 `session_failure`；未启动的尾部不伪造结果，可在之后重试。

人工验收使用外部 JSON 计划，每项必须声明准确的 `expected_outcome`：`acquired`、`no_pitch_accent_on_source`、`ambiguous_vocabulary` 或 `vocabulary_not_found`。可选限制：

- `expected_vocabulary_id`、`min_variant_count` 仅适用于 `acquired`。`min_variant_count` 指定词条中**检测到**的图形变体的最小数量；无论变体有多少，PNG 本身都必须恰好包含一个图形。
- `expected_candidate_ids` 仅适用于 `ambiguous_vocabulary`，须与实际候选 ID **完整集合**一致，顺序无关。
- `selection` 仅用于 `acquired`，必须包含 `vocabulary_id`、`detail_url`，ID 与 `expected_vocabulary_id` 一致，链接符合上述 JPDB 路径。

在创建报告或启动浏览器**之前**，验收程序检查请求、领域身份、消费者文件名、数值限制、重复请求及选择链接。搜索时修剪 `surface`/`reading` 首尾空白，但在报告中保留原始计划值。允许同一个词形/读音分别用于普通歧义请求及其显式选择；完全相同的请求加选择组合重复则拒绝。搜索后再次根据实际候选验证选择。

下面只是**合成示例**。真实运行须使用候选 `evidence` 中的 ID 和 `detail_url` 替换示例值：

```json
{
  "items": [
    {
      "surface": "合成語",
      "reading": "ごうせいご",
      "expected_outcome": "acquired",
      "expected_vocabulary_id": 42,
      "min_variant_count": 1
    },
    { "surface": "別の語", "expected_outcome": "no_pitch_accent_on_source" },
    {
      "surface": "重複語",
      "expected_outcome": "ambiguous_vocabulary",
      "expected_candidate_ids": [101, 102]
    },
    {
      "surface": "重複語",
      "expected_outcome": "acquired",
      "expected_vocabulary_id": 101,
      "selection": {
        "vocabulary_id": 101,
        "detail_url": "https://jpdb.io/vocabulary/101/重複語/ちょうふくご"
      }
    },
    { "surface": "欠落語", "expected_outcome": "vocabulary_not_found" }
  ]
}
```

从工作区根目录运行：

```bash
cargo run --locked -p asset-store --bin jpdb_pitch_acceptance -- \
  --plan ../jpdb-pitch-plan.json
```

每次运行将报告和证据封装为**一个经过验证的 ZIP**，不留下解压目录。默认路径为 `<std::env::temp_dir()>/anki-decks-evidence/jpdb_pitch_acceptance-run-<32 lowercase hexadecimal digits>.zip`。`--output <ZIP>` 指定**新建的** `.zip` 文件，相对路径以当前目录为基准；父目录须存在，目标文件不得存在，路径须位于工作副本与临时树之外。解压后查看 `reports/index.html`、`reports/evidence.json` 和 `reports/images/`。发布结束的 stdout 含 `LIVE EVIDENCE ZIP`，列出绝对 `Path`、`SHA-256` 和以字节计的 `Size`。即使计划验收失败，ZIP 仍可能包含已有的部分证据，并记录最终状态与可用结果。

各项目报告包括预期/实际结果、匹配情况、候选、实际词条 ID/链接、词形/读音、检测到的图形变体数量、所选变体、选择依据、PNG 中的图形数量、带选择分数的变体清单、深色主题证明、截图几何、DSF、PNG 尺寸、SHA-256 和核心验证器决定。`acquired` 只有在验证器为 `VERIFIED` 且满足全部计划条件时才算成功。符合预期的歧义、词条不存在、没有重音图也可算成功；未预期的词条缺失则验收失败。提供者结果缺失或本地处理错误同样算不匹配。单项失败不会抹掉其他证据。已获取的原始字节不变；签名错误或无法解码的 PNG 以 `.bin` 保存，不作为图片展示。此工具不修改规范存储、用户媒体或 `decks/**`；ZIP 仅是本地产物。

## `pitch-assets`：规范资源库与批处理

CLI 获取音高重音图片、保存批次状态，并将已验证 PNG 发布到 `.asset-store/pitch-accent`。它不读取 CrowdAnki 导出，也不知道卡片类型或字段名。不指定 `--store` 时使用当前工作副本的音高重音存储。`--output json` 输出有版本的机器可读结果。

`pitch-assets` 退出码：

- `0`：命令成功；对于 `ensure`、`batch run`、`batch resume`，全部项目已经得到解决。
- `3`：获取后仍处于 `needs_review`，或 `AssetError` 表示输入错误、身份冲突、禁止的状态转换。通过 JSON 中的 `outcome`、`blockers`、`error` 区分。
- `4`：`AssetError` 表示目录边界、完整性、存储所有权、存储不存在或结构版本不受支持。
- `5`：I/O 或验证器错误（`AssetError::exit_code()`），或除 `BrokenPipe` 外的 stdout 写入错误；发生 `BrokenPipe` 时保留命令自身结果码。

CLI 参数解析失败在运行前返回 `2`。存储错误根据 `AssetError::exit_code()`，而不是批次状态确定。

```bash
# 在不进行网络获取的情况下查看和检查资源库。
cargo run --locked -p asset-store --bin pitch-assets -- corpus list
cargo run --locked -p asset-store --bin pitch-assets -- corpus check

# 单次获取，或按严格的 JSON 计划执行批次。
cargo run --locked -p asset-store --bin pitch-assets -- ensure \
  --surface '幽霊' --reading 'ゆうれい'
cargo run --locked -p asset-store --bin pitch-assets -- batch start \
  --plan ./pitch-plan.json
cargo run --locked -p asset-store --bin pitch-assets -- batch run \
  --batch-id my-pitch-batch --workers 4

# 查看状态、复核报告，并在作出决定后继续。
cargo run --locked -p asset-store --bin pitch-assets -- batch status \
  --batch-id my-pitch-batch
cargo run --locked -p asset-store --bin pitch-assets -- batch review \
  --batch-id my-pitch-batch
cargo run --locked -p asset-store --bin pitch-assets -- batch resume \
  --batch-id my-pitch-batch --workers 4

# 显式选择候选、按条目或按批次重试、针对精确 SHA 的拒绝。
cargo run --locked -p asset-store --bin pitch-assets -- batch select \
  --batch-id my-pitch-batch --surface '重複語' --vocabulary-id 101 \
  --detail-url 'https://jpdb.io/vocabulary/101/重複語/ちょうふくご'
cargo run --locked -p asset-store --bin pitch-assets -- batch retry \
  --batch-id my-pitch-batch --surface '幽霊' --reason 'повтор после временной ошибки'
cargo run --locked -p asset-store --bin pitch-assets -- batch retry-technical \
  --batch-id my-pitch-batch --reason 'пакетный повтор временных ошибок'
cargo run --locked -p asset-store --bin pitch-assets -- batch reacquire \
  --batch-id my-pitch-batch --surface '幽霊' --reason 'явное обновление записи'
cargo run --locked -p asset-store --bin pitch-assets -- batch reject \
  --batch-id my-pitch-batch --surface '幽霊' \
  --sha256 "$CANDIDATE_SHA256" --reason 'визуально отклонён'

# 审查快照及基于快照的人工决定。
cargo run --locked -p asset-store --bin pitch-assets -- --output json batch review-export \
  --batch-id my-pitch-batch
cargo run --locked -p asset-store --bin pitch-assets -- --output json batch review-apply \
  --batch-id my-pitch-batch --review-id FULL_REVIEW_SHA256
```

外部 `PitchPlan` JSON 要求 `schema_version: 1`，拒绝未知字段或版本。持久批次状态则独立使用**版本 3**；早期版本或使用早期验证器的状态（包括使用验证器 v5 的版本 2）会在解析前以代码 `unsupported_schema_version` 被拒绝，CLI 不会凭历史猜测当前候选尝试或原始计划，也不会沿用早期证据的可信状态；请创建新批次。每项包含准确的 `surface`、可选 `reading`、可选带 `vocabulary_id` 和 `detail_url` 的 `selection`。完全相同的请求合并，具有相同 `surface` 但不兼容的请求在浏览器启动前触发 `identity_conflict`。

创建批次时，CLI 将请求规范化后保存为不可变的 `original_plan` 及其 SHA-256。选择、重试、重新获取只修改当前活动请求，不改变原始计划。重复使用 `batch_id` 必须满足相同原始计划与摘要。为 `ambiguous_vocabulary` 指定选择时，应使用审阅报告中候选的准确 ID 和 `detail_url`；下次 `batch run` 会重新搜索并验证词条。

```json
{
  "schema_version": 1,
  "items": [
    { "surface": "幽霊", "reading": "ゆうれい" },
    {
      "surface": "重複語",
      "reading": "ちょうふくご",
      "selection": {
        "vocabulary_id": 101,
        "detail_url": "https://jpdb.io/vocabulary/101/重複語/ちょうふくご"
      }
    }
  ]
}
```

`batch status` 验证存储完整性，并与规范记录的最新完整快照对账；它不会启动浏览器或访问网络，但存储变化时可能保存更新后的批次状态。JSON 中 `changed: true` 表示持久资源库/批次数据发生变化，包括新状态、候选字节、`review.html`；创建辅助锁或空目录不算数据变化。否则为 `changed: false`。

批次状态包含有类型的提供者结果、尝试历史、歧义、验证决定及发布意图。当前候选绑定准确的尝试编号、世代和内容 SHA-256，不会因不同尝试具有相同 SHA 而混淆请求/证据。候选字节按 SHA 去重，在发布前存于 `.runtime/batches/<id>/candidates/`。浏览器获取期间释放运行时锁，返回后检查项目修订与世代，丢弃过期结果。重启后 `resume` 可使用当前验证器重新验证已保存候选并发布，无需再次向 JPDB 请求。

`batch run` 和 `batch resume` 的 `--workers` 取值 `1`–`4`，默认 `4`；非法值在获取前拒绝。每位 worker 独立拥有 `BrowserSession`、`Page`、CDP 连接、浏览器配置目录、`TMPDIR` 和 `TempWorkspace`。并行完成顺序不一定是计划顺序。默认 4 位来自吞吐量对比测试；如果 JPDB 返回 HTTP 429，请降低并发数。

**单一协调器**串行化状态变化。每位 worker 最多分配一个项目。结果到达时核对分配信息及保存的 token/CAS（世代、请求、修订号）。符合条件的结果和候选字节单独可靠写入检查点，然后发布已经准备好的资源，再分配下一个项目。过期 token 的结果直接丢弃。无论完成顺序如何，检查点立即保存，因此已完成集合未必是计划的连续前缀。未分配及检查点前被中断的项目保持 `pending`。`resume` 对账后重新建立当前 `pending` 队列；已有候选可直接复验/发布。

每次 `batch run`/`batch resume` 开始都会显式清理孤立临时树，即使无需浏览器。worker 仅在启动浏览器时创建临时树。

进度立即输出到 `stderr`：普通模式为文字，`--output json` 时为 JSON Lines。每条 JSONL 事件均有 `schema_version: 1`、`operation: "batch_run"`、`event`、`batch_id`、`elapsed_ms`、`run_completed`、`run_total`、`batch_total`，并始终具有 `workers`（设置的并发数）与 `in_flight`（已分配但未完成数）。`run_total` 是启动对账后 `pending` 的项目数，`batch_total` 是完整批次大小。只有检查点可靠保存后才增加 `run_completed`，过期结果不增加。

按事件还可能出现 `identity`、`worker`、`worker_session`、`session`、`next_session`、`attempt`、`recovery`、`outcome`、`reason`。`worker` 从 `1` 编号，`worker_session` 是 worker 内从 `1` 开始的会话编号；`session`、`next_session` 在当前运行内跨 worker 唯一。事件名称：`run_started`、`item_started`、`retry_started`、`heartbeat`、`item_checkpointed`、`item_discarded_stale`、`item_requeued`、`browser_session_started`、`browser_session_ended`、`browser_session_recovered`、`browser_session_rotated`、`worker_recovery_exhausted`、`run_stopped`、`run_finished`。`attempt` 取自保存的尝试历史。`recovery` 表示 worker 当前连续会话失败数，从 `1` 开始，仅在恢复相关事件中出现。音高重音进度**没有轮次字段**。`ensure` 事件的 `operation` 为 `ensure`。JSON 模式下进度仍在 stderr，stdout **只有一个**最终 JSON 文档。

进程 panic 处理器另行输出包含 `schema_version: 1`、`event`、固定 `message` 的 JSONL `panic` 事件，不含 `operation`、`batch_id` 或进度计数，也不暴露 panic 内容和堆栈。

每位 worker 在下一项目开始前，只要会话达到**64 次已完成请求**或**20 分钟**之一，就独立轮换浏览器。不会为轮换打断已启动的请求；提供者超时为 **90 秒**。

会话故障限于对应 worker。可恢复故障包括启动/设置失败，或最多一个结果且同时包含会话错误的报告。worker 关闭损坏的会话、验证清理成功并创建新会话，其余 worker 继续工作。单个浏览器问题不能阻止整个工作池派发。

报告**没有结果**时，项目放回队列末尾，不计一次尝试、不更改世代、不伪造 `technical_failure`。报告**已有结果**时只保存一次检查点，之后轮换会话，不重复获取该项目。连续失败计数在取得结果后重置；如果同份报告也显示会话失败，该失败作为新连续序列的第一次。正常轮换和 `Ctrl+C` 不计入。worker 连续失败**三次**后退出池并关闭其任务通道；其他 worker 继续。若仍有未处理项，`batch run` 返回 `session_recovery_exhausted`，未处理项仍为 `pending`，不伪造结果/尝试，可以通过 `batch resume` 继续。

无法通过更换会话解决的故障会中止整个运行：返回两个以上结果的报告，或没有结果且没有会话失败的报告（`invalid_provider_report`）、无法确认会话清理（`browser_cleanup_failed`）、worker panic、任务通道关闭、检查点/CAS/存储错误。本地会话恢复不能掩盖这些故障。

收到 `Ctrl+C`、`SIGTERM` 或 `SIGHUP` 后，协调器停止新派发并向正在运行的请求发出取消信号。worker 的浏览器位于各自的进程组中，不会收到终端 `SIGINT`。与信号同时完成、已经被 worker 接收到的结果仍须通过 CAS 并可靠保存。所有会话关闭，等待 worker 清理临时目录后退出。未保存检查点和未分配项继续为 `pending`，不增加虚假尝试。之后的 `batch resume` 对账保存状态，继续当前 `pending`，保留原检查点。

在这一停止过程中再次收到信号不会中断它：否则位于各自进程组中的浏览器会比 CLI 存活更久。信号由进程级监听器处理：当没有任何获取运行拥有浏览器（获取尚未开始或已经关闭）时，`SIGINT`、`SIGTERM` 和 `SIGHUP` 以 `128 + 信号编号`（130、143、129）的退出码结束进程，与默认行为一致。

发布只读取**指定当前尝试**的元数据和 `evidence`，要求 `PitchAccentImageValidator` v6 给出当前 `VERIFIED`。写入前 `AssetStore::ingest_verified` 再次验证字节，通过 compare-and-swap 核对上一记录的预期 SHA。规范路径为 `assets/png/<surface>.png`，消费者名为 `<surface>.pitch.png`。如果最新存储对账证明已有规范记录符合现行验证器，可不联网而复用。

显式 `reacquire` 开始新世代，并记录资源所有者当前 SHA 作为 CAS 基线。如果此后所有者 SHA 变化，`batch status` 会显示 `refresh_owner_drift`，阻止发布过期候选。`batch reacquire` 必须配合 `--reason`，将冲突的发布意图归档，采用当前所有者 SHA 作为新 CAS 基线、开启下一世代，并解除对旧尝试的引用，使其无法发布。重新获取即使产生相同 SHA，也只有通过 v6 再验证且 CAS 与存储 SHA 一致时，才更新元数据/证据；发布总采用准确的当前尝试。视觉拒绝绑定具体 SHA，其他字节不会继承。

定向 `retry` 只处理可重试技术失败，不影响相邻已完成项目。对账后可能出现存储为 `ExistingVerified`、而批次最后一次结果仍是可恢复技术错误；此时仍允许 `retry`，清除 `ExistingVerified`、开始新世代，并记录当前所有者 SHA 作为 CAS 基线。

`batch retry-technical` 必须配合 `--reason`，对全批次按**相同条件**选择可重试技术失败，并为其开始新世代。不可重试错误、已发布项、未完成发布意图、歧义、无重音、找不到词条、冲突项、`pending` 均不变。该操作要么全部应用，要么全部不应用；成功后重复执行不会再选中项目。若所有者对账没有造成其他保存变化，返回 `changed: false`，否则 `changed` 包含该变化。JSON 中 `retried_identities` 按原计划顺序列出选中身份；空集则不提供此字段。`no_pitch_accent_on_source` 只结束**当前批次**，不会创建空记录或永久负缓存；`vocabulary_not_found` 保持独立未解决结果。

`batch review` 将 `review.html` 保存到被 Git 忽略的批次 `.runtime` 中。页面显示获取及规范 PNG、词形/读音、JPDB 链接、检测到的图形变体数量、所选变体及选择依据、PNG 中的图形数量、SHA-256、大小、验证状态及来源/渲染证据。歧义列出全部候选；无重音图、词条不存在、技术故障以有类型结果显示，绝不伪造图片。文字经过转义；HTML 不修改批次状态，也不是权威来源。决策使用明确的 `select`、`reject`、`retry` 和 `reacquire`；没有绕过现行自动验证而手动发布字节的办法。

### 审查快照及其决定

`batch review-export` 使用与 `kanji-assets` 共用的机制创建可寻址快照 `.runtime/batches/<id>/review-<review_id>/`：`review_id` 是规范 `snapshot.json` 的 SHA-256，旁边是精确的 PNG 副本 `<surface>-<SHA>.png`、`index.html` 和可编辑的 `decisions.tsv`。快照列出歧义词条，以及状态为 `acquired_verified`、`published`、`candidate_rejected` 的当前候选；每行绑定世代、项目修订号、请求指纹、尝试编号、精确 SHA 和验证状态。对同一状态再次导出返回相同 `review_id`，不会覆盖已填写的决定文件。JSON 中 `review_id` 标识快照，`review_artifact` 是 `decisions.tsv` 的路径。

只编辑第一列：候选行 `y` 表示已查看——当前自动 `VERIFIED` 按原样接受；`r` 拒绝该精确 SHA；歧义词条行填写数字，即该行候选列中的 JPDB 词条 ID（ID 与 URL 成对取自快照）；空表示无决定，`-` 表示跳过。

`batch review-apply` 原子执行：先检查所有行，再一次性保存批次状态。若签名、某列、行顺序、`snapshot.json` 或 PNG 副本被改动；`review_id` 不属于该批次；导出后某行的世代、修订号、尝试或当前 SHA 已变化（`review устарел`）；或所选 ID 不在该行候选中（`identity_conflict`），则整个文件被拒绝，批次、语料库和日志均不改变。对没有当前验证器最新 `VERIFIED` 的候选填写 `y` 以 `invalid_validation_evidence` 拒绝。竞争命令由批次锁串行化：`review-apply` 等待其完成后再检查各行是否最新。重复应用同一快照是幂等的：已应用的决定计入 `already_applied`，不做任何修改。

信任级别不变：该领域仍为 `AUTOMATED_VERIFIED_ONLY`。`y` 既不发布也不修改所有者记录，因此任何人工决定都不能使 `UNCERTAIN`、`REJECTED`、`CORRUPT`、缺失或旧 v5 证据变为可用。选择词条与 `batch select` 使用相同转换；下一次 `batch run` 会通过 JPDB 搜索再次确认。`r` 与 `batch reject` 使用相同转换；若被拒绝的 SHA 与已发布的所有者记录一致，则通过 `AssetStore::attest` 隔离，并写入 `trust_revoked` 日志事件，重复执行不会写入第二个事件。应用摘要在 `review_decisions` 中返回。

可选的外部检查——例如本地模型的视觉检查（`batch llm-review`，见“本地推理”）——只能为某行附加提示，并绑定到该行身份和精确 SHA，正如 `kanji-assets` 的 OCR。提示永远不会成为决定：只有显式的第一列才是决定，也不能绕过领域的信任规则。

### 从验证器 v5 迁移

使用 `PitchAccentImageValidator` v5 发布的记录包含词条全部图形的并集，且没有边距。验证器 v6 不把它们视为当前记录：规范记录、其字节、尝试历史和人工决定都保留原处，但不被信任，也不被复用。`pitch-corpus-gate` 和 `corpus check` 报告证据已过时，`read_verified_with_policy` 拒绝读取，批次对账将该项标记为 `owner_not_current_verified` 冲突，因此 `ensure` 或 `batch run` 以代码 `3` 结束，既不联网获取，也不覆盖。人工 `Approve` 不会改变这一点。

更新只能显式进行：`ensure --refresh` 或带必需 `--reason` 的 `batch reacquire` 记录当前记录的 SHA 作为 `compare-and-swap` 基线，获取新 PNG，并且仅当验证器 v6 返回 `VERIFIED` 且所有者 SHA 未变化时才发布。有歧义的词条仍需要 `batch select`。迁移前创建的批次不会继续：其状态以代码 `unsupported_schema_version` 被拒绝，因此请为更新创建新批次。

### 清理已经完成的批次

每个 `ensure` 都创建自动生成 ID 的新目录，不接受 `--batch-id`。`batch start` 可提供 ID，不提供时自动生成。目录保留在 `.runtime/batches/`，**不会自动清理**；即使复用已验证资源，历史、候选字节和报告也会不断积累。手动删除前先通过 `batch status` 核对，并把需要的报告/历史保存在存储之外。停止所有使用该存储的 CLI 进程，然后**只删除**指定已完成批次的 `.runtime/batches/<batch_id>/`。需要 `review`、`retry`、`resume`、`reacquire` 的批次不能删除，否则会丢失这些能力和历史，但规范资源库中已经发布的记录、`assets/` 和清单不会因此被删除。

## 从 Yarxi 获取图片

`ensure` 创建独立的无界面 Chromium 会话，与 `https://www.yarxi.su/` 的真实动态页面交互。搜索限制在汉字搜索容器、其中的“Чтение”（读音）输入框和“Найти”（查找）按钮之内。打开“Информация”（信息）标签页后，提供者必须核对所请求字符的准确 Unicode 码点，不匹配的词条会被拒绝。Yarxi 文章编号与来源元数据加入图片来源证据。

普通 `ensure` 优先采用主图片 `img.kakijun-gif`，通过公共浏览器资源层读取 GIF 字节。只有**证明 GIF 不存在**才允许后备方案：标签页及图片区准备就绪、码点准确、相关网络请求无网络/HTTP/JavaScript 错误地完成、DOM 稳定两秒，而且 GIF 元素确实不存在。仍在等待或加载失败的 GIF、超时、未完成图片、网络/JS 问题都属于获取错误，**绝不能**解释成 `gif_absent`。

普通 `ensure` 使用 `AcquisitionTarget::PreferredSource`，先取 GIF。只有确认不存在时才检查 `.font-sample`。如果最左侧符合要求的方块有合适原始 PNG，直接读取字节；如果图形以字体绘制，就截图该方块。显式 `AcquisitionTarget::RenderedFontSamplePng` 即使有 GIF 也会要求渲染 PNG，但不会改变普通 `ensure` 的优先级。

确认 GIF 缺失后，选取最左侧可见、符合条件的 `.font-sample` 方块，排除 `stroke-order`。如果存在可用原始 PNG 就直接读取。字体渲染样本则设置可复现深色方案：`prefers-color-scheme: dark`、Yarxi 根元素的 `data-theme="dark"`，并将页面 `#app` 文字颜色设为当前深色调色板值，避免普通 CSS 固定的浅色文字影响主题。提供者**不修改** `.font-sample` 本身的着色或布局，只截取该 DOM 元素，保留背景、边框、内边距和实际绘制字形，不进行整页截图。来源证据包含方块、CSS 矩形、计算颜色、字体、视口、设备比例和 PNG 尺寸。

右侧 `kakijun.jp` 链接禁止作为图片来源。获准的 `blob:`/`data:` 后备图片字节在浏览器上下文中读取。语义验证前先核对 GIF/PNG 签名，之后解码器完整检查文件、尺寸、非空绘制区域和 GIF 帧流。浏览器渲染样本保持原比例，实际尺寸、视口及设备比例都保存于来源证据。

### 深色 PNG 的人工验收

`yarxi_dark_acceptance` 用于人工核验真实渲染 PNG 及 HTML 报告。外部 JSON 计划可以有任意数量的项目和分组。`items` 不得为空，各项目有唯一 `character`，且须被 `KanjiCharacter` 接受为受支持 Han/CJK Unicode 标量。可选 `group` 提供时不能为空；`unicode_codepoint`、`yarxi_article_number`、`frequency_index` 也可选。提供者总会将字符码点与 Yarxi 文章核对；若指定 `unicode_codepoint`，它也必须匹配。文章号和频率索引按 `evidence` 实际值验证。

例如，将计划保存到工作副本之外：

```json
{
  "items": [
    {
      "character": "漢",
      "group": "пример",
      "unicode_codepoint": "U+6F22",
      "yarxi_article_number": 123,
      "frequency_index": 45
    },
    { "character": "字" }
  ]
}
```

从工作区根目录运行：

```bash
cargo run --locked -p asset-store --bin yarxi_dark_acceptance -- \
  --plan ../yarxi-plan.json
```

默认允许**严格验证后的** Yarxi TLS 警告页面继续访问；`--allow-insecure-tls=false` 禁止例外，要求有效证书。每次运行把证据与报告写成**一个经过验证的 ZIP**，不留下解压目录。默认路径为 `<std::env::temp_dir()>/anki-decks-evidence/yarxi_dark_acceptance-run-<32 lowercase hexadecimal digits>.zip`。`--output <ZIP>` 指定新建的 `.zip` 路径，使用当前目录解析相对路径；父目录必须存在，文件不能已存在，路径须位于工作副本及临时树之外。ZIP 内含 `reports/index.html`、`reports/evidence.json` 和 `reports/images/`，stdout 末尾报告 `LIVE EVIDENCE ZIP` 与绝对 `Path`、`SHA-256`、字节 `Size`。失败时 ZIP 可能保存部分结果与错误。验收不把候选导入存储或正式发布。计划与 ZIP 保持本地，不提交 Git；即使自动验证为 `VERIFIED`，仓库所有者仍须在单独发布前人工查看真实图片。

并行请求使用独立的浏览器 worker，每位都有自己的 `TempWorkspace`、隔离配置目录和 `TMPDIR`。需要 Chrome/Chromium；可用 `CHROME_BIN` 或 `CHROMIUM_BIN` 指定，否则检查 `PATH` 和 Playwright 缓存。浏览器配置、cookie 及会话状态不包含在证据 ZIP 中。

#### 手动测试浏览器会话轮换与汉字批次恢复

为了在真实汉字批次中测试会话轮换，可使用 Cargo 功能 `session-rotation-acceptance` 编译 `kanji-assets`。**只有**这类编译版本的 `batch run` 才接受 `--acceptance-rotate-after-items N`；普通版本没有该参数。正整数 `N` 表示一个浏览器会话内保存了 N 个 `item_checkpointed` 后进行计划轮换，剩余队列在新会话继续。每个会话重新计数，不改变批次成员、尝试、世代、检查点或恢复规则。

该功能仅加快验收。普通默认仍是会话最长 **20 分钟**、每项 **90 秒**、项目间 **900 毫秒**，最多两次获取尝试。未传参数时不提前轮换。

在工作区根目录使用三个字符。观察到 `item_checkpointed`、`browser_session_rotated` 与下一项 `item_started` 后按 `Ctrl+C`。随后用**相同**批次 ID 和临时存储检查/恢复：

```bash
STORE="$(mktemp -d)"
BATCH_ID=yarxi-rotation-check

cargo run --locked --features session-rotation-acceptance -p asset-store --bin kanji-assets -- \
  --store "$STORE" batch start --batch-id "$BATCH_ID" 漢 字 語

cargo run --locked --features session-rotation-acceptance -p asset-store --bin kanji-assets -- \
  --store "$STORE" batch run --batch-id "$BATCH_ID" --rounds 1 \
  --acceptance-rotate-after-items 1

cargo run --locked --features session-rotation-acceptance -p asset-store --bin kanji-assets -- \
  --store "$STORE" batch status --batch-id "$BATCH_ID"

cargo run --locked --features session-rotation-acceptance -p asset-store --bin kanji-assets -- \
  --store "$STORE" batch run --batch-id "$BATCH_ID" --rounds 1 \
  --acceptance-rotate-after-items 1
```

使用 `status` 确认已有检查点保留，第二次 `batch run` 继续剩余项目。验收参数只控制浏览器会话轮换的时机，不改变批次语义。

### 临时目录树与验收 ZIP

两个验收程序共用 `EvidenceRun` 和 `TempWorkspace`。默认私有运行目录为 `/tmp/anki-decks-<uid>/run-<32 lowercase hex>`，权限 `0700`。`ASSET_STORE_TEMP_ROOT` 可指定**已经存在、绝对路径、非符号链接**的替代根目录；此时树位于 `<ASSET_STORE_TEMP_ROOT>/anki-decks-<uid>`，启动垃圾回收**只扫描此根目录而非 `/tmp`**。启动 GC 的结果（包括错误）按绝对根目录在进程生命周期内缓存，避免多个 worker 反复扫描。隐式 `cleanup_orphans`、`plan_orphans`、`cleanup_legacy_orphans`、`snapshot` 及音高重音批次启动清理均使用选定根目录。带 `_under` 后缀的 API 仅使用显式传入的根；`GcReport.root` 在空结果时仍有值。个别树真实错误不会被缓存为成功清理。`EvidenceRun` 固定根目录，确保 `startup_gc`、`final_gc` 及快照采用同一位置。

当前运行树位于按 UID 隔离的命名空间；旧目录 GC 只识别同一根下严格规定的旧名称。替代根适用于 `/tmp` 有限制的环境，使用相对 `TMPDIR` 可缩短 Chromium Unix socket 的最终路径。浏览器 `TMPDIR` 原样传递字节；若非 UTF-8，在浏览器启动前返回 `browser_tmpdir_invalid` 并清理已建配置目录。相对 `ASSET_STORE_TEMP_ROOT` 返回 `InvalidInput`。

每个运行树包含 `.anki-decks-owner.json`，字段为 `schema`、`repository`、`tool`、PID、创建时间、`run_id` 和用途。新 schema 2 还包含 `boot_id`、`process_start_ticks`，并原子发布。旧 schema 1 仍可读，但清理需要至少 **24 小时 TTL**。需要浏览器时才创建其配置目录。`browser-profile-<32 lowercase hex>` 与 Chromium 的简短 `t<3 lowercase hex>` 目录，是同一 `BrowserProfile` 管理的两个相邻私有（`0700`）目录。短名称保证 Unix socket 不超过 `sockaddr_un.sun_path` 限制。

启动时**只有 Chromium 子进程**通过 `BrowserLaunchSpec` 收到 `TMPDIR=<run>/t<3 lowercase hex>`；不修改 `asset-store` 进程环境或用户 shell。仅当规范化的 `ASSET_STORE_TEMP_ROOT` 与 `asset-store` 当前目录一致，才使用相对 `TMPDIR`；其他情况用绝对路径。相对值按 Chromium 进程自己的 `/proc/<pid>/cwd` 验证。

删除配置目录前，检查关联运行树：只要同 UID Chromium 进程的命令行、`TMPDIR` 或文件描述符还指向该树，就拒绝删除并保留目录。必须**先**终止浏览器和 CDP 处理器，再删除浏览器配置/临时目录，最后关闭运行工作区。组装证据后显式关闭；正常退出时 `Drop` 尽可能清理。清理失败包含 `stage`、稳定 `code`、`path_category` 和去除绝对路径的安全消息。ZIP 独立创建，即使临时工作区删除也会保留。

ZIP 必需项：`summary.md`、`manifest.sha256`、`raw-evidence/events.jsonl`、`raw-evidence/technical-failures.json`、`runtime/report-snapshot.json`、`timeline.jsonl`、`commands.json`、`checks.json`、`fault-injection.json`、`metadata.json`、`hygiene.json`、`stdout/acceptance.txt`、`stderr/acceptance.txt`、`logs/diagnostic.jsonl`、`reports/index.html`。所有其他文件也须在 `manifest.sha256` 中由 SHA-256 覆盖。发布前验证全部必需项、安全且唯一的路径、完整 SHA-256 匹配；绝不覆盖已有 ZIP。可选项包括 `reports/evidence.json`、`reports/images/*` 及 `git`、`rustc`、`cargo` 和临时文件系统检查在 `stdout/`、`stderr/` 下的真实输出。

档案只允许由报告、遥测和日志区域组成，**不包括**原始计划、store、所有权标记、浏览器配置、cookie 或会话数据。JSON 证据与诊断事件会清除已知秘密字段；URL 删除查询和片段。`reports/index.html` 是结果预览，不是网页原始 DOM。

创建 `TempWorkspace` 时，GC 查找带所有权标记的孤立树。schema 2 使用准确 `(boot_id, PID, process_start_ticks)` 区分存活所有者、系统重启、PID 消失或重用；经证明死亡的所有者无需等待年龄阈值。schema 1 仍要求最短 **24 小时**。验收程序记录启动和结束阶段对标记树及旧树的清理。

带标记的 GC 只处理私有 `<temp-root>/anki-decks-<uid>/run-<32 lowercase hex>`，必须具有有效项目/工具标记、匹配的 `run_id`、UID 和进程引用证明。默认 `temp-root` 是 `/tmp`，也可被覆盖。schema 2 中不同 `boot_id`、PID 不存在或启动 ticks 不同证明原所有者死亡；身份一致则仍活着。schema 1 使用 PID 和 TTL。

所有检查通过 `NOFOLLOW` 目录描述符进行。外来 UID、不安全权限、跨挂载点、未知符号链接或特殊文件、活跃所有者、未来时间戳、不确定标记或进程身份读取失败，都必须**跳过删除**。删除 schema-2 孤立树前，扫描同 UID 进程的 `cwd`、打开的 fd、`--user-data-dir`（Chromium 还检查 `TMPDIR`），遍历后删除前再查一次。GC 自身安全遍历持有的 fd 排除，但当前进程的 cwd/环境仍需检查。schema-1 旧 Chromium 检查在存在浏览器文件时进行。

`TMPDIR` 检查先解析已有祖先及符号链接，然后处理不存在后缀中的普通名称。确定不存在且位于树外的路径不阻止其他目录的清理。指向树内的链接、权限失败、悬空链接、在不存在祖先后出现的 `..` 会保留目录。绝对 `TMPDIR` 无需读取 cwd，相对值要求已证明的进程 cwd。进程消失须通过 `/proc` 中的 PID 项单独确认，不可混同于某项读取失败。

必要进程信息无法取得时跳过该树。对于**经过确认的孤立树**，只识别 Chromium 自己创建的形式：配置中的 `SingletonCookie`、`SingletonLock`、`SingletonSocket`、目录 `org.chromium.Chromium.<6 alphanumeric characters>`、它指向 cookie/socket 的链接，以及私有浏览器临时目录内的常规 `.org.chromium.Chromium.<6 alphanumeric characters>` 文件。绝不跟随链接，删除前检查类型、fd 边界、UID 和挂载点。全局 `/tmp/org.chromium.*`、`/tmp/.org.chromium.*` **不属于清理目标**。

首次 `snapshot` 在启动 GC **前**取得，末次快照在删除当前运行工作区并完成最终 GC 后取得。`hygiene.json` 记录清理结果及工作区是否删除。

验收程序在启动和发布 ZIP 前执行 `cleanup_legacy_orphans(min_age)`，仅识别严格旧名称：`anki-manifest-<listed-label>-<pid>-ThreadId(<id>)`、`asset-store-pitch-cli-<pid>-<sequence>`、`asset-store-pitch-batch-<pid>-<sequence>`、`jpdb-live-acceptance-<pid>-<timestamp>`、`yarxi-live-acceptance-<pid>-<timestamp>`、`kanji-<32 lowercase hex>`。旧目录 TTL 不得少于 **24 小时**；若名称含 PID，存活进程阻止删除。还要求正确 UID、组/其他用户不可写、无符号链接、挂载点边界或特殊文件；任何无法证明安全的目录都跳过。`snapshot()` 只测量已知临时树，不删除。两个 GC **只处理各自已识别前缀**，不遍历系统临时目录中的任意内容。

### Yarxi 证书

目前 Chrome 可能显示 Yarxi 的 `ERR_CERT_AUTHORITY_INVALID`。默认仅允许在 Chrome 警告页对**原始 URL**、准确主机 `www.yarxi.su`、大小写不敏感的准确错误码 `ERR_CERT_AUTHORITY_INVALID`、并存在真实继续访问链接时点击“Перейти на сайт”（继续访问）。**不会**启用全局 `--ignore-certificate-errors` 或关闭整个进程的 TLS 验证。例外仅限 Yarxi；`--allow-insecure-tls=false` 强制使用有效证书。其他主机、TLS 错误或 URL 形式都导致运行终止。

## 语义验证器与校准测试

`KanjiImageValidator` 分析 PNG/GIF 的**真实像素**，不根据文件名、URL、DOM 文本或来源说明进行判断。PNG 完整解码。`GifDecoder::into_frames()` 按 GIF 帧替换规则合成帧；逐帧检查后选择前景图形面积最大的帧。没有前景的帧（例如笔顺动画中的空白帧）会被跳过，而不会导致整个 GIF 被拒绝；所有帧都没有前景时，GIF 被拒绝。

汉字验证器和领域批处理适配器限制图片最大 **8 MiB**。公共批处理运行时为状态和内容文件规定 **64 MiB** 上限，领域适配器可以进一步降低。人工批准时采用当前领域策略限制，未定义则为 8 MiB。读取最多允许上限再加一个字节，用于在不无限分配内存的情况下检测超限，之后仍必须检查完整性并完整解码。

主要参考字形库来自 KanjiVG 发布版 `r20250816`，提交 `bd13ffbcc9d85cb86ae98bbbf001d9069220b901`。按 Unicode 码点查找的 SVG 被规范化为 64×64 二值遮罩，包含 **6,430** 个字符，压缩于 `src/data/kanjivg-r20250816.maskdb.zlib`，SHA-256 为 `5581e65d5a681cdf8e441be04c1cea5f6ec61342ce8830ed083ba417d83a88ff`。KanjiVG 使用 CC BY-SA 3.0；版权归属、版本与原始档案 SHA-256 见 `src/data/kanjivg-ATTRIBUTION.txt`。

深色方块另采用公开发布的 Noto Serif JP Regular 1.001 字体作为附加像素参考：`google/fonts` 仓库固定提交 `beff1d8226e598dfaf06f9f8312c01021f70e9d2` 中的 `ofl/notoserifjp/NotoSerifJP-Regular.otf`，SHA-256 为 `a12875dcde3979f7901cd52c6f750f763e5ad2824cf3c27019a61dd5de5a8d9d`，许可为 SIL Open Font License 1.1。这是独立来源，而不是 Yarxi 网站过去提供的字体文件：那个 TTF 已无法获取，网站及其数据也不再是生成输入。由该字体生成 **6,428** 个规范遮罩，存于 `src/data/noto-serif-jp-1.001-regular.maskdb.zlib`，SHA-256 `cce25df61382ccb8e14d1eca181609916aac483b4949fcbf637f4fb5ac353608`；字体本身**未提交**仓库。来源、许可和重新生成命令见 `src/data/noto-serif-jp-ATTRIBUTION.txt`。

生成器 `build-noto-font-reference` 校验字体 SHA-256，只使用固定 KanjiVG 字形库中同时存在于字体 cmap 的字符。对 cmap 之外的字符，`resvg` 会绘制 `.notdef` 字形，同一个方框会成为多个无关 Unicode 字符的参考；因此跳过 U+55BB 和 U+27491。测试 `noto_catalog_has_no_notdef_templates` 只允许规范等价的字符（CJK 兼容表意文字及其统一形式）具有相同遮罩。

参考库生成器仅在 `reference-generator` 功能中使用 `resvg` 和 `skrifa`；生产验证器读取已嵌入的字形库，与 `resvg` 版本无关。两个字形库在 `resvg 0.48.1` 上均可逐字节复现，并与 `resvg 0.47.0` 在相同输入上的结果一致。新的 Noto 字形库与先前基于 Yarxi TTF 的字形库在 6,428 个遮罩中有 6,257 个不同（中位数为 4,096 个像素中的 7 个），因此只在新的验证器版本下被接受。测试 `embedded_reference_catalogs_match_registered_identity_and_attribution` 将两个嵌入字形库及原始字体的 SHA-256 与验证器版本及版权归属文件绑定，未同步更换验证器标识就替换字形库将无法通过检查。

验证器先从图像四角估计背景，通过对比度决定极性，删除边缘相连的边框成分，并提取前景字形遮罩。参考库生成器和生产验证器共用 `kanji_mask::normalize_binary_mask`：裁剪到非空前景，缩放适配到 56×56，再居中放入 64×64。每个输出像素按照 `(2*i+1)*source_size/(2*output_size)` 选择原始网格中心最近的像素。因此 KanjiVG、Noto 和候选图都采用相同规范化算法。

在居中遮罩之间计算**对称 Chamfer 距离**：两个方向分别求像素到对方最近像素的平均距离，再按遮罩大小归一化。深色 PNG 同时与 KanjiVG 和 Noto Serif JP 比较，取较小距离。最近的**其他** Unicode 字符在所有适用的固定字形库并集中查找，确保目标和竞争字符接受相同字体覆盖。验证器版本为 `kanjivg-r20250816-5581e65d-noto-serif-jp-1.001-a12875dc-cce25df6-mask64-v7`。

阈值属于正式验证契约：

- `VERIFIED`：与目标参考的距离 ≤ `0.020`，且比最近其他 Unicode 字符至少好 `0.004`。
- `REJECTED`：目标参考距离比最近其他字符差至少 `0.015`。
- 介于两者之间为 `UNCERTAIN`。
- 无效、损坏或空数据为 `CORRUPT`；缺少参考字形为 `UNCERTAIN`。

阈值变化必须产生新的验证器版本和对应回归测试。不确定图形保持 `UNCERTAIN`，不能根据文件名、文章或来源说明强制通过。

使用固定库进行离线可复现校准：

```bash
cargo test -p asset-store offline_threshold_calibration -- --nocapture
```

测试从 Noto 字形库按 Unicode 顺序均匀选取 **64** 个字形，渲染成 142 px 的合成方块，计算正确身份的距离、区分边际和状态。每个样本还作为最近的竞争 Unicode 字符进行验证，禁止竞争者得到 `VERIFIED`。输出按状态统计数量、最大正确距离、最小边际、最大竞争差值及其验证状态。数据完全固定、离线，不依赖人工验收或当前字符计划。

当前 v7 验证器中，64 个正向合成案例有 **58 个 `VERIFIED`**、**6 个 `UNCERTAIN`**、**0 个 `REJECTED`**；最大参考距离 `0.004004`，最小区分边际 `0.001969`。最近竞争字符中 **0** 个 `VERIFIED`、**5** 个 `REJECTED`、**59** 个 `UNCERTAIN`，最大差值 `0.022829`。先前的 v6 验证器在同一样本上得到相同的 58/6/0 和 0/5/59。样本从 16 个扩大到 64 个字形：在 16 个字形上 v6 的结果恰好位于 75 % 边界，更换一个字形就会改变结论；而在全部 6,428 个参考上，v6 和 v7 的 `VERIFIED` 比例都约为 92 %。这些统计**不修改阈值**。

使用内置 Unicode 参考和合成像素的确定性回归测试涵盖：

- 在 PNG 与动画 GIF 中准确重现 KanjiVG 遮罩。
- Noto Serif JP 风格、带边框的 Yarxi 深色方块，以及对错误 Unicode 身份的拒绝。
- 指定比例的深色方块和近似但错误字符的拒绝。
- Yarxi GIF 的“完整帧 → 逐步描绘 → 完整帧”，检查最终完整的语义遮罩，而非选择了第几帧。
- 含空白首帧或中间帧的 GIF 在验证和 OCR 准备中都选择完整帧，仅由空白帧组成的 GIF 被拒绝。
- 视觉相近但错误的 Unicode 字符。
- 笔画变粗和裁剪帧。
- 损坏签名/帧流、空图与缺失参考。
- 距离、边际和拒绝阈值的边界。

校准仅覆盖固定字形和最近的错误竞争字符，**不估计**真实 Yarxi 图像或所有生产来源的误通过率。它不能取代人工查看真实来源 PNG；区分不充分时仍为 `UNCERTAIN`。

## 命令

从工作区根目录执行：

```bash
# 幂等地获取并验证一个或多个汉字。
cargo run --locked --bin kanji-assets -- \
  --output json ensure 漢 字

# 只验证新增或已过期的验证决定。
cargo run --locked --bin kanji-assets -- --output json validate --mode new

# 重新验证全部资源。
cargo run --locked --bin kanji-assets -- --output json validate --mode full

# 查看状态和确定性的验证计划。
cargo run --locked --bin kanji-assets -- --output json list
cargo run --locked --bin kanji-assets -- --output json plan \
  --mode new --validator-id kanjivg-pixel-chamfer \
  --validator-version kanjivg-r20250816-5581e65d-noto-serif-jp-1.001-a12875dc-cce25df6-mask64-v7

# 手动导入本地候选文件（它将保持 pending 状态）。
cargo run --locked --bin kanji-assets -- --output json ingest \
  --character 漢 --file ./candidate.png

# 创建批次（ID 自动生成），并最多执行五轮广度优先遍历。
cargo run --locked --bin kanji-assets -- --output json batch start 漢 字
cargo run --locked --bin kanji-assets -- --output json batch run --batch-id <id>

# 稍后继续批次、以 JSON 查看状态，或写入本地 HTML 报告。
cargo run --locked --bin kanji-assets -- --output json batch status --batch-id <id>
cargo run --locked --bin kanji-assets -- --output json batch review --batch-id <id>

# 针对精确 SHA-256 应用明确的用户决定。
cargo run --locked --bin kanji-assets -- --output json batch decide \
  --batch-id <id> --character 漢 --sha256 <sha256> \
  --action confirm --reason "пользователь подтвердил текущий кандидат"

# 在一系列技术故障之后，允许新的获取世代。
cargo run --locked --bin kanji-assets -- --output json batch retry \
  --batch-id <id> --character 字 --reason "источник снова доступен"
```

`--store <path>` 指定存储根目录，`--repository-root <path>` 指定用来验证 `decks/` 边界的工作副本。默认启用严格受限的 Yarxi TLS 警告处理；`--allow-insecure-tls=false` 要求有效证书。`ensure` 只保存确实通过验证的 `VERIFIED` 记录；复用可信且 SHA 相同的资源无需网络请求。

`list`、`plan`、`validate` 都会打开存储。打开过程可能创建状态文件、恢复中断操作或把旧记录迁移到 `.runtime` 边界。`plan` 自身不写数据，但 `changed: false` **不表示**打开过程完全只读。`validate` 调用核心验证器：`new` 只选择缺失或已过期决定，`full` 重新检查整个清单。验证失败的状态会进入隔离区；请求的目标未达到 `VERIFIED` 时退出码非零。

JSON 响应包含 `schema_version`、`operation`、`store`、`mode`、`assets`、`changed`、`conflicts`、`blockers`、`outcome`；错误包含稳定的 `error.code`、`category`、`message`、`details`。`ensure` 的每项资源有 `item_outcome`，防止某些字符成功掩盖其他字符失败。

`ingest` 只读取**一个**明确指定、位于 `decks/` 之外的源文件，并创建 `pending` 候选。格式和哈希从真实字节识别。需要按原状态 CAS 替换时，使用 `--replace-expected-sha256 <current-hash>`。

### 汉字批处理

`batch start [--batch-id ID] CHARACTER...` 去除重复身份，在 `.runtime/batches/<id>/` 创建有版本的状态，并在获取前标记资源所有者已经确认仍然有效的 `verified` 记录。若使用已存在的 ID，请求身份集合及验证器必须一致；继续已有状态，而不是创建新批次。

`batch run --batch-id ID [--rounds 1..5] [--interactive-review]` 最多执行五轮获取。新批次采用 `kanji-single-candidate-v1`：技术上有效但判为 `UNCERTAIN` 或 `REJECTED` 的候选立即进入 `awaiting_human`，不通过重复下载其他图片或平均结果来提升可信度。可重试的技术失败仍最多五轮。每轮处理当前整个队列再进入下一轮 `retry`；已经发布或仍有效的验证资源不重复请求。单项失败保留在 `issues`，不撤销其他项目成功结果。浏览器获取期间不持有批次锁。每项真实结果返回后，CLI 重新取得运行时锁，验证世代和尝试数、保存候选字节及 `evidence`，再原子写入检查点。获取期间项目状态已变化的旧结果会丢弃。

`batch run --workers` 支持 `1`–`4`，默认 `4`；每位 worker 使用独立浏览器会话。默认值来自针对相同 **300 个汉字**的网络吞吐量对比。

`batch run` 进度立即流向 `stderr`：普通模式为可读文字，`--output json` 为 JSON Lines。各 JSONL 事件包括 `schema_version: 1`、`operation: "batch_run"`、`event`、`batch_id`、`elapsed_ms`、`round`、`round_limit`、`round_completed`、`round_total`、`run_completed`、`batch_total`、`workers`、`in_flight`。仅有上下文时才出现 `identity`、`session`、`worker_id`、`browser_session_id`、`attempt`、`generation`、`owner_attempt`、`item_elapsed_ms`、`outcome`、`reason`；可读输出也显示相关 worker、会话和所有者尝试。worker 池中的 `heartbeat` 只由协调器为活动分配发送，含会话准备阶段；内部获取 heartbeat 不重复输出。

可选 `stage_timings` 是 `{ "stage": "…", "duration_ms": N }` 对象数组，提供者给出获取阶段诊断时附于 `item_checkpointed` 或 `item_discarded_stale`，数组也可能为空。它记录**验证结果和保存检查点之前**的实测获取阶段，因此完整诊断日志另外记录验证与保存步骤。

稳定进度事件：`run_started`、`round_started`、`browser_session_started`、`browser_session_ended`、`browser_session_rotated`、`browser_session_bootstrap_started`、`browser_session_recovery_started`、`worker_assigned`、`worker_retired`、`worker_stopped`、`worker_exhausted`、`item_started`、`retry_started`、`retry_recovery_started`、`heartbeat`、`item_checkpointed`、`item_discarded_stale`、`item_deferred`、`item_requeued`、`round_finished`、`run_stopped`、`run_finished`。单项获取错误作为项目结果保存；CDP/监视器失败、超时或页面恢复失败反映在会话结束原因中。每次检查点后（包括最后一个项目）检查监视器健康，**已保存结果不会撤销或再次分配**。若监视器故障后队列已空，且新获取没有重新证明会话健康，运行返回错误但保留已有结果。若仍有待处理项目，仅恢复用于这些项目的会话。写入/完整性或清理失败直接停止，不启动自动恢复。会话初始准备仅对 `browser_session_setup_runtime_unhealthy` 重试；浏览器启动、TLS 及其他设置错误立即按原原因停止，不伪造 worker 恢复预算耗尽。

只有检查点可靠保存才增加 `round_completed`。JSON 模式仍将独立的单个最终 JSON 写到 `stdout`，不混入进度。

每次 `kanji-assets batch run` 与音高重音批处理 worker（`pitch-assets ensure`、`batch run`、`batch resume`）都会创建独立详细日志 `<store_root>/.runtime/batches/<batch_id>/logs/<run_id>.jsonl`。`SafeBatchRuntime` 在已经验证的目录边界内创建文件，不跟随符号链接，也不覆盖其他运行的日志。最终 JSON 的可选 `diagnostic_log` 包含其路径，包括失败情况；普通模式启动时向 `stderr` 打印。

详细日志为带时间戳、级别、模块和结构化字段的 JSONL，记录安全的类型化错误、会话、JPDB 阶段、检查点、CAS 和停止原因，**不包含 DOM、cookie 或授权头**。`--output json` 时，详细诊断仅写文件，`stderr` 保持纯进度 JSONL，不混入格式化诊断。普通模式可额外在 `stderr` 输出可读 tracing 信息，stdout 仍为命令结果。`browser_acquisition_*` DEBUG 事件只写文件：可读层仅接受 INFO，JSON 模式完全关闭该层。获取前创建日志，返回前等待全部事件写入。

公共的单项/阶段 DEBUG 事件名是 `browser_acquisition_item` 和 `browser_acquisition_stage`，使用 `schema: "browser_acquisition_v1"`、`schema_version: 1`。单项事件的 `stage` 为 `item`；`stage_duration_ms` 是当前阶段耗时，`item_duration_ms` 是项目开始后的累计时间。音高重音各 worker 分配有独立单调时钟计时器，沿 worker → 提供者 → 协调器传递直到 CAS 检查点。Yarxi 计时覆盖内部重试/退避、候选验证和检查点。项目总耗时包括获取、最终验证和持久保存。仅运行 JPDB 提供者时，没有所有者检查点，计时到获取结束即止。

必需字段：`event`、`schema`、`schema_version`、`provider`、清理过的 `identity`、`attempt`、`generation`、`stage`、`stage_duration_ms`、`item_duration_ms`、`outcome`、`retryable`。可选 `worker`、`worker_session`、`browser_session`、`failure_code`、`stop_reason`、`runtime_snapshot`。JPDB 的 `provider` 为 `jpdb`，Yarxi 为 `yarxi-suu-browser`。音高重音批次中 worker/session 是 worker 与本地会话标识，其他场景可缺失。`browser_session` 是诊断会话 ID。`provider`、`outcome`、`failure_code`、`stop_reason` 限制为安全 token，不合法值替换为 `redacted`。`identity` 清理并限制在 **256 字节**。失败/中断时如有数据会提供 `failure_code` 和最新 `runtime_snapshot`，`stop_reason` 单独解释停止。`retryable` 表示提供者判断可否重试，**不是**已经重试的事实。`generation` 是汉字批次所有者世代，在只有提供者、无相应上下文时为 `null`，不使用虚构的零。若领域结果之后才发生监视器/会话故障，项目保留原结果，`stop_reason` 与新快照解释队列停止。

`stage` 与真实执行边界一致。JPDB 阶段：`browser_launch`、`configure_browser`、`request_validation`、`search_resolution`、`search_navigation`、`search_readiness`、`detail_navigation`、`detail_readiness`、`detail_verification`、`pitch_inspection`、`capture`、`post_capture_verification`、`candidate_validation`、`outcome_validation`、`checkpoint`。后三者分别涉及音高重音结果验证与持久保存；`checkpoint` 包含重新获取锁、重读状态、CAS 及保存。直接重定向到词条详情可能没有 `detail_navigation`。使用默认方法的其他 `PitchRunDriver` 将获取整体记作 `provider_acquisition`，生产 JPDB 驱动则记录细分阶段。

Yarxi 阶段：`search_submission`、`information_tab`、`article_verification`、`media_readiness`、`media_selection`、`dark_theme`、`resource_read`、`font_sample_capture`、`retry_backoff`、`retry_recovery`、`semantic_validation`、`final_validation`、`checkpoint`。最后三者覆盖图片语义、候选最终验证和 CAS 检查点。嵌套或并行阶段的时间可能重叠，**不得简单相加**。成功阶段 `outcome: "success"`。JPDB 的成功项目结果为 `acquired`、`no_pitch_accent_on_source`、`ambiguous_vocabulary`、`vocabulary_not_found`；失败/中断为 `failure`、`interrupted`，原因在 `failure_code`；成功 Yarxi 项目的结果为 `success`。

`runtime_snapshot` 是最新 CDP 遥测的 JSON 投影，以字符串嵌入，限制 **32 KiB**。包括 `epoch`、`pending_request_count`、`network_failure_count`、`http_error_count`，及 `pending_requests`、`network_failures`、`http_errors` 中每类最多 16 条示例，还有 `javascript_exceptions`、`monitor_failed`。旧 epoch 有独立的 `stale_pending_request_count`、`stale_network_failure_count`、`stale_http_error_count`、`stale_pending_requests`、`stale_network_failures`、`stale_http_errors`；JavaScript 异常按当前、初始、旧 epoch 分组。当前/旧列表每组最多 **16** 项。样本包含 SHA-256 哈希 `request_key`（不是原始 CDP ID）、资源类型、epoch、是否当前/顶层、观察时间、HTTP 状态和网络错误类别。快照中**没有**原始 URL、请求头、cookie、DOM 或 CDP 消息。超限后保留带 `truncated: true` 的紧凑对象、当前 epoch、当前/旧计数、JS 异常分布及 `monitor_failed`，计数来源仍为同一个诊断投影。

单项获取事件与 `browser_session_started`、`browser_session_ended`、`browser_session_rotated` 会话生命周期事件分开。公共 `browser_runtime` 还记录 `event: "browser_lifecycle"`，反映进程所拥有的传输、CDP 处理器、遥测和配置/临时目录的停止或清理。传输断开**不足以证明**清理成功：只有进程确定停止且所属目录成功删除，断开的传输才算无害。否则仍是 `browser_cleanup_failed`；运行工作区本身的清理错误保留 `temp_workspace_cleanup_failed`。

从一次运行中提取错误示例：

```bash
jq -c 'select(.level == "ERROR") | .fields' \
  <store_root>/.runtime/batches/<batch_id>/logs/<run_id>.jsonl
```

**20 分钟**限制针对单次浏览器会话；在下一项目之前到期，就关闭会话并用新会话继续队列。超时或页面恢复错误只能为**实际已经启动**的项目产生技术结果；未启动项目保持 `pending`，不增加尝试、世代或汇总结果。

在 `browser_session_started` 前，Yarxi 必须确认新会话没有待处理请求、网络/HTTP 错误、JS 异常或监视器失败。如果只有待处理请求，最多等待 **20 秒**；其他错误立即视为不健康。准备超时或不健康时关闭当前会话，在最多**两个新会话**中重新准备同一队列位置，期间不记录项目尝试。成功准备后恢复计数清零。清理或其他设置错误停止命令，但保留已经完成的项目；下次 `batch run` 继续剩余项。`Ctrl+C` 后提供者关闭当前会话，并在最后一个已保存结果后结束；没有保存检查点的项目仍可用相同 `batch_id` 重试。

自动发布之前先把候选尝试与字节保存到批次 `.runtime`。单一 `VERIFIED` 候选须通过领域所有者验证后，才能原子发布到规范路径。旧策略批次对技术有效但 SHA 不同的候选，分别计算 `expected_distance` 与 `nearest_margin` 的平均、最小、最大和数量；重复 SHA 不增加样本。`kanji-distinct-mean-v2` 保留 `0.020`/`0.004` 阈值，要求选中候选通过与最近其他字形的区分边际，并固定其准确 SHA。汇总信任**不能取代**选中候选自身的自动 `VERIFIED`；汇总版本和证据单独存储。

技术重试全部用尽后，没有候选的项目成为 `awaiting_human`；新批次中技术有效但不确定的候选首次保存后就等待人工。`batch review` 生成本地、被 Git 忽略的 HTML 报告，包括动画 GIF、所有不同候选图片、准确 SHA、自动验证结果、每次尝试证据及汇总指标。候选保存在按 SHA 寻址的 `candidates/`；HTML 不是权威来源。

`batch decide` 仅允许针对当前 `--character`、`--sha256` 执行结构化的 `confirm`、`reject`、`reacquire`。`confirm` 必须在人工批准**之前**完整解码 GIF/PNG、验证技术正确性、准确的源 SHA 与 CAS，以及领域所有者自动证据。`reject` 记录针对准确候选的人类决定，只为该身份开始新一代获取。`reacquire` 计划定向重新获取，但不判断语义是否正确。没有候选且五次技术失败后使用不带 `--sha256` 的 `batch retry`；有候选则要求 SHA。

`batch status` 返回状态，`batch review` 另外写入报告。批次 JSON 包含 `batch_id`、`counts`、`items`、项目 `issues`、`blockers`、可选 `review_artifact`，及完整的 `batch` 状态和证据。只有所有资源都在规范存储中解决，`run` 才返回 `0`；等待人工或仅部分完成返回 `3`。各项错误绑定各自身份。成功的 `start`、`status`、`review`、`decide`、`retry` 返回 `0`；目录边界、结构、完整性等系统失败使用资源所有者的标准退出码。

进程重启后使用相同 ID，按 `status` → `run`/`review`/`decide` 继续。已保存候选和人工意图可恢复准确的所有者发布，无需再次获取。`.runtime`、HTML 和候选字节不是规范资源库内容，**不可提交 Git**。

### 文本决策包与显式交互

普通命令、管道和 JSON 模式**不弹出问题，也不启动外部应用**。必须在完整 TTY 中明确启用交互：

```bash
cargo run --quiet --locked --bin kanji-assets -- batch run --batch-id ID --interactive-review
# 在没有网络获取的情况下回到已保存的候选：
cargo run --quiet --locked --bin kanji-assets -- batch review-interactive --batch-id ID
```

获取及浏览器清理完成后若仍有候选，显示 `[y/N]`。回答 `y`/`Y` 生成派生决策包并打开文件管理器和编辑器。保存之后还须**再次**回答 `y`/`Y` 才能应用；退出 GUI 进程不会自动批准任何内容。拒绝、EOF、`Ctrl+C` 或应用启动失败会保留候选及已完成的工作。空队列不询问；编辑时**不持有批次锁**。获取失败不会进入交互阶段。

无需 GUI 的命令：

```bash
cargo run --quiet --locked --bin kanji-assets -- --output json batch review-export --batch-id ID
# 编辑指定的 review_artifact，然后用其 review_id 应用：
cargo run --quiet --locked --bin kanji-assets -- --output json batch review-apply --batch-id ID --review-id FULL_REVIEW_SHA256
```

`review-export` 返回 `review_id` 及 `review_artifact` 路径，指向 `decisions.tsv`。目录 `.runtime/batches/<id>/review-<review_id>/` 包含元数据快照、可编辑 UTF-8 TSV、用于查看建议分组的 `index.html`，及独立验证的 GIF/PNG 副本 `<Unicode>-<full SHA>.<ext>`。普通批次还把图片嵌入 HTML，使 Windows/WSL 浏览不依赖过长的单独图片路径。内嵌原始图片总预算为 **32 MiB**；超出后使用副本的相对链接。原始文件不变。再次导出相同快照会保留已填内容；图片副本被修改时，在重新打开或应用时拒绝。仅需 HTML 时仍使用 `batch review`。

`decisions.tsv` **只能编辑第一列**，字段分隔符是 TAB：

- `y`：确认当前候选的**准确字节**。
- `e`：拒绝不合适图片。
- 单个其他受支持的 Han 字符：以 `actual_character=<Unicode>` 为原因拒绝。
- 空值或 `-`：该行暂不决策。

支持 BOM、CRLF 和以 `#` 开头的注释。任意单词、修改其他列、签名、结构、批次、世代、尝试数、SHA 或路径，以及缺失、额外、重复的行，均会导致**整个文件**被拒绝。只读取选定快照的 `decisions.tsv`，不扫描备份或隐藏文件；不打开不可信路径或符号链接。快照通过元数据 SHA 寻址，各行与资源所有者当前状态核对。

先在锁内检查**全部行**，然后通过 `KanjiBatch::decide` 将全部决定原子保存到普通批次状态。其 `human_decisions` 是**唯一的意图日志**，不存在第二套信任来源。发布仍经过正常解码/SHA/CAS 和所有者人工批准。保存意图后若发布失败，重复 `review-apply` 或 `batch run` 会继续发布；已应用行幂等，**即使新世代出现相同 SHA，也不会自动批准新世代**。发布错误显示在 `issues` 和退出码 `3`，`review_decisions` 汇总应用、确认、拒绝、跳过及已应用数量。

文件式拒绝采用 `reject_and_hold` 和 `rejected` 状态，**不会自动重新获取**。如需主动重试，执行 `batch retry --batch-id ID --character 漢 --reason '明确重试'`，不带 SHA，因为当前候选已拒绝。旧 `batch decide reject` 保留原来规划新世代的行为。错误识别为其他字符时，绝不把它变成新的身份或重命名图片。

`--interactive-review` 和 `review-interactive` 与 `--output json` 不兼容，会在打开 store 或获取前拒绝。也拒绝不完整 TTY、`TERM=dumb`、CI/`CODEX_NONINTERACTIVE`、`cargo test` 或测试框架的子进程，**包括使用 PTY 的情况**；`cargo run` 允许。进程必须处于终端前台进程组，后台任务不询问；普通测试不会启动应用。

打开 `decisions.tsv` 时先尝试已配置文本编辑器。如果失败，尝试 `text/plain` 桌面关联，**不是 `.tsv` 关联**，避免电子表格软件损坏格式（`xdg-mime`、`gtk-launch`；缺少后者时可用 `gio launch` 打开已找到的 desktop 条目）。目录先通过 Linux MIME 的 `xdg-open` 打开。若失败或 WSL 没有 Linux 桌面，并启用了 binfmt 互操作，则通过 `PATH` 或以 `wslpath` 转换的已验证 Windows 路径启动 Explorer。Windows 程序使用的 `\\\\wsl.localhost\\...` UNC 路径会尽可能缩短为 `\\\\wsl$\\...`。仍过长时，应改用 Linux 编辑器或更短的存储路径。

未配置编辑器且没有 Linux `text/plain` 关联时，可通过 WSL 互操作尝试 Windows Notepad（程序必须存在）。过长路径会返回明确错误并建议 Linux 编辑器或缩短存储位置。显式配置的 Windows 编辑器无需 Explorer，但必须启用互操作。Windows 可执行文件路径通过 `wslpath -u` 转换；`.exe` 编辑器的 TSV 路径（包括 `PATH` 命令或 `/mnt/...` 路径）通过 `wslpath -w` 转换。路径转换、启动、退出失败都有诊断。

**不要求** Windows、WSL 或 `notepad.exe`。无法打开应用时，打印目录和 TSV 绝对路径以及 `review-export`/`review-apply` 指令。编辑器选择顺序为 `GIT_EDITOR`、Git `core.editor`、`VISUAL`、`EDITOR`；支持引号和含空格参数，**不支持** shell 命令、替换或管道。目录打开与 TSV 编辑器启动的失败分别报告；成功打开目录不代表编辑器成功。全部启动失败时保留批次，仍可使用手动命令。用户配置的编辑器是可信程序；禁止 shell 语法**不是沙箱**。编辑器配置必须为 UTF-8，Linux 可执行文件接收非 UTF-8 TSV 文件系统路径时仍保留原始字节。编辑器直接启动，决策文字不会被执行。进程退出不代表批准或应用：用户显式确认后重新读取 TSV。无 `DISPLAY`/`WAYLAND_DISPLAY` 的服务器中，配置的终端编辑器只能在此显式交互模式使用；没有应用时打印路径和命令，绝不安装桌面软件。

旧 `kanji-distinct-mean-v2` 批次仍按旧规则读取。显式 `batch revalidate --batch-id ID` 使用当前验证器/解码器重新验证保存的字节，并原子迁移到 `kanji-single-candidate-v1`，**不联网、不伪造尝试**。旧人工批准保留在 `human_decisions`，与准确 SHA 绑定。相同字节通过当前技术检查且非 `Corrupt` 时，旧批准仍决定 `HumanVerified`；否则仅作为历史保存，重新计算有效状态并清理发布标志。`ExistingVerified` 继续受资源所有者信任规则约束，迁移后与最新规范存储对账。不同版本验证器的旧自动状态不继承；批次状态/版本变化后应重新导出编辑快照。

### 本地推理：llama.cpp 与 Ollama

两个可选任务使用本地视觉模型：对已保存的不确定汉字候选进行盲识别 OCR（`kanji_ocr`），以及对已保存的 JPDB PNG 进行视觉检查（`jpdb_pitch_review`）。普通 `start`、`run`、`status`、`review`、已验证资源读取和发布从不调用模型。模型结果只是给人的**建议**：它从不授予 `AutoVerified` 或 `HumanVerified`，也不改变候选、决定、信任或汉字验证器。

#### 配置

设置由多个层组合而成，后一层替换前一层的键：

1. `tools/asset-store/config/inference.toml` —— 受版本控制的默认值，编译进二进制，并用注释说明每个键；
2. `tools/asset-store/config/inference.local.toml` —— 工作副本的个人部分覆盖（被 Git 忽略；样例为 `inference.local.example.toml`）；
3. `--inference-config FILE` —— 同格式的显式文件；
4. `--inference-set KEY=VALUE` 以及针对所选任务配置档的快捷参数 `--backend`、`--model`、`--endpoint`、`--allow-remote`、`--timeout-seconds`。

模式是严格的（`schema_version = 1`）：未知键、错误类型、超出范围的值或指向不存在配置档的引用，都会在启动模型之前返回 `inference_config_invalid`（退出码 `3`），其他 `schema_version` 值返回 `unsupported_schema_version`（退出码 `4`）。配置分为三层：

- `[backends.<名称>]` —— 推理运行方式：`kind = "llama_server"`（托管 `mode = "managed"` 或外部 `mode = "external"`）或 `kind = "ollama"`；
- `[models.<配置档>]` —— 选定的 `backend`，以及 `llama_server` 和 `ollama` 子表中的权重、projector、上下文、CPU 线程、batch/ubatch、并行槽、GPU 层放置、`flash_attention`、`reasoning`、`keep_alive_seconds`；
- `[tasks.kanji_ocr]`、`[tasks.jpdb_pitch_review]` —— 模型配置档、提示词文件、`temperature`、`max_output_tokens`、`structured_output`、`timeout_seconds`、`seed`。

提示词是单独的文本文件，无需重新编译即可修改。JSON 答复格式、其校验和信任规则属于代码，不放入提示词。相对路径相对于设置它的文件所在目录解析；CLI 层相对于当前目录。前缀 `~/` 展开为主目录，Windows 路径 `C:\…` 和 `C:/…` 转换为 `/mnt/c/…`（WSL 标准磁盘挂载）。空字符串表示“未配置”。没有其他替换，包括环境变量或 shell。默认未设置任何模型：任务返回类型化拒绝 `inference_unavailable`，原因为 `backend_not_configured`、`model_not_configured` 或 `projector_not_configured`，其他命令照常工作。工具从不下载模型，也不访问云服务。

从 WSL 启动 Windows 版 llama.cpp：`llama-server.exe` 进程使用 Windows 的 CPU、GPU 和内存，并像 Windows 版 Ollama 一样监听 loopback。这仅在 WSL 镜像网络（`networkingMode=mirrored`）下可用：NAT 模式下 WSL 无法访问 Windows 的 loopback，启动会以 `startup_timeout` 结束；`windows_interop` 与 `host = "::1"` 的组合会在配置校验时被拒绝（WSL 不转发 IPv6 loopback）。设置 `windows_interop = true`；模型和 projector 的 `/mnt/<盘符>/…` 路径以 `<盘符>:\…` 形式传给服务器，不在已挂载 Windows 磁盘上的路径会以 `path_not_visible_to_windows` 拒绝。

```toml
# tools/asset-store/config/inference.local.toml
[backends.llama]
binary = "/mnt/c/Tools/llama.cpp/llama-server.exe"
windows_interop = true

[models.kanji_vision.llama_server]
model = "/mnt/c/Models/model.gguf"
projector = "/mnt/c/Models/mmproj.gguf"

[models.kanji_vision.ollama]
model = "qwen3.5:9b-q4_K_M"
```

#### 诊断与试探请求

```bash
# 生效设置、每个键的来源和任务就绪状态；不启动模型：
cargo run --quiet --locked --bin kanji-assets -- inference show
# 启动并验证任务后端，然后停止自己启动的服务器：
cargo run --quiet --locked --bin kanji-assets -- --output json inference check --task kanji-ocr
# 对批次之外的本地图片发送一次请求；不修改资源库：
cargo run --quiet --locked --bin kanji-assets -- --output json inference probe \
  --task kanji-ocr --image /path/to/kanji.gif
cargo run --quiet --locked --bin pitch-assets -- --output json inference probe \
  --task jpdb-pitch-review --image /path/to/graph.png \
  --inference-set tasks.jpdb_pitch_review.max_output_tokens=512
# 以往运行遗留的托管服务器 lease；--stop 重新停止自己的进程：
cargo run --quiet --locked --bin kanji-assets -- --output json inference orphans
cargo run --quiet --locked --bin kanji-assets -- --output json inference orphans --stop
```

`inference show` 从不输出提示词内容或访问密钥。`check` 返回 `ready`，附带后端与模型身份、就绪耗时、进程 PID 以及停止证明；启用 `windows_interop` 时还附带 Windows 进程的 PID 与内存（原生 `llama-server` 不报告内存）。`probe` 拒绝读取工作副本 `decks/` 下的文件，单张图片限制为 8 MiB，并输出最多 2000 个字符的原始答复。所有输入在启动模型之前检查：若没有任何可用输入，返回 `no_valid_images` 及每个输入的拒绝原因，且不启动模型；混合集合中可用输入照常执行，其余输入保留拒绝原因（`completed_with_failures`）。退出码：`0` 成功，`3` 模型不可用、推理拒绝或服务器停止未被证明，`130` 被信号中断。人类可读模式下，任何非零退出码结果的摘要输出到 `stderr`，成功结果输出到 `stdout`；`--output json` 始终把 JSON 写入 `stdout`。`check` 与 `probe` 的 JSON 包含 `stop_unproven` 和 `blockers`，`check` 还包含 `cache_reusable`（见下文）。

通用推理接口既执行文本任务，也执行带图片的任务：任务的答复契约说明是否发送图片。只有带图片的任务才会事先确认视觉能力；文本请求不发送图片，也不要求视觉能力。内置任务 `kanji_ocr` 和 `jpdb_pitch_review` 都是带图片的任务；目前还没有内置文本任务及其 CLI。

#### 托管 `llama-server`

在 `mode = "managed"` 下，一次操作启动自己的进程，证明其就绪与身份，执行请求并将其停止。服务器不经 shell 启动，使用显式参数、随机 `--alias` 和 `--api-key`；不继承父进程的 `LLAMA_ARG_*` 变量。就绪意味着 `/health` 成功且 `/props` 一致：别名、权重路径和视觉支持。所选端口上的他人进程返回 `foreign_server`，无视觉能力的模型返回 `model_does_not_support_vision`，其他权重返回 `model_mismatch`。端口 `0` 表示从 `10000`–`65535` 中随机选取的空闲端口，排除实际的 Linux 临时端口范围（`/proc/sys/net/ipv4/ip_local_port_range`），启用 `windows_interop` 时还排除 Windows 动态范围 `49152`–`65535`。若原生服务器没有剩余范围，由内核分配端口；对 Windows 进程则返回列出各范围的 `port_unavailable`——请显式设置 `port`。被占用的显式端口返回 `port_in_use`。启动失败按服务器输出区分：`projector_load_failed`、`model_load_failed`、`invalid_server_arguments`、`server_exited_during_startup`、`startup_timeout`、`model_file_missing`。

`gpu_layers = "auto"` 按启动时的空闲显存决定各层放置。如果显存被其他进程占用——例如 Ollama 在 `keep_alive` 期间保留模型——大部分层会放到 CPU 上，直到下一次启动前答复都会慢数倍。启用 `windows_interop` 时，`inference check` 显示 Windows 进程工作集，答复的 `timings` 显示生成速度。

同一时间只加载一个重型模型（`max_loaded_models = 1`）：第二次启动得到 `managed_server_busy` 而不是并行加载，切换配置档会在启动下一个进程之前停止前一个。停止时向进程组发送 SIGTERM，超过 `shutdown_timeout_seconds` 后发送 SIGKILL；随后检查进程组为空、端口已释放，以及（Windows 情况下）Windows 进程已消失。Windows 进程按命令行中自己的别名查找，而不是按可执行文件名：二进制文件可以不叫 `llama-server.exe`。如果 SIGTERM 之后它仍未消失，则仅在其命令行仍包含该别名时按 PID 终止。

lease 文件 `leases/<alias>.json` 位于 `$XDG_CACHE_HOME/anki-deck-toolkit/inference/`（否则为 `~/.cache/…`），在进程启动后立即写入并同步到磁盘。如果写入失败，进程会被停止，启动以 `lease_write_failed` 被拒绝：崩溃后下一次运行无法找到的服务器不会被使用。只有被证明的停止才会删除 lease。崩溃后遗留进程的 lease 按 PID、启动时间和命令行中的别名核对；下一次运行会停止这样的进程，绝不触碰他人的进程。对于 `windows_interop`，只有在证明 Windows 进程已消失后才会删除 lease。在此之前，任何托管启动（包括原生配置档）都会被拒绝：之前的模型可能仍占用内存和显存，而同一时间只允许一个重型模型。代码区分以下状态：`orphan_server_alive`——进程仍然存活（已知时，消息和 `failure.orphans` 给出 Windows PID）；`orphan_server_unverified`——无法检查 Windows 进程（`powershell_unavailable`、`windows_probe_timeout`、`windows_probe_exit:*`）。两种情况下消息都给出 lease 路径。

恢复：`inference orphans` 显示每个 lease 及其状态（`process_alive`、`windows_process_alive`、`windows_probe_failed`、`stale`、`invalid`；`--stop` 之后还有 `stopped`）、PID 和文件路径，不做任何修改；`inference orphans --stop` 重新停止自己的进程，只删除已被证明释放的 lease（没有存活进程时退出码 `0`，否则 `3`）。不可读的 lease（`invalid`）即使使用 `--stop` 也不会被删除，并会以 `orphan_server_alive` 拒绝启动：进程是否存活未知，请手动检查该文件和 `llama-server` 进程。如果无法检查 Windows，请恢复 WSL interop（必须能从 WSL 启动 `powershell.exe`）后重试该命令。不要手动删除 lease：没有它，就再也没有人能找到遗留的 Windows 进程。

Ctrl+C 中断当前请求、停止服务器并以退出码 `130` 返回 `interrupted`；已保存的记录保留。退出码 `130` 优先，但停止未被证明这一事实不会丢失：`error.details`（对 `check`/`probe` 而言是答复本身）保留含 `stop` 信息的 `inference`、`stop_unproven` 和阻塞项 `inference_server_stop_unproven`。没有中断时，停止未被证明返回退出码 `3` 和同一阻塞项。

`mode = "external"` 连接到 `endpoint` 上已经运行的服务器；由用户启动和停止。如果配置档设置了 `model`，`/props` 中的权重路径必须一致，并对本地文件计算 SHA-256（`sha256:<hex>`）。对于数 GB 的 GGUF，首次运行时这需要可观的时间；之后哈希按路径、大小和修改时间从缓存中读取。未设置 `model` 时只知道服务器报告的路径：`reported-path:<路径>`。外部服务器的 API 不报告单独的 projector：在 `vision = "projector"` 时记录为 `unverified:external`，只有配置档中的 `vision = "integrated"` 声明不存在 projector。

只有当模型身份可验证时，已保存的 OCR 或 JPDB 检查结果才会在不发请求的情况下复用：权重为 `sha256:` 或 Ollama digest，projector 不存在或为 `sha256:`。对外部服务器而言，这意味着配置档中设置了 `model` 且 `vision = "integrated"`；否则每次运行都会重新询问模型，报告包含 `cache_reuse_blocked: true`，`inference check` 报告 `cache_reusable: false`。请求前后比较 `/props` 只能证明一次运行之内模型未变，不能证明多次运行之间未变。之前的记录连同其来源保留在日志中。

地址只支持 HTTP，不允许 credentials、路径、query 或 fragment。除**字面 IPv4/IPv6 loopback** 外的任何地址都需要 `allow_remote = true` 或 `--allow-remote`，即明确同意向该服务器发送图片。禁用代理和重定向；响应限制为 1 MiB，模型文本限制为 64 KiB。在 WSL 镜像网络模式下，Windows 上的 Ollama 可通过 `http://127.0.0.1:11434` 访问。

#### 汉字 OCR

`batch ocr` 按顺序处理**已经保存**的批次。首先核对规范资源库完整性和现行信任，复用已经确认的资源。只有当前技术有效、处于 `awaiting_human` 且汉字验证器为 `Uncertain` 的候选可使用 OCR。`Verified`、`Rejected`、`Corrupt` 和已经解决的项目不会启动模型，也不会重新访问 Yarxi。

```bash
# 常规路径，不使用 OCR：
cargo run --quiet --locked --bin kanji-assets -- batch start --batch-id my-kanji 漢 字
cargo run --quiet --locked --bin kanji-assets -- batch run --batch-id my-kanji

# 用已配置的后端（默认 llama.cpp）对已保存的不确定候选执行 OCR：
cargo run --quiet --locked --bin kanji-assets -- --output json batch ocr --batch-id my-kanji

# 通过本地 Ollama 执行同样的 OCR：
cargo run --quiet --locked --bin kanji-assets -- --output json batch ocr \
  --batch-id my-kanji --backend ollama --model 'qwen3.5:9b-q4_K_M' --timeout-seconds 120

# 获取 TSV 及其旁边的 index.html；决定最初为空，'-' 表示显式跳过：
cargo run --quiet --locked --bin kanji-assets -- --output json batch review-export --batch-id my-kanji
# 查看匹配分组后，把可确认的行留空，跳过的行填 '-'；
# 在 TSV 中为其余行填入明确决定。
# 该命令会把它们与已确认的匹配分组一起应用。
cargo run --quiet --locked --bin kanji-assets -- --output json batch review-confirm-matches \
  --batch-id my-kanji --review-id FULL_REVIEW_SHA256 --confirm-reviewed
# 应用该分组后，为剩余行创建新的快照。
cargo run --quiet --locked --bin kanji-assets -- --output json batch review-export --batch-id my-kanji
# 填入剩余决定，并用新的 review_id 应用：
cargo run --quiet --locked --bin kanji-assets -- --output json batch review-apply \
  --batch-id my-kanji --review-id NEW_REVIEW_SHA256
# 显式重跑 OCR，包括已保存的拒绝结果：
cargo run --quiet --locked --bin kanji-assets -- --output json batch ocr \
  --batch-id my-kanji --rescan
```

应用一组匹配之后，应为剩余行重新执行 `review-export`：它们的状态已经变化，之前的快照仍包含已应用的行。`review-confirm-matches` 与 `review-apply` 采用完全相同的 TSV/SHA/世代/图片副本检查和原子 `human_decisions`。只会为匹配组中的**空白行**新增确认；`-` 明确跳过。TSV 中显式填入的其他决定也会应用。`--confirm-reviewed` 表示使用者实际检查图片，并知晓 OCR 可能误识别；仅生成/打开报告不会确认任何内容。行顺序为匹配、不匹配、无法识别/失败、没有 OCR。增加的 TSV 列（`observed`、匹配标志、拒绝原因、建议键）属于受保护元数据，**不可编辑**；缺少这些列的旧快照仍可读取。

默认提示词为 `config/prompts/kanji_ocr.txt`；与答复格式指令组合后逐字再现 Structure 研究的 `structure-v1` 指令，其 SHA-256 有测试固定。模型**仅接收**提示词和 PNG，绝不发送期望 Unicode、路径、原文件名或文章上下文。答复是只有一个字符串字段 `observed` 的 JSON；仅接受**一个**受支持 Han/CJK 字符或 `?`，不对 Unicode 字形变体规范化。多余字段、解释文字、多字符 `observed`、不完整答复以及因 token 上限截断都记为建议拒绝。

准备输入时使用已保存的原始字节与 SHA。受限的 v7 汉字验证器解码器选择完整的代表性 GIF 帧，并跳过没有前景的帧（因此转换版本为 `v7-representative-…`：v6 会整体拒绝此类 GIF，先前缓存的准备失败不适用于新键），再通过第二次有界解码获取该合成帧；PNG 必须完全解码。透明内容合成到白色背景，Rust `image` 使用 Lanczos3 **放大两倍**并编码单张 PNG。这是独立 Rust Lanczos3 实现，**不保证**与历史 PowerShell/System.Drawing 像素一致。源文件最大 **8 MiB**，单边最长 **2048** 像素，GIF 最多 **512 帧**，解码上限 **32 MiB**；输出最大 **4096×4096** 和 **8 MiB**。

建议只存于 `.runtime/batches/<id>/ocr/suggestions.json`。缓存键和来源记录包括身份、原始 SHA、任务、答复契约、转换版本、组合后提示词的 SHA 及其来源、后端及其模式和版本、模型配置档与名称、权重文件 SHA-256（Ollama 为 digest）、projector SHA-256、加载与生成参数。其中任何一项变化都会产生新键：旧记录绝不会被当作另一模型或提示词的结果。大型 GGUF 的 SHA-256 按路径、大小和修改时间缓存在状态目录中。检查缓存需要已准备好的后端：托管服务器会被启动，之后命中缓存时进行 **0 次**模型请求。每次请求前后都重新读取模型身份：扫描过程中模型变化时存为 `model_changed_during_scan` 拒绝。后端准备失败会为每个候选以权重 `unavailable` 保存；稍后成功启动则产生不同的键。命中缓存要求模型身份可验证（见 `mode = "external"`）。同配置下重试拒绝需要 `--rescan`。模型工作期间释放运行时锁；返回后检查当前 SHA、世代和候选，丢弃过期结果。

旧版仅 Ollama 路径的记录（`provider`、`endpoint`、`digest` 等）仍可读取，并在 `review-export` 中标注为旧版仅 Ollama 路径。它们的键与新模式的键不同，因此绝不会成为新运行的缓存命中。

命令 JSON 包含 `ocr_summary`（`saved`、`cache_hits`、`chat_requests`）以及带有服务器身份、就绪耗时和停止证明的 `inference` 报告。保存的模型拒绝返回退出码 `3`，并针对准确身份生成 `issues`，**不修改**候选及自动验证决定。磁盘/目录边界错误使用标准 `AssetError` 退出码。更新可选模型不会重新评估已确认的规范资源。

#### JPDB PNG 视觉检查

```bash
cargo run --quiet --locked --bin pitch-assets -- --output json batch llm-review --batch-id my-pitch
cargo run --quiet --locked --bin pitch-assets -- --output json batch review-export --batch-id my-pitch
```

`batch llm-review` 检查审核快照（`batch review-export`）中当前候选行已保存的 PNG，不会重新从 JPDB 获取。模型只看到用 Lanczos3 放大到长边约 448 px 的 PNG：不发送期望读音和 surface。答复包含 `verdict`（`no_defect_observed`、`defect_observed`、`insufficient`）、最多六条观察（类别为 `cropping`、`incomplete_glyph`、`artifact`、`foreign_element`、`illegible_text`、`other`）以及读到的文本 `visible_text`。模型用俄语简要书写观察（契约 `jpdb-pitch-visual-v2`；之前契约中英文观察的记录按原样读取）。代码先做 Unicode 规范组合（NFC：`か` + U+3099 等于 `が`），再把片假名折叠为平假名并删除空白，然后将读到的文本与期望读音比较；不使用兼容规范化 NFKC。模型字符串按收到的原样保存。不一致或不完整的答复记为拒绝。

建议保存在批次的 `llm-review/suggestions.json` 中，并绑定到身份、SHA、世代、条目修订、请求指纹、尝试以及完整的模型与提示词来源。在 `review-export` 中，该行获得两个受保护的列——`llm:<verdict>`（或 `llm_failure:<代码>`）和建议键——并在 HTML 中显示“LLM（建议，而非决定）”摘要。决定仍然只是显式的第一列；`AUTOMATED_VERIFIED_ONLY` 规则、`VERIFIED` 和发布都不变。如果请求期间该行发生变化，结果会被丢弃（`stale_discarded`）。退出码 `3` 表示存在模型拒绝或停止未被证明。

## 检查

本地资源获取和图片测试使用合成数据与临时目录，**不访问网络**。真实浏览器获取另行针对网站执行，不属于常规测试。所有工作区检查从根目录运行：

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

根 Cargo 工作区仍是唯一构建入口。牌组媒体从不用作测试夹具或数据来源。
