# anki-repo

[Русский](README.md) · **English** · [简体中文](README.zh-CN.md)

<!-- i18n-source-git-blob: cf4c051e39d649969d08fa819a37cdfe7c7332e2 -->

`anki-repo` is a toolkit for deterministic analysis of CrowdAnki exports and strictly constrained edits to their contents in the `Anki-decks` repository. It gives agents fast, predictable access to the structure of `deck.json` without requiring them to read a multi-megabyte JSON file manually.

The `inspect`, `find`, `stats`, `validate`, `qa`, `review`, `review-check`, and `models` commands only read the export. `visual-report` reads exports and writes only a separate report directory. None of these read-only commands modifies an export: the interactive disk-maintenance prompt (see “Disk maintenance”) stores the user's preference in a private file outside the export. The separate `maintenance` command group does not access exports at all: it reports automatic-cleanup status and manages the user's `systemd --user` timer. Commands that modify an export are:

- `edit` changes **values** of existing fields in existing notes;
- `create` **adds** notes to an existing deck using an existing note model; under explicit domain rules, it may place verified kanji and pitch-accent images and declare their filenames in `media_files`;
- `retire` **adds a retirement tag** to an existing note;
- `migrate-media` moves references from legacy filenames to the domain's current canonical filenames. It is the only command permitted to change an existing note's media reference, and only after proving its semantics.

Separately from these four commands, `migrate-layout --apply` migrates the directory layout: it moves export directories from the legacy structure to the flat `decks/output/` layout, removes the layout's placeholder files, and deletes byte-identical source directories. Export contents are unchanged: `deck.json`, `media/`, and identities are moved byte for byte, with SHA-256 verification of every export before and after. This is a layout migration, not an edit to the export.

The toolkit does not physically delete notes: an obsolete note is marked with a tag and is removed by a person in Anki. No command creates or deletes decks, models, fields, or configurations, moves entities inside an export, or changes their identities (`guid`, `note_model_uuid`, `crowdanki_uuid`, `deck_config_uuid`). Mutating commands default to dry-run; writing requires explicit `--apply`. `create` can place only verified files authorized for the exact domain and field. `migrate-media` materializes a canonical file and removes its old counterpart only after proving the migration. The toolkit does not offer unrestricted writing to `media/`; `migrate-layout` moves the complete `media/` directory without altering its bytes.

`qa`, `review`, and `review-check` form an external card-content review pipeline: the tool deterministically identifies issues, packages compact evidence, accepts structured suggestions from an external agent, and converts them into a ready-to-use `edit` request. The tool itself **never calls an LLM API** or makes any network request: the external agent interprets content, while `anki-repo` remains the repository's deterministic boundary.

The “create and verify” workflow uses `models` to obtain the actual note-model field schema and evidence about field semantics, `create` to add a note under that schema, `retire` to tag obsolete notes, and `visual-report` to produce a static offline before/after report with card previews rendered from the real model templates. For migrated references, the narrowly scoped `migrate-media` first proves the identifier-to-legacy-filename match, inventories consumers, and then updates only proven references.

## Purpose

- One CLI invocation instead of manual `jq` or scripts: `inspect`, `find`, `stats`, `validate`, `qa`, `review`, `review-check`, `models`, `visual-report`, `edit`, `create`, `retire`, `migrate-media`, `migrate-layout`.
- Field-name resolution through `note_model_uuid` → `note_models[].flds[].ord` is centralized, so agents do not reimplement it when reading, editing, or creating notes.
- `models` returns not only field names but also evidence: actual values, counts of empty values, parsed template constructs, and an explicit list of constructs that cannot be evaluated statically. Agents do not infer a field's semantics solely from its name.
- Targeted edits cannot affect unrelated parts of the file: they require precondition checks, default dry-run, and a byte-level diff check before writing.
- Safe note creation: new field values have no media references by default; explicitly configured domain handlers allow only verified images from their domain and materialize them on `--apply`. Deck and model are addressed by identity, and the candidate is reparsed and validated before writing.
- Retirement without physical deletion: notes remain present and addressable; only their tags change.
- An offline visual report with actual card previews based on model templates, rather than a prose summary of a diff.
- An external review pipeline that avoids reading the entire multi-megabyte `deck.json`: deterministic selection, compact pages of 25 notes, and validation of agent proposals against the current export state.
- Stable machine-readable output: identical input yields identical JSON.
- Human-readable output by default, with a separate JSON mode for automation.

## Requirements

- **Linux only.** Verified reads depend on the Linux-only `asset-store` library, which does not build on other platforms.
- Rust toolchain using edition 2024. The minimum supported Rust version (MSRV) is declared once, in `[workspace.package] rust-version` of the root `Cargo.toml`. Workspace members inherit it (`edition.workspace = true`, `rust-version.workspace = true`), so `tools/anki-repo/Cargo.toml` does not duplicate the version. Verified using stable `rustc`/`cargo` and the declared MSRV.
- The root Cargo workspace (`/Cargo.toml`, containing `tools/anki-repo`) is the sole build entry point; `Cargo.lock` also resides at the repository root. Build and test from the root, not the package directory.
- No external services or networking: export operations use local files only, and no command calls LLM APIs or any other external service. The sole system-service interaction is the `maintenance` group: following an explicit user command or consent, it manages the user timer (`systemd --user`) of the `repository-maintenance` package and stores the user's choice in a private file. It neither reads nor changes an export.

## Build and run

From the repository root:

```bash
cargo build --release --workspace
./target/release/anki-repo --help

# or without installing
cargo run --quiet --bin anki-repo -- inspect decks/output/Words__N3
```

Export paths may be relative to the current directory or absolute.

### Disk maintenance

`anki-repo maintenance status` reports actual automatic-cleanup state, the size of managed artifacts, measurement completeness, the last attempt, and structured diagnostic events. `anki-repo --json maintenance status` makes the same data available to agents under `result`; status queries never delete anything.

On the first eligible interactive CLI invocation, the tool may offer `[y/N]` to enable the user timer. Accepted affirmative answers are `y`, `yes`, `д`, and `да`; negative answers are `n`, `no`, `н`, `нет`, or empty input (the `[y/N]` default). An unrecognized response is not saved: the preference remains unset and the prompt will appear again. Measurements use the repository containing the export: its root is resolved from the export's location (nearest ancestor with `.git`), not the process's current directory. Otherwise a prompt could measure another project's `target` and install a timer that cleans someone else's `target` daily. An explicit `ANKI_REPOSITORY_ROOT` takes precedence over the export location; no prompt is shown if the export is outside a Cargo workspace. After refusal, background size checks happen no more than once a day: the interval is consumed by a check itself, not only by displaying a warning. Prompts are suppressed for `--json`, redirected streams, CI (detected from `CI`, not merely from stream properties because some CI systems allocate a pseudo-terminal), and `--help`/`--version`. They are also suppressed when the action cannot be performed: for example, if the user's systemd manager is unavailable or the timer is enabled without verified ownership by the checkout. Explicit acceptance or refusal is available via:

```bash
anki-repo maintenance status
anki-repo maintenance enable
anki-repo maintenance disable
```

`maintenance disable` does not report success until disablement has been verified: if the checkout binding is unverified, it retains unit files and returns `write_failed`. For a verified checkout, autostart, timer activity, and service shutdown are checked under one lock; `no` is not recorded if any check fails. Thus, saved `no` cannot mean “automatic cleanup is off” while cleanup is still active.

The policy and cleanup implementation belong to `repository-maintenance`. See [`repository-maintenance` documentation](../repository-maintenance/README.en.md): `/tmp` and other users' data are not part of the measured size of managed artifacts.

## Commands

Throughout the following examples, `EXPORT` is the export directory, such as `decks/output/Words__N3`. Provide the directory, not the `deck.json` file itself; passing the file path produces a clear error.

Example paths and field names are illustrative. The toolkit supports arbitrary CrowdAnki exports and does not know any particular model's field names: `Слово`, `Часть речи`, `Значение`, other field names, deck levels, and paths come from current user decks only as examples and are not contracts. Callers always specify a field name, which is checked against the actual `note_models[].flds[].name` in the target export.

### Restricted `decks/input` area

`decks/input/` is a closed area reserved for a future trusted importer. No command reads, traverses, or changes it. Any path addressing `decks/input` (an export directory, `--before`/`--after`, `--out`, or `--root`) is rejected with `invalid_request` and `details.reason = input_area_forbidden`; the error message identifies the restricted area and its purpose. There is no bypass flag: this is an API boundary, not a filesystem restriction. The check is part of the export-directory contract, so library calls also respect it, not just CLI calls. Use `decks/output/` for working decks.

### Layout boundaries

`decks/` contains local data, is entirely ignored by Git, and does not exist in a fresh clone. `decks/output/` is created as needed (including by `migrate-layout`); its absence is normal, not an error. Only code, documentation, and self-contained fixtures outside `decks/` are tracked.

### `inspect`

A compact overview of export structure: root deck, number of deck nodes, total notes, note models with fields in `ord` order, deck configurations, media summary, and note distribution across deck paths.

```bash
anki-repo inspect decks/output/Words__N1
anki-repo inspect decks/output/Words__N1 --verbose
```

`--verbose` adds UUIDs, the parent-to-child deck tree, card templates, `guid` statistics, and a limited sample of diagnostics (including missing and undeclared media filenames). In JSON mode, the details appear in `result.verbose`, and only with `--verbose`.

### `find`

Search notes by stable criteria. Exactly one criterion is required: `--guid`, or `--field` together with `--value`.

```bash
# search by guid: identity lookup
anki-repo find decks/output/Words__N1 --guid 'D!TAYVuHi,'

# search by raw field value: contains a substring
anki-repo find decks/output/Words__N1 --field Слово --value 偶然

# generic search over any field name: substring and exact match
anki-repo find decks/output/Words__N2 --field 'Часть речи' --value Существительное
anki-repo find decks/output/Words__N1 --field 'Часть речи' --value Существительное --match exact
```

Behavior:

- `--field` specifies a field name. `--match` accepts `contains` (default) or `exact` and is valid only with `--field`. The field name is checked against actual `note_models[].flds[].name`; an unknown field is the domain error `unknown_field` (exit code 3), not an empty result.
- There is no separate “word” entity selector: callers always provide a field name, so `--field X --value Y --match contains` does exactly what the now-removed `--word` flag used to do (see “Public contract changes”).
- `--deck` limits results to a deck and all of its descendants.
- `--limit` (default 20, maximum 500) limits output, not the search itself: `matched_total` counts every match; `returned` and `truncated` show what was included in the response.
- Matching is case-sensitive on raw field values, with no HTML normalization.

### `stats`

Aggregated structural statistics: note counts per deck and model, deck-configuration usage, empty/nonempty fields, and media-file totals.

```bash
anki-repo stats decks/output/Words__N3
anki-repo stats decks/output/Words__N3 --group-by 'Часть речи' --top 10
```

- `--group-by` counts the distribution of raw values for a specified field; `--top` (default 20, maximum 500) limits output, while `distinct_values` and `truncated` describe the full distribution.
- Distribution columns are sorted by decreasing count and then by value.

### `validate`

Deterministic structural-integrity validation of a single export.

```bash
anki-repo validate decks/output/Words__N1
anki-repo --json validate decks/output/Words__N1
```

`valid` is `true` when no `ERROR`-level problems are found. Warnings (`WARNING`) and informational entries (`INFO`) do not make the export invalid. Path problems (missing directory or `deck.json`) are domain errors with exit code 3; content problems (including invalid JSON) are reported with exit code 6.

Issue severities and codes:

| Severity | Codes |
|---|---|
| `ERROR` | `invalid_json`, `root_not_deck`, `schema_invalid`, `node_not_deck`, `duplicate_note_model_uuid`, `duplicate_deck_config_uuid`, `note_model_identity_missing`, `deck_config_identity_missing`, `note_model_uuid_missing`, `note_model_unresolved`, `deck_config_unresolved`, `note_fields_count_mismatch`, `note_field_value_not_string`, `note_guid_missing`, `duplicate_note_guid`, `field_name_duplicate`, `field_ord_invalid`, `field_ord_negative`, `field_ord_duplicate`, `field_ord_out_of_range`, `field_ord_gap`, `template_field_unresolved` |
| `WARNING` | `media_reference_undeclared`, `media_files_duplicate`, `media_physical_missing`, `media_dir_missing`, `media_name_not_basename`, `template_construct_unchecked`, `deck_config_uuid_missing`, `deck_name_missing`, `conflicting_note_model_definition`, `conflicting_deck_config_definition`, `template_ord_invalid`, `template_ord_duplicate` |
| `INFO` | `export_summary`, `media_physical_unused`, `note_model_unused`, `deck_config_unused`, `empty_field_values`, `issues_truncated` |

Important interpretation rules:

