//! Только синтетические экспорты и изолированные хранилища. Тестовая заглушка
//! задаёт данные о проверке жизненного цикла; алгоритм обработки пикселей
//! тестируется его владельцем.
use super::*;
use crate::ops::create::{MAX_REPORTED_NOTES, create_with_options, parse_request_bytes};
use crate::test_support::{MINIMAL_EXPORT, TempDir};
use asset_store::hashing::sha256_hex;
use asset_store::{
    AssetRecord, DetectedFormat, HumanAttestationRequest, HumanDecision, IngestRequest,
    LifecycleState, Provenance, SelectionMode, SemanticDecision, SemanticStatus, SemanticValidator,
    StoreOptions, ValidationEvidence, ValidatorFailure, ValidatorIdentity, VerifiedAssetBytes,
    VerifiedIngestRequest,
};
use serde_json::json;
use std::fs;

struct Fixture {
    temp: TempDir,
    export: PathBuf,
    options: MediaOptions,
}
struct Stub;
impl SemanticValidator for Stub {
    fn identity(&self) -> ValidatorIdentity {
        KanjiImageValidator::validator_identity()
    }
    fn validate(
        &self,
        _: &AssetRecord,
        _: &mut dyn Read,
    ) -> Result<SemanticDecision, ValidatorFailure> {
        Ok(SemanticDecision::new(
            SemanticStatus::Verified,
            vec![ValidationEvidence {
                kind: "synthetic".into(),
                summary: "контролируемые байты тестовой фикстуры".into(),
                details: None,
            }],
        ))
    }
}
/// Заглушка pitch-домена: заявляет идентификатор владельца домена, а сами байты
/// и отрисовка остаются предметом его собственных тестов.
struct PitchStub;
impl SemanticValidator for PitchStub {
    fn identity(&self) -> ValidatorIdentity {
        PitchAccentImageValidator::validator_identity()
    }
    fn validate(
        &self,
        _: &AssetRecord,
        _: &mut dyn Read,
    ) -> Result<SemanticDecision, ValidatorFailure> {
        Ok(SemanticDecision::new(
            SemanticStatus::Verified,
            vec![ValidationEvidence {
                kind: "synthetic".into(),
                summary: "контролируемые байты тестовой фикстуры".into(),
                details: None,
            }],
        ))
    }
}
impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new("configured-media");
        let export = temp.path().join("export");
        fs::create_dir(&export).unwrap();
        let value: Value = serde_json::from_str(MINIMAL_EXPORT).unwrap();
        fs::write(
            export.join("deck.json"),
            crate::loader::render_canonical_bytes(&value).unwrap(),
        )
        .unwrap();
        let config = temp.path().join("create.yaml");
        fs::write(&config, policy("model-1", "Заголовок")).unwrap();
        let options = MediaOptions {
            config: Some(config),
            asset_store: Some(temp.path().join("store")),
            pitch_asset_store: Some(temp.path().join("pitch-store")),
        };
        Self {
            temp,
            export,
            options,
        }
    }
    fn bytes(&self) -> Vec<u8> {
        fs::read(self.export.join("deck.json")).unwrap()
    }
    fn asset(&self, character: &str, bytes: &[u8], previous: Option<String>) -> String {
        let store =
            AssetStore::open_kanji(StoreOptions::new(self.options.asset_store.clone().unwrap()))
                .unwrap();
        let result = store
            .ingest_verified(
                VerifiedIngestRequest {
                    identity: AssetIdentity::new("kanji", character).unwrap(),
                    bytes: bytes.to_vec(),
                    provenance: Provenance {
                        source_kind: "local_import".into(),
                        source_name: "synthetic.gif".into(),
                    },
                    domain_metadata: None,
                    replace_expected_sha256: previous,
                },
                &Stub,
            )
            .unwrap();
        result.sha256
    }
    fn request(&self, fields: &[&str]) -> crate::ops::create::CreateRequest {
        let notes = fields.iter().enumerate().map(|(i, field)| json!({"guid": format!("New{i}"), "deck": {"crowdanki_uuid": "deck-uuid-1"}, "model": {"mode": "explicit", "crowdanki_uuid": "model-1"}, "fields": {"Заголовок": field, "Толкование": "значение"}, "tags": ["TEMP"]})).collect::<Vec<_>>();
        parse_request_bytes(
            &serde_json::to_vec(&json!({"schema_version":1,"notes":notes})).unwrap(),
            "synthetic",
        )
        .unwrap()
    }
    fn run(
        &self,
        fields: &[&str],
        apply: bool,
    ) -> Result<crate::ops::create::CreateResult, DomainError> {
        create_with_options(
            &self.export,
            &self.request(fields),
            apply,
            None,
            &self.options,
        )
    }
    fn resolved_request(&self, fields: &[&str]) -> crate::ops::create::CreateRequest {
        let result = self.run(fields, false).unwrap();
        parse_request_bytes(
            &serde_json::to_vec(&result.resolved_request).unwrap(),
            "resolved",
        )
        .unwrap()
    }
    fn set_policy(&self, yaml: &str) {
        fs::write(self.options.config.as_ref().unwrap(), yaml).unwrap();
    }
    fn pitch(&self, surface: &str, bytes: &[u8]) -> String {
        let store = AssetStore::open_with_policy(
            StoreOptions::new(self.options.pitch_asset_store.clone().unwrap()),
            PitchAccentDomainPolicy,
        )
        .unwrap();
        store
            .ingest_verified(
                VerifiedIngestRequest {
                    identity: AssetIdentity::new("pitch_accent", surface).unwrap(),
                    bytes: bytes.to_vec(),
                    provenance: Provenance {
                        source_kind: "jpdb_browser_capture".into(),
                        source_name: "synthetic.pitch.png".into(),
                    },
                    domain_metadata: Some(json!({
                        "surface": surface,
                        "reading": "よみかた",
                        "vocabulary_id": 1,
                    })),
                    replace_expected_sha256: None,
                },
                &PitchStub,
            )
            .unwrap()
            .sha256
    }
    /// Прогон запроса с явными парами «имя поля → значение»: нужен там, где
    /// правила включают разные обработчики на разные поля одной модели.
    fn run_fields(
        &self,
        values: &[(&str, &str)],
        apply: bool,
    ) -> Result<crate::ops::create::CreateResult, DomainError> {
        self.run_fields_with(values, apply, &self.options)
    }
    fn run_fields_with(
        &self,
        values: &[(&str, &str)],
        apply: bool,
        options: &MediaOptions,
    ) -> Result<crate::ops::create::CreateResult, DomainError> {
        let fields = values
            .iter()
            .map(|(name, value)| ((*name).to_string(), json!(value)))
            .collect::<serde_json::Map<_, _>>();
        let notes = vec![json!({
            "guid": "New0",
            "deck": {"crowdanki_uuid": "deck-uuid-1"},
            "model": {"mode": "explicit", "crowdanki_uuid": "model-1"},
            "fields": fields,
            "tags": ["TEMP"],
        })];
        let request = parse_request_bytes(
            &serde_json::to_vec(&json!({"schema_version": 1, "notes": notes})).unwrap(),
            "synthetic",
        )
        .unwrap();
        create_with_options(&self.export, &request, apply, None, options)
    }
}
const GIF: &[u8] = b"GIF89a-synthetic-one";
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n-synthetic";
// Полный GIF размером 1×1: подтверждение человеком проверяет декодирование,
// а не только сигнатуру.
// Полный PNG 1×1: подтверждение человеком проверяет декодирование, а не только
// сигнатуру.
const DECODABLE_PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01\x00\x00\x00\x01\x08\x00\x00\x00\x00\x3a\x7e\x9b\x55\x00\x00\x00\x0aIDATx\x9ccp\x00\x00\x00\x42\x00\x41\x29\x37\xf4\xef\x00\x00\x00\x00IEND\xae\x42\x60\x82";
const DECODABLE_GIF: &[u8] = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff\x21\xf9\x04\x01\x00\x00\x00\x00\x2c\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02\x44\x01\x00\x3b";
fn policy(uuid: &str, field: &str) -> String {
    format!(
        "schema_version: 1\nnote_models:\n  - crowdanki_uuid: '{uuid}'\n    fields:\n      {field}:\n        processors:\n          - type: kanji_assets\n"
    )
}
fn pitch_policy(uuid: &str, field: &str) -> String {
    format!(
        "schema_version: 1\nnote_models:\n  - crowdanki_uuid: '{uuid}'\n    fields:\n      {field}:\n        processors:\n          - type: pitch_accent\n"
    )
}
fn dual_policy(uuid: &str, kanji_field: &str, pitch_field: &str) -> String {
    format!(
        "schema_version: 1\nnote_models:\n  - crowdanki_uuid: '{uuid}'\n    fields:\n      {kanji_field}:\n        processors:\n          - type: kanji_assets\n      {pitch_field}:\n        processors:\n          - type: pitch_accent\n"
    )
}
fn reason(error: &DomainError, expected: &str) {
    assert_eq!(error.details["reason"], expected, "{error:?}");
}

