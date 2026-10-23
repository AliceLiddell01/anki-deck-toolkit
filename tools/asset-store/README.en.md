# Asset store and `kanji-assets`

[Русский](README.md) · **English** · [简体中文](README.zh-CN.md)

<!-- i18n-source-git-blob: 9203dbe7747e8dcaf294a73dc2c45e5b971057de -->

`asset-store` is the shared Rust library for program-managed assets, responsible for manifests, checksums, integrity, and lifecycle. The domain-specific `kanji-assets` CLI provides Yarxi entry discovery, kanji image validation, and publication of approved files to the canonical corpus. The tool does not read `decks/**/media/`, does not modify `media_files`, and does not copy images into decks.

## Data locations

The Kanji CLI defaults to `.asset-store/kanji`. The separate pitch-accent domain uses `.asset-store/pitch-accent`; these published corpora do not share manifests or `assets/`.

```text
.asset-store/                          # ignored by Git in its entirety (/.asset-store/)
├── kanji/
│   ├── .owner.json                 # owner marker; not published to Git
│   ├── manifest.json               # only active VERIFIED entries
│   ├── assets/
│   │   ├── gif/漢.gif
│   │   └── png/饅.png
│   ├── .runtime/                   # candidates, quarantine, batches
│   ├── .journal/events.jsonl       # operation journal; append-only
│   ├── .tmp/                       # staging and transaction markers
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

The root `.gitignore` excludes `/.asset-store/` and `/decks/` in their entirety: there are no whitelist exceptions inside these areas, so neither `.owner.json`, nor `manifest.json`, nor asset bytes, nor `.runtime/`, nor the operation journal ever reach Git. A missing corpus in a clean clone is a normal state, not a reason to publish anything.

The internal `storage_path` and the consumer filename are different manifest values. For example, `assets/gif/漢.gif` has `consumer_filename: "漢.gif"` and lands in `media/漢.gif`. `anki-repo` uses the explicit consumer filename rather than deriving it from the asset store layout.

`.runtime/manifest.json` uses the same `store_id` as the main manifest. `verify_integrity` checks both areas. `Pending` and `Quarantined` entries stay in `.runtime`; the check of verified entries only compares the canonical corpus and the absence of identity overlap with `.runtime`. A missing corpus is allowed.

## Domain policy and manifest

`AssetDomainPolicy` defines permitted identities, canonical storage location, consumer filename, formats, and size limits. `GenericDomainPolicy` preserves content-hash paths for unpublished generic assets. Domain policies reject foreign namespaces: `KanjiDomainPolicy` owns `kanji` and `PitchAccentDomainPolicy` owns `pitch_accent`.

Legacy consumer filenames require separate proof through `AssetDomainPolicy::proves_legacy_consumer_filename(identity, filename)`. Default and generic policies provide none. Kanji accepts only exact `<key>.gif` or `<key>.png`, pitch-accent only `<key>.png`, always as safe basenames. This does not consult `legacy_location`, which describes an old location inside the manifest rather than the name Anki sees in `media/`.

Kanji GIF and PNG assets occupy separate `assets/gif/` and `assets/png/` directories, named exactly after a character and extension. For pitch-accent, the identity key is the exact surface form, not the reading: `assets/png/<surface>.png` is published to consumers as `<surface>.pitch.png`. For `飴`, storage is `assets/png/飴.png`, while Anki receives `飴.pitch.png`, independently of kanji media named `飴.png`. Reading and positive JPDB vocabulary ID are preserved as metadata/evidence.

The store owns the **canonical** consumer filename but never rewrites existing note references. `anki-repo migrate-media` handles a legacy `<surface>.png` pitch reference (see [anki-repo documentation](../anki-repo/README.en.md)); the store provides verified bytes but does not rename its own files. Reading comparison treats hiragana/katakana as equivalent but requires the exact surface/reading pair to match a form of the selected dictionary entry, retaining the original JPDB reading in `evidence`.

Decoding a PNG alone does not justify `VERIFIED`. `PitchAccentImageValidator` checks surface/reading, the real page of the entry with matching ID, the inventory of graph variants, the selection of exactly one of them, and dark-mode proof.

Every PNG contains **exactly one** graph. `evidence.variant_selection` records all graph variants of the selected entry in DOM order (`detected_variant_count`, `variants`), the selected variant index (`selected_index`), and the selection basis (`basis`); `evidence.render` records `captured_graph_count`, which is always `1`, the selected variant's selector, `padding_css_px`, and the final `capture_rect`. Each variant records its graph text, the SHA-256 of its markup (`markup_sha256`), its selection score (`selection_score`), and rectangles: the node in viewport and document coordinates, plus `paint_rect`, the union of the node with all its descendants. JPDB graph segments shifted by `margin-right: -2px` protrude beyond the node rectangle, so the capture region is built from `paint_rect`. `capture_rect` equals the selected variant's `paint_rect` expanded by 3 CSS px on every side and outward to whole CSS pixels. At DSF `3.0` an integer region yields a PNG of exact size: the validator requires PNG dimensions to equal `capture_rect × 3` with no tolerance. The region must stay inside the document and must not intersect any other variant's `paint_rect`.

Dark-mode evidence links browser color-scheme settings, document class, computed background and PNG pixels. At least one background pixel must differ by no more than 8 per RGB channel from the observed color, and at least eight graph pixels must differ by more than 16 in some channel. Every PNG pixel must have `alpha=255`. In addition, every pixel outside the selected graph's painted area (allowing one physical pixel of antialiasing at its edge) must match the observed background within the same tolerance of 8: the actual bytes thus prove that the margins are clean, that neighboring variants and buttons are not captured, and that lines and glyphs do not touch the PNG edges. The margins are the page's genuine background from the native capture, not a drawn border. Browser provenance is compulsory. Validator v6 enforces the current rendering contract: proof from earlier versions does not inherit trust; incomplete proof, or proof in an earlier shape (for example with version 5 `graph_count` and `graph_union_rect`), yields `UNCERTAIN`.

Manifest schema is version `5`. In addition to SHA-256, byte size, format, state, origin and semantic decision, each entry has `consumer_filename`, and the store has `domain_id`. Automatic `ValidationRecord` and optional `HumanAttestation` bind to exact identity and SHA. For pitch-accent, a human `Approve` does not replace the automatic `ValidationRecord`: the same bytes still require a record with status `VERIFIED` and the expected `validator.id`/`validator.version` for `PitchAccentImageValidator` v6. Human approval cannot rehabilitate old/incomplete evidence or `REJECTED`/`CORRUPT`; Kanji's existing approval policy remains unchanged. New bytes never inherit trust from a previous SHA, and record replacement requires compare-and-swap using the expected old SHA.

Schema v3/v4 and the legacy flat Kanji layout can be read. A mutating open performs versioned migration under an exclusive lock: preserves old bytes with hard links, checks SHA-256, atomically updates the manifest and removes old flat links. `.tmp/` transaction markers support completion or rollback after interruption. Unchanged SHA, `ValidationRecord`, and `HumanAttestation` remain, without refetching. After commit, old/new canonical paths do not coexist. CLI JSON reports `store.layout_migrated_on_open`.

`AssetStore::read_verified` remains a read API compatible with `KanjiDomainPolicy`. Other domains use `read_verified_with_policy`, which can read v3/v4 with legacy mapping and return the explicit consumer filename without modifying manifests, making state/lock files, changing layout, or performing recovery. Only a mutating open migrates. Exact bytes, checksum, size, format, validator identity, and human decision are checked.

The shared batch runtime owns safe filesystem operations under `.runtime/batches/<batch-id>`: `NOFOLLOW`, locks, revisions/CAS, atomic state writes, bounded reads, and SHA-addressed payloads. It does not know about `KanjiBatch`, `KanjiMetrics`, Yarxi data, or HTML reports: domain adapters own those. A plain-text test state exercises reopening and reading blobs without kanji or image semantics.

Shared Chromium sessions, executable discovery, launch diagnostics, CDP metrics, and technical network telemetry live in `browser_runtime.rs`. Yarxi retains selectors, URLs, TLS policies, filters, fallback, dark theme, and screenshot geometry. The shared runtime supports a separate `3.0` scale factor. JPDB uses isolated sessions; `pitch-assets` manages its own batches and publishes verified PNGs. Integration with `anki-repo` remains separate.

The Chromium process is owned by `browser_process.rs`, not by `chromiumoxide`: the browser starts with the same arguments as the former `Browser::launch`, but in **its own process group** (`process_group(0)`), and CDP connects to the printed DevTools address. Terminal `Ctrl+C` therefore delivers `SIGINT` only to the CLI, not to the browsers: the coordinator performs the cancellation, and an active request does not turn into a fabricated `technical_failure` because its browser died. Shutdown distinguishes its stages: CDP close (`browser_transport_stop`), the group leader's own exit within a window of up to 10 seconds (`browser_process_exited`) or forced termination of the group (`browser_process_killed`), then reaping the group's descendants until `ESRCH`; zombies left in the group (for example without a reaper, when the CLI is PID 1 of a container) are not counted as its members according to `/proc`. A signal is sent only to the group created by this launch: until the leader is reaped (`wait`), its number cannot pass to another process, and after reaping `SIGKILL` is sent to descendants only while a probe confirms the group is non-empty; a narrow window remains between the probe and the signal. An abnormal exit of the CLI itself via `SIGKILL`/`SIGQUIT` bypasses this path: while leftover processes still reference the run tree, orphaned-run cleanup refuses to remove it.

The parallel acquisition layer shared by Yarxi and JPDB is `acquisition_pool.rs`: the assignment ledger (at most one item per worker, verification that a result belongs to its assignment, requeue, retiring a worker), pool cancellation with the first reason (`ctrl_c` or coordinator failure), the session-recovery budget, and the `SIGINT`/`SIGTERM`/`SIGHUP` subscription. It knows neither browsers nor providers: `token`/`CAS` checks, checkpoints, validators, and trust rules remain with the domain coordinator, so a non-browser source can be attached as well.

User areas are not published through Git, in whole or in part: the root `.gitignore` excludes `/.asset-store/` and `/decks/` in their entirety, and there are no exceptions inside them. You can verify these rules with a real `git check-ignore` through a dedicated regression script — it copies the root `.gitignore` into a temporary repository, proves that all paths inside the two areas (including `.owner.json`, `manifest.json`, `assets/**`, `.runtime/**`, `.journal/events.jsonl`, legacy grouping directories, and nested directories) are ignored, and checks that after `git add -A` not a single such path remains in the index:

```bash
bash tools/asset-store/tests/gitignore_regression.sh
```

`kanji-corpus-gate` and `pitch-corpus-gate` check domain-corpus publishability, rejecting unverified lifecycle states, outdated validator proof, legacy flat layouts, SHA-suffixed paths, orphan files and unregistered canonical paths. Read-only checks from the workspace root:

```bash
cargo run --quiet --locked -p asset-store --bin kanji-corpus-gate
cargo run --quiet --locked -p asset-store --bin pitch-corpus-gate
```

They exit nonzero for missing current `VERIFIED`, unsupported domain/layout, stale evidence, orphan bytes, or corrupted runtime. They never mutate data or count temporary content as part of the canonical corpus.

## Operation journal

The append-only journal at `<store-root>/.journal/events.jsonl` records how the store reached its state. It sits beside `manifest.json` but outside `.runtime/batches/**`, so individual batch cleanup does not erase history. Each line is one NDJSON object; Git ignores the journal with the store.

It records transitions, not current truth: `manifest.json` remains authoritative for the `VERIFIED` corpus. It helps find manifest changes not followed by a closing event. It contains no tokens, cookies, CDP dumps, HTML reports or images, only identity, operation, SHA-256, lifecycle, manifest revision, technical reason and baseline counts.

Example actual journal line:

```json
{"schema_version":1,"seq":2,"event_id":"c3dca04948e2cecdef3dda4b70c81f8b2268a2844707d64b197ba7e64bd5a96e","event_key":"a143ed00d781b3439a0f0688125f45e9d34f08aa9e75ac91e5e30c67746e4b68","recorded_at":"2026-10-15T22:08:27.113Z","domain":"kanji","namespace":"kanji","identity":"漢","operation":"candidate_written","outcome":"completed","intent_id":"441132dfc6eff6e5cd75a3d55566026727b115bedf2724beb252106874778a14","manifest_revision":1,"previous_sha256":null,"new_sha256":"693d949d8c3fdc7fd4ace7c340b5f177a9f0c5be7bafee8bc93a7d88b7523d75","previous_lifecycle":null,"new_lifecycle":"pending","verified":false,"run_id":null,"batch_id":null,"provenance":"local_import:han.gif","reason":null,"snapshot":null}
```

Full reads are capped at 256 MiB. Larger journals make `journal list`, `journal check`, and `journal reconcile` return `journal_too_large` (exit 4); appending still streams only the necessary event keys and last sequence. No automatic rotation exists: archive `.journal/events.jsonl` manually to read an entire history again, without changing manifest authority or corpus state.

| Field | Meaning |
|---|---|
| `schema_version` | Event schema version (`1`) |
| `seq` | Strictly consecutive `+1` event number |
| `event_id` | Event identifier referenced by `intent_id` |
| `event_key` | Idempotency hash over event fields; checked by `journal check`; the old schema-1 key without `intent_id` in the hash is accepted |
| `recorded_at` | RFC 3339 UTC recording time |
| `domain`, `namespace`, `identity` | Record owner; store-wide `identity` is `*` |
| `operation` | `candidate_written`, `verified_published`, `verified_reused`, `trust_granted`, `trust_revoked`, `revalidated`, `quarantined`, `removed`, `recovered`, `layout_migrated`, `baseline` |
| `outcome` | `pending`, `completed`, `refused`, `recovered`, `aborted`, `unknown` |
| `intent_id` | Closing event's reference to the intent `event_id` |
| `manifest_revision` | Combined revision of canonical and `.runtime` manifests |
| `previous_sha256`, `new_sha256`, `previous_lifecycle`, `new_lifecycle` | Before/after record state |
| `verified` | `true` if verified after event, `false` if unverified/removed, `null` if not about a single record |
| `run_id`, `batch_id` | Optional execution context |
| `provenance` | `source_kind:source_name`, no paths |
| `reason` | Technical refusal/recovery/removal reason |
| `snapshot` | `baseline` counts for `assets`/`verified`/`pending`/`quarantined` |

### Two-phase writes

A state change has two surrounding events. Before modification, a `pending` intent records current `previous_*`, expected `new_*`, and its `event_id` becomes `intent_id`. After `save_manifest`, a closing `completed` event references it and records actual `manifest_revision`, `new_sha256`, `new_lifecycle`, `verified`. Normal failure before commit closes it as `refused` with `reason`, avoiding lingering intents.

Only code already holding the exclusive store lock writes events, and only after an actual record change; parallel batch workers do not write to the journal, preserving throughput. `verified_reused` is emitted only for an actual reuse of previously trusted bytes with the same SHA. An existing `VERIFIED` entry does not imply a new `verified_published` event.

### Reconciliation

If a process dies after `save_manifest` but before closing, `journal reconcile` compares open intents under lock with canonical and `.runtime` manifests. Valid open intents can be resolved; invalid references, broken/truncated lines, mismatched `domain`/`namespace`/`identity`/`operation`, ordering errors, multiple closes, and bad derived `event_key` yield `manifest_corrupt` with no write.

- Actual state equals expected `new_*`: `outcome: recovered`.
- Actual state equals prior `previous_*`: `outcome: aborted`.
- Neither: `outcome: unknown`, with `reason`.

Validator and human-trust decisions modify `validation` or `human_attestation` without altering SHA/lifecycle, so the manifest cannot distinguish an applied from an unapplied transition. Their dangling intents therefore close as `unknown` with the reason `манифест не различает применённое и неприменённое изменение` ("the manifest does not distinguish an applied from an unapplied change"), and a failing transition body is never passed off as `refused`: the outcome stays unknown rather than guessed. Reconciliation is idempotent: a repeated call appends no events.

### Commands

Both CLIs read the journal of their own store, which the shared `--store` option selects:

```bash
# Events in ascending seq order; --limit keeps the last N.
kanji-assets --store .asset-store/kanji journal list --limit 20
pitch-assets --store .asset-store/pitch-accent journal list \
  --identity 幽霊 --operation verified_published

# Integrity: strict seq numbering, unique event_id, valid event_key;
# the result references the preceding Pending of the same area and operation;
# allowed single-phase events, a truncated tail, broken lines, and a revision
# that is not above the actual one are checked as well.
# Exit code 3 if a problem is found; open intents are listed.
# A journal above the full-read limit yields journal_too_large (exit code 4).
kanji-assets --store .asset-store/kanji journal check

# Reconcile open intents; prints the appended events.
kanji-assets --store .asset-store/kanji journal reconcile

# One baseline event with a snapshot of the current state; a repeat is refused.
kanji-assets --store .asset-store/kanji journal baseline
```

`journal baseline` records exactly one `operation: baseline` with `identity: "*"`, current manifest revision, and `assets`/`verified`/`pending`/`quarantined` snapshot. It starts trusted history and refuses repetition.

## Acquiring pitch-accent images from JPDB

`JpdbPitchProvider` navigates JPDB search and vocabulary pages. Search URL parameter `q` is set via the `URL` API, with `lang=english` explicitly selected. After navigation, either `/search` or an allowed direct redirect to `/vocabulary/<id>/...` is accepted. On a direct redirect, the entry ID, correspondence to the request, and optional explicit selection are independently verified before graph acquisition. Any other route or origin is rejected.

Matching requires a paired surface form and reading among observed dictionary forms, including verified alternatives. For ambiguity, the provider does **not** select the first or top-ranked result. It returns `ambiguous_vocabulary` with candidates' IDs, spellings/readings, parts of speech, glosses, and actual detail links. A caller may submit `JpdbPitchSelection` through `JpdbPitchRequest`; the provider searches again and accepts it only when ID, surface, and optional reading in the link match a newly observed candidate. URL query and fragment do not contribute to identity. A valid detail URL requires HTTPS on `jpdb.io`, default port, and exactly three or four nonempty path segments: `/vocabulary/<positive-id>/<surface>[/<reading>]`, without trailing slash. The provider never trusts an arbitrary supplied URL/ID nor builds detail links from input strings: it uses an observed result `href`. A single entry without alternative spellings, which JPDB shows on `/search` in details mode, has only a `Used in` link in its row (`/vocabulary/<id>/<surface>/used-in`); it is not a route and is never turned into one by dropping the segment. The route is then the page's `link[rel=canonical]`, but only if one DOM snapshot has exactly one row and one result in details mode, the document has exactly one canonical link and it passes the same route check, every `Used in` link of the row names the same ID and surface, and the canonical surface/reading pair is observed in the row headword. A canonical without a reading segment is accepted only for a kana entry whose reading equals its surface. Otherwise a row without a link remains a `page_contract` failure, and after navigating to the canonical route the detail page passes the same ID, form, and query checks. Without selection, ambiguity remains; `vocabulary_not_found` is distinct from technical failure.

On a verified dictionary page, `no_pitch_accent_on_source` is returned **only** if the expected page structure is recognized and lacks the `Pitch accent` section. Unknown/empty graph DOM, network or JS failures, unverified dark mode, and malformed screenshots remain technical errors. Before confirming absence, the provider waits for critical page requests to complete and rechecks structure; an arbitrary delay is not evidence. Execution or decoding errors from `page.evaluate()` are `browser_evaluation` with the active stage; contradictory DOM is `page_contract`, timeouts are `timeout`.

The pitch graphs of an unambiguously resolved entry are variants within that entry. Every row of the `Pitch accent` section must contain exactly one graph and, optionally, `a.vocabulary-audio` audio buttons (any number of them, not counted in the selection); any other row element, including possible per-variant data, is a technical `capture_contract` failure rather than being ignored. A single graph is captured as is. With several variants, **exactly one** is captured, without neighboring graphs.

JPDB does not provide the prevalence or priority of **individual** pitch variants: a variant row's markup contains only the audio button and the graph itself, while `Top N` and frequency belong to the word as a whole. Selection is therefore reproducible and pseudorandom (`basis: seeded_hash_without_source_priority`): each variant gets `selection_score = SHA-256("anki-decks/jpdb-pitch-variant-selection/v1\0<entry ID>\0<markup_sha256>")`, and the variant with the lowest score is chosen. The choice does not depend on graph order on the page, on a new run, on parallelism, or on `resume`, and it does not reduce to "always the first". Identical markup gives equal scores; then the lower DOM index wins. When a variant is added, the choice either stays or moves to the new variant. A change of markup or of the variant set changes `markup_sha256` and the scores, so the choice and the evidence stay consistent: the validator recomputes the choice from the stored inventory. Choosing a graph never chooses a dictionary entry: several matching IDs still yield `ambiguous_vocabulary` until the user selects one explicitly.

The selected variant is captured through CDP at page scale `1.0` and device scale factor (DSF) `3.0`; the PNG is never subsequently resized, cropped, or recoded. Coordinates are recorded in viewport/document space with `scrollX`/`scrollY`. Before the screenshot, the provider compares the variant inventory and geometry twice and checks that no ancestor clips the graph's painted area with `overflow` or `clip-path`, that the padded region stays inside the document and does not reach a neighboring variant, and that an opaque dark ancestor background covers the whole region. After the screenshot it rechecks the same conditions, the PNG dimensions, and the margin pixels. Any violation yields a typed technical failure (`capture_contract` or `dark_theme_unverified`) with diagnostics instead of a PNG. The validator does not interpret the pitch drawing itself and does not verify intonation from pixels.

Every acquisition returns original bytes plus `PitchAccentDomainMetadata` usable by `PitchAccentImageValidator`. `JpdbPitchProvider::acquire_requests` starts an isolated session for its request set; `acquire_requests_in_session` accepts a caller-owned session. Both process requests sequentially, with a fresh search for each. A normal individual failure does not halt the batch. If the CDP observer loses data and session integrity can no longer be trusted, outcomes cover **only already started** requests and a separate `session_failure`; the unstarted tail receives no fabricated outcomes and can be retried.

For manual acceptance use an external JSON plan with mandatory `expected_outcome` per item: exactly `acquired`, `no_pitch_accent_on_source`, `ambiguous_vocabulary`, or `vocabulary_not_found`. Optional conditions:

- `expected_vocabulary_id` and `min_variant_count` only for `acquired`. `min_variant_count` sets the minimum number of **detected** graph variants of the entry; the PNG itself must contain exactly one graph regardless of how many there are.
- `expected_candidate_ids` only for `ambiguous_vocabulary`, matching the complete set of actual candidate IDs regardless of order.
- `selection` only for `acquired`, requiring `vocabulary_id` and `detail_url` with the same ID and valid JPDB route as above. The ID must equal `expected_vocabulary_id`.

Before creating a report or launching a browser, the acceptance program validates request/domain identity, consumer filename, numerical limits, duplicate requests, and selection URL. Search strips leading/trailing whitespace from `surface` and `reading`, but preserves original plan values in diagnostic reports. Repeating a surface/reading pair for an ordinary ambiguous request and its separate explicit selection is allowed; an identical request/selection pair is rejected. The provider validates the selection against fresh results after searching.

The following is a **synthetic** plan. For live runs, replace IDs and selected `detail_url` with values from the chosen candidate's `evidence`:

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

Run from the workspace root:

```bash
cargo run --locked -p asset-store --bin jpdb_pitch_acceptance -- \
  --plan ../jpdb-pitch-plan.json
```

Each run packages its report and evidence in a single verified ZIP and leaves no extracted report directory. By default it is saved to `<std::env::temp_dir()>/anki-decks-evidence/jpdb_pitch_acceptance-run-<32 lowercase hexadecimal digits>.zip`. `--output <ZIP>` selects a **new** `.zip` file, resolving relative paths from the current directory. The parent must exist; the file must not; the path must be outside the checkout and temporary tree. Unzip to inspect `reports/index.html`, `reports/evidence.json`, and acquired `reports/images/`. On publication, stdout ends with `LIVE EVIDENCE ZIP`, absolute `Path`, `SHA-256`, and byte `Size`. A ZIP may contain partial evidence on plan-check failure, recording final status and available results.

Per-item reports include expected and actual outcomes, match result, candidates, actual dictionary IDs/links, forms/readings, the number of detected graph variants, the selected variant, the selection basis, the number of graphs in the PNG, the variant inventory with selection scores, dark-mode and screenshot geometry evidence, DSF, PNG dimensions, SHA-256, and core-validator verdict. `acquired` succeeds only with validator `VERIFIED` **and** all plan constraints. Expected ambiguity, vocabulary absence, and pitch-graph absence may count as successful outcomes; unexpected missing vocabulary fails acceptance. Missing provider output or local processing failure also counts as mismatch. One failure does not erase other collected evidence. Acquired original bytes remain unchanged; invalid-signature or undecodable PNG is stored as `.bin`, not displayed as an image. The tool never modifies the canonical store, user media, or `decks/**`; ZIP remains a local artifact.

## `pitch-assets`: canonical corpus and batch execution

The CLI acquires pitch-accent images, persists batch state, and publishes verified PNGs into `.asset-store/pitch-accent`. It does not inspect CrowdAnki exports or know card-model/field names. Without `--store`, it uses the current checkout's pitch store. `--output json` produces versioned machine-readable output.

Exit codes for `pitch-assets`:

- `0`: successful command; for `ensure`, `batch run`, and `batch resume`, all batch items are resolved.
- `3`: acquisition leaves `needs_review`, or `AssetError` signals invalid input, identity conflict, or forbidden transition. Inspect JSON `outcome`, `blockers`, and `error`.
- `4`: `AssetError` for boundary, integrity, store ownership, missing store, or unsupported schema.
- `5`: I/O or validator error (`AssetError::exit_code()`), or stdout failure other than `BrokenPipe`. `BrokenPipe` preserves the command outcome code.

CLI argument parsing errors return `2` before execution. Store errors follow `AssetError::exit_code()`, not the batch outcome.

```bash
# Inspect and check the corpus without a network acquisition.
cargo run --locked -p asset-store --bin pitch-assets -- corpus list
cargo run --locked -p asset-store --bin pitch-assets -- corpus check

# A single acquisition, or a batch driven by a strict JSON plan.
cargo run --locked -p asset-store --bin pitch-assets -- ensure \
  --surface '幽霊' --reading 'ゆうれい'
cargo run --locked -p asset-store --bin pitch-assets -- batch start \
  --plan ./pitch-plan.json
cargo run --locked -p asset-store --bin pitch-assets -- batch run \
  --batch-id my-pitch-batch --workers 4

# State, review report, and continuation after a decision.
cargo run --locked -p asset-store --bin pitch-assets -- batch status \
  --batch-id my-pitch-batch
cargo run --locked -p asset-store --bin pitch-assets -- batch review \
  --batch-id my-pitch-batch
cargo run --locked -p asset-store --bin pitch-assets -- batch resume \
  --batch-id my-pitch-batch --workers 4

# Explicit candidate selection, addressed or batch retry, and rejection for an exact SHA.
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

# Review snapshot and human decisions against it.
cargo run --locked -p asset-store --bin pitch-assets -- --output json batch review-export \
  --batch-id my-pitch-batch
cargo run --locked -p asset-store --bin pitch-assets -- --output json batch review-apply \
  --batch-id my-pitch-batch --review-id FULL_REVIEW_SHA256
```

External `PitchPlan` JSON has `schema_version: 1`; unknown fields and versions are rejected. Persisted batch state has its **own schema version 3**. State of an earlier schema or with an earlier validator (including schema 2 with validator v5) is rejected before parsing with code `unsupported_schema_version`: the CLI will not guess the current candidate attempt or original plan from history and does not carry over trust in earlier evidence; create a new batch. Each plan item specifies exact `surface`, optional `reading`, and optional `selection` with `vocabulary_id`/`detail_url`. Identical requests collapse; incompatible requests with the same surface produce `identity_conflict` before browser launch.

When creating a batch, normalized requests form an immutable `original_plan` with SHA-256. Selection, retry and reacquisition modify active requests but not that original plan. Reusing a `batch_id` is allowed only with the same original plan and digest. To resolve `ambiguous_vocabulary`, copy the exact candidate ID and `detail_url` from review evidence. The next `batch run` repeats the search and verifies the chosen entry at JPDB.

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

`batch status` checks store integrity and reconciles persisted state against a fresh full snapshot of canonical entries. It neither starts a browser nor accesses the network, but may save updated batch state if the store changed. JSON `changed: true` denotes changes to persisted corpus/batch data, including new state, candidate bytes, and `review.html`; creating incidental locks or empty directories does not count. Otherwise `changed: false`.

Batch state records typed provider outcomes, attempt history, ambiguity, validator decisions, and publication intents. A current candidate is bound to exact attempt number, generation and content SHA-256, preventing evidence from separate attempts with identical bytes from mixing. Candidate bytes are deduplicated by SHA under `.runtime/batches/<id>/candidates/` until publication. The runtime lock is released during browser fetch; afterward, per-item revision and generation are compared and stale results discarded. On restart, `resume` can revalidate saved bytes with the current validator and publish without another JPDB request.

`batch run` and `batch resume` accept `--workers` from `1` to `4`, default `4`; invalid values fail before acquisition. Each worker has its own `BrowserSession`, `Page`, CDP connection, browser profile, `TMPDIR`, and `TempWorkspace`. Items are acquired concurrently, so completion order need not match plan order. Four workers were selected based on comparative throughput; reduce the value in case of JPDB HTTP 429.

A single coordinator serializes state changes. It assigns at most one item to each worker and verifies assignment plus persisted token/CAS—generation, request, and item revision—when a result arrives. Matching results and candidate bytes are durably checkpointed individually, followed by any ready publication and assignment of another item. A stale token is discarded. Checkpoints persist immediately regardless of completion order: the persisted set is not necessarily a prefix of the plan. Unassigned items and acquisitions interrupted before checkpoint remain `pending`. On `resume`, reconciliation rebuilds a queue of current `pending` items; stored candidates can be revalidated/published without refetching.

At the start of every `batch run`/`batch resume`, orphaned temporary trees are explicitly cleaned, even if no browser is needed. Workers create temporary trees only when starting their browsers.

Progress streams immediately to `stderr`: human-readable lines by default, JSON Lines with `--output json`. Every JSONL event has `schema_version: 1`, `operation: "batch_run"`, `event`, `batch_id`, `elapsed_ms`, `run_completed`, `run_total`, and `batch_total`, plus always-present `workers` (configured concurrency) and `in_flight` (assigned but not completed). `run_total` counts items `pending` after start-of-run reconciliation; `batch_total` is the full plan size. `run_completed` increases only on a durable checkpoint, not for discarded stale results.

Events may also include `identity`, `worker`, `worker_session`, `session`, `next_session`, `attempt`, `recovery`, `outcome`, `reason`. Worker numbers start at `1`; `worker_session` numbers are per worker and start at `1`. `session` and `next_session` numbers are globally unique per run. Supported event names are `run_started`, `item_started`, `retry_started`, `heartbeat`, `item_checkpointed`, `item_discarded_stale`, `item_requeued`, `browser_session_started`, `browser_session_ended`, `browser_session_recovered`, `browser_session_rotated`, `worker_recovery_exhausted`, `run_stopped`, `run_finished`. `attempt` follows saved history. `recovery` is the current consecutive session-failure count for that worker, starting at `1`, and appears only in recovery-related events. Pitch-accent progress has no round fields. For `ensure`, `operation` is `ensure`. JSONL stays on stderr and a single final JSON result goes to stdout.

The process panic handler emits a separate `panic` JSONL event with `schema_version: 1`, `event`, and a fixed `message`, **without** `operation`, `batch_id`, or progress counters; it does not leak panic details or stack traces.

Each worker rotates its own browser session before the next item after either **64 completed requests** or **20 minutes** since session start. An in-flight request is not interrupted solely for rotation; provider timeout is 90 seconds.

Browser-session failures are worker-local. Recoverable failures include browser session launch/setup and a provider report containing at most one outcome plus a session failure. The worker closes the broken session, proves cleanup, and opens another while others continue processing; one broken browser cannot stall all workers.

If a report has **no result**, its assigned item goes to the queue's end without recording an attempt, advancing generation, or fabricating a `technical_failure`. If there is a result, it is checkpointed exactly once, then the session rotates and the item is not reacquired. Consecutive session failures reset on successful acquisition; if the same report also signals session failure, that becomes the first failure of a new streak. Routine rotation and `Ctrl+C` do not count. After **three** consecutive failures, the worker leaves the pool and its task channel closes; remaining workers proceed. If outstanding items remain, `batch run` reports `session_recovery_exhausted`, leaving unprocessed entries `pending`, with no synthetic outcomes/attempts, so `batch resume` can continue.

Unrecoverable failures abort the whole run: provider report with more than one result or no result without a session failure (`invalid_provider_report`), inability to confirm broken-session cleanup (`browser_cleanup_failed`), worker panic, closed task channel, or checkpoint/CAS/store error. Local session recovery does not conceal them.

On `Ctrl+C`, `SIGTERM`, or `SIGHUP`, the coordinator stops dispatching and signals cancellation to active requests. Worker browsers run in their own process groups and do not receive the terminal `SIGINT`. A result already received as the signal arrives is still CAS-checked and durably checkpointed. Sessions close and workers are joined after temporary-directory cleanup. Uncheckpointed and undispatched items remain `pending`, without fabricated attempts. `batch resume` reconciles the persisted state and continues only current `pending`, retaining earlier checkpoints.

A repeated signal during this shutdown does not cut it short: otherwise browsers in their own groups would outlive the CLI. Signals are handled by a process-wide listener: when no acquisition run owns browsers (acquisition has not started yet or is already closed), `SIGINT`, `SIGTERM`, and `SIGHUP` terminate the process with code `128 + signal number` (130, 143, 129), as the default behavior does.

Publication reads metadata and `evidence` **only** from the identified attempt and requires its current `VERIFIED` result from `PitchAccentImageValidator` v6. `AssetStore::ingest_verified` rechecks bytes and CAS against the expected previous SHA before writing. Storage uses `assets/png/<surface>.png` and consumer name `<surface>.pitch.png`. If fresh reconciliation proves an existing canonical entry is valid under the current validator, it can be reused without a new network acquisition.

Explicit `reacquire` starts a new generation and captures the owner's current SHA as the CAS baseline. If the store owner changes later, `batch status` detects `refresh_owner_drift` and blocks publishing the stale candidate. `batch reacquire` with the required `--reason` archives the conflicting publication intent, adopts the current owner SHA as new baseline, starts another generation, and unlinks the old attempt so it cannot publish. If reacquisition produces the same SHA, metadata/evidence update only after v6 revalidation and matching CAS at that same store SHA. Publication always uses metadata from the exact active attempt. Visual rejection binds to one SHA and is not inherited by different bytes.

Addressed `retry` handles retryable technical failures without resetting neighboring completed items. After reconciliation, a record may be `ExistingVerified` although the latest batch outcome is a recoverable technical failure; targeted `retry` is then allowed to clear `ExistingVerified`, start a new generation, and set the current owner SHA as CAS baseline.

`batch retry-technical` with the required `--reason` applies the **same criterion** across all batch items, starting new generations for all currently retryable technical failures. It does not change nonretryable failures, published items, unfinished publication intents, ambiguous entries, no-pitch results, not-found results, conflicting items, or `pending` items. The transition is all-or-nothing; repeating a successful operation selects nothing. If owner reconciliation saves no other changes, `changed: false`; otherwise `changed` includes them. Selected identities are returned as `retried_identities` in plan order, absent for an empty selection. `no_pitch_accent_on_source` completes only the *current batch*, creating no empty record or permanent negative cache; `vocabulary_not_found` remains separately unresolved.

`batch review` saves `review.html` inside ignored batch `.runtime`. It shows acquired and canonical PNGs, surface/reading, JPDB detail link, number of detected graph variants, selected variant and selection basis, number of graphs in the PNG, SHA-256, size, validator status, and provenance/rendering proof. Ambiguous outcomes list all candidates; no-pitch, not-found, and technical failures are typed, without invented images. Text is escaped, and HTML neither modifies state nor establishes authority. Use explicit `select`, `reject`, `retry`, or `reacquire` for decisions; there is no manual bypass of current automatic verification.

### Review snapshot and decisions against it

`batch review-export` creates an addressable snapshot `.runtime/batches/<id>/review-<review_id>/` using the mechanism shared with `kanji-assets`: `review_id` is the SHA-256 of the canonical `snapshot.json`, accompanied by exact PNG copies `<surface>-<SHA>.png`, `index.html`, and an editable `decisions.tsv`. The snapshot lists ambiguous dictionary entries and current candidates with status `acquired_verified`, `published`, or `candidate_rejected`; each row is bound to generation, item revision, request fingerprint, attempt number, exact SHA, and validator status. Exporting the same state again returns the same `review_id` and does not overwrite a filled decision file. In JSON, `review_id` identifies the snapshot and `review_artifact` is the path to `decisions.tsv`.

Only the first column is edited: on a candidate row, `y` means reviewed — the current automatic `VERIFIED` is accepted as is — and `r` rejects the exact SHA; on an ambiguous-entry row, a number is a JPDB dictionary-entry ID from that row's candidate column (the ID/URL pair comes from the snapshot); empty means no decision, `-` means skip.

`batch review-apply` is atomic: all rows are checked first, then batch state is saved in one write. The whole file is refused, with no change to batch, corpus, or journal, if the signature, a column, row order, `snapshot.json`, or a PNG copy changed; if `review_id` is foreign; if a row's generation, revision, attempt, or current SHA changed after export (`review устарел`); or if an ID outside the row's candidates is chosen (`identity_conflict`). `y` on a candidate without a current `VERIFIED` from the current validator is refused with `invalid_validation_evidence`. A competing command is serialized by the batch lock: `review-apply` waits for it and then checks row freshness. Repeating the same snapshot is idempotent: already applied decisions are counted in `already_applied` and change nothing.

Trust does not change: the domain stays `AUTOMATED_VERIFIED_ONLY`. `y` neither publishes nor modifies the owner record, so no human decision makes `UNCERTAIN`, `REJECTED`, `CORRUPT`, missing, or earlier v5 evidence usable. A dictionary-entry choice uses the same transition as `batch select`; the next `batch run` re-confirms it with a JPDB search. `r` uses the same transition as `batch reject`; if the rejected SHA matches the published owner record, it is quarantined via `AssetStore::attest` with a `trust_revoked` journal event, and a repeat writes no second event. The application summary is returned in `review_decisions`.

Optional external checks — for example, a visual check by a local model (`batch llm-review`, see "Local inference") — may only attach a hint to a row, bound to its identity and exact SHA, just as OCR does for `kanji-assets`. A hint never becomes a decision: only the explicit first column decides, and it cannot bypass the domain's trust rules.

### Migrating from validator v5

Entries published with `PitchAccentImageValidator` v5 contained the union of all of the entry's graphs without margins. Validator v6 does not treat them as current: the canonical entry, its bytes, attempt history, and human decisions stay in place but are not trusted and not reused. `pitch-corpus-gate` and `corpus check` report outdated evidence, `read_verified_with_policy` refuses, and batch reconciliation marks the item with the `owner_not_current_verified` conflict, so `ensure` or `batch run` exits with code `3` without a network acquisition and without overwriting. A human `Approve` does not change this.

Updating happens only explicitly: `ensure --refresh` or `batch reacquire` with the required `--reason` records the current entry's SHA as the `compare-and-swap` baseline, acquires a new PNG, and publishes it only if validator v6 returns `VERIFIED` and the owner's SHA has not changed. An ambiguous dictionary entry still requires `batch select`. Batches created before the migration do not continue: their state is rejected with code `unsupported_schema_version`, so create a new batch for the update.

### Cleaning up completed batches

Each `ensure` creates a new automatically generated batch directory and does not accept `--batch-id`. `batch start` allows explicit IDs, generating one if omitted. Directories remain under `.runtime/batches/` and **are not automatically cleaned**; candidate bytes, reports, and history accumulate even on reuse of verified resources. Before manual cleanup, use `batch status` and save desired reports/history outside the store. Stop all CLI processes using the store, then remove **only** the selected completed batch's `.runtime/batches/<batch_id>/`. Do not remove batches needed for `review`, `retry`, `resume`, or `reacquire`: deletion loses these capabilities and history, but does **not** delete published corpus records, `assets/`, or the store manifest.

## Acquisition from Yarxi

`ensure` starts an isolated headless Chromium session and interacts with the real dynamic interface at `https://www.yarxi.su/`. It searches only within the kanji-search container, its “Чтение” (Reading) field, and its “Найти” (Find) button. After the “Информация” (Information) tab is opened, the provider requires the exact Unicode code point of the requested character; a mismatched entry is rejected. The Yarxi article number and source metadata become image provenance.

Normal `ensure` prioritizes primary `img.kakijun-gif` and reads its GIF bytes via shared browser asset facilities. Fallback is permitted only after **proving GIF absence**: the image area and tab are ready, Unicode matches, relevant network requests completed without network/HTTP/JavaScript errors, DOM has been stable for two seconds, and the GIF node is absent. A pending/failed GIF request, timeout, unfinished image, network or JS problem is an acquisition error, never `gif_absent`.

Normal `ensure` uses `AcquisitionTarget::PreferredSource`, fetching GIF first. Only after proven absence can it use `.font-sample`. If the leftmost eligible tile contains an acceptable original PNG, it reads those bytes; if it is rendered as text, it captures the tile element. Explicit `AcquisitionTarget::RenderedFontSamplePng` requests a rendered PNG even when GIF exists, without changing normal `ensure` priority.

For confirmed GIF absence, the provider chooses the leftmost visible eligible `.font-sample` tile, excluding `stroke-order`. Original PNG is preferred when available. For font-rendered samples, the provider sets a reproducible dark page scheme using `prefers-color-scheme: dark` and `data-theme="dark"` on Yarxi's root. At page level, `#app` gets the current dark-palette text color because its ordinary rule otherwise sets light text irrespective of theme. The provider neither repaints nor rearranges `.font-sample` itself. It screenshots only the DOM tile, preserving background, border, padding, and actual glyph; no full-page screenshot. Provenance includes tile, CSS rectangle, computed colors, font settings, viewport, device scale and PNG dimensions.

The right-hand `kakijun.jp` link is forbidden as an image source. Approved `blob:`/`data:` fallback bytes are read in the browser context. Before semantic checks, GIF/PNG signatures are verified; a decoder then verifies the complete file, dimensions, nonempty drawn region, and GIF frame stream. Rendered samples retain their original scale, with actual dimensions/viewport/DSF in provenance.

### Manual acceptance of dark PNGs

`yarxi_dark_acceptance` is for manually accepting real rendered PNGs and reviewing HTML results. An external JSON plan supplies an arbitrary number of items/groups. `items` must be nonempty; each item has a unique `character` supported by `KanjiCharacter` as a Han/CJK Unicode scalar. Optional `group` must be nonempty when present. `unicode_codepoint`, `yarxi_article_number`, and `frequency_index` are optional. The provider always checks the exact Unicode point against Yarxi; provided `unicode_codepoint` must also match. Supplied article number and frequency index are compared to Yarxi `evidence`.

For example, save the plan outside the checkout:

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

Run from workspace root:

```bash
cargo run --locked -p asset-store --bin yarxi_dark_acceptance -- \
  --plan ../yarxi-plan.json
```

By default, the narrowly verified Yarxi TLS warning bypass is permitted; `--allow-insecure-tls=false` disables it and requires a valid certificate. Each run stores its report/evidence in **one verified ZIP**, leaving no extracted directory, by default at `<std::env::temp_dir()>/anki-decks-evidence/yarxi_dark_acceptance-run-<32 lowercase hexadecimal digits>.zip`. `--output <ZIP>` chooses a **new** `.zip` path; relative paths resolve against current directory. Its parent must exist, the file must not, and the path must lie outside checkout and temporary tree. Inside ZIP: `reports/index.html`, `reports/evidence.json`, `reports/images/`. Stdout ends with `LIVE EVIDENCE ZIP`, absolute `Path`, `SHA-256`, byte `Size`. On failure a ZIP may contain partial results and error details. The run does not ingest candidates into the store or publish them; both plan and archive remain local/uncommitted. The repository owner reviews real images before separate publication even if automatic validation yielded `VERIFIED`.

Parallel acquisitions use separate browser workers with isolated profiles and `TMPDIR` inside individual `TempWorkspace` instances. Chrome/Chromium is required; executable can be set using `CHROME_BIN` or `CHROMIUM_BIN`, otherwise `PATH` and Playwright cache are searched. Browser profiles, cookies and session state never enter evidence ZIPs.

#### Manual test of browser-session rotation and Kanji batch resumption

For real Kanji-batch acceptance testing, build `kanji-assets` with Cargo feature `session-rotation-acceptance`. **Only** that build exposes `batch run --acceptance-rotate-after-items N`; ordinary builds do not. A positive `N` triggers planned session rotation after N `item_checkpointed` results within one browser session, continuing the remaining queue in a new session. The count resets per session; batch membership, attempts, generations, checkpoints, and resumption rules are unchanged.

The feature only speeds acceptance testing. Normal behavior remains 20-minute session limit, 90 seconds per item, 900 ms between items and at most two acquisition attempts. Without the flag there is no early rotation.

From the workspace root, use three characters. After `item_checkpointed`, `browser_session_rotated`, and the next `item_started`, press `Ctrl+C`. Then inspect status and continue the **same** batch ID/store:

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

Check with `status` that existing checkpoints survived; another `batch run` resumes outstanding items. The acceptance flag controls only the moment of browser-session rotation and does not change batch semantics.

### Temporary trees and acceptance ZIP artifacts

Both acceptance programs use shared `EvidenceRun` and `TempWorkspace`. Default private run directories are `/tmp/anki-decks-<uid>/run-<32 lowercase hex>` with `0700` permissions. `ASSET_STORE_TEMP_ROOT` overrides the root with an **existing absolute non-symlink** directory; run trees are then under `<ASSET_STORE_TEMP_ROOT>/anki-decks-<uid>`, and startup GC scans only that root, **not** `/tmp`. Its result, including errors, is cached per absolute root for the process lifetime to avoid repeat scans by workers. Implicit `cleanup_orphans`, `plan_orphans`, `cleanup_legacy_orphans`, `snapshot` and pitch batch startup cleanup use this root. `_under` APIs inspect only their explicit root; `GcReport.root` is populated even for empty results. Actual per-tree errors are not cached as successful startup cleanups. `EvidenceRun` pins the chosen root for its `startup_gc`, `final_gc` and snapshots.

Current run trees occupy a per-UID namespace; legacy cleanup recognizes only strict previous names directly under the same root. Alternate roots support constrained `/tmp` environments. A relative `TMPDIR` may shorten Chromium's absolute Unix socket path. Browser `TMPDIR` is forwarded without byte substitutions; non-UTF-8 values cause `browser_tmpdir_invalid` before browser launch, with the created profile cleaned. Relative `ASSET_STORE_TEMP_ROOT` causes `InvalidInput`.

Each run contains `.anki-decks-owner.json` with `schema`, `repository`, `tool`, PID, creation time, `run_id`, and purpose. New schema-2 markers add `boot_id` and `process_start_ticks` and are written atomically. Old schema-1 markers remain readable but require the original 24-hour TTL for cleanup. A browser profile is created as a subdirectory only when needed. `browser-profile-<32 lowercase hex>` and Chromium's short `t<3 lowercase hex>` temp directory are sibling private (`0700`) entries managed by one `BrowserProfile`. The short name keeps the Unix socket below `sockaddr_un.sun_path` limit.

At browser launch, **only the Chromium child** receives `TMPDIR=<run>/t<3 lowercase hex>` through `BrowserLaunchSpec`; neither the asset-store process environment nor user's shell is changed. Relative `TMPDIR` is used only when the canonicalized `ASSET_STORE_TEMP_ROOT` matches the asset-store process's current directory; otherwise it stays absolute. For relative values, verification resolves Chromium's own `/proc/<pid>/cwd`.

Before removing a profile, the tool checks the associated workspace: if the same-UID Chromium process's command line, `TMPDIR`, or file descriptors still refer to it, cleanup refuses and preserves the tree. Browser and CDP handler are stopped **first**, then browser profile/temp dir removed, and only then run workspace closed. Explicit closure occurs after evidence assembly; `Drop` attempts best-effort cleanup on normal exit. Failures carry `stage`, stable `code`, `path_category`, and a sanitized message without absolute paths. The ZIP is created independently and remains after temporary workspace removal.

Required ZIP entries are `summary.md`, `manifest.sha256`, `raw-evidence/events.jsonl`, `raw-evidence/technical-failures.json`, `runtime/report-snapshot.json`, `timeline.jsonl`, `commands.json`, `checks.json`, `fault-injection.json`, `metadata.json`, `hygiene.json`, `stdout/acceptance.txt`, `stderr/acceptance.txt`, `logs/diagnostic.jsonl`, and `reports/index.html`. Every other included file must be listed with SHA-256 in `manifest.sha256`. Before publishing a new ZIP, all required entries, unique/safe paths, and exact SHA-256 are checked; an existing ZIP is never overwritten. Optional entries include `reports/evidence.json`, `reports/images/*`, and real `stdout/`/`stderr/` outputs for `git`, `rustc`, `cargo`, and temporary-filesystem checks.

Archives contain only permitted report, telemetry, and log areas, **not** original plans, stores, ownership markers, profiles, cookies, or session data. JSON evidence and diagnostic events redact known secret fields; query strings and fragments are removed from URLs. `reports/index.html` is a rendered report, not the page's raw DOM.

When creating `TempWorkspace`, GC looks for orphan trees with ownership markers. Schema 2's precise `(boot_id, PID, process_start_ticks)` distinguishes a live owner from reboot, absent PID, or PID reuse, and proven dead owners do not need to satisfy an age threshold. Schema 1 retains a minimum 24-hour TTL. Acceptance programs record startup/final cleanup of marked and legacy trees.

Marked-tree cleanup only considers private `<temp-root>/anki-decks-<uid>/run-<32 lowercase hex>` with a valid project/tool marker and matching `run_id`, UID, plus process-reference checks. `temp-root` defaults to `/tmp` or the override. In schema 2, different boot ID, absent PID, or different start ticks prove the owner is dead; matching identity means live. Schema 1 uses PID plus TTL.

Checks are performed relative to `NOFOLLOW` directory descriptors. Foreign UID, unsafe permissions, mount boundaries, unknown symlinks or special files, a live owner, future timestamps, uncertain markers or process-identity read errors **prevent deletion**. Before removing a schema-2 orphan, GC checks same-UID processes' `cwd`, open file descriptors, and `--user-data-dir` (plus Chromium `TMPDIR`), then repeats this check immediately before deletion. GC's own traversal FDs are excluded, but its current process's cwd/environment still count. Schema-1 Chromium checks run when browser files exist.

For `TMPDIR`, existing ancestors and symlinks are resolved, then ordinary names in the missing suffix. A provably nonexistent path outside the tree does not block independent cleanup. An inward link, access failure, dangling link, or `..` following a missing ancestor preserves the tree. Absolute `TMPDIR` does not require cwd; relative paths require proven process cwd. Process disappearance is verified using `/proc` PID entries, separately from individual read failures.

If needed process information cannot be obtained, GC skips the tree. Within a **verified orphan**, it recognizes only Chromium-created forms: profile `SingletonCookie`, `SingletonLock`, `SingletonSocket`, `org.chromium.Chromium.<6 alphanumeric characters>` directory, its symlink to the cookie and socket, and regular `.org.chromium.Chromium.<6 alphanumeric characters>` files in its private browser temp dir. No symlink traversal; type, FD boundary, UID and mount boundary are verified before removal. Global `/tmp/org.chromium.*` and `/tmp/.org.chromium.*` are **not** targets.

The initial `snapshot` is taken before startup cleanup; the final snapshot follows deletion of the active workspace and final GC. `hygiene.json` records cleanup results and whether the workspace was removed.

Acceptance programs run `cleanup_legacy_orphans(min_age)` at startup and before publishing ZIP. Only strict old-name forms are eligible: `anki-manifest-<listed-label>-<pid>-ThreadId(<id>)`, `asset-store-pitch-cli-<pid>-<sequence>`, `asset-store-pitch-batch-<pid>-<sequence>`, `jpdb-live-acceptance-<pid>-<timestamp>`, `yarxi-live-acceptance-<pid>-<timestamp>`, and `kanji-<32 lowercase hex>`. Legacy TTL is at least **24 hours**; a live PID blocks deletion. UID, no group/other write permissions, absence of symlinks/mount boundaries/special files are required. Uncertain/foreign trees are skipped. `snapshot()` measures known temporary trees without deleting. Both garbage collectors target only their own recognized prefixes, never arbitrary system-temp entries.

### Yarxi certificate

Chrome can currently show `ERR_CERT_AUTHORITY_INVALID` for Yarxi. By default the tool may follow only the Chrome warning page's “Перейти на сайт” (Proceed to site) link for the original URL with exact host `www.yarxi.su`, exact error code `ERR_CERT_AUTHORITY_INVALID` (case-insensitive), and an actual proceed link. It does **not** enable global `--ignore-certificate-errors` or process-wide TLS verification bypass. The exception is restricted to Yarxi; `--allow-insecure-tls=false` requires a valid certificate. Different host, TLS error, or URL form still aborts.

## Semantic validator and calibration run

`KanjiImageValidator` examines PNG/GIF **pixels**, not filename, URL, DOM text, or provenance. PNG is fully decoded. `GifDecoder::into_frames()` composites frames according to GIF disposal rules; every frame is checked, and the one with greatest foreground-object area is selected. Frames without foreground (for example, an empty frame of a stroke-order animation) are skipped instead of rejecting the whole GIF; a GIF without foreground in any frame is rejected.

Kanji validator and batch adapter limit images to **8 MiB**. Shared batch runtime caps state and blob files at **64 MiB**; adapters may choose a smaller bound. Human approvals use the current domain-policy size limit, or 8 MiB if none is defined. Reading is limited to at most that limit plus one byte to detect oversize without unbounded allocations; integrity and full image decoding are then required.

The main reference catalog is based on KanjiVG release `r20250816`, commit `bd13ffbcc9d85cb86ae98bbbf001d9069220b901`. Unicode-codepoint SVGs are normalized into 64×64 binary masks; **6,430** characters are embedded in compressed `src/data/kanjivg-r20250816.maskdb.zlib`, SHA-256 `5581e65d5a681cdf8e441be04c1cea5f6ec61342ce8830ed083ba417d83a88ff`. KanjiVG uses CC BY-SA 3.0; attribution, release version and source-archive SHA-256 are in `src/data/kanjivg-ATTRIBUTION.txt`.

Dark tiles also use an additional pixel reference from the publicly released Noto Serif JP Regular 1.001 font: `ofl/notoserifjp/NotoSerifJP-Regular.otf` from the `google/fonts` repository at pinned commit `beff1d8226e598dfaf06f9f8312c01021f70e9d2`, SHA-256 `a12875dcde3979f7901cd52c6f750f763e5ad2824cf3c27019a61dd5de5a8d9d`, licensed under the SIL Open Font License 1.1. It is an independent source, not the font file formerly served by the Yarxi site: that TTF is unavailable, and neither the site nor its data is an input to generation anymore. The font yields **6,428** normalized masks in `src/data/noto-serif-jp-1.001-regular.maskdb.zlib`, SHA-256 `cce25df61382ccb8e14d1eca181609916aac483b4949fcbf637f4fb5ac353608`; the font itself is **not** committed. Source, license and the regeneration command are in `src/data/noto-serif-jp-ATTRIBUTION.txt`.

The `build-noto-font-reference` generator checks the font SHA-256 and uses only those characters of the pinned KanjiVG catalog that are present in the font's cmap. For a character outside the cmap `resvg` draws the `.notdef` glyph, and one and the same box would become the reference of several unrelated Unicode characters; U+55BB and U+27491 are therefore skipped. The `noto_catalog_has_no_notdef_templates` test allows identical masks only for canonically equivalent characters (CJK compatibility ideographs and their unified forms).

Reference generators use `resvg` and `skrifa` only behind the `reference-generator` feature; the runtime validator reads the embedded catalogs and does not depend on the `resvg` version. Both catalogs reproduce byte-for-byte on `resvg 0.48.1` and match the `resvg 0.47.0` output for the same inputs. The new Noto catalog differs from the previous catalog based on the Yarxi TTF in 6,257 of 6,428 masks (median 7 of 4,096 pixels) and was therefore accepted only under a new validator version. The `embedded_reference_catalogs_match_registered_identity_and_attribution` test binds the SHA-256 of both embedded catalogs and of the source font to the validator version and the attribution files, so replacing a catalog without a matching validator identity change fails the check.

Validation estimates the background from image corners, determines contrast polarity, removes frame-connected components at the edge, and extracts the foreground glyph mask. Reference generators and runtime use the same `kanji_mask::normalize_binary_mask`: crop to nonempty foreground, fit inside 56×56, center in 64×64. Each destination pixel chooses the closest-to-center source-cell pixel using `(2*i+1)*source_size/(2*output_size)`. Thus KanjiVG, Noto and candidates share one normalization algorithm.

A symmetric Chamfer distance is calculated between centered masks: mean nearest-pixel distance in both directions, normalized to mask size. Dark-background PNG candidates are compared against both KanjiVG and Noto Serif JP; the smaller distance wins. The nearest **different** Unicode character is searched across the union of applicable pinned catalogs so expected and rival glyphs have equivalent font coverage. Validator version: `kanjivg-r20250816-5581e65d-noto-serif-jp-1.001-a12875dc-cce25df6-mask64-v7`.

Thresholds are contractual:

- `VERIFIED`: expected-mask distance ≤ `0.020` and margin from nearest other Unicode identity ≥ `0.004`.
- `REJECTED`: expected-mask distance is worse than the nearest other identity by at least `0.015`.
- Otherwise `UNCERTAIN`.
- Invalid/corrupt/empty bytes: `CORRUPT`; missing reference: `UNCERTAIN`.

Threshold changes require a new validator version and associated regression suite. Ambiguous pixels remain `UNCERTAIN`; file names, entry titles and provenance cannot force acceptance.

Offline reproducible calibration:

```bash
cargo test -p asset-store offline_threshold_calibration -- --nocapture
```

The test selects **64** glyphs evenly by Unicode order from the pinned Noto catalog, renders synthetic 142 px tiles, and measures true-identity distances, margins and statuses. It also evaluates each sample under the nearest competing Unicode identity and forbids `VERIFIED` for it. Output reports counts by status, maximum true distance, minimum margin, maximum competing-identity difference, and competing status results. It is entirely pinned/offline, independent of manual image review and current character plans.

For current validator v7, **58/64** positive synthetic cases were `VERIFIED`, **6/64** `UNCERTAIN`, **0** `REJECTED`; maximum reference distance `0.004004`, minimum margin `0.001969`. For the nearest competing identities **0** were `VERIFIED`, **5** `REJECTED` and **59** `UNCERTAIN`, maximum difference `0.022829`. The previous validator v6 gives the same 58/6/0 and 0/5/59 on this sample. The sample was enlarged from 16 to 64 glyphs: on 16 glyphs the v6 result sat exactly at the 75 % boundary and a single glyph changed the outcome, while the `VERIFIED` share over all 6,428 references is about 92 % for both v6 and v7. These observed results do **not** change thresholds.

Deterministic tests with embedded Unicode catalogs and synthetic raster bytes cover:

- Exact KanjiVG mask in PNG and animated GIF.
- Dark Yarxi-style bordered Noto Serif JP tile and rejection of incorrect Unicode identity.
- Dark tile rendered at chosen scale and rejection of visually similar but wrong Unicode identity.
- Yarxi GIF sequence “full frame → incremental drawing → full frame”, checking the final full semantic mask, not a selected frame number.
- a GIF with an empty first or intermediate frame selects the full frame for both validation and OCR preparation, and a GIF of only empty frames is rejected.
- Similar-but-wrong Unicode identities.
- Increased stroke weight and cropped frames.
- Corrupt signatures/streams, empty images and missing references.
- Distance, margin and rejection threshold boundaries.

Calibration covers pinned glyphs and their closest incorrect identities but **does not estimate** the false-verification rate for real Yarxi images or all production sources. It never replaces human review of source PNGs; insufficient separation remains `UNCERTAIN`.

## Commands

Run from the workspace root:

```bash
# Idempotently acquire and validate one or more kanji.
cargo run --locked --bin kanji-assets -- \
  --output json ensure 漢 字

# Validate only new or outdated validation decisions.
cargo run --locked --bin kanji-assets -- --output json validate --mode new

# Revalidate all assets.
cargo run --locked --bin kanji-assets -- --output json validate --mode full

# Inspect state and the deterministic validation plan.
cargo run --locked --bin kanji-assets -- --output json list
cargo run --locked --bin kanji-assets -- --output json plan \
  --mode new --validator-id kanjivg-pixel-chamfer \
  --validator-version kanjivg-r20250816-5581e65d-noto-serif-jp-1.001-a12875dc-cce25df6-mask64-v7

# Manually import a local candidate (it stays in the pending state).
cargo run --locked --bin kanji-assets -- --output json ingest \
  --character 漢 --file ./candidate.png

# Create a batch (the ID is generated automatically) and run up to five breadth-first rounds.
cargo run --locked --bin kanji-assets -- --output json batch start 漢 字
cargo run --locked --bin kanji-assets -- --output json batch run --batch-id <id>

# Continue the batch later, inspect state as JSON, or write a local HTML report.
cargo run --locked --bin kanji-assets -- --output json batch status --batch-id <id>
cargo run --locked --bin kanji-assets -- --output json batch review --batch-id <id>

# Apply an explicit user decision for an exact SHA-256.
cargo run --locked --bin kanji-assets -- --output json batch decide \
  --batch-id <id> --character 漢 --sha256 <sha256> \
  --action confirm --reason "пользователь подтвердил текущий кандидат"

# After a series of technical failures, allow a new acquisition generation.
cargo run --locked --bin kanji-assets -- --output json batch retry \
  --batch-id <id> --character 字 --reason "источник снова доступен"
```

`--store <path>` selects the store root, `--repository-root <path>` selects checkout for verifying the `decks/` boundary. The narrowly scoped Yarxi TLS-warning handling is enabled by default; `--allow-insecure-tls=false` requires a valid certificate. `ensure` stores only truly verified `VERIFIED` entries; reuse of trusted identical-SHA content makes no network request.

`list`, `plan` and `validate` open the store; opening can create state files, recover incomplete operations, or migrate old records across `.runtime`. `plan` itself does not write, but `changed: false` does **not** imply the open operation was read-only. `validate` invokes the core validator: `new` selects missing or stale decisions, `full` rechecks the whole manifest. Any failed validation enters quarantine; exit code is nonzero when the requested target is not `VERIFIED`.

JSON output includes `schema_version`, `operation`, `store`, `mode`, `assets`, `changed`, `conflicts`, `blockers`, and `outcome`. Errors include stable `error.code`, `category`, `message`, and `details`. For `ensure`, every asset has an `item_outcome`, preventing independent successes from hiding neighboring failures.

`ingest` reads exactly one explicitly selected source file **outside `decks/`** and creates a `pending` candidate. Format and hash are determined from bytes. For compare-and-swap replacement, provide `--replace-expected-sha256 <current-hash>`.

### Kanji batch processing

`batch start [--batch-id ID] CHARACTER...` deduplicates identities, creates versioned `.runtime/batches/<id>/` state, and marks resources already confirmed as currently `verified` by their owner before acquisition. Reusing an ID requires identical requested identities and validator and resumes saved state instead of starting over.

`batch run --batch-id ID [--rounds 1..5] [--interactive-review]` performs up to five acquisition rounds. New batches use `kanji-single-candidate-v1`: a technically valid but `UNCERTAIN` or `REJECTED` candidate immediately becomes `awaiting_human`, without redownloading alternatives or inflating trust by averaging. Retryable technical failures remain limited to five rounds. Each round processes the current queue before retry; existing published/verified entries are not reacquired. Per-item failures remain in `issues` without undoing neighboring results. Browsers run without holding the batch lock. After each actual result, CLI reacquires the runtime lock, checks generation and attempt count, saves candidate bytes/evidence and atomically checkpoints state before processing the next item. A result is discarded as stale if the item's state changed during acquisition.

`batch run --workers` allows `1`–`4` workers, default `4`, with independent browser sessions. The default came from comparative live measurements on an identical set of 300 kanji.

During `batch run`, progress immediately goes to `stderr` as text or, with `--output json`, JSON Lines. Every JSONL event includes `schema_version: 1`, `operation: "batch_run"`, `event`, `batch_id`, `elapsed_ms`, `round`, `round_limit`, `round_completed`, `round_total`, `run_completed`, `batch_total`, `workers`, `in_flight`. Contextual optional fields are `identity`, `session`, `worker_id`, `browser_session_id`, `attempt`, `generation`, `owner_attempt`, `item_elapsed_ms`, `outcome`, `reason`; readable output shows relevant worker/session/owner-attempt details. Only the coordinator sends pool `heartbeat`, including session preparation; internal acquisition heartbeats are not duplicated.

`stage_timings` is an optional array of `{ "stage": "…", "duration_ms": N }` on `item_checkpointed` or `item_discarded_stale` when the provider supplies acquisition diagnostics; it may be empty. It records acquisition stages **before** outcome validation/checkpoint, so the detailed diagnostic log supplements it with those later steps.

Stable progress `event` values: `run_started`, `round_started`, `browser_session_started`, `browser_session_ended`, `browser_session_rotated`, `browser_session_bootstrap_started`, `browser_session_recovery_started`, `worker_assigned`, `worker_retired`, `worker_stopped`, `worker_exhausted`, `item_started`, `retry_started`, `retry_recovery_started`, `heartbeat`, `item_checkpointed`, `item_discarded_stale`, `item_deferred`, `item_requeued`, `round_finished`, `run_stopped`, `run_finished`. Individual acquisition errors remain item outcomes; CDP/monitor failure, timeout, or page-recovery failure appears in session-ending reason. Monitor health is checked after each successful checkpoint, including the last item; saved results are not undone or reacquired. If the queue is empty after monitor failure and health was not proven by a new acquisition, the run fails but preserves results. Otherwise only a session for unfinished items is recovered. Write/integrity or cleanup failure stops the run; it does not trigger automatic recovery. During bootstrap, only `browser_session_setup_runtime_unhealthy` is retried. Browser launch, TLS and other setup failures stop immediately with their original reason, not a fabricated worker-recovery exhaustion.

`round_completed` increments only after durable checkpoint. JSON mode still sends one final standalone JSON document on `stdout`, with progress kept separate.

Every `kanji-assets batch run` and pitch-accent batch worker (`pitch-assets ensure`, `batch run`, `batch resume`) creates a dedicated diagnostic log at `<store_root>/.runtime/batches/<batch_id>/logs/<run_id>.jsonl`. `SafeBatchRuntime` creates path/file inside verified boundaries, without symlink traversal or overwriting another run's log. Optional final-JSON `diagnostic_log` includes its path even on failure; text mode prints it to `stderr` at startup.

Diagnostic JSONL contains timestamps, severity, module, and structured runtime fields. It stores safe typed errors, browser sessions, JPDB stages, checkpoints, CAS and stop details, but **no DOM, cookies, or authorization headers**. With `--output json`, details go only to the file while `stderr` contains pure progress JSONL, without formatted diagnostics. Text mode may show tracing messages on `stderr` only. `browser_acquisition_*` debug events remain file-only: human output accepts INFO and JSON mode disables that layer. The log starts before acquisition and all events are flushed before command return.

Shared per-item/per-stage DEBUG events are `browser_acquisition_item` and `browser_acquisition_stage` with `schema: "browser_acquisition_v1"`, `schema_version: 1`. For item events, `stage: "item"`. `stage_duration_ms` is this stage's duration; `item_duration_ms` is cumulative since item start. Each pitch assignment has its own monotonic timer passed worker → provider → coordinator through CAS checkpoint. Yarxi measures internal retries/backoff, candidate validation and checkpoint. Total item duration includes acquisition, validation and persisted result. Provider-only JPDB ends timing at acquisition, without a coordinator checkpoint.

Required event fields: `event`, `schema`, `schema_version`, `provider`, sanitized `identity`, `attempt`, `generation`, `stage`, `stage_duration_ms`, `item_duration_ms`, `outcome`, `retryable`. Optional: `worker`, `worker_session`, `browser_session`, `failure_code`, `stop_reason`, `runtime_snapshot`. Providers are `jpdb` and `yarxi-suu-browser`. Pitch worker/session IDs are local to that worker; they may be absent elsewhere. `browser_session` identifies a browser session. `provider`, `outcome`, `failure_code`, and `stop_reason` are restricted to safe tokens, replacing invalid entries with `redacted`. Identity is sanitized and bounded at 256 bytes. On failure/interruption, `failure_code` and available final `runtime_snapshot` are included; `stop_reason` identifies the stop independently. `retryable` represents the provider's decision, not whether retry occurred. `generation` is Kanji owner generation or `null` for provider-only use—never a synthetic zero. When a monitor/session fails **after** a domain outcome, the item keeps its original result, while `stop_reason`/fresh snapshot explain queue termination.

`stage` names correspond to actual execution boundaries. JPDB uses `browser_launch`, `configure_browser`, `request_validation`, `search_resolution`, `search_navigation`, `search_readiness`, `detail_navigation`, `detail_readiness`, `detail_verification`, `pitch_inspection`, `capture`, `post_capture_verification`, `candidate_validation`, `outcome_validation`, `checkpoint`. The final three represent pitch-result validation and persistence, including lock reacquisition, state reread and CAS. Redirects directly to detail pages may omit `detail_navigation`. A custom `PitchRunDriver` using the default method records `provider_acquisition` for the whole acquisition, whereas the production JPDB driver records the specific provider stages. Yarxi uses `search_submission`, `information_tab`, `article_verification`, `media_readiness`, `media_selection`, `dark_theme`, `resource_read`, `font_sample_capture`, `retry_backoff`, `retry_recovery`, `semantic_validation`, `final_validation`, `checkpoint`. The last three cover image semantics, candidate checks and CAS checkpoint. Nested or parallel durations may overlap; **never sum them** as sequential intervals. Successful stages use `outcome: "success"`. Successful JPDB item outcomes are `acquired`, `no_pitch_accent_on_source`, `ambiguous_vocabulary`, `vocabulary_not_found`; errors/interruption are `failure`/`interrupted`, with `failure_code`. Successful Yarxi item outcome is `success`.

`runtime_snapshot` is a JSON projection of latest CDP telemetry encoded as a string, limited to **32 KiB**. It includes `epoch`, `pending_request_count`, `network_failure_count`, `http_error_count`, up to 16 records each in `pending_requests`, `network_failures`, `http_errors`, plus `javascript_exceptions` and `monitor_failed`. Stale-epoch counts/collections are `stale_pending_request_count`, `stale_network_failure_count`, `stale_http_error_count`, `stale_pending_requests`, `stale_network_failures`, `stale_http_errors`; JavaScript exceptions are separated into current, startup and stale groups. Every current/stale collection is capped at 16. A sample has hashed `request_key` (SHA-256, never raw CDP ID), resource type, epoch, whether current/top-level, observation age, HTTP status and network error category. No raw URL, headers, cookies, DOM or CDP messages enter snapshots. If too large, a compact object with `truncated: true` retains current epoch, current/stale counts, JS exception breakdown and `monitor_failed`, using the same diagnostic projection for counts.

Acquisition events describe individual items, separate from `browser_session_started`, `browser_session_ended`, and `browser_session_rotated` session lifecycle events. Shared `browser_runtime` also records `event: "browser_lifecycle"` for shutting down owned transports, CDP handlers, telemetry and profile/temp directories. Transport termination does not by itself prove cleanup: a disconnected transport is harmless only when the process is demonstrably stopped and owned directories were successfully deleted. Failure remains `browser_cleanup_failed`; run workspace cleanup errors retain `temp_workspace_cleanup_failed`.

Example error extraction from a run:

```bash
jq -c 'select(.level == "ERROR") | .fields' \
  <store_root>/.runtime/batches/<batch_id>/logs/<run_id>.jsonl
```

The 20-minute limit applies per browser session. When it expires before the next item, the session closes and remaining work uses a new one. Timeout/page-recovery failure can mark only a **started** item; unstarted items stay `pending`, with no attempts, generation changes or aggregated outcomes.

Before `browser_session_started`, Yarxi checks that its fresh runtime has no pending requests, network/HTTP errors, JS exceptions or monitor failure. If only pending requests remain, it waits up to **20 seconds** for them. Other errors fail readiness immediately. If readiness times out or runtime is unhealthy, the same queue position is retried in up to **two new sessions** without recording item attempts. Successful readiness resets this count. Cleanup and other setup errors abort with processed batch results preserved; subsequent `batch run` resumes remaining entries. On `Ctrl+C`, the provider closes the current browser session after the last checkpointed result. Uncheckpointed work remains retryable under the same `batch_id`.

Before publication, candidates/attempts are stored in batch `.runtime`. A single `VERIFIED` candidate passes domain-owner checks and is atomically published at canonical path. Old-policy batches with distinct technically valid SHA hashes compute mean/min/max/count of `expected_distance` and `nearest_margin`; duplicate SHA does not increase sample count. `kanji-distinct-mean-v2` retains original `0.020`/`0.004` thresholds, requires nearest-rival margin for the chosen candidate and pins exact SHA. Aggregated trust does **not** replace automatic validation of that candidate; aggregation version/evidence remains a separate decision.

After exhausting technical retries, items without candidates become `awaiting_human`; valid uncertain candidates in new batches reach it after the first persisted attempt. `batch review` creates an ignored local HTML report with animated GIFs, all distinct candidate images, exact SHAs, automatic verdicts, attempt evidence and aggregate metrics. Candidate blobs remain SHA-addressed in `candidates/`; HTML is never authoritative.

`batch decide` accepts only structured `confirm`, `reject`, `reacquire` for current `--character`/`--sha256`. `confirm` requires technically valid GIF/PNG, exact comparison and replacement of the expected source SHA, domain-owned automatic evidence, and complete decoding **before** human approval. `reject` stores the person's decision against the exact candidate and starts a new generation for that identity alone; `reacquire` plans a targeted refresh without determining semantic suitability. After five technical failures with no candidate, use `batch retry` **without** `--sha256`; with an existing candidate, SHA is required.

`batch status` returns state; `batch review` also writes the report. Batch JSON includes `batch_id`, `counts`, `items`, per-item `issues`, `blockers`, optional `review_artifact`, and complete `batch` state/evidence. `run` returns 0 only when all assets are resolved in the canonical store; unresolved human review/partial progress returns 3. Individual errors remain attached to their identities. Successful `start`, `status`, `review`, `decide`, `retry` return 0; boundary/schema/integrity system failures use the owner library's usual exit codes.

After restart, use the same ID: `status` → `run`/`review`/`decide`. Persisted candidates and human-decision intents allow resuming exact owner publication without refetching. `.runtime` state, HTML and candidate bytes are not canonical and must not be committed.

### Text decision packages and explicit interactivity

Ordinary commands, pipelines and JSON mode never prompt or open applications. An interactive run with a full TTY requires explicit opt-in:

```bash
cargo run --quiet --locked --bin kanji-assets -- batch run --batch-id ID --interactive-review
# Return to saved candidates without a network acquisition:
cargo run --quiet --locked --bin kanji-assets -- batch review-interactive --batch-id ID
```

If candidates remain after acquisition and browser cleanup, the prompt `[y/N]` appears. `y` or `Y` produces a derived decision package and opens the file manager and editor. After editing, the user must answer `y`/`Y` **again** to apply: exiting a GUI process does not commit any decision. Refusal, EOF, `Ctrl+C`, or app-launch failure preserve candidates and completed work. Empty queues never prompt; the batch lock is **not** held during editing. Acquisition errors suppress the interactive phase.

GUI-free commands:

```bash
cargo run --quiet --locked --bin kanji-assets -- --output json batch review-export --batch-id ID
# Edit the indicated review_artifact, then apply its review_id:
cargo run --quiet --locked --bin kanji-assets -- --output json batch review-apply --batch-id ID --review-id FULL_REVIEW_SHA256
```

`review-export` returns `review_id` and a `review_artifact` path to `decisions.tsv`. The `.runtime/batches/<id>/review-<review_id>/` directory holds a metadata snapshot, editable UTF-8 TSV, `index.html` for reviewing proposal groups, and independently verified GIF/PNG copies named `<Unicode>-<full SHA>.<ext>`. For ordinary batches, images are also embedded in HTML so Windows/WSL viewers do not depend on long paths to files. Total embedded source images are limited to **32 MiB**; remaining copies use relative links. Originals never change. Exporting the same snapshot again preserves previously filled entries. Modified image copies are refused on reopen/apply. `batch review` remains available for HTML-only output.

Only the **first column** of `decisions.tsv` may be edited; separator is TAB:

- `y` approves the **exact current candidate bytes**.
- `e` rejects an unsuitable image.
- A single other supported Han character rejects with `actual_character=<Unicode>`.
- Empty input or `-` leaves the record undecided.

BOM, CRLF and `#` comment lines are supported. Arbitrary words; edits to other columns, signatures, schema, batch, generation, attempts, SHA or paths; omitted, extra, or duplicate rows—**reject the entire file**. Only the selected snapshot's `decisions.tsv` is read, not backups or hidden files. Untrusted paths and symlinks are never opened. The snapshot is addressed by its metadata SHA; each row is checked against the current owner state.

All rows are validated under the lock **first**; then all `KanjiBatch::decide` decisions are atomically saved to normal batch state. Its `human_decisions` is the sole intent journal—there is no second trust source. Publication uses standard decode/SHA/CAS and owner human-approval logic. On failure after saving intents, rerunning `review-apply` or `batch run` resumes publication. Already-applied rows are idempotent and never approve a new generation even if SHA matches. Publication errors appear in `issues` and exit code `3`; `review_decisions` summarizes applied, approved, rejected, skipped and previously-applied entries.

File-based rejection uses `reject_and_hold` and state `rejected`, with **no automatic reacquisition**. To intentionally retry, use `batch retry --batch-id ID --character 漢 --reason 'explicit retry'` without SHA because the candidate is already rejected. Legacy `batch decide reject` retains its behavior of planning a new generation. A wrong character never becomes a new identity or causes filename renaming.

`--interactive-review` and `review-interactive` are incompatible with `--output json` and fail **before** opening the store/acquiring. Incomplete TTY, `TERM=dumb`, CI/`CODEX_NONINTERACTIVE`, and child processes of `cargo test` or any test harness are forbidden—even through a PTY. `cargo run` is allowed. The process must be in the terminal foreground process group; background jobs cannot prompt. Ordinary tests never open apps.

For `decisions.tsv`, the configured text editor is attempted first. If it fails, the file is opened using the desktop association for **`text/plain`**, rather than `.tsv`, to avoid spreadsheet software rewriting the format (`xdg-mime` and `gtk-launch`; `gio launch` can replace missing `gtk-launch` for a resolved desktop entry). The directory first uses Linux MIME via `xdg-open`. If that fails or WSL has no Linux desktop, and WSL binfmt interop is enabled, Windows Explorer is tried via `PATH` or a verified Windows path converted by `wslpath`. For Windows apps, `\\\\wsl.localhost\\...` UNC paths are shortened to supported `\\\\wsl$\\...` where possible. If still too long, use a Linux editor or shorter store path.

If no editor was configured and there is no Linux `text/plain` association, WSL interop may optionally try Windows Notepad (only if executable). Excessively long paths cause a clear refusal with advice to use a Linux editor or move the store. A configured Windows editor does not require Explorer but does require interop. Its Windows executable path uses `wslpath -u`; TSV paths for `.exe` editors, including a `PATH` command or `/mnt/...` path, are converted by `wslpath -w`. Conversion, launch and process-exit failures are reported.

No Windows, WSL or `notepad.exe` dependency is mandatory. If nothing can open files, absolute directory/TSV paths and `review-export`/`review-apply` instructions are printed. Editor selection order is `GIT_EDITOR`, Git `core.editor`, `VISUAL`, `EDITOR`; quotes and arguments with spaces are supported, but shell commands, substitutions and pipelines are **not**. Launch failures are diagnosed separately for folder vs TSV: successfully opening the folder does not prove the editor launched. If all launches fail, the command reports failure and leaves the package usable by manual commands. A configured editor is a user-trusted program: rejecting shell syntax does not sandbox it. Editor settings must be UTF-8, while a non-UTF-8 TSV filesystem path is passed losslessly to Linux executables. The editor launches directly, never executing decision text. Process exit does not approve/apply: after explicit confirmation, the TSV is read afresh. On a headless server without `DISPLAY`/`WAYLAND_DISPLAY`, a configured terminal editor works only under explicit interactive opt-in; absent apps yield printed paths. Desktop packages are never installed.

Old `kanji-distinct-mean-v2` batches remain readable under their prior rules. Explicit `batch revalidate --batch-id ID` decodes/revalidates saved bytes with the current validator and atomically migrates to `kanji-single-candidate-v1`, without network or fake attempts. Old human approvals stay in `human_decisions`, tied to exact SHA. If identical bytes pass current technical validation and are not `Corrupt`, approval still determines `HumanVerified`; otherwise it is preserved only as history, effective state is recalculated, and publication flags are cleared. `ExistingVerified` remains subject to owner trust and is reconciled with the current canonical store. Old automatic verdicts do not transfer between validator versions. After state/version changes, export a new decision snapshot.

### Local inference: llama.cpp and Ollama

Two optional tasks use a local vision model: blind OCR of saved uncertain kanji candidates (`kanji_ocr`) and a visual check of saved JPDB PNGs (`jpdb_pitch_review`). Ordinary `start`, `run`, `status`, `review`, verified reads and publication never call a model. A model result is only a **suggestion** for a human: it never grants `AutoVerified` or `HumanVerified` and never changes a candidate, decision, trust or the kanji validator.

#### Configuration

Settings are assembled from layers; each later layer replaces keys of the earlier ones:

1. `tools/asset-store/config/inference.toml` — versioned defaults, embedded in the binary and documenting every key with comments;
2. `tools/asset-store/config/inference.local.toml` — a personal partial override for the checkout (ignored by Git; template: `inference.local.example.toml`);
3. `--inference-config FILE` — an explicit file in the same format;
4. `--inference-set KEY=VALUE` and the shortcuts `--backend`, `--model`, `--endpoint`, `--allow-remote`, `--timeout-seconds` for the selected task's profile.

The schema is strict (`schema_version = 1`): an unknown key, wrong type, out-of-range value or reference to a missing profile yields `inference_config_invalid` (exit code `3`) before any model starts, and any other `schema_version` yields `unsupported_schema_version` (exit code `4`). Configuration has three levels:

- `[backends.<name>]` — how inference runs: `kind = "llama_server"` (managed `mode = "managed"` or external `mode = "external"`) or `kind = "ollama"`;
- `[models.<profile>]` — the selected `backend`, plus `llama_server` and `ollama` subtables with weights, projector, context, CPU threads, batch/ubatch, parallel slots, GPU layer placement, `flash_attention`, `reasoning`, `keep_alive_seconds`;
- `[tasks.kanji_ocr]`, `[tasks.jpdb_pitch_review]` — model profile, prompt file, `temperature`, `max_output_tokens`, `structured_output`, `timeout_seconds`, `seed`.

The prompt is a separate text file and can change without rebuilding. The JSON answer format, its validation and trust rules belong to code and are not part of the prompt. A relative path resolves against the directory of the file that set it; for the CLI layer, against the current directory. A `~/` prefix expands to the home directory, and Windows paths `C:\…` and `C:/…` become `/mnt/c/…` (the standard WSL drive mount). An empty string means "not configured". There is no other substitution, including environment variables or shell. By default no models are set: a task returns the typed `inference_unavailable` refusal with reason `backend_not_configured`, `model_not_configured` or `projector_not_configured`, while all other commands keep working. The tool never downloads models or contacts cloud services.

Windows llama.cpp launched from WSL: the `llama-server.exe` process uses Windows CPU, GPU and memory and listens on loopback, just like Ollama for Windows. This works only with WSL mirrored networking (`networkingMode=mirrored`): in NAT mode the Windows loopback is unreachable from WSL and startup ends with `startup_timeout`; `host = "::1"` together with `windows_interop` is rejected during configuration validation (WSL does not forward the IPv6 loopback). Set `windows_interop = true`; `/mnt/<drive>/…` model and projector paths are passed to the server as `<DRIVE>:\…`, and a path outside a mounted Windows drive is rejected as `path_not_visible_to_windows`.

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

#### Diagnostics and probe requests

```bash
# Effective settings, the source of every key and task readiness; no model is started:
cargo run --quiet --locked --bin kanji-assets -- inference show
# Start and verify the task backend, then stop the owned server:
cargo run --quiet --locked --bin kanji-assets -- --output json inference check --task kanji-ocr
# One request on local images outside a batch; the store is not changed:
cargo run --quiet --locked --bin kanji-assets -- --output json inference probe \
  --task kanji-ocr --image /path/to/kanji.gif
cargo run --quiet --locked --bin pitch-assets -- --output json inference probe \
  --task jpdb-pitch-review --image /path/to/graph.png \
  --inference-set tasks.jpdb_pitch_review.max_output_tokens=512
# Leases of managed servers from earlier runs; --stop repeats stopping the owned processes:
cargo run --quiet --locked --bin kanji-assets -- --output json inference orphans
cargo run --quiet --locked --bin kanji-assets -- --output json inference orphans --stop
```

`inference show` never prints prompt contents or access keys. `check` returns `ready` with backend and model identity, readiness time, process PID and the stop proof; with `windows_interop`, also the Windows process PID and memory (memory is not reported for a native `llama-server`). `probe` refuses files under the checkout's `decks/`, limits an image to 8 MiB and prints up to 2000 characters of the raw answer. All inputs are checked before the model starts: if none is usable, the result is `no_valid_images` with each input's refusal and the model is not started; in a mixed set the usable inputs run and the others keep their refusals (`completed_with_failures`). Exit codes: `0` success, `3` model unavailable, inference refusal or unproven server stop, `130` interrupted by a signal. In human-readable mode the summary of any outcome with a nonzero code goes to `stderr` and a successful one to `stdout`; `--output json` always writes JSON to `stdout`. The `check` and `probe` JSON contains `stop_unproven` and `blockers`, and `check` also `cache_reusable` (see below).

The common inference interface runs both text tasks and image tasks: the task's answer contract states whether an image is sent. Vision is confirmed in advance only for image tasks; a text request sends no image and needs no vision. The built-in tasks `kanji_ocr` and `jpdb_pitch_review` are image tasks; there is no built-in text task or CLI for one yet.

#### Managed `llama-server`

With `mode = "managed"` one operation starts its own process, proves its readiness and identity, runs requests and stops it. The server is launched without a shell, with explicit arguments, a random `--alias` and an `--api-key`; the parent's `LLAMA_ARG_*` variables are not inherited. Readiness means a successful `/health` and matching `/props`: alias, weights path and vision support. A foreign process on the chosen port yields `foreign_server`, a model without vision `model_does_not_support_vision`, other weights `model_mismatch`. Port `0` means a random free port in `10000`–`65535` excluding the actual Linux ephemeral range (`/proc/sys/net/ipv4/ip_local_port_range`) and, with `windows_interop`, also the Windows dynamic range `49152`–`65535`. If nothing remains for a native server, the kernel assigns the port; for a Windows process this is `port_unavailable` listing the ranges — set `port` explicitly. A busy explicit port yields `port_in_use`. Startup failures are distinguished by server output: `projector_load_failed`, `model_load_failed`, `invalid_server_arguments`, `server_exited_during_startup`, `startup_timeout`, `model_file_missing`.

`gpu_layers = "auto"` chooses layer placement from the free video memory at launch time. If another process holds it — for example, Ollama keeping a model for its `keep_alive` — most layers go to the CPU and answers become several times slower until the next launch. With `windows_interop`, `inference check` shows the Windows process working set, and the answer `timings` show generation speed.

One heavy model is loaded at a time (`max_loaded_models = 1`): a second launch gets `managed_server_busy` instead of loading in parallel, and switching profiles stops the previous process before the next one starts. Stopping sends SIGTERM to the group, then SIGKILL after `shutdown_timeout_seconds`; afterwards an empty group, a released port and, for Windows, the disappearance of the Windows process are checked. The Windows process is found by its own alias in the command line, not by the executable name: the binary may be named differently from `llama-server.exe`. If it has not disappeared after SIGTERM, it is terminated by PID only while its command line still contains that alias.

The lease file `leases/<alias>.json` in `$XDG_CACHE_HOME/anki-deck-toolkit/inference/` (otherwise `~/.cache/…`) is written right after the process starts and synced to disk. If it cannot be written, the process is stopped and the launch is refused with `lease_write_failed`: a server the next run could not find after a crash is not used. Only a proven stop removes the lease. The lease of a process left over after a crash is checked by PID, start time and the alias in its command line; the next run stops such a process and never touches foreign ones. With `windows_interop`, the lease is removed only once the Windows process is proven gone. Until then every managed launch, including a native profile, is refused: the previous model may still hold memory and video memory, and only one heavy model is allowed at a time. The code distinguishes the states: `orphan_server_alive` — the process is still alive (the message and `failure.orphans` name the Windows PID when known), `orphan_server_unverified` — the Windows process could not be checked (`powershell_unavailable`, `windows_probe_timeout`, `windows_probe_exit:*`). In both cases the message names the lease path.

Recovery: `inference orphans` shows every lease, its state (`process_alive`, `windows_process_alive`, `windows_probe_failed`, `stale`, `invalid`; after `--stop` also `stopped`), PID and file path without changing anything; `inference orphans --stop` repeats stopping the owned processes and removes only provably released leases (code `0` if nothing alive remains, otherwise `3`). An unreadable lease (`invalid`) is not deleted even with `--stop` and rejects the start as `orphan_server_alive`: whether the process is alive is unknown, so inspect the file and the `llama-server` processes by hand. If the Windows check is unavailable, restore WSL interop (`powershell.exe` must start from WSL) and repeat the command. Do not delete a lease by hand: without it nobody will find the leftover Windows process again.

Ctrl+C interrupts the current request, stops the server and returns `interrupted` with code `130`; records already saved remain. Code `130` takes precedence, but an unproven stop is not lost: `error.details` (for `check`/`probe`, the response itself) keeps `inference` with the `stop` details, `stop_unproven` and the `inference_server_stop_unproven` blocker. Without an interruption an unproven stop yields code `3` and the same blocker.

`mode = "external"` connects to an already running server at `endpoint`; the user starts and stops it. If the profile sets `model`, the weights path from `/props` must match, and SHA-256 is computed over the local file (`sha256:<hex>`). For a multi-gigabyte GGUF this takes noticeable time on the first run; afterwards the hash comes from the cache keyed by path, size and modification time. Without `model` only the path reported by the server is known: `reported-path:<path>`. The external server's API does not report a separate projector: with `vision = "projector"` it is recorded as `unverified:external`, and only `vision = "integrated"` in the profile declares that there is none.

A saved OCR or JPDB-check result is reused without a request only if the model identity is verifiable: weights are `sha256:` or an Ollama digest, and the projector is absent or `sha256:`. For an external server this means `model` in the profile and `vision = "integrated"`; otherwise every run asks the model again, the report contains `cache_reuse_blocked: true`, and `inference check` reports `cache_reusable: false`. Comparing `/props` before and after a request proves the model unchanged only within one run, not between runs. Earlier records stay in the ledger with their provenance.

The address is HTTP without credentials, path, query or fragment. **Anything other than literal loopback IPv4/IPv6** requires `allow_remote = true` or `--allow-remote`, an explicit consent to send images to that server. Proxies and redirects are disabled; a response is limited to 1 MiB and model text to 64 KiB. With WSL mirrored networking, Ollama on Windows is reachable as `http://127.0.0.1:11434`.

#### Kanji OCR

`batch ocr` processes an **already saved** batch sequentially. The owner first checks canonical-store integrity and current trust, reusing verified assets. OCR is permitted **only** for current, technically valid `awaiting_human` candidates with kanji-validator `Uncertain`. `Verified`, `Rejected`, `Corrupt` and resolved entries never start a model. No Yarxi refetch occurs.

```bash
# The ordinary path, without OCR:
cargo run --quiet --locked --bin kanji-assets -- batch start --batch-id my-kanji 漢 字
cargo run --quiet --locked --bin kanji-assets -- batch run --batch-id my-kanji

# OCR of saved uncertain candidates with the configured backend (llama.cpp by default):
cargo run --quiet --locked --bin kanji-assets -- --output json batch ocr --batch-id my-kanji

# The same OCR through a local Ollama:
cargo run --quiet --locked --bin kanji-assets -- --output json batch ocr \
  --batch-id my-kanji --backend ollama --model 'qwen3.5:9b-q4_K_M' --timeout-seconds 120

# Get the TSV and the index.html beside it; decisions start empty, '-' means an explicit skip:
cargo run --quiet --locked --bin kanji-assets -- --output json batch review-export --batch-id my-kanji
# After reviewing a matching group, leave confirmable rows empty and put '-' to skip them;
# enter explicit decisions for the other rows in the TSV.
# The command applies them together with the confirmed matching group.
cargo run --quiet --locked --bin kanji-assets -- --output json batch review-confirm-matches \
  --batch-id my-kanji --review-id FULL_REVIEW_SHA256 --confirm-reviewed
# After applying the group, create a new snapshot for the remaining rows.
cargo run --quiet --locked --bin kanji-assets -- --output json batch review-export --batch-id my-kanji
# Enter the remaining decisions and apply them with the new review_id:
cargo run --quiet --locked --bin kanji-assets -- --output json batch review-apply \
  --batch-id my-kanji --review-id NEW_REVIEW_SHA256
# An explicit OCR re-run, including saved refusals:
cargo run --quiet --locked --bin kanji-assets -- --output json batch ocr \
  --batch-id my-kanji --rescan
```

After applying a matching group, create a new `review-export` for the remaining rows: their state has changed, and the previous snapshot still contains the rows already applied. `review-confirm-matches` uses the same full TSV, SHA, generation, copied-image verification and atomic `human_decisions` checks as `review-apply`. Only empty rows in the matching group receive new approvals; `-` explicitly skips a row. Already populated TSV decisions are applied too. `--confirm-reviewed` means the human actually examined images, acknowledging possible OCR misidentification. Neither generating nor opening a report approves anything. Row order is matches, mismatches, unknown/refused and no OCR. Additional TSV columns (`observed`, match flag, refusal, suggestion key) are protected metadata and cannot be edited. Old snapshots lacking these columns remain readable.

The default prompt is `config/prompts/kanji_ocr.txt`; together with the answer-format instruction it reproduces the `structure-v1` instruction of the Structure study verbatim, and its SHA-256 is pinned by a test. The model receives only the prompt and a PNG: expected Unicode, paths, source filenames and article context are **not** sent. The answer is JSON with a sole string `observed`; exactly one supported Han/CJK character or `?` is accepted, without Unicode variant normalization. Extra fields, explanatory text, multi-character values, incomplete responses and a token-limit cut-off are recorded as refused suggestions.

Preparation uses exact saved source bytes/SHA. The bounded v7 kanji-validator decoder chooses the full representative GIF frame and skips frames without foreground (hence the `v7-representative-…` transform version: v6 rejected such a GIF entirely, and a previously cached preparation failure does not apply to the new key), and a second bounded decode materializes that composited frame; PNG is fully decoded. Alpha is composited onto white; Rust `image` resizes 2× with Lanczos3 and encodes one PNG. This is independent Rust Lanczos3, **not** promised pixel-identical to historical PowerShell/System.Drawing. Source: at most 8 MiB, 2048-pixel sides, 512 GIF frames, 32 MiB decode; output at most 4096×4096 and 8 MiB.

Suggestions live **only** at `.runtime/batches/<id>/ocr/suggestions.json`. The cache key and provenance include identity, source SHA, task, answer contract, transformation version, the composed prompt SHA and its source, backend with its mode and version, model profile and name, weights file SHA-256 (the digest for Ollama), projector SHA-256, load and generation parameters. Changing any of them yields a new key: an old record is never presented as the result of another model or prompt. SHA-256 of large GGUF files is cached in the state directory by path, size and modification time. Checking the cache needs a prepared backend: the managed server is started, and a cache hit then makes **zero** model requests. Model identity is re-read before and after each request: a model change during a scan is stored as the `model_changed_during_scan` refusal. A backend preparation failure is stored for each candidate with weights `unavailable`; a later successful start has another key. A cache hit requires a verifiable model identity (see `mode = "external"`). Retrying a refusal with the same settings requires `--rescan`. The runtime lock is released while the model works; on return current SHA, generation and candidate are checked and stale results discarded.

Records of the former Ollama-only path (`provider`, `endpoint`, `digest`, …) remain readable and are shown in `review-export` marked as the former Ollama-only path. Their keys differ from the new schema's keys, so they never become a cache hit for a new run.

Command JSON includes `ocr_summary` (`saved`, `cache_hits`, `chat_requests`) and an `inference` report with server identity, readiness time and stop proof. A stored model refusal returns code `3` and per-identity `issues` without modifying the candidate or its automatic verdict. Store-boundary errors use normal `AssetError` codes. Updating an optional model does not re-evaluate confirmed canonical resources.

#### Visual check of JPDB PNGs

```bash
cargo run --quiet --locked --bin pitch-assets -- --output json batch llm-review --batch-id my-pitch
cargo run --quiet --locked --bin pitch-assets -- --output json batch review-export --batch-id my-pitch
```

`batch llm-review` checks the saved PNGs of the current candidate rows of the review snapshot (`batch review-export`) without fetching from JPDB again. The model sees only the PNG, upscaled with Lanczos3 to about 448 px on the long side: the expected reading and surface are not sent. The answer is a `verdict` (`no_defect_observed`, `defect_observed`, `insufficient`), up to six observations with categories `cropping`, `incomplete_glyph`, `artifact`, `foreign_element`, `illegible_text`, `other`, and the read text `visible_text`. The model writes observations briefly in Russian (contract `jpdb-pitch-visual-v2`; records of the previous contract with English observations are read as they are). Code compares the read text with the expected reading after canonical Unicode composition (NFC: `か` + U+3099 equals `が`), folding katakana to hiragana and removing whitespace; compatibility normalization NFKC is not applied. Model strings are stored as received. An inconsistent or incomplete answer is stored as a refusal.

Suggestions live in the batch's `llm-review/suggestions.json` and are bound to identity, SHA, generation, item revision, request fingerprint, attempt and full model and prompt provenance. In `review-export` a row gets two protected columns — `llm:<verdict>` (or `llm_failure:<code>`) and the suggestion key — and an "LLM (suggestion, not a decision)" summary in HTML. The decision is still only the explicit first column; the `AUTOMATED_VERIFIED_ONLY` rule, `VERIFIED` and publication do not change. If a row changes during a request, the result is discarded (`stale_discarded`). Code `3` means model refusals or an unproven stop.

## Verification

Local acquisition and image tests use synthetic data/temporary directories **without network**. Live browser acquisition against websites is separate from the normal tests. Run workspace checks from the root:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

The root Cargo workspace remains the sole build entry point. Deck media are never used as test fixtures or data sources.