- A physically missing media file is a `WARNING`, not an `ERROR`: the repository's media-storage policy is not separately defined.
- Media filenames are compared as basenames only; paths from `media_files` are never used for filesystem access.
- Repeated declarations of an identical model/configuration (common in nested CrowdAnki nodes) are not errors. Conflicting declarations under the same UUID are a `WARNING`. Only semantic parts are compared: for models, `name` plus `flds` entries (`name` and `ord`); for configurations, `name`. Differences in `tmpls`, `css`, and scheduling parameters are not compared here.
- The limited template parser checks simple references such as `{{Поле}}` and `{{tts ja_JP:Поле}}`: a construct counts as known if at least one segment matches a model field name. Unclosed `{{`, and filter constructs in which no known field name occurs, receive `template_construct_unchecked` (`WARNING`) to avoid false errors. Matching `{{#X}}` with `{{/X}}` and other template logic is not checked.
- The count of issues for a given code is capped; `issues_truncated` reports how many entries were omitted.
- Empty `guid` yields `note_guid_missing` (`ERROR`), is not separately reported as a duplicate, and is excluded from the `guid` index. Consequently, `find --guid ''` returns no match.

### `qa`

Deterministic card-content findings. Rules inspect only the export and do not depend on the repository's current state.

```bash
anki-repo qa decks/output/Words__N1
anki-repo qa decks/output/Words__N3 --code empty_field_value --max-per-code 5
anki-repo --json qa decks/output/Words__N5 > findings.json
```

Rule registry (findings are ordered by registry order, which also determines the order of `rules` in JSON):

| Code | Severity | Detection |
|---|---|---|
| `empty_field_value` | `warning` | a string field value equals `""` |
| `leading_whitespace` | `warning` | a value begins with whitespace |
| `trailing_whitespace` | `warning` | a value ends with whitespace |
| `forbidden_white_span` | `error` | a `<span>` with white text color in its `style` |
| `duplicate_note_content` | `warning` | multiple notes of the same model have completely identical content |

Behavior:

- `--code` may be repeated to restrict results to selected codes. An unknown code raises domain error `unknown_qa_code` (exit code 3), not an empty result: a typo must not appear to mean “no findings.”
- `--max-per-code` (default 20, maximum 200) limits findings of each code in the output, not the actual counts. `findings_total` and `by_code` describe all findings of the selected codes, while `truncated` indicates whether anything was omitted. `rules[].findings` is not affected by filters and always counts the whole export under all rules.
- Severity `error` describes the defect's significance to readers, not command failure: `qa` still returns exit code `0` even if it finds `error`-severity issues.
- A field consisting entirely of whitespace produces both `leading_whitespace` and `trailing_whitespace`.
- An empty value is not the same as a missing one: both `""` and a whitespace-only string receive findings; `null` and nonstring values are not treated as empty fields (those are handled by `validate`).
- `duplicate_note_content` compares complete positional arrays of raw field values for notes of the same model without normalizing HTML or whitespace. It does not identify a “primary” field: primary-field semantics belong to individual note models, not to the CrowdAnki format, so the toolkit has no correct universal semantics for it. Callers can explicitly group by field value using `--field` and `--value` in `find` or `review`.
- The registry contains exactly these five codes. `duplicate_primary_field` was deliberately removed, along with the assumption that a field named `Слово` is universally primary (see “Public contract changes”). Asking for a removed code returns `unknown_qa_code`, not an empty result.
- Rules are independent of model and field names: the same field values under renamed fields or a different model produce the same findings.
- Findings never make an export structurally invalid or block `edit`. The `error | warning | info` scale describes content quality independently of `validate`; a QA `error` is not a structural export error.
- Findings may concern notes that cannot be addressed: their model cannot be resolved, their `guid` is missing, or the `guid` is not unique. These are **marked** `addressable: false` and counted in `unaddressable_findings` rather than silently omitted. `validate` identifies the structural reason; `qa` does not present an unexecutable address as an ordinary `edit` target.
- Duplicate findings describe the whole group: `group_size` gives its size, while `related_note_indices` and `related_guids` list group members in the original finding's canonical order, including the finding owner (unlike `review`'s `related`, which excludes the owner). The list is capped at 20 (`MAX_RELATED_NOTES`); `related_truncated: true` indicates truncation, never silently hiding members.

Every finding identifies the note's export position (`note_index`), `guid`, deck path, model, and field name plus `ord`. Its `evidence` contains bounded samples: value length, truncated excerpts, and rule-specific details (such as `boundary` for whitespace rules and `first_tag` for `forbidden_white_span`). Group information is not duplicated inside `evidence`; it is stored in `group_size`, `related_note_indices`, and `related_guids`.

### `review`

Produces a bounded, compact set of notes for external review. Unlike `qa`, which reports problems, `review` supplies the material to examine: note fields in `ord` order and previously detected QA findings for each note.

```bash
# select notes for a detected problem
anki-repo review decks/output/Words__N1 --qa-code empty_field_value --limit 25

# next page
anki-repo review decks/output/Words__N1 --qa-code empty_field_value --offset 25

# selection by content and exact lookup
anki-repo review decks/output/Words__N2 --field 'Часть речи' --value Существительное
anki-repo review decks/output/Words__N1 --guid 'D!TAYVuHi,'
```

Exactly one selection criterion is required:

| Criterion | Meaning |
|---|---|
| `--all` | all notes in the selected scope |
| `--guid` | one note by `guid` |
| `--field` + `--value` | match a named field; `--match contains` (default) or `exact` |
| `--qa-code` | notes with a finding of the given QA code |

`--deck` narrows the scope to a deck and its descendants; `--offset` skips selected notes; `--limit` (default 25, maximum 200) sets page size. The response includes `total_selected`, `returned`, `truncated`, and `next_offset`; `next_offset` is `null` on the last page. Selection and field-name resolution use the same implementation as `find`, so unknown fields, decks, or GUIDs yield the same errors and exit codes.

`review` is a read-and-paginate command, not a search that must always find something. An empty page (no filter matches or `--offset` beyond the end) succeeds with `returned: 0` and exit code `0`, otherwise pagination could not terminate. The exception is `--guid`, which addresses a specific note: if absent it returns `not_found` (code 4), and if ambiguous, `ambiguous` (code 5).

Package contents:

- Notes lacking an unambiguous addressable `guid` are excluded: notes with no `guid`, an empty `guid`, or a repeated `guid` (all occurrences of duplicates are excluded). Their count appears in `excluded_unaddressable`. `notes_total` still describes the whole export, not just the page; even an empty page explains excluded entries through this counter.
- For duplicate rules, `--qa-code` selects group members, not only the finding owner; otherwise the `qa → review` workflow would lose the other notes. The owner carries the actual finding (`group_size` and `related`, listing other members), while each member carries `group_membership` referring back to the owner (`code`, `owner_note_index`, `owner_guid`, `group_size`). The member list comes from the domain result and is capped at `MAX_RELATED_NOTES` (20). In a group of 30 notes the package contains the owner and 20 members; `related_truncated: true` on the owner's finding signals an incomplete list. It is incorrect to promise *all* members beyond the limit; retrieve others via `review --all` or `--field`.
- `--all`, `--guid`, `--field`, and `--qa-code` do not otherwise expand groups; they select exactly what their criteria specify.

### `review-check`

Validates proposals from an external agent against the **current** export state. The command is read-only: neither `deck.json` nor any auxiliary file is written.

```bash
# validate proposals and get a ready request for `edit`
anki-repo --json review-check decks/output/Words__N1 \
  --proposals proposals.json | jq .result.edit_request > request.json

# the same with stdin
anki-repo review-check decks/output/Words__N1 --proposals - < proposals.json

# the ready request goes through the existing write boundary
anki-repo edit decks/output/Words__N1 --request request.json
anki-repo edit decks/output/Words__N1 --request request.json --apply
anki-repo validate decks/output/Words__N1
```

Its proposal document has its own versioned schema, distinct from an `edit` request:

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

`proposal_id` and `reason` are part of the proposal contract. The former becomes `edit_id` in the generated request; the latter remains review metadata (truncated safely in the report and never copied to `edit_request`). Unknown keys are prohibited: proposals form a separate public input schema and must not accept keys from other formats; errors use the vocabulary of the proposal contract. `review-check` reuses `edit`'s internal parsing and target resolution, so the emitted `edit_request` can be passed directly to `edit`.

Proposal statuses:

| Status | Condition | Included in `edit_request` |
|---|---|---|
| `valid` | target resolves; current value equals `expected` and differs from `replacement` | yes |
| `already_correct` | current value equals both `expected` and `replacement` | no |
| `already_applied` | current value already equals `replacement` | no |
| `conflict` | current value matches neither `expected` nor `replacement` | no |
| `invalid` | target cannot be resolved: note or field missing, field not editable as a string, or ambiguous `guid` | no |

For an `invalid` proposal, `problem` specifies the actual cause: `note_not_found`, `unknown_field`, `ambiguous_guid`, or `field_not_resolvable`. For the same target, `edit` represents the last two conditions as a generic `internal_error` and stops the entire operation. `review-check` deliberately does not do that: an unresolved target is an unusable proposal in the report, not an internal tool failure, and other proposals are still checked.

Result and exit code:

| `outcome` | Condition | Exit code |
|---|---|---|
| `ok` | neither `invalid` nor `conflict` proposals exist | `0` |
| `stale` | at least one `conflict` indicates the agent's view is outdated | `7` |
| `invalid` | at least one `invalid`; ambiguous `guid` takes precedence (5), otherwise missing note (4), otherwise 3 | `5` / `4` / `3` |

`edit_request` is emitted only when `outcome: ok`, at least one proposal is `valid`, and the source is mutable (`source_editable: true`). Empty `edits` would violate the `edit` contract, and constructing a request from a partially invalid proposal document would silently edit “whatever worked.” The `expected` value in the emitted request comes directly from the agent proposal; this is safe because a `valid` status is assigned only after verifying `expected` against the actual current value.

Accordingly, `review-check` also checks the source itself using exactly the same conditions that stop `edit` (one shared function, not an independent implementation):

| Blocker | Code in `source_blockers` | Exit code |
|---|---|---|
| `deck.json` is not in canonical form | `source_not_canonical` | `3` |
| export contains any `ERROR` | `export_invalid` | `6` |
| ambiguous model-field ordering (`WARNING conflicting_note_model_definition`) | `export_not_mutable` | `6` |

The proposal report is still printed in these cases: its statuses, counters, and `proposals` help the agent, while `source_editable: false` and `source_blockers` explain why no request was generated. The exit code identifies the source blocker: a noneditable source makes every request impossible, so `ok` cannot be mistaken for readiness to write.

`stale` and `invalid` are validation outcomes rather than command failures, so complete reports are produced in both ordinary and JSON modes (under `result`, not `error` in JSON). Only the exit code indicates the nonzero outcome, much as `validate` does for exit code 6.

The report is bounded (`proposals` at most 50 entries, with `proposals_truncated`), but `counts` and `edit_request` are not truncated. The cap applies only to display, and human-readable output explicitly says so rather than promising a complete list in `--json`.

Document-wide problems remain domain errors with exit code 3: if a proposal document cannot be parsed, nothing can be checked. The proposal schema returns `invalid_request` with these `details.reason` values: `malformed_proposals` (malformed or foreign shape), `unsupported_schema_version`, `empty_proposals`, `too_many_proposals` (over 20,000), `proposals_too_large` (over 8 MiB), and `duplicate_proposal_id`. Checks shared with `edit` requests (`edit::validate_request`) add `empty_guid` and `empty_field`; a repeated (`guid`, field) pair is a separate `duplicate_edit_target` error, not a `reason` inside `invalid_request`.

### `models`

Displays the actual field schema for models in the selected scope: field names, positions in notes' `fields`, observed content, and the template constructs that previews understand. This supplies note creation and visual reports: field names are properties of a specific export, not global contracts.

```bash
EXPORT=decks/output/Words__N1

# note models of the export's root deck (default selector: preorder 0)
anki-repo models "$EXPORT"

# a deck by name, by identity, or by position in the traversal
anki-repo models "$EXPORT" --deck "Words::N1"
anki-repo models "$EXPORT" --deck-uuid 00000000-0000-0000-0000-000000000000
anki-repo models "$EXPORT" --deck-preorder 1

# how many sample values to show per field (1..10, default 3)
anki-repo models "$EXPORT" --sample-limit 5
```

Selectors can be combined. Every specified selector must identify the same deck node; disagreement is `deck_identity_mismatch` (exit code 3), not a reason to arbitrarily choose one. An empty selection defaults to the export's root deck.

For each model in the scope, the output includes:

- identity (`crowdanki_uuid`), name, kind (`standard` or `cloze`), model type, and whether declared in the deck subtree or inherited;
- note counts for the model in the current deck and entire subtree;
- fields **in `ord` order**. `ord` is the index in a note's `fields` array, so fields declared in a different order are displayed in their actual usage order, not declaration order;
- for each field: its description if declared in the model, count of empty values in the deck, and up to `--sample-limit` nonempty examples with donor-note `guid`;
- for each template: `ord`, name, used ordinary and special fields, and an explicit list of constructs previews cannot statically evaluate (`{{cloze:…}}`, other filters, `<script>`, malformed templates);
- `schema_problems`: reasons why the field schema cannot be used to construct values (missing, duplicate, or nonnumeric `ord`). The model is still included with evidence, but `create` refuses it.

An example value is evidence, not a format guarantee. It comes from a real note and is length-limited. This allows the agent to recognize a field's meaning (for example, that `Ударение` contains an `<img>`, not text).

`models` has no write checks because it never writes anything.