fn verified_asset_fixture(
    identity: AssetIdentity,
    storage_path: &str,
    consumer_filename: &str,
    bytes: &[u8],
) -> VerifiedAssetBytes {
    let format = DetectedFormat::from_signature(bytes);
    VerifiedAssetBytes {
        record: AssetRecord {
            identity,
            storage_path: storage_path.into(),
            consumer_filename: consumer_filename.into(),
            sha256: sha256_hex(bytes),
            byte_length: bytes.len() as u64,
            format,
            provenance: Provenance {
                source_kind: "fixture".into(),
                source_name: "verified-media.bin".into(),
            },
            lifecycle: LifecycleState::Verified,
            validation: None,
            human_attestation: None,
            domain_metadata: None,
        },
        bytes: bytes.to_vec(),
    }
}

/// Только внутренние тестовые реализации: ради проверки композиции в YAML
/// не добавляются новые типы обработчиков.
struct TestProcessor<'a> {
    name: &'static str,
    filename: &'static str,
    identity: &'static str,
    calls: &'a std::cell::RefCell<Vec<&'static str>>,
}
impl MediaProcessor for TestProcessor<'_> {
    fn name(&self) -> &'static str {
        self.name
    }
    fn claim(&self, reference: &media::MediaReference) -> Option<AssetIdentity> {
        self.calls.borrow_mut().push(self.name);
        (reference.value == self.filename)
            .then(|| AssetIdentity::new("kanji", self.identity).unwrap())
    }
}

#[test]
fn processor_list_schema_accepts_multiple_definitions_and_duplicate_validation_is_explicit() {
    let yaml = policy("model-1", "Заголовок").replace(
        "          - type: kanji_assets",
        "          - type: kanji_assets\n          - type: kanji_assets",
    );
    // Размер списка не является ограничением структуры YAML. Повтор одного
    // конкретного типа отвергает отдельная предметная проверка до исполнения.
    let parsed: Policy = serde_yaml::from_str(&yaml).unwrap();
    assert_eq!(
        parsed.note_models[0].fields["Заголовок"].processors.len(),
        2
    );
    let error = Routing::parse(yaml.as_bytes()).unwrap_err();
    reason(&error, "config_invalid");
    assert_eq!(
        error.details["evidence"]["duplicate_processor"],
        "kanji_assets"
    );

    let f = Fixture::new();
    fs::write(f.options.config.as_ref().unwrap(), yaml).unwrap();
    let before = f.bytes();
    reason(
        &f.run(&["<img src=一.gif>"], true).unwrap_err(),
        "config_invalid",
    );
    assert_eq!(before, f.bytes());
    assert!(!f.export.join("media").exists());
    assert!(!f.options.asset_store.as_ref().unwrap().exists());
}

#[test]
fn configured_empty_processor_chain_is_invalid_even_for_media_free_fields() {
    let f = Fixture::new();
    fs::write(
        f.options.config.as_ref().unwrap(),
        policy("model-1", "Заголовок").replace(
            "processors:\n          - type: kanji_assets",
            "processors: []",
        ),
    )
    .unwrap();
    reason(&f.run(&["слово"], true).unwrap_err(), "config_invalid");
    assert!(!f.export.join("media").exists());
}

#[test]
fn processor_engine_composes_two_owners_in_configured_order() {
    let calls = std::cell::RefCell::new(Vec::new());
    let first = TestProcessor {
        name: "first",
        filename: "一.gif",
        identity: "一",
        calls: &calls,
    };
    let second = TestProcessor {
        name: "second",
        filename: "二.png",
        identity: "二",
        calls: &calls,
    };
    let value = "<img src=二.png><img src=一.gif>";
    let refs = collect_processor_chain(&[&first, &second], 7, "model", "field", value).unwrap();
    assert_eq!(*calls.borrow(), vec!["first", "first", "second", "second"]);
    assert_eq!(refs.len(), 2);
    assert_eq!(refs[0].filename, "二.png");
    assert_eq!(refs[0].identity, AssetIdentity::new("kanji", "二").unwrap());
    assert_eq!(refs[1].filename, "一.gif");
    assert_eq!(refs[1].identity, AssetIdentity::new("kanji", "一").unwrap());
    assert_eq!(refs[1].note_index, 7);
    assert_eq!(refs[1].model_uuid, "model");
    assert_eq!(refs[1].field, "field");
    calls.borrow_mut().clear();
    let reversed = collect_processor_chain(&[&second, &first], 7, "model", "field", value).unwrap();
    assert_eq!(*calls.borrow(), vec!["second", "second", "first", "first"]);
    assert_eq!(
        serde_json::to_value(refs).unwrap(),
        serde_json::to_value(reversed).unwrap()
    );
}

#[test]
fn processor_engine_rejects_conflicting_claims_and_unclaimed_media() {
    let calls = std::cell::RefCell::new(Vec::new());
    let first = TestProcessor {
        name: "first",
        filename: "一.gif",
        identity: "一",
        calls: &calls,
    };
    let conflict = TestProcessor {
        name: "conflict",
        filename: "一.gif",
        identity: "二",
        calls: &calls,
    };
    let error = collect_processor_chain(
        &[&first, &conflict],
        0,
        "model",
        "field",
        "<img src=一.gif>",
    )
    .unwrap_err();
    reason(&error, "media_reference_conflict");
    assert_eq!(
        error.details["evidence"]["processors"],
        json!(["first", "conflict"])
    );
    for value in [
        "<img src=一.gif><img src=三.gif>",
        "<img src=一.gif>[sound:一.gif]",
        "<img src=一.gif><style>x{background:url(一.gif)}</style>",
    ] {
        reason(
            &collect_processor_chain(&[&first], 0, "model", "field", value).unwrap_err(),
            "media_reference_unclaimed",
        );
    }
    reason(
        &collect_processor_chain(&[], 0, "model", "field", "<img src=一.gif>").unwrap_err(),
        "media_reference_unclaimed",
    );
}