### `create`

Adds new notes to an **existing** deck using an **existing** note model. It does not create decks, models, or fields: the export itself defines the schema.

```bash
EXPORT=decks/output/Words__N1
REQUEST=create.json

# 1. dry-run first: see what the result will be
anki-repo create "$EXPORT" --request "$REQUEST"

# 2. write that very same note
anki-repo create "$EXPORT" --request "$REQUEST" --apply

# 3. record the result in a portable form
anki-repo create "$EXPORT" --request "$REQUEST" --apply --emit-resolved resolved.json

# the request can be read from stdin
cat "$REQUEST" | anki-repo create "$EXPORT" --request -
```

#### Request format

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

- `note_id` is a caller-defined label returned in reports and errors to correlate outcomes with requests. It is not stored in the export.
- `deck` selects a deck using `path`, `crowdanki_uuid`, and/or `preorder`; all specified selectors must resolve to the same node. Writing to a deck without `crowdanki_uuid` is rejected as `unresolved_deck_identity`.
- `model` is `{"mode": "auto"}` (default) or `{"mode": "explicit", "crowdanki_uuid": "…"}` / `{"mode": "explicit", "name": "…"}`. In `auto` mode, compatibility is determined from the exact set of requested field names; a typo may therefore produce `unknown_model`, with an error listing the actual requested names. Ambiguity (`ambiguous_model`) is an exit-code-5 error, never a reason to pick the first model. The complete `auto` selection rules, with no guesses or popularity priorities, are:

  1. Candidates are export models whose field schemas are fully usable (broken schemas are rejected as `model_schema_unusable`) and whose set of field names matches the request exactly, with no missing or extra fields. Invalid schemas are rejected first: if a matching model was discarded because of its schema, the error is still `unknown_model`, but `details.rejected` identifies every excluded model and its `schema_problems`. “No model” and “existing but unusable model” must not be conflated.
  2. If any remaining model has notes in the target deck's subtree, only those models remain candidates.
  3. Otherwise (such as in an empty deck), all compatible models from the export remain. This is the “single compatible schema” rule, explicitly identified in `model_evidence` (“no notes of compatible models in the target subtree…”). Multiple compatible models require rejection: mere proximity of decks does not justify a choice.
  4. Exactly one candidate is selected; zero means `unknown_model`, and more than one means `ambiguous_model`, with candidate names and their note counts in the subtree.

  Every outcome contains `model_evidence` explaining the rule used and how target-deck notes affected it. When an exact model is required, specify `mode = explicit`: this alone guarantees a particular model among several compatible schemas.
- `fields` maps **model field names** to values. All model fields must be present; the empty string is a valid empty value. Key order is irrelevant; values are stored in note `fields` by `ord`.
- `guid` is optional. If absent, it is generated (`guid_generated: true` in the report) and included in `--emit-resolved`, allowing idempotent retries.

A successful `--emit-resolved` invocation produces **two** outputs: the resolved request file and a report on `stdout`. JSON mode does not duplicate the resolved request in its envelope (a request with 20,000 notes could be enormous); the portable artifact is the file, not command output.

Write order is contractual, not incidental:

1. Resolve and plan the complete request, including every note, `guid`, and model.
2. Check the artifact path against normalized `deck.json`: pointing `--emit-resolved` at `deck.json`, a symlink to it, or a path differing only in spelling (`./`, `..`, or case on a case-insensitive filesystem) produces `invalid_request` with `reason = emit_resolved_aliases_source` **before** modifying anything.
3. Write the complete artifact atomically, using a temporary sibling and `rename`.
4. Only then publish `deck.json`.
5. If publication fails, no note is created but the artifact remains with every `guid`. A retry using it either adds exactly one note or reports `already_applied`; no silent duplicate appears.

Reversing this order (`deck.json` first) would return an artifact-write failure after successfully changing the export and losing the generated `guid`, so it is prohibited. Rolling back after `apply` is likewise rejected because a partial failure could otherwise destroy another writer's edits.

Even in dry-run, `--emit-resolved` writes the artifact without changing `deck.json`: that file is an independent command output, not a side effect of changing the export. Its parent directory must already exist; `create` will not create arbitrary directories and instead returns `write_failed`. Without `--emit-resolved`, behavior is unchanged and every `guid` still appears in the report.
- `tags` is an optional list of new-note tags. Tags use the same validation as `retire` and are checked case-insensitively, as Anki does: `["Дом", "дом"]` is a duplicate and causes `duplicate_tag`, rather than adding the same tag twice.

#### Three possible outcomes per note

| Status | Meaning |
|---|---|
| `created` | no note with this `guid` existed; the note was added or would be added in dry-run |
| `dry_run` | no write was requested; reports what would be written |
| `already_applied` | a note with this `guid` already exists and matches the request; the file is not rewritten |

If a note already has the `guid` but its **contents differ**, `guid_conflict` (exit code 6) is returned, not a silent update. `create` never edits existing notes; use `edit` for that.

#### Guarantees

- New field values have no media references by default (`media_forbidden`, exit code 3). The only exception is an explicitly configured domain handler described below, which permits verified `img/src` references to that domain's images.

  References include:

  - `[sound:name]` in any spelling, including `[sound: name]` or `[sound:path/name]`;
  - media-bearing HTML attributes identified by the shared `ADDRESS_ATTRIBUTES` table: `src` on `img`, `audio`, `video`, `source`, `track`, `input`, `embed`, `iframe`, `frame`, `script`; `srcset` on `img` and `source`; `poster` on `video`; `data` on `object`; `href`/`xlink:href` on SVG `image` and `feImage`; legacy `background` on `body`, `table`, `thead`, `tbody`, `tfoot`, `tr`, `td`, and `th`;
  - a media-bearing attribute in an unfinished tag such as `<img src="a.png"` without `>`: a field is not a complete document, and Anki concatenates it with a template; an unclosed tag may become a real file request there. Unrecognized markup therefore fails closed. Navigational attributes such as `href` do not request files, so an unfinished navigation link is not treated as a media reference. The report trust boundary also rejects unfinished constructs, but under a separate rule;
  - CSS addresses in field values: `url(…)` in `style` attributes and `<style>`, `image()`/`image-set()`, `src` in `@font-face`, and `@import`. CSS addresses are also file references, and `@import` can load external stylesheets. Address recognition is owned by `report::css`, not reimplemented in this gate. Attribute text is HTML-decoded before CSS parsing: a browser treats `url&#40;a.png&#41;` as a URL. Unknown/unparseable address constructs are rejected rather than overlooked.

  Tag or attribute capitalization and spacing do not matter: `<IMG SRC = a.png>`, `<img srcset="…">`, and `<img src="a.png">` are media references. `<img alt="a.png">` and `<функция src=x>` are not, while an unclosed quote around an actual media attribute still indicates an address.

  `<div src="не-адрес">` is not a reference either: `div` has no media-bearing `src`, so the browser requests no file. The address table therefore distinguishes elements and attributes, while navigation attributes (`href`, `xlink:href`, `action`, `formaction`, `ping`, `manifest`, `cite`, `longdesc`, `usemap`, `archive`, `classid`, `codebase`) are identified for all elements. Navigation does not fetch a local media file but can leave the offline report, so incomplete navigation detection could expose live external addresses. SVG `use` with `href` deliberately remains navigational because it can point to an in-document fragment (`#иконка`), which requires no file. Shared `htmlscan` parsing and this address table are used by both the creation gate and `visual-report`; CSS recognition similarly delegates to `report::css`.
- New note objects contain exactly `__type__`, `fields`, `guid`, `note_model_uuid`, and `tags`; no foreign keys are injected.
- The note is appended **at the end** of the target node's `notes`; existing notes are neither moved nor rewritten. The single exception to the broader “existing notes are immutable” rule is proven legacy-media migration via [`migrate-media`](#migrate-media), which is a separate command; `create` itself does not touch them.
- The candidate is reparsed and validated before writing (`candidate_reparsed`, `appended_notes_verified`, `only_notes_and_media_files_appended`, `guids_resolved_without_conflict`). `validation` compares issue codes before and after; no write occurs if the candidate introduces a new error.
- Canonical `deck.json` is required; writes are atomic and protected against concurrent edits, as with `edit`.

#### Explicit media authorization rules

`create` finds the export's nearest ancestor containing `.git` (directory or Git worktree marker) and loads `.anki-repo/create.yaml` from that repository. The process's current directory does not influence policy selection. If the policy file does not exist, the legacy media-free behavior remains. `--create-config PATH`, `--asset-store PATH`, and `--pitch-asset-store PATH` can override the policy file and each domain's store; a nonexistent explicitly specified config is an error. Defaults are `.asset-store/kanji` and `.asset-store/pitch-accent` in the same repository. No asset store is opened if there are no media references; unreferenced domains need no store.

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

A model is identified by its actual `crowdanki_uuid`, and a field by its exact `models` name. Model display names do not select processors. Policies for other models are permitted; a nonexistent field in the selected model triggers `config_stale_field`. Unsupported versions, unknown keys/types, duplicate keys, model UUIDs, field names, processors, or ambiguous YAML structures cause complete rejection. The config chooses processors, not which images exist. Its `note_models[].crowdanki_uuid` refers to a specific currently used note model; that is today's data description, not a code contract.

`processors` is a nonempty ordered chain, executed in YAML list order. Two types exist: `kanji_assets` and `pitch_accent`. An unknown type is a configuration error; repeating a type within the same list fails before execution because each entry contains only `type`, with no additional configuration to distinguish instances. The composition boundary lets each handler claim only links it recognizes. Every media reference must have exactly one owner; multiple claims give `media_reference_conflict`, and no claims give `media_reference_unclaimed`. Configuring processors does not permit arbitrary additional references in that field. Unconfigured models or fields still have none.

The request supplies the field HTML. A processor accepts only complete `img/src` elements parsed by the shared HTML parser. `kanji_assets` accepts a basename of one supported Han/CJK character plus exact `.gif` or `.png`, such as `<IMG SRC = '漢.gif'>`. `pitch_accent` requires `<surface>.pitch.png`, exactly the canonical consumer filename for that surface. URLs, directory paths, query strings, fragments, `[sound:...]`, CSS, `srcset`, and all other references remain forbidden. The processor neither creates HTML nor changes extensions; a filename mismatch reports the available canonical name.

`asset-store::AssetStore::read_verified_with_policy` returns domain-verified bytes and a record. Creation requires canonical `VERIFIED` state and final trust attested by the asset owner: either the current domain-validator decision or (where supported) explicit human approval of the exact SHA-256 after a correct full image decode. `kanji_assets` allows human approval while preserving automatic evidence; an explicit human rejection revokes trust in the current bytes. `pitch_accent` trusts only current automatic `VERIFIED` from `jpdb-pitch-accent-render` for that exact SHA; human approval cannot override it, and evidence from older validator versions is insufficient even with the same SHA. Checksum, size, format, and a file descriptor opened without following symlinks and confined to the asset-store boundary must also match. `.runtime` is not used. Resolving a reference never downloads images or accesses the network; missing images block creation and require separate retrieval through `kanji-assets` or `pitch-assets`.

The flat CrowdAnki `media/` namespace is shared by all domains. If two distinct identities or processors map different bytes to the same filename, `create` rejects the entire operation with `media_filename_collision`; processor order does not resolve it. The same SHA under one filename is not a collision. An unknown pinned `media_assets` `namespace` produces `media_domain_unknown`: processor routing uses `identity.namespace`, not the filename.

The one known namespace collision comes from an older workflow that named pitch images `<surface>.png`. For a single-character word with a kanji fallback image, that old name collides with the character image's canonical filename. The resolution is not overwriting or manual `deck.json` edits, but the separate [`migrate-media`](#migrate-media) command, which updates references to the current canonical filename and releases the old name. `create` can then place the fallback kanji image under the freed filename.

Dry-run performs all preflight checks without writing JSON, `media/`, the policy, or any store. JSON `media.references` shows at most 50 entries (note index, model UUID, field, filename, identity), plus complete `references_total` and `references_truncated`; `assets` describes identities, `canonical_filename`, `sha256`, relative `destination`, and `action: copy|reuse`, with `media_files_added`, `mutations_planned`, and `mutations_applied`. An occupied target produces an error with `action: conflict`. `outcomes[].processor_fields` lists configured fields even when a request has no media references; `media_references` can nevertheless be nonzero. Human-readable output summarizes the media plan; JSON contains the full plan.

A resolved v1 request additionally contains `media_assets`: a sorted, machine-independent list of asset identities, filenames, and SHA-256 hashes. The list may be empty and still pins the asset set. It can be passed back through `--request`; changing, removing, or renaming a pinned image causes `stale_pinned_asset`. Legacy v1 requests without `media_assets` remain compatible and do not pin the set. To make a fresh selection, use the original unpinned request. `--emit-resolved` is written before media modifications and cannot overwrite the source, `media/`, configuration, or asset store.

With `--apply`, the request, rules, assets, all destinations, and candidate are checked before the first export mutation. One durable export-directory lock serializes `edit`, `retire`, and `create`; a changed source is rejected before copying any file. Verified bytes first go to a temporary file and are published via hard link without replacing an existing destination. If another process created the destination with identical bytes, it is reused; differing bytes produce `conflict`. Symlinks and nonregular files are rejected. User `media/` is not fully inventoried to select images.

Only after media placement does `deck.json` get published. Filenames are appended to root `media_files` if no node already declares them; existing declarations and ordering are preserved. Checks verify exact new notes, then exact `media_files` additions by comparing the entire JSON. `only_notes_and_media_files_appended` and `media_assets_verified` attest to the checks; `only_notes_appended` is true only without new declarations. `media_references_absent` reflects actual absence of media links.

After a partial failure, `deck.json` is not published; valid media files already copied remain available for safe reuse. Retrying the same request restores missing files or declarations without duplicating notes. `already_applied` pertains to the note, while `media.mutations_applied` covers media restoration. Once repaired, another identical run is a no-op. The shared lock coordinates cooperating writers; external writers must follow the protocol too.

New reasons appear under `error.details.reason`, with evidence under `error.details.evidence`; localized Russian message text is not API:

| `reason` | Error / exit code | Meaning |
|---|---|---|
| `config_absent`, `processor_not_enabled` | `media_forbidden` / 3 | Field has no explicit authorization |
| `config_invalid`, `config_unsupported`, `config_stale_field`, `config_context` | `invalid_request` / 3 | Policy is invalid/stale, or export context could not be resolved |
| `media_reference_unclaimed` | `media_forbidden` / 3 | No handler accepts the reference |
| `media_reference_conflict` | `media_forbidden` / 3 | Multiple handlers claim the same reference |
| `media_domain_unknown` | `media_forbidden` / 3 | Pinned identity `namespace` is unknown to configured handlers |
| `kanji_asset_missing`, `pitch_asset_missing` | `invalid_request` / 3 | Verified domain image or store is absent |
| `asset_integrity_invalid` | `invalid_request` / 3 | State, decision, checksum, format, or boundary mismatch; `asset_code` identifies the cause |
| `canonical_filename_mismatch`, `stale_pinned_asset` | `expected_mismatch` / 7 | Wrong canonical filename, or pinned image changed, vanished, or was renamed |
| `media_filename_collision` | `expected_mismatch` / 7 | Distinct identities/domains yield different bytes at one filename |
| `destination_media_conflict` | `expected_mismatch` / 7 | Destination occupied by other bytes or an unsafe object |
| `legacy_media_changed` | `expected_mismatch` / 7 | Old file appeared, disappeared, or changed after planning migration |
| `asset_store_boundary`, `media_declaration_conflict`, `emit_resolved_protected_path` | `invalid_request` / 3 | Path boundary violation or media declaration conflict |
| `media_materialization_failed`, `media_disappeared`, `media_directory_changed` | `write_failed` / 8 | Placement or final media verification failed |
| `source_modified`, `source_missing`, `export_directory_changed` | `source_changed` / 7 | Source changed or export directory replaced under shared lock |

If publishing JSON fails, the existing `write_failed` / exit 8 and `details.operation` remain; verified extra media files can safely be reused on retry.

#### Report