#[test]
fn create_reads_human_approved_exact_bytes_through_asset_owner_api() {
    struct NonVerified(SemanticStatus);
    impl SemanticValidator for NonVerified {
        fn identity(&self) -> ValidatorIdentity {
            KanjiImageValidator::validator_identity()
        }
        fn validate(
            &self,
            _: &AssetRecord,
            _: &mut dyn Read,
        ) -> Result<SemanticDecision, ValidatorFailure> {
            Ok(SemanticDecision::new(
                self.0,
                vec![ValidationEvidence {
                    kind: "synthetic".into(),
                    summary: "синтетическое спорное semantic решение".into(),
                    details: None,
                }],
            ))
        }
    }
    for status in [SemanticStatus::Uncertain, SemanticStatus::Rejected] {
        let f = Fixture::new();
        let root = f.options.asset_store.as_ref().unwrap();
        let store = AssetStore::open_kanji(StoreOptions::new(root.clone())).unwrap();
        let source = f.temp.path().join("candidate.gif");
        fs::write(&source, DECODABLE_GIF).unwrap();
        let identity = AssetIdentity::new("kanji", "一").unwrap();
        let pending = store
            .ingest(IngestRequest {
                identity: identity.clone(),
                source_path: source,
                expected_source_sha256: None,
                domain_metadata: None,
                replace_expected_sha256: None,
            })
            .unwrap();
        store
            .validate(SelectionMode::New, &NonVerified(status))
            .unwrap();
        assert!(f.run(&["<img src=一.gif>"], false).is_err());
        let approved = store
            .attest(HumanAttestationRequest {
                identity: identity.clone(),
                expected_sha256: pending.asset.sha256.clone(),
                decision: HumanDecision::Approve,
                reason: "пользователь подтвердил точный candidate в review".into(),
            })
            .unwrap();
        assert_eq!(approved.asset.validation.as_ref().unwrap().status, status);
        assert_eq!(
            approved.asset.effective_status(),
            Some(SemanticStatus::Verified)
        );
        let verified = AssetStore::read_verified_with_policy(
            root,
            &[identity],
            &KanjiImageValidator::validator_identity(),
            &asset_store::KanjiDomainPolicy,
        )
        .unwrap();
        assert_eq!(verified[0].bytes, DECODABLE_GIF);
        let before = f.bytes();
        let dry = f.run(&["<img src=一.gif>"], false).unwrap();
        assert_eq!(dry.media.pins()[0].sha256, pending.asset.sha256);
        assert_eq!(dry.media.evidence()["mutations_planned"], 1);
        assert_eq!(before, f.bytes());
        assert!(!f.export.join("media").exists());
        let applied = f.run(&["<img src=一.gif>"], true).unwrap();
        assert_eq!(applied.notes_created, 1);
        assert_eq!(
            fs::read(f.export.join("media/一.gif")).unwrap(),
            DECODABLE_GIF
        );
        let repeated = f.run(&["<img src=一.gif>"], true).unwrap();
        assert_eq!(repeated.notes_already_applied, 1);
        assert_eq!(repeated.media.mutations, 0);
    }
}

#[test]
fn config_free_and_configured_media_free_have_no_media_side_effects() {
    let f = Fixture::new();
    let r = f.run(&["слово"], false).unwrap();
    assert!(r.media.is_empty());
    assert!(!f.export.join("media").exists());
    assert!(!f.options.asset_store.as_ref().unwrap().exists());
    fs::remove_file(f.options.config.as_ref().unwrap()).unwrap();
    let r = create_with_options(
        &f.export,
        &f.request(&["слово"]),
        true,
        None,
        &MediaOptions::default(),
    )
    .unwrap();
    assert_eq!(r.notes_created, 1);
    assert!(!f.export.join("media").exists());
    let err = create_with_options(
        &f.export,
        &f.request(&["<img src='一.gif'>"]),
        false,
        None,
        &MediaOptions::default(),
    )
    .unwrap_err();
    reason(&err, "config_absent");
}
#[test]
fn batch_deduplicates_exact_assets_and_repairs_media_and_declaration() {
    let f = Fixture::new();
    f.asset("一", GIF, None);
    f.asset("二", PNG, None);
    let before = f.bytes();
    let fields = ["<IMG SRC = '一.gif'>", "<img src=一.gif><img src=二.png>"];
    let dry = f.run(&fields, false).unwrap();
    assert_eq!(dry.media.evidence()["mutations_planned"], 2);
    assert_eq!(dry.media.references.len(), 3);
    assert_eq!(before, f.bytes());
    assert!(!f.export.join("media").exists());
    let applied = f.run(&fields, true).unwrap();
    assert_eq!(applied.media.mutations, 2);
    assert_eq!(applied.notes_created, 2);
    assert_eq!(fs::read(f.export.join("media/一.gif")).unwrap(), GIF);
    let value: Value = serde_json::from_slice(&f.bytes()).unwrap();
    assert_eq!(
        value["media_files"],
        json!(["a.mp3", "b.png", "一.gif", "二.png"])
    );
    let after = f.bytes();
    let repeat = f.run(&fields, true).unwrap();
    assert!(!repeat.applied);
    assert_eq!(repeat.notes_already_applied, 2);
    assert_eq!(repeat.media.mutations, 0);
    assert_eq!(after, f.bytes());
    fs::remove_file(f.export.join("media/一.gif")).unwrap();
    let repaired = f.run(&fields, true).unwrap();
    assert_eq!(repaired.media.mutations, 1);
    assert!(!repaired.applied);
    assert_eq!(after, f.bytes());
    let mut damaged = value.clone();
    damaged["media_files"] = json!(["a.mp3", "b.png", "二.png"]);
    fs::write(
        f.export.join("deck.json"),
        crate::loader::render_canonical_bytes(&damaged).unwrap(),
    )
    .unwrap();
    let repaired = f.run(&fields, true).unwrap();
    assert!(repaired.applied);
    assert_eq!(repaired.notes_created, 0);
    assert_eq!(repaired.media.mutations, 0);
    assert_eq!(repaired.media.declarations_added, vec!["一.gif"]);
    let current = f.bytes();
    assert!(!f.run(&fields, true).unwrap().applied);
    assert_eq!(current, f.bytes());
}
#[test]
fn config_routing_depends_on_identity_and_exact_field_only() {
    let f = Fixture::new();
    fs::write(
        f.options.config.as_ref().unwrap(),
        policy("other-model", "Заголовок"),
    )
    .unwrap();
    reason(
        &f.run(&["<img src=一.gif>"], false).unwrap_err(),
        "processor_not_enabled",
    );
    f.run(&["обычный текст"], false).unwrap(); // неизвестная foreign модель не блокирует.
    fs::write(
        f.options.config.as_ref().unwrap(),
        policy("model-1", "Толкование"),
    )
    .unwrap();
    reason(
        &f.run(&["<img src=一.gif>"], false).unwrap_err(),
        "processor_not_enabled",
    );
    fs::write(
        f.options.config.as_ref().unwrap(),
        policy("model-1", "УдалённоеПоле"),
    )
    .unwrap();
    reason(
        &f.run(&["обычный текст"], false).unwrap_err(),
        "config_stale_field",
    );
}
#[test]
fn invalid_yaml_fails_closed_including_duplicate_mapping_keys() {
    let f = Fixture::new();
    let good = policy("model-1", "Заголовок");
    let cases = vec![
        good.replace("schema_version: 1", "schema_version: 2"),
        good.replace("kanji_assets", "unknown"),
        good.replace("schema_version: 1", "schema_version: 1\nschema_version: 1"),
        good.replace(
            "crowdanki_uuid: 'model-1'",
            "crowdanki_uuid: 'model-1'\n    crowdanki_uuid: 'model-1'",
        ),
        good.replace(
            "          - type: kanji_assets",
            "          - type: kanji_assets\n          - type: kanji_assets",
        ),
        format!("{good}      Заголовок:\n        processors:\n          - type: kanji_assets\n"),
        format!("{good}{}", good.split("note_models:\n").nth(1).unwrap()),
        good.replace(
            "type: kanji_assets",
            "type: kanji_assets\n            unexpected: true",
        ),
        good.replace(
            "type: kanji_assets",
            "type: kanji_assets\n            type: kanji_assets",
        ),
        "schema_version: 1\nnote_models: [".into(),
        format!("{good}---\n{good}"),
    ];
    for case in cases {
        fs::write(f.options.config.as_ref().unwrap(), &case).unwrap();
        let before = f.bytes();
        let e = match f.run(&["обычный текст"], true) {
            Err(e) => e,
            Ok(_) => panic!("не отклонён YAML: {case}"),
        };
        assert!(
            matches!(
                e.details["reason"].as_str(),
                Some("config_invalid" | "config_unsupported")
            ),
            "{case}: {e:?}"
        );
        assert_eq!(before, f.bytes());
    }
}
#[test]
fn configured_field_claims_only_complete_kanji_img_src() {
    let f = Fixture::new();
    for value in [
        "<img src=x.png>",
        "<audio src=一.gif>",
        "[sound:一.gif]",
        "<img srcset='一.gif 1x'>",
        "<div style='background:url(一.gif)'>",
        "<img src=../一.gif>",
        "<img src=/一.gif>",
        "<img src=https://host/一.gif>",
        "<img src=一.gif?q>",
        "<img src=一.gif#x>",
        "<img src='一.gif'",
        "<img src=一.gif><audio src=一.gif>",
        "<img src=一.gif><style>x{background:url(一.gif)}</style>",
    ] {
        let e = f.run(&[value], true).unwrap_err();
        reason(&e, "media_reference_unclaimed");
        assert!(!f.export.join("media").exists());
    }
}
#[test]
fn missing_integrity_nonverified_and_filename_mismatch_write_nothing() {
    let f = Fixture::new();
    let before = f.bytes();
    reason(
        &f.run(&["<img src=一.gif>"], true).unwrap_err(),
        "kanji_asset_missing",
    );
    f.asset("一", PNG, None);
    reason(
        &f.run(&["<img src=一.gif>"], true).unwrap_err(),
        "canonical_filename_mismatch",
    );
    let path = f
        .options
        .asset_store
        .as_ref()
        .unwrap()
        .join("assets/png/一.png");
    fs::remove_file(&path).unwrap();
    fs::write(&path, b"corrupt").unwrap();
    reason(
        &f.run(&["<img src=一.png>"], true).unwrap_err(),
        "asset_integrity_invalid",
    );
    assert_eq!(f.bytes(), before);
    assert!(!f.export.join("media").exists());
}
#[test]
fn pending_and_quarantine_are_never_used_as_canonical() {
    use asset_store::IngestRequest;
    let f = Fixture::new();
    let source = f.temp.path().join("pending.gif");
    fs::write(&source, GIF).unwrap();
    let store =
        AssetStore::open_kanji(StoreOptions::new(f.options.asset_store.clone().unwrap())).unwrap();
    store
        .ingest(IngestRequest {
            identity: AssetIdentity::new("kanji", "一").unwrap(),
            source_path: source,
            expected_source_sha256: None,
            domain_metadata: None,
            replace_expected_sha256: None,
        })
        .unwrap();
    reason(
        &f.run(&["<img src=一.gif>"], true).unwrap_err(),
        "kanji_asset_missing",
    );
    let manifest_path = store.root().join(".runtime/manifest.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["assets"][0]["lifecycle"] = json!("quarantined");
    fs::write(manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    reason(
        &f.run(&["<img src=一.gif>"], true).unwrap_err(),
        "kanji_asset_missing",
    );
}
#[test]
fn resolved_artifact_pins_hash_and_rejects_changed_corpus() {
    let f = Fixture::new();
    let old = f.asset("一", GIF, None);
    let result = f.run(&["<img src=一.gif>"], false).unwrap();
    assert_eq!(result.resolved_request["media_assets"][0]["sha256"], old);
    let request = parse_request_bytes(
        &serde_json::to_vec(&result.resolved_request).unwrap(),
        "resolved",
    )
    .unwrap();
    fs::create_dir(f.export.join("media")).unwrap();
    fs::write(f.export.join("media/一.gif"), GIF).unwrap();
    f.asset("一", b"GIF89a-synthetic-two", Some(old));
    let before = f.bytes();
    reason(
        &create_with_options(&f.export, &request, true, None, &f.options).unwrap_err(),
        "stale_pinned_asset",
    );
    assert_eq!(before, f.bytes());
    assert_eq!(fs::read(f.export.join("media/一.gif")).unwrap(), GIF);
    fs::write(f.export.join("media/一.gif"), b"GIF89a-synthetic-two").unwrap();
    reason(
        &create_with_options(&f.export, &request, true, None, &f.options).unwrap_err(),
        "stale_pinned_asset",
    );
    fs::write(f.export.join("media/一.gif"), GIF).unwrap();
    reason(
        &f.run(&["<img src=一.gif>"], true).unwrap_err(),
        "destination_media_conflict",
    );
    fs::remove_file(f.export.join("media/一.gif")).unwrap();
    f.run(&["<img src=一.gif>"], true).unwrap();
}

#[test]
fn pinned_missing_asset_and_filename_drift_are_stale_before_destination_checks() {
    let f = Fixture::new();
    f.asset("一", GIF, None);
    let request = f.resolved_request(&["<img src=一.gif>"]);
    fs::create_dir(f.export.join("media")).unwrap();
    fs::write(f.export.join("media/一.gif"), b"foreign destination").unwrap();
    fs::remove_file(
        f.options
            .asset_store
            .as_ref()
            .unwrap()
            .join("assets/gif/一.gif"),
    )
    .unwrap();
    reason(
        &create_with_options(&f.export, &request, true, None, &f.options).unwrap_err(),
        "stale_pinned_asset",
    );

    // Форматы — независимые записи: появление доверенного GIF рядом с PNG
    // меняет предпочтительное имя файла, и закреплённый запрос устаревает.
    let f = Fixture::new();
    f.asset("一", PNG, None);
    let request = f.resolved_request(&["<img src=一.png>"]);
    fs::create_dir(f.export.join("media")).unwrap();
    fs::write(f.export.join("media/一.png"), PNG).unwrap();
    f.asset("一", GIF, None);
    reason(
        &create_with_options(&f.export, &request, true, None, &f.options).unwrap_err(),
        "stale_pinned_asset",
    );
}

#[test]
fn missing_store_is_stale_for_pinned_requests_but_missing_for_unpinned_requests() {
    let f = Fixture::new();
    f.asset("一", GIF, None);
    let pinned = f.resolved_request(&["<img src=一.gif>"]);
    fs::remove_dir_all(f.options.asset_store.as_ref().unwrap()).unwrap();

    reason(
        &create_with_options(&f.export, &pinned, false, None, &f.options).unwrap_err(),
        "stale_pinned_asset",
    );
    reason(
        &f.run(&["<img src=一.gif>"], false).unwrap_err(),
        "kanji_asset_missing",
    );
}

#[test]
fn resolved_empty_pins_are_distinct_from_legacy_unpinned_requests() {
    let f = Fixture::new();
    f.asset("一", GIF, None);
    let legacy = f.request(&["обычный текст"]);
    assert_eq!(legacy.media_assets, None);

    let mut resolved = f.resolved_request(&["обычный текст"]);
    assert_eq!(resolved.media_assets, Some(Vec::new()));
    let mut null_pins = f.run(&["обычный текст"], false).unwrap().resolved_request;
    null_pins["media_assets"] = serde_json::Value::Null;
    assert!(parse_request_bytes(&serde_json::to_vec(&null_pins).unwrap(), "null").is_err());
    resolved.notes[0]
        .fields
        .insert("Заголовок".into(), "<img src=一.gif>".into());
    reason(
        &create_with_options(&f.export, &resolved, false, None, &f.options).unwrap_err(),
        "stale_pinned_asset",
    );
    let no_store = MediaOptions {
        config: f.options.config.clone(),
        asset_store: None,
        pitch_asset_store: f.options.pitch_asset_store.clone(),
    };
    reason(
        &create_with_options(&f.export, &resolved, false, None, &no_store).unwrap_err(),
        "stale_pinned_asset",
    );
}
#[test]
fn destination_conflict_and_symlinks_never_overwrite() {
    let f = Fixture::new();
    f.asset("一", GIF, None);
    fs::create_dir(f.export.join("media")).unwrap();
    let destination = f.export.join("media/一.gif");
    fs::write(&destination, b"foreign").unwrap();
    let before = f.bytes();
    reason(
        &f.run(&["<img src=一.gif>"], true).unwrap_err(),
        "destination_media_conflict",
    );
    assert_eq!(fs::read(&destination).unwrap(), b"foreign");
    assert_eq!(before, f.bytes());
    fs::remove_file(&destination).unwrap();
    std::os::unix::fs::symlink(
        f.options
            .asset_store
            .as_ref()
            .unwrap()
            .join("assets/gif/一.gif"),
        &destination,
    )
    .unwrap();
    reason(
        &f.run(&["<img src=一.gif>"], true).unwrap_err(),
        "destination_media_conflict",
    );
}

#[test]
fn media_evidence_caps_references_and_reports_truncation() {
    let identity = AssetIdentity::new("kanji", "一").unwrap();
    let references = (0..=MAX_REPORTED_NOTES)
        .map(|note_index| Reference {
            note_index,
            model_uuid: "model".into(),
            field: "Expression".into(),
            filename: "一.gif".into(),
            identity: identity.clone(),
        })
        .collect();
    let plan = MediaPlan {
        references,
        ..MediaPlan::default()
    };

    let evidence = plan.evidence();
    assert_eq!(
        evidence["references"].as_array().unwrap().len(),
        MAX_REPORTED_NOTES
    );
    assert_eq!(evidence["references_total"], MAX_REPORTED_NOTES + 1);
    assert!(evidence["references_truncated"].as_bool().unwrap());
}

#[test]
fn nested_storage_paths_keep_the_explicit_consumer_filename_flat_in_media() {
    let cases = [
        (
            AssetIdentity::new("kanji", "漢").unwrap(),
            "assets/gif/漢.gif",
            "漢.gif",
            GIF,
        ),
        (
            AssetIdentity::new("kanji", "饅").unwrap(),
            "assets/png/饅.png",
            "饅.png",
            PNG,
        ),
        (
            AssetIdentity::new("pitch_accent", "幽霊").unwrap(),
            "assets/png/幽霊.png",
            "幽霊.pitch.png",
            PNG,
        ),
    ];

    for (identity, storage_path, consumer_filename, bytes) in cases {
        let f = Fixture::new();
        let asset =
            verified_asset_fixture(identity.clone(), storage_path, consumer_filename, bytes);
        assert_eq!(verified_asset_filename(&asset).unwrap(), consumer_filename);

        let mut plan = MediaPlan {
            items: vec![Item {
                pin: Pin {
                    identity,
                    filename: consumer_filename.into(),
                    sha256: asset.record.sha256.clone(),
                },
                action: "copy".into(),
                asset,
            }],
            ..MediaPlan::default()
        };
        let guard = crate::write::ExportLock::acquire(&f.export).unwrap();
        plan.materialize(&guard).unwrap();

        assert_eq!(
            fs::read(f.export.join("media").join(consumer_filename)).unwrap(),
            bytes
        );
        assert!(!f.export.join("media/gif").exists());
        assert!(!f.export.join("media/png").exists());
    }
}

#[test]
fn unsafe_consumer_filenames_and_format_mismatches_fail_closed() {
    let identity = AssetIdentity::new("pitch_accent", "幽霊").unwrap();
    for filename in [
        "",
        ".",
        "..",
        "../幽霊.png",
        "nested/幽霊.png",
        r"nested\幽霊.png",
        "幽\u{0}霊.png",
        "幽\n霊.png",
        ".png",
        "幽霊.gif",
    ] {
        let asset = verified_asset_fixture(identity.clone(), "assets/png/幽霊.png", filename, PNG);
        let error = verified_asset_filename(&asset).unwrap_err();
        assert_eq!(error.code, ErrorCode::Internal, "имя файла={filename:?}");
        assert_eq!(
            error.details["reason"], "mutation_internal_invariant",
            "имя файла={filename:?}"
        );
    }

    let wrong_detected_format = verified_asset_fixture(
        identity.clone(),
        "assets/png/幽霊.png",
        "幽霊.pitch.png",
        GIF,
    );
    let error = verified_asset_filename(&wrong_detected_format).unwrap_err();
    assert_eq!(error.code, ErrorCode::Internal);

    let mut wrong_recorded_format =
        verified_asset_fixture(identity.clone(), "assets/png/幽霊.png", "幽霊.gif", PNG);
    wrong_recorded_format.record.format = DetectedFormat::Gif;
    let error = verified_asset_filename(&wrong_recorded_format).unwrap_err();
    assert_eq!(error.code, ErrorCode::Internal);

    let unknown_format = verified_asset_fixture(
        identity,
        "assets/bin/幽霊.bin",
        "幽霊.bin",
        b"synthetic unknown bytes",
    );
    let error = verified_asset_filename(&unknown_format).unwrap_err();
    assert_eq!(error.code, ErrorCode::Internal);
}

#[test]
fn materialization_stages_verified_bytes_outside_media() {
    let f = Fixture::new();
    f.asset("一", GIF, None);
    let export = f.export.clone();
    TEST_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move |phase| {
            if phase == "before_link" {
                let staged_in_export = fs::read_dir(&export).unwrap().any(|entry| {
                    entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".anki-repo-media-")
                });
                let staged_in_media = fs::read_dir(export.join("media")).unwrap().any(|entry| {
                    entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".anki-repo-media-")
                });
                if !staged_in_export || staged_in_media {
                    return Err(blocker(
                        ErrorCode::Internal,
                        "test_staging_boundary_failed",
                        json!({}),
                    ));
                }
            }
            Ok(())
        }));
    });
    let result = f.run(&["<img src=一.gif>"], true);
    TEST_HOOK.with(|hook| *hook.borrow_mut() = None);
    let result = result.unwrap();
    assert_eq!(result.notes_created, 1);
    let human = crate::render::human::create(&result);
    assert!(human.contains(
        "План медиафайлов: ссылок=1, проверенных файлов=1, новых объявлений media_files=1"
    ));
    assert!(!human.contains("\"references\""));

    for directory in [f.export.clone(), f.export.join("media")] {
        assert!(!fs::read_dir(directory).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".anki-repo-media-")
        }));
    }
    assert_eq!(fs::read(f.export.join("media/一.gif")).unwrap(), GIF);
}