| Key | Meaning |
|---|---|
| `export_dir`, `deck_json` | Export and `deck.json` paths |
| `dry_run`, `applied` | Mode and whether the file was replaced |
| `source_bytes`, `candidate_bytes`, `byte_delta` | Sizes and difference |
| `notes_total`, `notes_created`, `notes_already_applied` | Counts by status |
| `outcomes` | Per-note outcomes, up to 50, with `outcomes[].processor_fields` and `outcomes_truncated` |
| `decks_touched` | Touched decks, prior note count, and additions |
| `validation` | Counts and new issue codes before/after |
| `checks` | JSON, model, GUID, and media verification results |
| `media` | Media reference and verified-file plan; see [explicit media authorization rules](#explicit-media-authorization-rules) |
| `resolved_request` | **Not** present in `--json`: written to the `--emit-resolved` file, containing the same request with resolved `guid`, deck identity, and explicit model for every note. Either the file is written or the command fails with `write_failed` |

### `migrate-media`

Migrates proven media references from a legacy filename to the domain's current canonical filename. This is the only narrowly scoped exception allowing a change to an existing note's field.

```bash
# 1. dry-run first: prove the reference semantics and inspect the plan
anki-repo migrate-media EXPORT --namespace pitch_accent --key 飴 --from 飴.png

# 2. write: rewrite references, place the canonical bytes, free the old name
anki-repo migrate-media EXPORT --namespace pitch_accent --key 飴 --from 飴.png --apply
```

If the plan frees a legacy filename, `--apply` holds `ExportLock` and rechecks that file's presence, size, and SHA-256 against the planning snapshot. A mismatch produces `legacy_media_changed` before file placement or `deck.json` publication. The check is repeated immediately before deleting the legacy file.

Why this exists: CrowdAnki's `media/` directory is a flat namespace shared across domains. Canonical names deliberately differ: kanji images use `<char>.gif|png`, while pitch-accent images use `<surface>.pitch.png`. The previous workflow named pitch images `<surface>.png`. For a single-character word requiring a fallback kanji image, both images would then have the same filename despite different bytes. `create` must refuse (`destination_media_conflict`), not choose which contents to overwrite. Migrating the reference to the current contract frees the filename for the canonical character image.

`--namespace` specifies the resource domain owning the **canonical** name (`kanji` or `pitch_accent`); `--key` is the domain identity key (e.g. surface form). `--from` is the exact old basename, containing no path, `..`, or control characters. The domain owner defines the canonical name through `AssetDomainPolicy::canonical_location`; the command resolves and verifies it using the identity. `AssetDomainPolicy::proves_legacy_consumer_filename` separately validates the old filename/`AssetIdentity` pair. `--create-config`, `--asset-store`, and `--pitch-asset-store` have the same meanings as in `create`.

#### What constitutes proof

Before opening a store, the domain policy must prove that the exact (`AssetIdentity`, `--from`) pair is allowed. The proof neither derives from a legacy storage path nor depends on whether the asset exists. Only exact safe names are recognized: `<key>.gif` or `<key>.png` for `kanji`, and `<key>.png` for `pitch_accent`. Generic domain policy does not define legacy filenames.

The command then inventories **all** consumers of that filename in the export:

- note-field values authorized by actual model UUIDs and field names in `.anki-repo/create.yaml`. Each address must be claimed by exactly one configured processor and match its precise recognized shape: a complete `img/src` element whose value is `--from`;
- `note_models[].tmpls[].qfmt`, `afmt`, and `note_models[].css`. These are static consumers; their ownership by a particular note or identity cannot be proven, so they block migration even in unrelated models.

The inventory finds addresses, not mere occurrences of text. It accounts for URL encoding, query parameters, and fragments; `飴.png?cache=1`, `飴.png#front`, and `%E9%A3%B4.png` all refer to the same file for inventory purposes. Yet `srcset`, CSS addresses in field values, `[sound:…]`, incomplete constructs, filenames with directories, and values that do not match a handler's exact parsed syntax cannot be claimed by that handler.

If any address or ambiguous construct cannot be proven, the entire operation fails without changes and returns evidence. It names the note's `guid` and field, or the static surface (`qfmt`, `afmt`, `css`), along with `model_uuid`, subtree path, and a reason such as `field_not_owned_by_domain`, `reference_shape_not_claimable`, `field_position_not_described_by_model`, or `static_model_reference_not_owned_by_domain`. Separate counters distinguish all detected file addresses from those successfully claimed by a handler, preventing any unproven consumer from silently retaining the legacy name.

#### Writes performed

- **Note field:** Exact source-value ranges are collected, checked for overlap, and replaced consistently. Multiple references in one field do not shift subsequent replacement positions; unrelated text remains unchanged.
- **`media/<canonical>`:** Bytes come from the verified domain store under the same trust requirements as `create` (for `pitch_accent`, only current automatic `VERIFIED` from the expected validator). The old file is **not** renamed. Equal SHA produces `reuse`; different bytes are copied; an occupied canonical filename with different bytes fails with `destination_media_conflict`.
- **`media_files`:** The command checks old and canonical filename declarations throughout the `children` tree. Duplicate old or canonical declarations in separate nodes produce `media_declaration_conflict`, including conflicting subtree paths. A canonical declaration must cover every moved note's subtree: an existing valid one is kept, otherwise a declaration is placed at the nearest common ancestor of consumers and the existing canonical declaration. For a new declaration, the common ancestor of the old declaration and migrated notes is used; at the same node the old name is replaced in place. Other declaration ordering is retained. An old declaration is not required to substitute a canonical filename.
- **`media/<from>`:** After publishing `deck.json`, the proven legacy name is deleted even if it was never declared in `media_files`. Only a verified regular file with the exact basename is eligible.

The operation order is consumer/candidate verification, placement of canonical bytes, publication of `deck.json`, and finally deletion of the old file. Interruption after JSON publication leaves an extra file rather than a broken reference. The report never passes a pre-operation snapshot off as a post-operation state: separate fields show actual file and declaration state on completion. A retry does not delete a leftover file “just in case.”

The command is idempotent: if there is nothing to migrate, no bytes change and `changed: false` is returned. As in `edit`, `create`, and `retire`, writing requires canonical `deck.json`, the same before/after `ERROR` checks, and the shared export-directory lock. Without `--apply`, it only displays a dry-run plan and does not write `deck.json`, `media/`, or `media_files`.

#### The sole exception to immutable existing notes

Ordinary operations do not change fields of existing notes. There is **exactly one** exception: `migrate-media`, and only for a reference in a field owned by the processor that also owns the proven identity/filename pair. If even one consumer is unowned, static, or ambiguous, the whole operation is blocked.

Manual `deck.json` edits, direct file deletion/renaming in `media/`, and other bypasses are prohibited: only this command can retain semantic proof, publish canonical bytes, and reconcile `media_files` across the deck tree.

#### Reasons for refusal

| `reason` | Error / exit code | Meaning |
|---|---|---|
| `migration_invalid_request` | `invalid_request` / 3 | Bad `--namespace`, `--key`, or `--from`, or `--from` already equals canonical name |
| `media_domain_unknown` | `invalid_request` / 3 | `--namespace` has no configured processor |
| `legacy_media_identity_unproven` | `invalid_request` / 3 | Domain owner cannot prove the exact (`AssetIdentity`, `--from`) pair; refusal precedes store access |
| `legacy_media_reference_unproven` | `expected_mismatch` / 7 | At least one consumer cannot be proven; `error.details.references` identifies unproven and static consumers |
| `kanji_asset_missing`, `pitch_asset_missing`, `asset_integrity_invalid` | `invalid_request` / 3 | Verified domain asset missing or failing trust requirements |
| `destination_media_conflict` | `expected_mismatch` / 7 | Canonical filename occupied by other bytes or an unsafe object |
| `legacy_media_changed` | `expected_mismatch` / 7 | Old file changed after plan creation; it is preserved and writing stops |
| `media_declaration_conflict` | `invalid_request` / 3 | Old/canonical filename declared more than once in tree; details identify subtrees |

#### Report

| Key | Meaning |
|---|---|
| `export_dir`, `deck_json` | Paths to export and `deck.json` |
| `dry_run`, `applied`, `changed` | Mode, whether file was replaced, and whether contents changed |
| `identity`, `namespace`, `key` | Migrating asset identity |
| `legacy_filename`, `canonical_filename`, `canonical_sha256`, `canonical_action` | Names, canonical SHA-256 and placement action (`copy`/`reuse`) |
| `references`, `references_total`, `references_truncated` | Proven references (`guid`, model, field, `ord`), total, and cap of 50 |
| `media_files_added`, `media_files_removed` | Changes to declarations |
| `legacy_declared`, `legacy_media` | Pre-write snapshots: whether `--from` was declared in the tree, and physical filename, size, SHA-256 (`null` if absent) |
| `legacy_declared_after`, `legacy_media_exists_after` | Actual declaration/file state at completion; dry-run leaves initial state |
| `legacy_released` | `true` only if the file was actually removed by successful `--apply` |
| `validation` | Before/after counts and new codes |

### `migrate-layout`

A one-time controlled repository export-layout migration: legacy exports under `decks/` move to flat `decks/output/<export_root_name>/`. Export contents—including `deck.json`, `media/`, and identities—remain unchanged.

```bash
# 1. plan first: what moves where
anki-repo migrate-layout --root . --json

# 2. write: move the exports and remove the previous layout's placeholder files
anki-repo migrate-layout --root . --apply
```

How the move is proven: before writing, the tool inventories **every** file in each export, including size and SHA-256, and folds them into one content fingerprint. Planning happens before the lock is acquired, so the source is fingerprinted again just before moving; discrepancies cause refusal before layout changes. The destination fingerprint is checked after moving; disagreement is `source_changed`, not success. On the same filesystem, `rename` moves an entire export. If `rename` fails, contents are copied into a temporary `.anki-repo-migrate-tmp-*` directory beside the destination, verified, then published by `rename`; only then is the original deleted.

The entire migration is refused without moving any export if:

- two exports produce the same flat destination name, or it collides with an existing entry in `decks/output/` (case and Unicode normalization are ignored for collision matching): `target_name_collision`;
- an occupied destination differs from the source (`target_conflict`), or one appeared between plan and write (`target_appeared`);
- the source or an entry within it is a symlink (`source_is_symlink`, `source_contains_symlink`), or the destination is a symlink (`target_is_symlink`);
- the export-root or contained filename is not valid UTF-8 (`source_non_utf8_name`);
- the source disappeared or is not a directory (`source_missing`, `source_not_directory`), or is unreadable (`source_unreadable`);
- `deck.json` is missing (`deck_json_missing`);
- the destination exists but is unreadable (`target_unreadable`);
- `decks/` itself contains `deck.json` (`legacy_area_is_export`): the area root must not be moved together with `decks/input` and `decks/output`.

Refusal returns code 3 and `refused: true` in the report; no exports are moved.

Idempotency: an existing destination whose bytes match the source is not overwritten; the tool reports `already_present` and removes the redundant source. An interrupted migration can therefore be finished with another `--apply`; a completed migration finds nothing to do. Free disk space is checked before the first write; insufficient space results in `write_failed` with `required_bytes` and `available_bytes`.

Legacy placeholder files (zero-byte `.gitkeep`) and empty directories are removed only when the old area contains nothing else. Any unrelated file leaves the area intact and appears in `legacy_area_retained` diagnostics with paths.

`ExportLock` protects the legacy area throughout writing, preventing two concurrent migrations from interleaving.

#### Report

| Key | Meaning |
|---|---|
| `root` | Absolute repository root; other report paths are relative to it |
| `legacy_area`, `output_dir` | Old layout area and flat destination |
| `apply`, `applied`, `refused` | Whether writing was requested/performed or any refusal occurred |
| `exports[]` | Per export: `name`, `source`, `target` (root-relative), `action` (`move` / `already_present` / `refused`), `state` (`planned` / `applied` / `refused`), `files`, `bytes`, `content_sha256`, `reason`, `message` |
| `pruned` | Removed legacy placeholders and emptied directories (root-relative) |
| `diagnostics` | `symlink_skipped`, `legacy_area_absent`, `nothing_to_migrate`, `legacy_area_retained`; root-relative `paths` |
| `summary` | `exports`, `planned`, `applied`, `already_present`, `refused`, `files`, `bytes` |

#### If the legacy layout was tracked by Git

`.gitignore` does not remove already tracked paths. After `migrate-layout --apply`, remove the old files from the Git index using `git rm -r --cached decks/<old_area>` and commit that removal. Conversely, when another worktree pulls a commit deleting tracked files, those files are deleted there as well. Back up `decks/` before receiving such a commit.

### `retire`

Retires a note by appending exactly one tag; nothing else changes. The toolkit cannot physically delete notes, so a note cannot be lost unnoticed; a human deletes it in Anki when they see fit.

```bash
EXPORT=decks/output/Words__N1

# dry-run for a single guid
anki-repo retire "$EXPORT" --guid "w0Wc3Ip@N#" --tag "archived::auto"

# write
anki-repo retire "$EXPORT" --guid "w0Wc3Ip@N#" --tag "archived::auto" --apply

# several notes and a batch request from a file or stdin
anki-repo retire "$EXPORT" --guid "…" --guid "…" --tag "archived::auto" --apply
anki-repo retire "$EXPORT" --request retire.json --apply
cat retire.json | anki-repo retire "$EXPORT" --request - --apply
```

Batch request file:

```json
{
  "schema_version": 1,
  "tag": "archived::auto",
  "notes": [
    { "note_id": "н1", "guid": "w0Wc3Ip@N#" }
  ]
}
```

#### Three outcomes per note

| Status | Meaning |
|---|---|
| `retired` | Tag was missing and has been added (or would be added in dry-run) |
| `dry_run` | Writing was not requested |
| `already_retired` | Tag already exists; the file is not rewritten or duplicated |

A tag follows Anki-style validation: it must be nonempty, with no whitespace or control characters. Comparison is case-insensitive, including non-ASCII/Cyrillic: `тэг` and `ТЭГ` are the same tag. `already_retired` recognizes any capitalization and never adds another spelling. Repeated `guid` in an export is a `validate`-level `ERROR`, so it blocks writing; a missing target `guid` yields `unresolved_guid` (exit code 3).

#### Guarantees

- Only target-note `tags` change; `fields`, `guid`, `note_model_uuid`, and position in `notes` remain byte-identical.
- The note remains addressable via `find --guid` (`retired_notes_still_resolvable` in the report).
- Existing tag order is retained, new tags are appended, and duplicates are not possible.
- The candidate is reparsed, validated for tag-only changes (`only_tags_appended`, `tags_appended_verified`), and compared against previous `validation`.
- Canonical `deck.json` is required, with atomic, concurrency-safe writing.

#### Report

| Key | Meaning |
|---|---|
| `dry_run`, `applied`, `tag` | Mode, file-replacement status, retirement tag |
| `source_bytes`, `candidate_bytes`, `byte_delta` | Size and difference |
| `notes_total`, `notes_retired`, `notes_already_retired` | Status counts |
| `outcomes` | Per-note `note_position`, `previous_tags`, `tags` (up to 50 entries) |
| `checks` | Results of five mandatory checks |

`retire` deliberately has no `--emit-resolved`: the request is already explicit and nothing needs resolution.

### `visual-report`

A static before/after report compares two export directories by `guid`. The output is an offline directory containing `index.html`, previews in `cards/*.html`, state-specific copied media under `media/<state>/*`, and an ownership manifest, `report-manifest.json`. Open it in a browser without a network connection or server.

Each changed note gets **two independent frames**, “Before” and “After,” each rendered from its own state. Created notes only get “After”; notes absent from the new state only get “Before.” The missing side is not invented. Frames are side by side on wide screens and stacked on narrow screens.

Report interactivity is self-contained in the generated files: light/dark theme switching, content-sized frames, and local audio playback. This does not imply full support for Anki templates; the trust boundary below is enforced by tests.

```bash
EXPORT=decks/output/Words__N1

# compare a deck with its modified copy
anki-repo visual-report \
  --before "$EXPORT" \
  --after /path/to/после \
  --out /path/to/отчёт \
  --retire-tag "archived::auto"

# how many notes to show in detail in each section (default 200, maximum 1000)
anki-repo visual-report --before … --after … --out … --preview-limit 50
```

Each note belongs to exactly one outcome:

| Outcome | Condition |
|---|---|
| `created` | `guid` occurs only in the after state |
| `changed` | `guid` occurs in both states and field values differ |
| `retired` | `guid` occurs in both states and `--retire-tag` was added in the after state |
| `removed` | `guid` occurs only in the before state |
| `unchanged` | `guid` occurs in both states with identical fields and tags |
| `ambiguous` | `guid` is repeated in either state: `ambiguous_guid` diagnostic; the report **never** guesses which note was intended |

`removed` is an observation, not a request to delete a note. CrowdAnki does not delete a note just because it disappeared from JSON, and `retire` does not physically delete it either. The report does not simulate import decisions or claim what Anki will create, update, or leave unchanged.

#### Report contents

- Outcome counts, note counts before/after, and number of written previews.
- `--preview-limit` limits detailed notes **per section**, separately. At most 4,000 preview files are written regardless of the limit; the `--json` list is capped at 500 entries, while `card_files_total` gives the actual number written.
- Outcome sections showing before/after fields, line-level differences between HTML tokens (not only visible text), and tags with newly added tags highlighted.
- Separate card-preview files under `cards/` isolate model CSS from report CSS. Changed notes have a preview for each state; created or removed notes have one.
- State-specific diagnostics for missing media and audio, paths in links, external links, symlinks, oversized files, repeated GUIDs, and incomplete previews.
- Unsupported template constructs found in affected models.
- An explicit report-limitations section so previews are not mistaken for Anki's actual rendering.

#### Card previews: supported and unsupported features

The template parser is static, and faithfully represents only what can be evaluated without Anki:

- `{{Поле}}` substitutes a field value; field names are case-sensitive.
- `{{#Поле}}…{{/Поле}}` and `{{^Поле}}…{{/Поле}}` are conditional sections for nonempty and empty values, including nesting. Whitespace-only content, `<br>`, and `<div>` count as empty.
- Special fields `Tags`, `Type`, `Deck`, `Subdeck`, `Card`, and `FrontSide`. Rendering `FrontSide` takes two passes, embedding the front in the back.
- Wrapper class `card card{N}`, where `N` is template `ord` plus one, and model CSS loaded after base styling.

Beyond this, the report implements only the Anki behavior needed for visual verification:

- **Night mode:** the theme toggle applies `nightMode` to the document root, `body`, and card itself, so `.card.nightMode` and `.nightMode …` selectors behave as in Anki.
- **Audio:** `[sound:name.mp3]` becomes a local replay button backed by `<audio preload="none">` and the copied media file, rather than remaining literal text.
- **Frame height:** after media has loaded, the content height is measured and reported instead of using a fixed constant.

Deliberately unsupported: execution of JavaScript from Anki templates or field values (Anki executes `<script>`, but static reports treat that side as not rendered and display the tag as visible text), evaluation of filters (`cloze`, `type`, `tts`, `furigana`, `hint`, etc.), runtime-only `CardID` and `CardFlag`, and media referenced through model CSS (`url(…)` is not copied). Unrecognized constructs are shown as raw text in the preview and listed in the limitations section.

#### Media files

Only files referenced by **actually displayed** previews are copied, and only if found in the corresponding state's `media/` directory. They remain state-separated: `media/before/<name>` and `media/after/<name>`. Two files with the same name in before/after may have different bytes; merging paths would silently show the after-state media in a before-state frame. For the same reason, a missing before-state file is never substituted from the after state. A reference is rewritten to `../media/<state>/<name>` only after its file has been copied. Audio `[sound:<name>]` follows the same state isolation; unresolved audio remains visible literal text with a warning and diagnostic.

External addresses (`http://`, `https://`, `//`, `data:`) are neither downloaded nor substituted. A path-bearing reference (`../../секрет`) is never read: only a basename may be copied, and the original address is reported as a diagnostic. Files with unusable URL names are not copied: path separators (`/`, `\`), URL metacharacters (`:`, `?`, `#`, `%`), markup metacharacters (`"`, `&`, `<`, `>`), and control characters make the URL inaccessible. Such names are reported under `media.traversal` and `media_name_unusable`, never silently ignored. Symlinks are never read. Files larger than 64 MiB are not copied, and at most 4,096 files are copied. `media_files` merely lists references; it does not prove file existence, so missing files are diagnosed instead of fabricated.

#### Trust boundary of the generated artifact

Because users open the report in a browser, its own executable code must be separated from data:

- The report's own runtime code is allowed: theme switching, dynamic height, and audio playback depend on it. It is embedded in generated files, deterministic, and network-independent.
- No external dependencies are permitted: no `<script src=…>`, CDNs, fonts, or network addresses in either report code or data.
- Anki-template code is never executed: `<script>` from templates or fields is an unsupported construct, the side is considered not rendered, and the tag is displayed as visible text rather than executable markup.
- **Structural address validation:** addresses are resolved relative to the report root and permitted only if the resulting path belongs to generated report files. Schemes (`http:`, `https:`, `data:`, `javascript:`), absolute paths, backslashes, `?`, `#`, `%`, and normalized paths escaping the root are rejected—not merely checked for a local-looking prefix. `srcset` can contain multiple candidates; **every** address is checked instead of only the first.
- Data is rendered as text, not executable markup. `<script>`, `on*` event handlers, forbidden elements (`iframe`, `object`, `embed`, `link`, `meta`, `base`, `form`, `applet`, `frame`, `frameset`), and external addresses become inert. Blocked content is not hidden silently: previews show an explicit “not displayed …” marker and `preview_construct_blocked` diagnostic. Event handlers are never exempted; even attributes named `data-report-runtime`, `data-report-state`, or `data-report-theme-value` in a field confer no privileges merely because the report runtime reads those names.
- Case and spelling do not relax checks: `<SCRIPT>`, `OnErRoR`, `SRC = "…"`, and unclosed quotes are interpreted like standard spellings.
- Parsing follows browser semantics, not just literal syntax: HTML5 ignores `/` on non-void elements, so `<script/>` opens a script whose body must be shown as text. HTML comments close according to browser rules (`-->`, `--!>`, empty `<!-->` and `<!--->`), preventing later tags from executing after a malformed comment. Attribute entities are decoded before URL inspection: `url&#40;a.png&#41;` is a URL in the browser. Unknown/ambiguous entities, including `&lpar;` or unterminated `&amp`, cause rejection rather than being skipped.
- Model CSS remains styling: `@import` is not loaded, nonlocal `url(…)` is replaced with an inert value, and `</style>` embedded in CSS or a field cannot end a style element and begin executable markup because `<` is escaped. `style` values are entity-decoded before CSS parsing, and quotes are escaped on output to prevent premature termination of attributes.
- CSS uses a lexer rather than substring matching. It detects `url(…)` and `@import`, including escaped function identifiers (`u\72l(`), escaped address characters (`url(https\3a //…)`), and incomplete `url(`. Ambiguously parsed material is treated as an address and blocked.
- A single `report::sanitize::inspect` answers whether foreign executable code or a network address survives in a generated file. **Every** `index.html` and `cards/*.html` file passes it both in memory before writing and again after reading the file back from disk. The actual file to be opened is verified, not merely an intermediate string. Runtime behavior itself is tested by executing report code in a minimal DOM harness. Failure is `preview_not_offline` or `index_not_offline`, not a warning.

CSP hashes and an iframe `sandbox` are intentionally not used: without `allow-same-origin`, a sandboxed preview has an opaque origin and cannot access its own copied media, defeating the report's main purpose. Strict sanitization treats user content as data while retaining offline functionality.

#### Report-directory boundaries and ownership

`--out` must be outside every `decks/**` area and both compared exports. Report generation writes hundreds of files; allowing an output inside ignored user-deck data would make report creation an unintended data mutation.

A nonempty directory is accepted only if it **proves** report ownership through `report-manifest.json`, the prior run's file list. An ordinary `index.html` is insufficient: other software also uses that name, and blindly overwriting it would destroy unrelated content.

Rules:

- An absent or empty output directory may be created or populated.
- A directory with a valid manifest permits reruns: current files take their places, and obsolete files of the previous report are deleted, individually identified by `out_dir_stale_removed`.
- A directory without a manifest fails with `out_dir_not_owned` **without changing anything**, even if it contains `index.html`.
- A damaged, foreign, or out-of-tree manifest fails with `out_dir_manifest_invalid` and makes no changes.
- A symlink used as `--out` fails with `out_dir_is_symlink` **without changes**: following it would confer control over an unselected directory.
- Symlinks **inside** the report directory cause `out_dir_entry_conflict` without changes, and traversal never follows directory symlinks. Otherwise “inside the report” could resolve outside it. Ownership permits replacing an entry by its own name, not following aliases into unrelated locations. Likewise, a manifest-listed file cannot actually be a directory, nor can a structural directory be a file; the report may not guess which entry it owns.
- Only report files may be replaced: those in the previous manifest or files within report-controlled `cards/` and `media/`, whose names are generated by the report. This exception does **not** cover arbitrary entries in the output root (`index.html` needs manifest proof). If another entry occupies a destination, the run refuses with `out_dir_entry_conflict` and makes no changes.
- Unknown files and directories, absent from both manifest and report structure, are foreign. They are never deleted or replaced, but listed under `out_dir_foreign_files`. “Missing file” and “file we do not own” are different events.

Writing is ordered to keep handled failures harmless:

1. The whole new report is built in a sibling staging directory (`.<name>.staging-<pid>`), so export reading, media planning/copying, and trust inspection do not affect the previous report.
2. **Every** new output path, its ancestor directories, and every obsolete filename are verified **before the first mutation** of the output directory. Partially applying a new report is not “almost good enough”: it could overwrite files an error should have preserved.
3. Complete staged files are moved into place with `rename`.
4. Obsolete owned files and empty report-structure subdirectories are removed. `remove_dir` never deletes nonempty directories, preserving directories with foreign contents.
5. The manifest is written **last**; before that, the directory remains associated with the old report.
6. After placement, each file is checked using its **actual canonicalized path**, not a string comparison. A file outside the output directory triggers `report_file_not_confined`.
7. The staging directory is removed after success or failure.

A process interruption **between** steps 3 and 5 may leave a readable mixture of old and new files with the old manifest. The next successful run converges to exactly the current report set: generated names within `cards/` and `media/` may be replaced, and obsolete entries in the former manifest removed. Entries not owned under these rules remain and are diagnosed. All *generation-time* failures—the failures the tool can handle—happen before step 3 and leave the original report directory unchanged.

`all_files_inside_out_dir` verifies that every report file exists inside the output directory after installation; `media_confined_to_out_dir` checks that copied media lies in the correct state-specific subdirectory.

#### Report

| Key | Meaning |
|---|---|
| `before`, `after` | Per state: directory, `deck.json`, deck name, `crowdanki_uuid`, counts of notes, decks and models |
| `out_dir`, `index_html` | Report directory and entry point |
| `card_files`, `card_files_total` | Relative preview paths and actual total |
| `preview_files`, `preview_files_truncated` | Preview file, state (`before`/`after`), `guid`, model, template, hint, resolved media, missing audio |
| `retire_tag` | Retirement tag used |
| `counts` | Outcome, preview, and before/after note counts |
| `outcomes` | Per-note `kind`, deck, model, before/after tags, changed fields, GUID occurrence counts (up to 500 entries) |
| `diagnostics` | Code, severity, message and subject of each diagnostic |
| `unsupported_constructs` | Constructs the preview does not evaluate and why |
| `media` | Copied files and issues, overall and broken down by state (`states`) |
| `limitations` | Same limitations listed in `index.html` |
| `checks` | Results of eight mandatory checks |

The report is deterministic: identical input states produce byte-identical files, including `report-manifest.json`, which contains no timestamp, report-directory name, or invocation path, and whose file list is sorted. Neither timestamp nor version is printed, so the report does not become stale merely with time.

### Typical workflow

```bash
EXPORT=decks/output/Words__N1

# 1. what the export actually contains
anki-repo qa "$EXPORT"

# 2. a compact batch for a detected problem
anki-repo --json review "$EXPORT" --qa-code empty_field_value --limit 25 > batch.json

# 3. an external agent reads batch.json and returns a proposal document
#    (schema_version, proposals: proposal_id, guid, field, expected, replacement)

# 4. validate the proposals against the current export state
anki-repo --json review-check "$EXPORT" --proposals proposals.json \
  | jq .result.edit_request > request.json

# 5. dry-run, write and verify the result with the existing commands
anki-repo edit "$EXPORT" --request request.json
anki-repo edit "$EXPORT" --request request.json --apply
anki-repo validate "$EXPORT"
```

Adding a new note and retiring an old one follow a similar path:

```bash
EXPORT=decks/output/Words__N1

# 1. the actual field schema: what the fields are called and what they hold
anki-repo models "$EXPORT"

# 2. by default new values contain no media references;
#    the exception is an explicitly configured domain handler
anki-repo create "$EXPORT" --request create.json            # dry-run
anki-repo create "$EXPORT" --request create.json --apply --emit-resolved create.resolved.json
anki-repo validate "$EXPORT"

# 3. a retired note is not deleted but tagged
anki-repo retire "$EXPORT" --guid "…" --tag "archived::auto" --apply

# 4. what exactly changed — a static report over the two states
anki-repo visual-report --before "$ЭТАЛОН" --after "$EXPORT" --out /tmp/отчёт \
  --retire-tag "archived::auto"
```

Responsibilities are deliberately separated: `qa` and `review` deterministically select material; an external agent proposes content changes; `review-check` validates proposals against facts; and only `edit`, `create`, `retire`, and `migrate-media` write to `deck.json`, explicitly requiring `--apply`. Deciding which note is needed or obsolete is a human/agent decision; the toolkit verifies feasibility and performs exactly the requested operation.

#### Reports in JSON mode

| Command | `result` keys |
|---|---|
| `qa` | `export_dir`, `notes_total`, `findings_total`, `findings_returned`, `unaddressable_findings`, `truncated`, `max_per_code`, `codes`, `by_code`, `rules`, `findings` |
| `review` | `export_dir`, `selection`, `notes_total`, `total_selected`, `excluded_unaddressable`, `offset`, `limit`, `returned`, `truncated`, `next_offset`, `items` |
| `review-check` | `export_dir`, `deck_json`, `outcome`, `proposals_total`, `counts`, `effective_proposals`, `proposals_truncated`, `source_editable`, `source_blockers`, `proposals`, `edit_request` |
| `models` | `export_dir`, `deck` (`path`, `crowdanki_uuid`, `preorder`, `notes_in_deck`), `sample_limit`, `models` |
| `create` | `export_dir`, `deck_json`, `dry_run`, `applied`, `source_bytes`, `candidate_bytes`, `byte_delta`, `notes_total`, `notes_created`, `notes_already_applied`, `outcomes`, `outcomes_truncated`, `decks_touched`, `validation`, `checks` |
| `retire` | `export_dir`, `deck_json`, `dry_run`, `applied`, `tag`, `source_bytes`, `candidate_bytes`, `byte_delta`, `notes_total`, `notes_retired`, `notes_already_retired`, `outcomes`, `outcomes_truncated`, `validation`, `checks` |
| `migrate-media` | `export_dir`, `deck_json`, `dry_run`, `applied`, `changed`, `identity`, `namespace`, `key`, `canonical_filename`, `canonical_sha256`, `canonical_action`, `legacy_filename`, `legacy_declared`, `legacy_media`, `legacy_media_exists_after`, `legacy_declared_after`, `legacy_released`, `references_total`, `references_truncated`, `references`, `media_files_added`, `media_files_removed`, `validation` |
| `visual-report` | `before`, `after`, `out_dir`, `index_html`, `card_files`, `card_files_total`, `preview_files`, `preview_files_truncated`, `retire_tag`, `counts`, `outcomes`, `outcomes_truncated`, `diagnostics`, `unsupported_constructs`, `media`, `limitations`, `checks` |

For `migrate-media`, `identity` contains `namespace` and `key`; `legacy_media` is either `null` if no file exists or an object with `filename`, `byte_length`, `sha256`. Each `references` entry has `guid`, `model_uuid`, `model_name`, `field`, and `field_ord`. `validation` contains `before`, `after`, `new_error_codes`, and `new_warning_codes`; both `before` and `after` counts contain `errors`, `warnings`, and `info`.

In `review.items`, note fields are represented by a `fields` object in model `ord` order, just like in `find`. In `qa.findings`, `field_ord` is `null` for rules not tied to a specific field position. Addressability (`addressable` in `qa`, `excluded_unaddressable` in `review`) and duplicate-group context (`group_size`, `related_note_indices`/`related_guids` in `qa`; `group_membership`, `related`, `group_size`, `related_truncated` in `review`) are always present, even when no groups exist and counters are zero.

### `edit`

The command that edits **field values of existing notes**. Each edit targets one field in one existing note by (`guid`, field name). The GUID is found in the export; the field name is resolved via the note's `note_model_uuid` and `note_models[].flds[].ord`; the replacement is written to `fields[ord]`.

Without `--apply` it performs a complete **dry-run**, calculating the candidate and all checks without writing it. With `--apply`, it atomically replaces the file only after every check succeeds.

```bash
# single edit: dry-run
anki-repo edit decks/output/Words__N1 \
  --guid 'D!TAYVuHi,' --field 'Значение' --expect 'было' --set 'стало'

# the same edit with writing
anki-repo edit decks/output/Words__N1 \
  --guid 'D!TAYVuHi,' --field 'Значение' --expect 'было' --set 'стало' --apply

# batch edit from a request file
anki-repo edit decks/output/Words__N1 --request edits.json --apply

# batch edit from stdin
anki-repo edit decks/output/Words__N1 --request - --apply < edits.json
```

A single edit uses `--guid`, `--field`, `--expect`, and `--set`, all four together and required. A batch edit uses exactly one `--request`, either a JSON filename or `-` for stdin. `--request` and `--guid` cannot be combined.

GUIDs, field names, and values shown above are examples. `--expect` is a precondition, not a hint; it must equal the current field value. If it does not, **no** conflicting batch edit is performed. This prevents edits based on stale content.

#### Request format

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

- `schema_version` is mandatory and must be `1`; unknown versions fail.
- `edits` must contain 1–20,000 edits; the request file must not exceed 8 MiB.
- `edit_id` is optional, but must be unique if provided.
- A (`guid`, `field`) pair may not occur twice in one request: applying two edits to the same value would be ambiguous.
- Unknown request keys are rejected, exposing typos rather than silently ignoring them.

#### Four outcomes per edit

| Status | Condition | Writing |
|---|---|---|
| `noop_identical` | current value equals both `expected` and `replacement` | not needed |
| `applied` | current value equals `expected`, differs from `replacement` | performed |
| `dry_run` | same condition without `--apply` | not performed |
| `already_applied` | current value already equals `replacement` | not needed |

`already_applied` and `noop_identical` are successful outcomes. Repeating the same `--apply` command exits with code `0` without rewriting the file or changing its `mtime`. If no edit requires a change, report `applied` is `false`.

When the current value matches neither `expected` nor `replacement`, this is a failed precondition: `expected_mismatch`, exit code 7. No batch edits are applied; the report lists all conflicts with samples of actual and expected values.

#### Guarantees

Edits require `deck.json` to be in the repository's exact canonical form: LF, four-space indentation, alphabetically sorted keys, unescaped non-ASCII (no `\u` escaping), and no trailing newline. Source bytes are compared with the toolkit's own rendering (`serde_json` using `PrettyFormatter` and four spaces), including numeric formatting (`1e3` becomes `1000.0`) and short control-character escapes `\b`, `\f`, `\n`, `\r`, `\t`, with `\uXXXX` for remaining U+0000–U+001F. This is essential, not cosmetic: canonical serialization means an edit can change only the requested value without reformatting unrelated bytes. Compact JSON, two-space indentation, CRLF, or a final newline result in `source_not_canonical` (exit code 3), not silent normalization.

Before writing, the tool verifies:

- the source is canonical, otherwise `source_not_canonical`;
- the export has no `ERROR` (`export_invalid`) and no `conflicting_note_model_definition` (`export_not_mutable`), because conflicting model declarations make fields ambiguous;
- note positions, `guid`, and `note_model_uuid` correspond exactly between the untyped JSON tree and typed tree, preventing edits to the wrong `fields[ord]`;
- every target resolves, each GUID is unique, and the field belongs to that note's model;
- reparsing the candidate yields exactly the planned value;
- byte-level diff contains precisely the requested changes: the count of changed lines equals the count of effective edits, every changed line has exactly one modified string token, and the multiset of “before → after” string pairs matches the request;
- file-size change is explained by replacement-token lengths including escaping;
- candidate validation introduces no new `ERROR` (new `WARNING` or `INFO` entries are permitted and reported);
- file replacement is atomic: the candidate is written to a temporary file in the same directory, flushed to disk, and renamed over `deck.json` while preserving permissions. On error, the temporary file is deleted and the original `deck.json` left unchanged.

Violations of these internal invariants indicate toolkit defects. They fail with `internal_error` (exit 70) and do not write.

#### Concurrent `--apply` invocations

Comparison and candidate publication form one critical section: an exclusive cooperative lock on the stable export directory is acquired before checking `deck.json` bytes and held until `rename` completes. A separate `read → compare → rename` sequence is unsafe: another writer could publish newer data between the comparison and rename, which would then be overwritten silently.

In practical terms:

- Concurrent `edit`, `retire`, and `create --apply` for one export share the lock. A losing writer rechecks under the lock and receives `source_changed` (exit code 7) if its snapshot is stale. Replacing the `deck.json` inode does not replace the locked directory, so publication remains ordered.
- The kernel releases the lock when the process exits; an `anki-repo` crash cannot leave a permanently stuck lock.
- Lock acquisition waits instead of failing immediately. This intentionally yields a meaningful domain response (`source_changed`) instead of an arbitrary lock-acquisition refusal.
- The guarantee applies to cooperating `anki-repo` processes. An external writer that ignores the lock is detected only by the byte comparison under the lock. Changes before comparison cause `source_changed`; changes between comparison and `rename` cannot be intercepted portably with `std`. Stronger guarantees for such external writers are not claimed.
- Only Linux is supported; the toolkit does not compile elsewhere, so fallback `lock_unsupported_platform` is not reachable at runtime.

#### Edit limitations

- Only field **values** are changed; `fields` cannot grow or shrink.
- `guid`, `note_model_uuid`, `tags`, `media_files`, models, configurations, and the deck tree cannot be edited through this command.
- `edit` neither reads nor modifies `media/`. Verified changes of media references require the separate `migrate-media` command.
- One invocation operates on one export.

#### Report

The human-readable report includes mode, before/after sizes, changed-line count, each edit's outcome with old/new value excerpts, check results, and before/after validation. In JSON mode `result` contains:

| Key | Meaning |
|---|---|
| `export_dir`, `deck_json` | Export path and `deck.json` |
| `dry_run`, `applied` | Mode and whether the file was replaced |
| `source_bytes`, `candidate_bytes`, `byte_delta` | Sizes and difference |
| `changed_lines`, `first_changed_line` | Number of changed lines and first changed line |
| `edits_total`, `effective_edits` | Requested edits and edits that change values |
| `summaries` | Status counters |
| `outcomes` | Edit outcomes (up to 50), plus `outcomes_truncated` |
| `validation` | Before/after counts and new issue codes |
| `checks` | Five mandatory check results |

## Public contract changes

### Added commands and guarantees

- Added `models`, `create`, `retire`, `migrate-media`, `migrate-layout`, and `visual-report`. Four commands edit export contents (`edit`, `create`, `retire`, `migrate-media`); the fifth writing command, `migrate-layout`, changes the directory layout, **not** export contents. None creates decks or models. `create` adds notes to existing decks/models and may place verified media under explicit rules; `retire` only appends a tag.
- `visual-report` writes a separate report directory and **never** writes into exports or `decks/**`; this is checked before the first write and verified using the paths actually written.
- `migrate-layout` migrates directory structure separately from the four export-editing commands: `--apply` moves legacy export directories into flat `decks/output/`, removes legacy placeholder files and byte-identical sources, and verifies each export by SHA-256. Neither `deck.json` contents nor identities are changed.
- `decks/input` is a restricted API boundary. Every path addressing that area fails with `invalid_request`, `details.reason = input_area_forbidden`. There is no bypass flag.
- The toolkit provides no physical deletion of notes. An absent JSON note remains merely an observation (`removed` in reports); retirement uses a tag for precisely that reason.

### Corrected contracts

- `models` now lists fields and samples by `ord`. Previously the declaration order of `flds` could associate samples with wrong fields, making field-semantic evidence unreliable.
- `models` deck selectors (`--deck`, `--deck-uuid`, `--deck-preorder`) can be combined. Mismatches produce `deck_identity_mismatch` with detailed differences, not argument parsing failure. This same validation also applies to selectors in `create` JSON requests.
- `create --apply` requires the target deck to declare `crowdanki_uuid` (`unresolved_deck_identity`), not that callers use a UUID selector. Deck name selection is still permitted, but the target must be stably addressable after writing.
- Incomplete `visual-report` previews, including `<script>` in a template, now receive a JSON `preview_incomplete` diagnostic, not only an HTML warning.
- `visual-report`'s `counts.previews` is the actual number of written preview files, not a pre-write estimate. Changed notes produce both before/after files; `--preview-limit` caps notes displayed in detail without splitting these pairs.
- Preview media are resolved from each compared export's own `media/` and rewritten as `../media/<state>/<name>` only when actually copied.
- Files referenced solely in the before state are copied from the before state's media, never substituted from after.
- Template or field `<script>` is shown as visible text and marked unsupported, never executed.

### Removed entities

Two features were deliberately removed: their semantics were defined by one particular note model, so they turned the makeup of the current user decks into a permanent toolkit contract. The toolkit handles arbitrary CrowdAnki exports and assumes no specific field names.

- Removed `--word` from `find` and `review`. It was merely an alias for substring matching on raw values of the field `Слово`, which is model-specific. The same functionality remains available explicitly through `--field <field name> --value <value> --match contains`.
- Removed QA rule `duplicate_primary_field`: the concept of a primary/headword field belongs to individual note models, not the CrowdAnki format. Exactly five QA rules remain (`empty_field_value`, `leading_whitespace`, `trailing_whitespace`, `forbidden_white_span`, `duplicate_note_content`); group metadata (`related`, `group_size`, `related_truncated`) belongs to `duplicate_note_content`.
- Removed public constant `PRIMARY_FIELD` from `selection`. Field names are supplied by callers, and `ensure_known_field` validates them against actual `note_models[].flds[].name`, returning `unknown_field` (exit 3).

Removed contracts are not silently accepted: `qa --code duplicate_primary_field` and `find`/`review --word` fail with unknown-code or unknown-argument errors. Negative coverage exists in `tests/cli_contract.rs` and `tests/qa_contract.rs`.

## Output modes

- Default: Russian human-readable text in `stdout`. Long field values are flattened onto a single line and length-capped.
- `--json` (before or after the subcommand): exactly one JSON document on `stdout`, nothing on `stderr`.
- Exception: clap argument-parsing errors (unknown flags, incompatible combinations, out-of-range values) produce text on `stderr` and exit 2. No JSON envelope exists because the command has not been identified.

Successful response:

```json
{
  "schema_version": 1,
  "command": "inspect",
  "result": { }
}
```

Error response:

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

`schema_version` identifies the output contract; `code` is a stable snake_case identifier independent of internal Rust type names.

In JSON mode note fields are an object whose keys follow the model's `ord` order. If a malformed model repeats a field name, later duplicate keys are suffixed with `#N` to keep them distinct.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Success |
| `1` | Reserved |
| `2` | CLI usage error: unknown option, incompatible arguments, or out-of-range value |
| `3` | Invalid input prevents execution: missing/non-directory export, missing or unreadable `deck.json`. Typed-export commands (`inspect`, `find`, `stats`, `qa`, `review`, `review-check`, `edit`) also use 3 for `invalid_json`, `root_not_deck`, or `schema_invalid`. For `find`, `review`, `qa`, also unknown field/deck/QA code; for `review-check`, malformed proposal document or noncanonical `deck.json` (`source_not_canonical`). `maintenance` uses `input_unreadable` when the Cargo workspace cannot be resolved (including invalid `--workspace-root`). Unlike these commands, `validate` analyzes invalid JSON as report findings and exits `6`. `migrate-layout` returns `3` with `refused: true` if any export is refused; this is not success but is not a domain error. |
| `4` | No target found by `find`, `review --guid`, `edit`, or a `review-check` proposal |
| `5` | Duplicate `guid` resolved by `find --guid` or `review --guid`; or an `invalid` `review-check` proposal with ambiguous `guid` |
| `6` | `validate` found any `ERROR`, including `invalid_json`, `root_not_deck`, or `schema_invalid`; `edit` rejected an export with `ERROR` or conflicting model definitions. Duplicate GUID (`duplicate_note_guid`) is an `ERROR`, so `edit` returns 6 rather than 5: source validation precedes target resolution. `review-check` identifies `export_invalid` / `export_not_mutable` and returns 6 instead of report outcome, emitting no request. |
| `7` | Preconditions conflict: `edit`'s `expected` or `deck.json` version changed, or `review-check` found stale proposals |
| `8` | Failed to write or publish operation output (`write_failed`) |
| `70` | Unexpected internal error, including edit-invariant failure or output-write failure (disk full, bad descriptor) |

A reader closing a pipe (`anki-repo … | head`) is not an internal error: the command result was already computed and its own exit code is preserved.

Domain `error.code` values and corresponding exit codes are listed below. `validate` content issues (`invalid_json`, `root_not_deck`, `schema_invalid`) are not domain errors in that command and are documented in its section.

| Code | Exit | When |
|---|---|---|
| `usage` | 2 | Incompatible arguments detected outside clap |
| `input_unreadable` | 3 | Could not read export directory, `edit` request, or `review-check` document; `migrate-layout` cannot traverse legacy area or fingerprint source; `maintenance` cannot identify Cargo workspace from missing/unresolvable `--workspace-root` |
| `deck_json_missing` | 3 | Export directory lacks `deck.json` |
| `invalid_json` | 3 | `deck.json` is not valid JSON |
| `root_not_deck` | 3 | Root is not a CrowdAnki `Deck` |
| `schema_invalid` | 3 | JSON is valid but incompatible with the typed core |
| `unknown_field` | 3 | No model in the export has the field, or for `edit`, target note's model lacks it |
| `unknown_deck` | 3 | Selected deck not found |
| `unknown_qa_code` | 3 | QA code not in registry (`qa --code`, `review --qa-code`) |
| `invalid_request` | 3 | Invalid `edit` request or `review-check` proposal document (format, version, fields, identifiers, limits); also any command with a path addressing restricted `decks/input` (`details.reason = input_area_forbidden`) |
| `duplicate_edit_target` | 3 | `edit`/`review-check`: repeated (`guid`, field) pair; separate error, not `invalid_request`'s `details.reason` |
| `proposal_not_executable` (`details.reason`) | — | Not a separate error code: `review-check` reports unusable proposals as `invalid` statuses |
| `source_not_canonical` | 3 | `edit`: noncanonical `deck.json`; `review-check`: blocker in `source_blockers`, no request generated |
| `export_invalid` | 6 | `edit`: source has `ERROR` (including `duplicate_note_guid`) or candidate introduces a new `ERROR`; `review-check`: same blocker in `source_blockers`, return 6 instead of normal outcome |
| `export_not_mutable` | 6 | `edit`: conflicting note-model definitions; `review-check`: same blocker in `source_blockers` |
| `expected_mismatch` | 7 | `edit`: current field value did not match `expected` |
| `source_changed` | 7 | `edit`: `deck.json` changed or vanished between checking and replacement. `migrate-layout`: after-move content fingerprint differs from proven source, including cross-filesystem copies |
| `write_failed` | 8 | Writing/publishing result, including `deck.json`, or configuring system maintenance failed |
| `not_found` | 4 | `find` / `review --guid`: no matches |
| `note_not_found` | 4 | `edit`: target `guid` absent; for `review-check`, proposal problem, not domain error |
| `ambiguous` | 5 | `find --guid`/`review --guid`: ambiguous GUID; for `edit`/`review-check` this is unreachable as an *exit code* because duplicate GUID is earlier rejected as `export_invalid` (6), though `review-check` proposals may show `invalid`/`ambiguous_guid` |
| `unknown_model` | 3 | `create`: no explicitly named or compatible `auto` model; error lists requested fields |
| `media_forbidden` | 3 | `create`: new field value contains a media reference (`[sound:…]`, media-bearing attribute in `ADDRESS_ATTRIBUTES`, or CSS address). Tag spelling does not matter; unfinished constructs fail. Explicitly configured verified `img/src` domain references are the exception |
| `unresolved_guid` | 3 | `retire`: requested GUID absent. An explicitly specified GUID in `create` yields `already_applied` when content matches or `guid_conflict` (6) otherwise |
| `unresolved_deck_identity` | 3 | `create --apply`: target deck has no `crowdanki_uuid` and cannot be stably addressed after write |
| `deck_identity_mismatch` | 3 | `models` / `create`: deck selectors refer to different nodes |
| `model_schema_unusable` | 6 | `create`: unusable model field schema (missing/duplicate/nonnumeric `ord`); `models` still reports it and its `schema_problems` |
| `guid_collision` | 6 | `create`: GUID repeated in request or generated GUID collides with an existing one and no free GUID found in allowed attempts |
| `guid_conflict` | 6 | `create`: existing GUID has different content; updating it belongs to `edit` |
| `ambiguous_model` | 5 | `create`: multiple `auto` candidates or ambiguous explicitly selected model name |
| `ambiguous_deck` | 5 | `models` / `create`: selector matched multiple deck nodes |
| `missing_field_value` | 3 | `models` / `create`: model requires a field omitted from request |
| `internal_error` | 70 | Internal error, including byte-level edit-invariant violation |

For `visual-report`, output-directory checks produce `invalid_request` with `details.reason` values: `out_dir_inside_decks` (`--out` in `decks/**`), `out_dir_overlaps_export` (within or equal to a compared export), `out_dir_not_owned` (nonempty directory without a valid report manifest), `out_dir_manifest_invalid` (corrupt, foreign, outside-pointing, or self-referential manifest), `out_dir_not_a_directory` (`--out` exists and is not a directory), `out_dir_is_symlink` (`--out` is a symbolic link), `out_dir_entry_conflict` (the report directory contains a symbolic link or a name that contradicts the ownership proof), `before_equals_after` (both states are the same directory), and `invalid_tag` (invalid `--retire-tag`; also used by `retire`).

Failures during report generation after directory checks are `write_failed` with `reason` values: `report_write_failed`, `media_copy_failed`, `preview_not_offline`, `index_not_offline`, `report_file_not_moved`, `stale_file_not_removed`, `staging_not_created`, `staging_destination_not_created`, `report_file_outside_out_dir`, `out_dir_entry_conflict` (a foreign entry appeared after preflight), `report_file_not_confined`, and `manifest_not_serializable`.

## Determinism

Identical inputs produce byte-identical outputs:

- Deck nodes are visited parent-first.
- Notes retain export order.
- Model fields appear in `ord` order.
- Distributions sort by decreasing count, then by value.
- `validate` issues sort by severity, code, deck path, location, and text.
- `edit` resolves targets in request order and serializes the canonical candidate with the same code that checks canonical form; identical inputs yield byte-identical candidates.
- `qa` and `review` findings sort by registry-rule position, export note position, then field `ord` and name, independently of discovery order.
- `models` sorts models by name, breaking ties with `crowdanki_uuid` (export traversal order is not part of the output), fields/templates by `ord`, and example values by export note order.
- `create` and `retire` resolve notes/decks in request order and build canonical candidates with the same canonicalizer.
- `visual-report` processes notes in export order, sorts diagnostics and media references, and derives preview filenames from indices rather than content. Identical states yield a byte-identical report directory without timestamps, version strings, or randomness.
- `review-check` preserves proposal-document order; `counts` are independent of whether entries were truncated from display.
- Failed edits leave `deck.json` byte-identical.

There is one explicit exception: `create` without a supplied `guid`. Generated GUIDs are random by design, so two identical requests yield different results and candidate bytes. `--emit-resolved` restores reproducibility: the resolved request contains concrete GUIDs, and a retry returns `already_applied` or `guid_conflict` instead of creating a second note.

## Limitations

- One export per invocation, by design.
- `edit` changes values of existing fields only; structure, identities (`guid`, `crowdanki_uuid`, `note_model_uuid`, `deck_config_uuid`), `tags`, `media_files`, and `media/` are outside its scope.
- `edit` requires canonical `deck.json`; normalization is a separate task.
- `create` adds notes only to existing decks and models; it neither creates/changes decks, models, fields, templates, and configurations nor moves notes among decks.
- `create` never edits existing notes; an existing GUID with different content yields `guid_conflict`.
- `create` forbids media references by default. Explicit domain rules allow only exact `img/src` references to verified domain images, and `--apply` copies only those bytes to export `media/` and declares their names in `media_files`. Audio, CSS media links, `srcset`, arbitrary images, unconfigured domains, and unconfigured fields remain forbidden.
- `migrate-media` is the sole method to modify a media reference in an existing note. It requires a proven identity/legacy-filename pair and a complete consumer inventory across fields, `qfmt`, `afmt`, and CSS. With `--apply`, it places verified canonical bytes, publishes updated references and declarations through the `children` tree, and deletes only the proven old file.
- `retire` only appends retirement tags, cannot remove tags or physically delete notes. Actual deletion belongs to a human using Anki.
- `visual-report` produces static previews for limited template constructs, not authentic Anki renders. It neither evaluates filters nor executes template JavaScript. Night mode, audio, and dynamic height implement only the stated subset, not full Anki WebView emulation.
- `visual-report` does not simulate CrowdAnki imports or predict Anki actions; it compares two states by GUID.
- The report directory is an artifact outside `decks/**`, contains no run identifiers, and does not automatically update itself.
- Unlike `create`/`migrate-media`, `visual-report` copies preview-specific media to the report directory without modifying exports. `create`/`migrate-media` may place only verified domain bytes in export `media/` under the narrow rules above; `migrate-media` may remove the exact legacy name after publication.
- Search has no regular expressions or HTML normalization.
- Field names are never hardcoded: the caller supplies `--field`, resolved against `note_models[].flds[].name`. Removed `--word` and `duplicate_primary_field` do not return.
- `--deck` expects the exact path constructed from deck-node `name` values.
- No semantic evaluation of Japanese content or cross-deck comparison. `qa` detects only enumerated structural/formatting issues; an external agent judges linguistic content.
- No LLM or network calls. `qa`/`review`/`review-check` stops at the repository boundary: an external agent brings proposals, and `anki-repo` validates them and supplies an `edit` request.
- Proposals can only express edits understood by `edit`: changes to existing field values of existing notes. `qa`/`review` do not propose creating, deleting, or renaming entities; `review-check` cannot accept such operations.
- Report truncation (per-code finding limits, review pages, edit/note limits, outcomes, preview files) is always explicit: counters cover all data, and text explains omitted material in both human and JSON modes.
- No mutating command expresses “delete”: `create` adds notes, `retire` appends a tag. A note missing from JSON is classified `removed` in `visual-report`, without requiring physical deletion.
- `review-check` checks proposals against current state but does not reserve it: data might change before `edit --apply`, in which case `edit` detects the precondition conflict.
- Unresolvable proposal targets (`field_not_resolvable`, `ambiguous_guid`) are nearly unreachable in normal operation because invalid field values, missing fields, or duplicate GUIDs are already `validate`-level `ERROR` and block the export as `export_invalid`. Their explicit classification and tests remain for source states that pass prevalidation.
- QA code/severity registry is not configurable; adding rules requires code and tests.

## Verification

Run all checks from the repository root containing the Cargo workspace (`/Cargo.toml`):

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

`--locked` is appropriate because the root `Cargo.lock` is shared by all workspace members.

CI (`.github/workflows/ci.yml`) runs the same checks independently of local setup on `pull_request`, push to the default branch, and manual dispatch. It also verifies user-data boundaries: no path under the fully ignored `decks/` or `.asset-store/` may be tracked, and ignore rules must exist without re-inclusion (`.github/scripts/check_local_data_untracked.py`). It inspects the actual Git index because `.gitignore` cannot untrack already indexed files and can be bypassed by `git add -f`.

CI deliberately does not validate user-owned content: those areas are absent in a clean checkout, and asserting their absence would not amount to testing private decks. Validate a real export locally with `anki-repo validate decks/output/<export_name>`, or a local corpus using `kanji-corpus-gate` / `pitch-corpus-gate` (see [`asset-store` documentation](../asset-store/README.en.md)). CI only reads the repository; adding or changing decks does not require CI changes.

Ordinary toolkit tests create synthetic exports and never read `decks/**`.

A dedicated MSRV job retrieves `[workspace.package] rust-version` via `cargo metadata`, verifies workspace members agree, and builds against it (`cargo +<msrv> check --workspace --all-targets --locked`). Neither CI nor this documentation duplicates the version. To reproduce locally:

```bash
MSRV=$(cargo metadata --no-deps --format-version 1 | jq -r '[.packages[].rust_version] | unique | .[]')
rustup toolchain install "$MSRV" --profile minimal
cargo "+$MSRV" check --workspace --all-targets --locked
```

Locally, `cargo nextest run` may replace `cargo test` when nextest is installed; CI uses `cargo test --workspace --locked`.

The test suite includes:

- Unit tests for domain logic: field resolution through `ord`, tree traversal, `find`/`stats`, template scanner, and exit-code contract.
- QA unit tests for each rule's boundaries (empty values, edge/internal whitespace, white `<span>`, duplicate groups), finding ordering and report truncation. A dedicated test fixes the five codes and their severities as a public contract and verifies name-independent behavior.
- `tests/synthetic.rs`: synthetic cases for each validation branch and domain error.
- `tests/export_contract.rs`: invariants for any well-formed CrowdAnki export. Counts, field order, and media are recomputed from raw JSON and directory contents. Fixtures deliberately differ in shape from real user decks; no test requires `decks/`.
- `tests/cli_contract.rs`: CLI arguments, exit codes, and `stdout`/`stderr` separation through a real binary, including removal of `--word`.
- `tests/local_data_contract.rs`: closed `decks/input` rejected everywhere with `input_area_forbidden`, readable exports under `decks/output`, and `migrate-layout` idempotency, name collisions, and byte preservation.
- `tests/qa_contract.rs`: exactly five QA rules (without `duplicate_primary_field`), nonfatal `error` severity, independence from `validate`/`edit`, bounded evidence, per-code limits versus full counts, determinism, explicit unaddressable findings, complete duplicate-group context, and independent recounting from raw synthetic `deck.json`, without hardcoded expected counts.
- `tests/review_contract.rs`: pagination size, complete traversal without loss or duplicates, empty beyond-end pages, compact packages, per-note finding caps, same `--guid` errors as `find`, duplicate-group inclusion up to `MAX_RELATED_NOTES`, explicit `excluded_unaddressable`, and refusal of `--word`.
- `tests/review_check_contract.rs`: all five proposal statuses; `stale`/`invalid` exit-code precedence (5 → 4 → 3); rejection of foreign `edits` documents and obsolete `--request`; all proposal `invalid_request` reasons (`malformed_proposals`, `unsupported_schema_version`, `empty_proposals`, `too_many_proposals`, `proposals_too_large`, `duplicate_proposal_id`); `proposal_id` to `edit_id`, `reason` retained only in report; no writes or temporary files; bounded reports with full counts; stdin; full `review → proposals → review-check → edit (dry-run/--apply) → validate` pipeline on a temporary export.
- `tests/edit_contract.rs`: dry-run and apply, all four edit outcomes, precondition conflicts, batch resolution failures, noncanonical/invalid export refusal, JSON response schema, exit codes, and equivalence of `--request FILE` and `--request -`.
- `tests/note_lifecycle_contract.rs`: synthetic schema → creation → retirement workflow through real CLI. Field evidence from `models`, including differing declaration and `ord` orders; canonical note write and structural validation; byte-level proof that only the appended note changed; idempotency via `--emit-resolved`; batch atomicity; all four `auto` selection rules (unique subtree compatible model, unique global fallback for empty deck with `model_evidence`, refusal of two compatible models and duplicate names); `media_forbidden`, `unknown_model`, `ambiguous_model`, `guid_conflict`, `unresolved_deck_identity`, `source_not_canonical`, `export_not_mutable`; tag-only changes and no duplicate tags; retained addressability; stdin.
- `tests/visual_report_contract.rs`: GUID-based outcomes, separate ambiguous-GUID diagnostics, offline safety for every file (only report-owned runtime, no network references), two state-specific frames with correct values, copying media/audio only for displayed previews, separate same-name media per state and no cross-state fallback, local replay and visible missing-audio warning, deterministic files with media, unmodified compared exports, no diff highlighting in card previews, model CSS isolated to its state's card, all model templates in separate preview files, output-directory refusal cases, unsupported constructs such as nonexecuted `<script>`, preview limits, and runtime behavior: `nightMode` on document root/body/card, synchronized theme, measured frame heights updated after media loading. Runtime JavaScript is tested in a minimal Node DOM harness and skipped when Node is unavailable.
- `tests/edit_source_preservation.rs`: one changed line per edit, unrelated bytes preserved, escaping/newlines/Unicode, idempotent no-rewrite behavior, noncanonical refusals, retained permissions, no temp files. Tests create full synthetic decks, including a six-field model and a one-field model in a child deck, instead of reading `decks/`.
- Concurrent publication tests: unit tests deterministically reproduce the compare-to-rename window with two writers from one snapshot (only one publishes; the other sees `source_changed`); a control demonstrates lost updates without the critical section; another verifies that publication waits for the lock; `tests/edit_source_preservation.rs` also runs two real concurrent `edit --apply` processes and checks the canonical result without lost edits.

The original race is guarded by three observable properties: `replace_atomically` leaves the winning update and rejects the losing writer with `source_changed`; the unlocked control demonstrates a lost update; and the lock test verifies that publication waits. The internal race window in `write_and_publish` cannot be triggered externally without adding a test seam, so the tests assert those properties rather than faking a “just about to publish” signal.