#[test]
fn concurrent_destination_appearance_same_bytes_reuses_different_bytes_conflicts() {
    for bytes in [GIF, b"foreign".as_slice()] {
        let f = Fixture::new();
        f.asset("一", GIF, None);
        let before = f.bytes();
        let destination = f.export.join("media/一.gif");
        let owned = bytes.to_vec();
        TEST_HOOK.with(|h| {
            *h.borrow_mut() = Some(Box::new(move |phase| {
                if phase == "before_link" {
                    fs::write(&destination, &owned).unwrap();
                }
                Ok(())
            }))
        });
        let result = f.run(&["<img src=一.gif>"], true);
        TEST_HOOK.with(|h| *h.borrow_mut() = None);
        if bytes == GIF {
            let r = result.unwrap();
            assert_eq!(r.media.mutations, 0);
            assert_eq!(r.notes_created, 1);
        } else {
            reason(&result.unwrap_err(), "destination_media_conflict");
            assert_eq!(before, f.bytes());
        }
    }
}
#[test]
fn partial_media_failure_does_not_publish_json_and_retry_converges() {
    let f = Fixture::new();
    f.asset("一", GIF, None);
    f.asset("二", PNG, None);
    let before = f.bytes();
    TEST_HOOK.with(|h| {
        *h.borrow_mut() = Some(Box::new(|phase| {
            if phase == "asset_copied" {
                Err(blocker(
                    ErrorCode::WriteFailed,
                    "injected_io_failure",
                    json!({}),
                ))
            } else {
                Ok(())
            }
        }))
    });
    let result = f.run(&["<img src=一.gif><img src=二.png>"], true);
    TEST_HOOK.with(|h| *h.borrow_mut() = None);
    reason(&result.unwrap_err(), "injected_io_failure");
    assert_eq!(before, f.bytes());
    assert!(f.export.join("media/一.gif").exists());
    assert!(!f.export.join("media/二.png").exists());
    let repaired = f.run(&["<img src=一.gif><img src=二.png>"], true).unwrap();
    assert_eq!(repaired.media.mutations, 1);
    assert!(
        !f.run(&["<img src=一.gif><img src=二.png>"], true)
            .unwrap()
            .applied
    );
}
#[test]
fn deck_publication_failure_leaves_safe_orphan_and_retry_converges() {
    let f = Fixture::new();
    f.asset("一", GIF, None);
    let before = f.bytes();
    TEST_HOOK.with(|h| {
        *h.borrow_mut() = Some(Box::new(|phase| {
            if phase == "before_deck_publish" {
                Err(blocker(
                    ErrorCode::WriteFailed,
                    "injected_publish_failure",
                    json!({}),
                ))
            } else {
                Ok(())
            }
        }))
    });
    let result = f.run(&["<img src=一.gif>"], true);
    TEST_HOOK.with(|h| *h.borrow_mut() = None);
    reason(&result.unwrap_err(), "injected_publish_failure");
    assert_eq!(before, f.bytes());
    assert_eq!(fs::read(f.export.join("media/一.gif")).unwrap(), GIF);
    let r = f.run(&["<img src=一.gif>"], true).unwrap();
    assert_eq!(r.media.mutations, 0);
    assert_eq!(r.notes_created, 1);
}
#[test]
fn source_changed_after_media_copy_refuses_publication() {
    let f = Fixture::new();
    f.asset("一", GIF, None);
    let source = f.export.join("deck.json");
    let mut external = f.bytes();
    external.push(b'\n');
    let edited = external.clone();
    TEST_HOOK.with(|h| {
        *h.borrow_mut() = Some(Box::new(move |phase| {
            if phase == "before_deck_publish" {
                fs::write(&source, &external).unwrap();
            }
            Ok(())
        }))
    });
    let result = f.run(&["<img src=一.gif>"], true);
    TEST_HOOK.with(|h| *h.borrow_mut() = None);
    assert_eq!(result.unwrap_err().code, ErrorCode::SourceChanged);
    assert_eq!(f.bytes(), edited);
    assert!(f.export.join("media/一.gif").exists());
}
#[test]
fn emit_resolved_cannot_replace_policy_or_corpus_or_media() {
    let f = Fixture::new();
    f.asset("一", GIF, None);
    for path in [
        f.options.config.clone().unwrap(),
        f.options
            .asset_store
            .as_ref()
            .unwrap()
            .join("manifest.json"),
        f.export.join("media/一.gif"),
    ] {
        reason(
            &create_with_options(
                &f.export,
                &f.request(&["<img src=一.gif>"]),
                true,
                Some(&path),
                &f.options,
            )
            .unwrap_err(),
            "emit_resolved_protected_path",
        );
    }
}

#[test]
fn concurrent_creates_share_one_export_lock_and_do_not_lose_updates() {
    use std::sync::{Arc, Barrier};
    let f = Fixture::new();
    f.asset("一", GIF, None);
    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for guid in ["ThreadA", "ThreadB"] {
        let export = f.export.clone();
        let options = f.options.clone();
        let barrier = barrier.clone();
        let mut request = f.request(&["<img src=一.gif>"]);
        request.notes[0].guid = Some(guid.into());
        handles.push(std::thread::spawn(move || {
            TEST_HOOK.with(|h| {
                *h.borrow_mut() = Some(Box::new(move |phase| {
                    if phase == "before_export_lock" {
                        barrier.wait();
                    }
                    Ok(())
                }))
            });
            create_with_options(&export, &request, true, None, &options)
        }));
    }
    let outcomes = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(outcomes.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        outcomes.into_iter().find_map(Result::err).unwrap().code,
        ErrorCode::SourceChanged
    );
    let value: Value = serde_json::from_slice(&f.bytes()).unwrap();
    assert_eq!(value["notes"].as_array().unwrap().len(), 3);
    assert_eq!(fs::read(f.export.join("media/一.gif")).unwrap(), GIF);
    assert_eq!(value["media_files"], json!(["a.mp3", "b.png", "一.gif"]));
}

#[test]
fn media_declared_in_child_is_not_duplicated_in_root() {
    let f = Fixture::new();
    f.asset("一", GIF, None);
    let mut value: Value = serde_json::from_slice(&f.bytes()).unwrap();
    value["children"] = json!([{"__type__": "Deck", "name":"Child", "crowdanki_uuid":"child", "deck_config_uuid":"cfg-1", "notes":[], "children":[], "media_files":["一.gif"]}]);
    fs::write(
        f.export.join("deck.json"),
        crate::loader::render_canonical_bytes(&value).unwrap(),
    )
    .unwrap();
    let r = f.run(&["<img src=一.gif>"], true).unwrap();
    assert!(r.media.declarations_added.is_empty());
    assert_eq!(r.media.mutations, 1);
    let after: Value = serde_json::from_slice(&f.bytes()).unwrap();
    assert_eq!(after["media_files"], value["media_files"]);
    assert_eq!(after["children"], value["children"]);
}

#[test]
fn matching_existing_destination_dry_run_has_no_mutation_and_store_is_unchanged() {
    let f = Fixture::new();
    f.asset("一", GIF, None);
    fs::create_dir(f.export.join("media")).unwrap();
    fs::write(f.export.join("media/一.gif"), GIF).unwrap();
    let store = f.options.asset_store.as_ref().unwrap();
    let manifest = fs::read(store.join("manifest.json")).unwrap();
    let file_time = fs::metadata(f.export.join("media/一.gif"))
        .unwrap()
        .modified()
        .unwrap();
    let before = f.bytes();
    let r = f.run(&["<img src=一.gif>"], false).unwrap();
    assert_eq!(r.media.evidence()["assets"][0]["action"], "reuse");
    assert_eq!(r.media.mutations, 0);
    assert_eq!(before, f.bytes());
    assert_eq!(manifest, fs::read(store.join("manifest.json")).unwrap());
    assert_eq!(
        file_time,
        fs::metadata(f.export.join("media/一.gif"))
            .unwrap()
            .modified()
            .unwrap()
    );
}

#[test]
fn repository_context_auto_policy_is_export_relative() {
    let f = Fixture::new();
    fs::create_dir(f.temp.path().join(".git")).unwrap();
    fs::create_dir(f.temp.path().join(".anki-repo")).unwrap();
    fs::write(
        f.temp.path().join(CONFIG_PATH),
        policy("model-1", "Заголовок"),
    )
    .unwrap();
    let store = f.temp.path().join(".asset-store/kanji");
    fs::create_dir(f.temp.path().join(".asset-store")).unwrap();
    let mut options = f.options.clone();
    options.asset_store = Some(store.clone());
    let fixture = Fixture {
        temp: f.temp,
        export: f.export,
        options,
    };
    fixture.asset("一", GIF, None);
    let r = create_with_options(
        &fixture.export,
        &fixture.request(&["<img src=一.gif>"]),
        false,
        None,
        &MediaOptions::default(),
    )
    .unwrap();
    assert_eq!(r.media.pins().len(), 1);
    assert_eq!(r.outcomes[0].processor_fields, vec!["Заголовок"]);
}

#[test]
fn canonical_lifecycle_decision_validator_and_hash_must_all_match() {
    for mutation in [
        "pending",
        "semantic",
        "decision_hash",
        "validator",
        "format",
        "missing_file",
    ] {
        let f = Fixture::new();
        f.asset("一", GIF, None);
        let store = f.options.asset_store.as_ref().unwrap();
        let path = store.join("manifest.json");
        let mut manifest: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        match mutation {
            "pending" => manifest["assets"][0]["lifecycle"] = json!("pending"),
            "semantic" => manifest["assets"][0]["validation"]["status"] = json!("uncertain"),
            "decision_hash" => {
                manifest["assets"][0]["validation"]["content_sha256"] = json!("0".repeat(64))
            }
            "validator" => {
                manifest["assets"][0]["validation"]["validator"]["version"] = json!("old")
            }
            "format" => manifest["assets"][0]["format"] = json!("png"),
            "missing_file" => fs::remove_file(store.join("assets/gif/一.gif")).unwrap(),
            _ => unreachable!(),
        }
        fs::write(path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let before = f.bytes();
        reason(
            &f.run(&["<img src=一.gif>"], true).unwrap_err(),
            "asset_integrity_invalid",
        );
        assert_eq!(before, f.bytes());
        assert!(!f.export.join("media").exists());
    }
}

#[test]
fn whole_batch_preflight_finishes_before_first_media_mutation() {
    let f = Fixture::new();
    f.asset("一", GIF, None);
    let before = f.bytes();
    reason(
        &f.run(&["<img src=一.gif>", "<img src=二.gif>"], true)
            .unwrap_err(),
        "kanji_asset_missing",
    );
    assert_eq!(before, f.bytes());
    assert!(!f.export.join("media").exists());
    f.asset("二", PNG, None);
    fs::create_dir(f.export.join("media")).unwrap();
    fs::write(f.export.join("media/二.png"), b"foreign").unwrap();
    reason(
        &f.run(&["<img src=一.gif>", "<img src=二.png>"], true)
            .unwrap_err(),
        "destination_media_conflict",
    );
    assert_eq!(before, f.bytes());
    assert!(!f.export.join("media/一.gif").exists());
}

#[test]
fn required_duplicate_declaration_is_a_blocker_and_unrelated_duplicates_are_preserved() {
    let f = Fixture::new();
    f.asset("一", GIF, None);
    let mut value: Value = serde_json::from_slice(&f.bytes()).unwrap();
    value["media_files"] = json!(["a.mp3", "a.mp3", "一.gif", "一.gif"]);
    fs::write(
        f.export.join("deck.json"),
        crate::loader::render_canonical_bytes(&value).unwrap(),
    )
    .unwrap();
    reason(
        &f.run(&["<img src=一.gif>"], true).unwrap_err(),
        "media_declaration_conflict",
    );
    assert!(!f.export.join("media").exists());
    value["media_files"] = json!(["a.mp3", "a.mp3"]);
    fs::write(
        f.export.join("deck.json"),
        crate::loader::render_canonical_bytes(&value).unwrap(),
    )
    .unwrap();
    f.run(&["<img src=一.gif>"], true).unwrap();
    let after: Value = serde_json::from_slice(&f.bytes()).unwrap();
    assert_eq!(after["media_files"], json!(["a.mp3", "a.mp3", "一.gif"]));
}

/// Канонический суффикс потребителя не выводится из памяти: он обязан совпадать
/// с тем, что фактически объявляет политика домена.
#[test]
fn pitch_consumer_suffix_matches_the_domain_policy() {
    let identity = AssetIdentity::new("pitch_accent", "見本").unwrap();
    let location = PitchAccentDomainPolicy
        .canonical_location(&identity, &"0".repeat(64), DetectedFormat::Png)
        .unwrap();
    assert_eq!(
        location.consumer_filename,
        format!("{}{PITCH_CONSUMER_SUFFIX}", identity.key)
    );
}

/// Поле с включённым pitch-обработчиком владеет только точным каноническим
/// именем домена. Произвольный PNG, URL, data URI, sound и CSS-адрес остаются
/// незаявленными: иначе ссылка молча прошла бы мимо проверки ресурсов.
#[test]
fn pitch_field_claims_only_the_canonical_consumer_filename() {
    let f = Fixture::new();
    f.set_policy(&pitch_policy("model-1", "Заголовок"));
    for value in [
        "<img src=幽霊.png>",
        "<img src=幽霊.pitch.PNG>",
        "<img src=.pitch.png>",
        "<img src=../幽霊.pitch.png>",
        "<img src=/幽霊.pitch.png>",
        "<img src=https://host/幽霊.pitch.png>",
        "<img src=幽霊.pitch.png?q>",
        "<img src=幽霊.pitch.png#x>",
        "<audio src=幽霊.pitch.png>",
        "[sound:幽霊.pitch.png]",
        "<div style='background:url(幽霊.pitch.png)'>",
        "<img src='幽霊.pitch.png'",
    ] {
        let error = f.run(&[value], true).unwrap_err();
        reason(&error, "media_reference_unclaimed");
        assert!(!f.export.join("media").exists());
    }
    reason(
        &f.run(&["<img src=幽霊.pitch.png>"], true).unwrap_err(),
        "pitch_asset_missing",
    );
}

/// Отсутствие pitch-ресурса не маскируется под отсутствие kanji-ресурса: у
/// каждого домена своя стабильная причина блокера.
#[test]
fn missing_asset_reason_stays_specific_to_the_domain() {
    let f = Fixture::new();
    reason(
        &f.run(&["<img src=一.gif>"], true).unwrap_err(),
        "kanji_asset_missing",
    );
    f.set_policy(&pitch_policy("model-1", "Заголовок"));
    reason(
        &f.run(&["<img src=幽霊.pitch.png>"], true).unwrap_err(),
        "pitch_asset_missing",
    );
}

/// Два домена разрешаются одним запросом: каждый читает своё хранилище, и оба
/// файла попадают в плоский media. Повторный прогон остаётся идемпотентным.
#[test]
fn dual_domain_request_resolves_both_stores_in_one_run() {
    let f = Fixture::new();
    f.set_policy(&dual_policy("model-1", "Заголовок", "Толкование"));
    f.asset("一", GIF, None);
    f.pitch("幽霊", PNG);
    let values = [
        ("Заголовок", "<img src=一.gif>"),
        ("Толкование", "<img src=幽霊.pitch.png>"),
    ];
    let before = f.bytes();
    let dry = f.run_fields(&values, false).unwrap();
    assert_eq!(dry.media.evidence()["mutations_planned"], 2);
    assert_eq!(dry.media.references.len(), 2);
    assert_eq!(before, f.bytes());
    assert!(!f.export.join("media").exists());

    let applied = f.run_fields(&values, true).unwrap();
    assert_eq!(applied.notes_created, 1);
    assert_eq!(applied.media.mutations, 2);
    assert_eq!(fs::read(f.export.join("media/一.gif")).unwrap(), GIF);
    assert_eq!(
        fs::read(f.export.join("media/幽霊.pitch.png")).unwrap(),
        PNG
    );
    let value: Value = serde_json::from_slice(&f.bytes()).unwrap();
    assert_eq!(
        value["media_files"],
        json!(["a.mp3", "b.png", "一.gif", "幽霊.pitch.png"])
    );
    let namespaces = applied.media.evidence()["assets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|asset| asset["identity"]["namespace"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(namespaces, vec!["kanji", "pitch_accent"]);

    let after = f.bytes();
    let repeat = f.run_fields(&values, true).unwrap();
    assert!(!repeat.applied);
    assert_eq!(repeat.notes_already_applied, 1);
    assert_eq!(repeat.media.mutations, 0);
    assert_eq!(after, f.bytes());
}

/// Отсутствие переопределения хранилища блокирует только свой домен: запрос с
/// kanji-ресурсом и без pitch-ресурса не должен проходить из-за чужого store.
#[test]
fn each_domain_needs_its_own_resolved_store() {
    let f = Fixture::new();
    f.set_policy(&dual_policy("model-1", "Заголовок", "Толкование"));
    f.asset("一", GIF, None);
    f.pitch("幽霊", PNG);
    let both = [
        ("Заголовок", "<img src=一.gif>"),
        ("Толкование", "<img src=幽霊.pitch.png>"),
    ];
    let mut without_pitch = f.options.clone();
    without_pitch.pitch_asset_store = None;
    let mut without_kanji = f.options.clone();
    without_kanji.asset_store = None;

    // Домен, на который запрос не ссылается, не обязан иметь хранилище.
    let kanji_only = f.run_fields_with(
        &[
            ("Заголовок", "<img src=一.gif>"),
            ("Толкование", "значение"),
        ],
        false,
        &without_pitch,
    );
    assert_eq!(kanji_only.unwrap().media.evidence()["mutations_planned"], 1);

    reason(
        &f.run_fields_with(&both, false, &without_pitch).unwrap_err(),
        "pitch_asset_missing",
    );
    reason(
        &f.run_fields_with(&both, false, &without_kanji).unwrap_err(),
        "kanji_asset_missing",
    );
    assert_eq!(
        f.run_fields_with(&both, false, &f.options)
            .unwrap()
            .media
            .evidence()["mutations_planned"],
        2
    );
    assert!(!f.export.join("media").exists());
}

#[test]
fn cross_domain_consumer_filename_collision_fails_closed() {
    let kanji = verified_asset_fixture(
        AssetIdentity::new("kanji", "一").unwrap(),
        "assets/gif/一.gif",
        "見本.png",
        GIF,
    );
    let pitch = verified_asset_fixture(
        AssetIdentity::new("pitch_accent", "幽霊").unwrap(),
        "assets/png/幽霊.png",
        "見本.png",
        PNG,
    );
    let item = |asset: VerifiedAssetBytes| Item {
        pin: Pin {
            identity: asset.record.identity.clone(),
            filename: asset.record.consumer_filename.clone(),
            sha256: asset.record.sha256.clone(),
        },
        action: String::new(),
        asset,
    };
    let error = check_filename_ownership(&[item(kanji.clone()), item(pitch)]).unwrap_err();
    reason(&error, "media_filename_collision");
    assert_eq!(error.details["evidence"]["filename"], "見本.png");

    let same_bytes = verified_asset_fixture(
        AssetIdentity::new("pitch_accent", "幽霊").unwrap(),
        "assets/png/幽霊.png",
        "見本.png",
        GIF,
    );
    assert!(check_filename_ownership(&[item(kanji), item(same_bytes)]).is_ok());
}

/// Подтверждение человека не делает pitch-ресурс пригодным для карточки:
/// доверие даёт только текущее автоматическое `verified` ожидаемого валидатора.
#[test]
fn pitch_asset_requires_current_automated_validation() {
    struct Disputed;
    impl SemanticValidator for Disputed {
        fn identity(&self) -> ValidatorIdentity {
            PitchAccentImageValidator::validator_identity()
        }
        fn validate(
            &self,
            _: &AssetRecord,
            _: &mut dyn Read,
        ) -> Result<SemanticDecision, ValidatorFailure> {
            Ok(SemanticDecision::new(
                SemanticStatus::Uncertain,
                vec![ValidationEvidence {
                    kind: "synthetic".into(),
                    summary: "синтетическое спорное semantic решение".into(),
                    details: None,
                }],
            ))
        }
    }
    let f = Fixture::new();
    f.set_policy(&pitch_policy("model-1", "Заголовок"));
    let root = f.options.pitch_asset_store.clone().unwrap();
    let store =
        AssetStore::open_with_policy(StoreOptions::new(&root), PitchAccentDomainPolicy).unwrap();
    let source = f.temp.path().join("candidate.pitch.png");
    fs::write(&source, DECODABLE_PNG).unwrap();
    let identity = AssetIdentity::new("pitch_accent", "幽霊").unwrap();
    let pending = store
        .ingest(IngestRequest {
            identity: identity.clone(),
            source_path: source,
            expected_source_sha256: None,
            domain_metadata: None,
            replace_expected_sha256: None,
        })
        .unwrap();
    store.validate(SelectionMode::New, &Disputed).unwrap();
    let approved = store
        .attest(HumanAttestationRequest {
            identity: identity.clone(),
            expected_sha256: pending.asset.sha256.clone(),
            decision: HumanDecision::Approve,
            reason: "пользователь подтвердил точный candidate в review".into(),
        })
        .unwrap();
    assert_eq!(
        approved.asset.effective_status(),
        Some(SemanticStatus::Verified),
        "решение человека фиксируется, но не подменяет автоматическое"
    );
    assert!(
        AssetStore::read_verified_with_policy(
            &root,
            std::slice::from_ref(&identity),
            &PitchAccentImageValidator::validator_identity(),
            &PitchAccentDomainPolicy,
        )
        .is_err(),
        "pitch-домен не доверяет подтверждению человека"
    );
    let before = f.bytes();
    reason(
        &f.run(&["<img src=幽霊.pitch.png>"], true).unwrap_err(),
        "asset_integrity_invalid",
    );
    assert_eq!(before, f.bytes());
    assert!(!f.export.join("media").exists());
}
