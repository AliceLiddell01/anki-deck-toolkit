use asset_store::temp_workspace::TempWorkspace;
use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use asset_store::{
    AssetIdentity, AssetStore, BrowserExecutableSource, BrowserRuntimeProvenance, DetectedFormat,
    ErrorCode, IngestRequest, LifecycleState, PitchAccentCaptureRect, PitchAccentCoordinateSpace,
    PitchAccentDarkThemeProof, PitchAccentDomainMetadata, PitchAccentDomainPolicy,
    PitchAccentEvidence, PitchAccentImageValidator, PitchAccentProvider, PitchAccentRenderEvidence,
    PitchAccentRenderKind, PitchAccentResolvedForm, PitchAccentVariantEvidence,
    PitchAccentVariantSelection, PitchAccentVariantSelectionBasis, Provenance, SelectionMode,
    SemanticDecision, SemanticStatus, SemanticValidator, StoreOptions, ValidationEvidence,
    ValidatorFailure, ValidatorIdentity, VerifiedIngestRequest,
};

fn publishable_validator() -> ValidatorIdentity {
    FixedValidator::new(SemanticStatus::Verified).identity()
}

struct TempDir {
    workspace: TempWorkspace,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let workspace = TempWorkspace::create(&format!("asset-store-contract-{label}"))
            .expect("fixture root создаётся");
        Self { workspace }
    }

    fn path(&self) -> &Path {
        self.workspace.path()
    }

    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.path().join(name);
        fs::write(&path, bytes).expect("synthetic fixture записывается");
        path
    }
}

fn open_store(path: &Path) -> AssetStore {
    AssetStore::open(StoreOptions::new(path)).expect("store открывается")
}

fn open_kanji_store(path: &Path) -> AssetStore {
    AssetStore::open_kanji(StoreOptions::new(path)).expect("kanji store открывается")
}

fn identity(key: &str) -> AssetIdentity {
    AssetIdentity::new("generic", key).expect("synthetic identity корректна")
}

fn ingest(
    store: &AssetStore,
    id: AssetIdentity,
    source_path: PathBuf,
) -> asset_store::IngestOutcome {
    store
        .ingest(IngestRequest {
            identity: id,
            source_path,
            expected_source_sha256: None,
            domain_metadata: None,
            replace_expected_sha256: None,
        })
        .expect("explicit ingest успешен")
}

fn asset_path(root: &Path, record: &asset_store::AssetRecord) -> PathBuf {
    let area = if record.lifecycle == LifecycleState::Verified {
        root.to_path_buf()
    } else {
        root.join(".runtime")
    };
    area.join(&record.storage_path)
}

struct FixedValidator {
    identity: ValidatorIdentity,
    status: SemanticStatus,
    fail: bool,
}

struct StatusByKey {
    identity: ValidatorIdentity,
}

struct NoEvidenceValidator;

impl SemanticValidator for NoEvidenceValidator {
    fn identity(&self) -> ValidatorIdentity {
        ValidatorIdentity::new("empty-evidence", "1").unwrap()
    }

    fn validate(
        &self,
        _asset: &asset_store::AssetRecord,
        _bytes: &mut dyn Read,
    ) -> Result<SemanticDecision, ValidatorFailure> {
        Ok(SemanticDecision::new(SemanticStatus::Verified, Vec::new()))
    }
}

impl SemanticValidator for StatusByKey {
    fn identity(&self) -> ValidatorIdentity {
        self.identity.clone()
    }

    fn validate(
        &self,
        asset: &asset_store::AssetRecord,
        _bytes: &mut dyn Read,
    ) -> Result<SemanticDecision, ValidatorFailure> {
        let status = match asset.identity.key.as_str() {
            "verified" => SemanticStatus::Verified,
            "rejected" => SemanticStatus::Rejected,
            "uncertain" => SemanticStatus::Uncertain,
            "corrupt" => SemanticStatus::Corrupt,
            _ => unreachable!("fixture содержит только четыре заданные identity"),
        };
        Ok(SemanticDecision::new(
            status,
            vec![ValidationEvidence {
                kind: "synthetic".to_owned(),
                summary: status.as_str().to_owned(),
                details: None,
            }],
        ))
    }
}

impl FixedValidator {
    fn new(status: SemanticStatus) -> Self {
        Self {
            identity: ValidatorIdentity::new("synthetic", "1").expect("validator valid"),
            status,
            fail: false,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct DuplicateConsumerPolicy;

impl asset_store::AssetDomainPolicy for DuplicateConsumerPolicy {
    fn domain_id(&self) -> &'static str {
        "duplicate-test"
    }

    fn validate_identity(&self, identity: &AssetIdentity) -> Result<(), asset_store::AssetError> {
        identity
            .validate()
            .map_err(|message| asset_store::AssetError::new(ErrorCode::InvalidIdentity, message))?;
        if identity.namespace != self.domain_id() {
            return Err(asset_store::AssetError::new(
                ErrorCode::InvalidIdentity,
                "identity принадлежит другому тестовому домену",
            ));
        }
        Ok(())
    }

    fn canonical_location(
        &self,
        identity: &AssetIdentity,
        _sha256: &str,
        format: DetectedFormat,
    ) -> Result<asset_store::CanonicalAssetLocation, asset_store::AssetError> {
        self.validate_identity(identity)?;
        let extension = asset_store::domain::extension_for_format(format);
        Ok(asset_store::CanonicalAssetLocation {
            storage_path: format!("assets/{}.{}", identity.key, extension),
            consumer_filename: format!("shared.{extension}"),
        })
    }

    fn is_publishable_format(&self, format: DetectedFormat) -> bool {
        format == DetectedFormat::Gif
    }

    fn max_asset_bytes(&self) -> Option<u64> {
        None
    }

    fn content_addressed_storage(&self) -> bool {
        false
    }
}

impl SemanticValidator for FixedValidator {
    fn identity(&self) -> ValidatorIdentity {
        self.identity.clone()
    }

    fn validate(
        &self,
        _asset: &asset_store::AssetRecord,
        bytes: &mut dyn Read,
    ) -> Result<SemanticDecision, ValidatorFailure> {
        if self.fail {
            return Err(ValidatorFailure::new(
                "synthetic_failure",
                "fixture failure",
            ));
        }
        let mut contents = Vec::new();
        bytes
            .read_to_end(&mut contents)
            .map_err(|error| ValidatorFailure::new("read_failed", error.to_string()))?;
        Ok(SemanticDecision::new(
            self.status,
            vec![ValidationEvidence {
                kind: "synthetic".to_owned(),
                summary: format!("checked {} bytes", contents.len()),
                details: None,
            }],
        ))
    }
}

#[test]
fn empty_store_reopens_and_explicit_ingest_is_idempotent_with_hash_conflicts() {
    let temp = TempDir::new("reopen-ingest");
    let root = temp.path().join("owned");
    let store = open_store(&root);
    let store_id = store.store_id().to_owned();
    assert!(
        store
            .verify_integrity()
            .expect("empty store valid")
            .is_empty()
    );

    let source = temp.write("claim.png", b"not actually a png");
    let created = ingest(&store, identity("item-a"), source.clone());
    assert!(created.changed);
    assert_eq!(created.asset.lifecycle, LifecycleState::Pending);
    assert_eq!(created.asset.format, DetectedFormat::Unknown);
    assert_eq!(created.asset.provenance.source_kind, "local_import");
    assert_eq!(created.asset.provenance.source_name, "claim.png");
    assert_eq!(
        created.asset.sha256,
        "fb228003c83decc652be4afdde0282a21fc3aacda92c9e0c912fe0967a9c2f88"
    );

    let repeated = ingest(&store, identity("item-a"), source.clone());
    assert!(!repeated.changed);
    assert_eq!(repeated.asset.sha256, created.asset.sha256);
    assert_eq!(
        store.verify_integrity().expect("state remains valid").len(),
        1
    );

    let different = temp.write("replacement.bin", b"different bytes");
    let error = store
        .ingest(IngestRequest {
            identity: identity("item-a"),
            source_path: different.clone(),
            expected_source_sha256: None,
            domain_metadata: None,
            replace_expected_sha256: None,
        })
        .expect_err("silent overwrite запрещён");
    assert_eq!(error.code, ErrorCode::IdentityConflict);
    assert_eq!(
        error.details["existing_sha256"],
        serde_json::json!(created.asset.sha256)
    );
    assert_eq!(
        store.verify_integrity().expect("conflict не изменил state")[0].sha256,
        created.asset.sha256
    );

    let checked = FixedValidator::new(SemanticStatus::Verified);
    store
        .validate(SelectionMode::New, &checked)
        .expect("synthetic validator фиксирует VERIFIED");
    let current = store.verify_integrity().expect("verified state valid")[0].clone();
    assert_eq!(current.lifecycle, LifecycleState::Verified);

    let replaced = store
        .ingest(IngestRequest {
            identity: identity("item-a"),
            source_path: different,
            expected_source_sha256: None,
            domain_metadata: None,
            replace_expected_sha256: Some(current.sha256.clone()),
        })
        .expect("явная compare-and-swap замена разрешена");
    assert!(replaced.changed);
    assert_eq!(replaced.previous.as_ref().unwrap().sha256, current.sha256);
    assert_eq!(replaced.asset.lifecycle, LifecycleState::Pending);
    assert!(replaced.asset.validation.is_none());

    drop(store);
    let reopened = open_store(&root);
    assert_eq!(reopened.store_id(), store_id);
    let assets = reopened.verify_integrity().expect("reopened state valid");
    assert_eq!(assets.len(), 1);
    assert_eq!(assets[0].sha256, replaced.asset.sha256);
    assert_eq!(assets[0].identity.key, "item-a");
    assert_eq!(
        reopened
            .select(SelectionMode::New, &checked.identity)
            .expect("replacement selection valid")
            .len(),
        1,
        "новые bytes не наследуют старый semantic decision"
    );
}

#[test]
fn lifecycle_maps_only_verified_decision_to_trusted_and_selection_is_hash_versioned() {
    let temp = TempDir::new("semantic-lifecycle");
    let store = open_store(&temp.path().join("store"));
    for (name, bytes) in [
        ("verified", b"v".as_slice()),
        ("rejected", b"r".as_slice()),
        ("uncertain", b"u".as_slice()),
        ("corrupt", b"c".as_slice()),
    ] {
        ingest(
            &store,
            identity(name),
            temp.write(&format!("{name}.bin"), bytes),
        );
    }

    let validator = StatusByKey {
        identity: ValidatorIdentity::new("synthetic", "1").expect("validator valid"),
    };
    let report = store
        .validate(SelectionMode::New, &validator)
        .expect("semantic decisions persisted");
    assert_eq!(report.considered, 4);
    assert_eq!(report.changed, 4);

    let records = store
        .verify_integrity()
        .expect("all lifecycle states valid");
    for record in &records {
        match record.validation.as_ref().expect("decision exists").status {
            SemanticStatus::Verified => assert_eq!(record.lifecycle, LifecycleState::Verified),
            SemanticStatus::Rejected | SemanticStatus::Uncertain | SemanticStatus::Corrupt => {
                assert_eq!(record.lifecycle, LifecycleState::Quarantined);
            }
        }
    }
    let current = ValidatorIdentity::new("synthetic", "1").expect("validator valid");
    assert!(
        store
            .select(SelectionMode::New, &current)
            .expect("selection valid")
            .is_empty()
    );
    assert_eq!(
        store.select(SelectionMode::Full, &current).unwrap().len(),
        4
    );
    let upgraded = ValidatorIdentity::new("synthetic", "2").expect("new version valid");
    assert_eq!(
        store.select(SelectionMode::New, &upgraded).unwrap().len(),
        4
    );

    let failing = FixedValidator {
        identity: ValidatorIdentity::new("failed", "1").unwrap(),
        status: SemanticStatus::Verified,
        fail: true,
    };
    let failed = store
        .validate(SelectionMode::New, &failing)
        .expect("domain failure is an outcome, not a store error");
    assert_eq!(failed.considered, 4);
    assert_eq!(failed.changed, 0);
    assert_eq!(failed.blockers, ["synthetic_failure"]);
    assert!(
        store.verify_integrity().unwrap().iter().all(|asset| asset
            .validation
            .as_ref()
            .unwrap()
            .validator
            .id
            == "synthetic")
    );
}

#[test]
fn verified_ingest_publishes_only_verified_bytes_and_is_idempotent() {
    let temp = TempDir::new("verified-ingest");
    let root = temp.path().join("store");
    let store = open_store(&root);

    for status in [
        SemanticStatus::Rejected,
        SemanticStatus::Uncertain,
        SemanticStatus::Corrupt,
    ] {
        let outcome = store
            .ingest_verified(
                VerifiedIngestRequest {
                    identity: identity(status.as_str()),
                    bytes: [b"GIF89a".as_slice(), status.as_str().as_bytes()].concat(),
                    provenance: Provenance {
                        source_kind: "fixture".into(),
                        source_name: "sample.gif".into(),
                    },
                    domain_metadata: None,
                    replace_expected_sha256: None,
                },
                &FixedValidator::new(status),
            )
            .expect("non-verified decision is an outcome, not a store failure");
        assert_eq!(outcome.status, status);
        assert!(outcome.asset.is_none());
        assert!(!outcome.changed);
    }
    assert!(store.verify_integrity().unwrap().is_empty());
    assert_eq!(fs::read_dir(root.join("assets")).unwrap().count(), 0);
    assert_eq!(fs::read_dir(root.join(".tmp")).unwrap().count(), 0);

    drop(store);
    let root = temp.path().join("kanji-store");
    let store = open_kanji_store(&root);
    let request = VerifiedIngestRequest {
        identity: AssetIdentity::new("kanji", "元").unwrap(),
        bytes: [b"GIF89a".as_slice(), b"verified fixture"].concat(),
        provenance: Provenance {
            source_kind: "yarxi-suu-browser".into(),
            source_name: "yarxi-primary.gif".into(),
        },
        domain_metadata: Some(serde_json::json!({ "character": "元" })),
        replace_expected_sha256: None,
    };
    let first = store
        .ingest_verified(
            request.clone(),
            &FixedValidator::new(SemanticStatus::Verified),
        )
        .expect("verified result publishes atomically");
    let asset = first.asset.expect("verified asset exists");
    assert!(first.changed);
    assert_eq!(asset.lifecycle, LifecycleState::Verified);
    assert_eq!(asset.storage_path, "assets/gif/元.gif");
    assert_eq!(asset.consumer_filename, "元.gif");
    assert_eq!(asset.validation.unwrap().content_sha256, asset.sha256);
    assert_eq!(store.verify_integrity().unwrap().len(), 1);

    let repeated = store
        .ingest_verified(request, &FixedValidator::new(SemanticStatus::Verified))
        .expect("repeat with same hash and validator reuses canonical asset");
    assert!(!repeated.changed);
    assert_eq!(repeated.asset.unwrap().sha256, asset.sha256);
}

#[test]
fn pending_and_quarantined_kanji_assets_stay_outside_publishable_tree() {
    let temp = TempDir::new("runtime-candidates");
    let root = temp.path().join("store");
    let store = open_kanji_store(&root);
    let record = ingest(
        &store,
        AssetIdentity::new("kanji", "漢").unwrap(),
        temp.write("candidate.gif", b"GIF89a pending candidate"),
    )
    .asset;

    assert_eq!(record.lifecycle, LifecycleState::Pending);
    assert!(!root.join("assets/gif/漢.gif").exists());
    assert!(root.join(".runtime/assets/gif/漢.gif").exists());
    assert!(AssetStore::verify_publishable_corpus(&root, &publishable_validator()).is_ok());

    let report = store
        .validate(
            SelectionMode::Full,
            &FixedValidator::new(SemanticStatus::Uncertain),
        )
        .expect("candidate quarantine сохраняется в runtime store");
    assert_eq!(report.changed, 1);
    let records = store.verify_integrity().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].lifecycle, LifecycleState::Quarantined);
    assert!(!root.join("assets/gif/漢.gif").exists());
    assert!(root.join(".runtime/assets/gif/漢.gif").exists());
    assert!(AssetStore::verify_publishable_corpus(&root, &publishable_validator()).is_ok());
}

#[test]
fn pending_replacement_removes_previous_bytes_from_publishable_tree() {
    let temp = TempDir::new("pending-replacement");
    let root = temp.path().join("store");
    let store = open_kanji_store(&root);
    let verified = store
        .ingest_verified(
            VerifiedIngestRequest {
                identity: AssetIdentity::new("kanji", "文").unwrap(),
                bytes: b"GIF89a verified old bytes".to_vec(),
                provenance: Provenance {
                    source_kind: "fixture".into(),
                    source_name: "old.gif".into(),
                },
                domain_metadata: None,
                replace_expected_sha256: None,
            },
            &FixedValidator::new(SemanticStatus::Verified),
        )
        .unwrap()
        .asset
        .unwrap();

    let replacement = store
        .ingest(IngestRequest {
            identity: verified.identity.clone(),
            source_path: temp.write("replacement.gif", b"GIF89a unvalidated new bytes"),
            expected_source_sha256: None,
            domain_metadata: None,
            replace_expected_sha256: Some(verified.sha256.clone()),
        })
        .unwrap();
    assert_eq!(replacement.previous.unwrap().sha256, verified.sha256);
    assert_eq!(replacement.asset.lifecycle, LifecycleState::Pending);
    assert!(!root.join(&verified.storage_path).exists());
    assert!(
        root.join(".runtime")
            .join(&replacement.asset.storage_path)
            .exists()
    );
    assert!(AssetStore::verify_publishable_corpus(&root, &publishable_validator()).is_ok());
}

#[test]
fn legacy_nonverified_manifest_is_moved_to_runtime_when_store_opens() {
    let temp = TempDir::new("legacy-runtime-migration");
    let root = temp.path().join("store");
    let store = open_kanji_store(&root);
    let record = ingest(
        &store,
        AssetIdentity::new("kanji", "書").unwrap(),
        temp.write("legacy.gif", b"GIF89a legacy pending bytes"),
    )
    .asset;
    let runtime_asset_path = root.join(".runtime").join(&record.storage_path);
    let canonical_asset_path = root.join(&record.storage_path);
    fs::create_dir_all(canonical_asset_path.parent().unwrap()).unwrap();
    fs::copy(&runtime_asset_path, &canonical_asset_path).unwrap();

    let runtime_manifest_path = root.join(".runtime/manifest.json");
    let mut runtime_manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&runtime_manifest_path).unwrap()).unwrap();
    let legacy_record = runtime_manifest["assets"][0].clone();
    runtime_manifest["assets"] = serde_json::json!([]);
    fs::write(
        &runtime_manifest_path,
        serde_json::to_vec_pretty(&runtime_manifest).unwrap(),
    )
    .unwrap();
    fs::remove_file(&runtime_asset_path).unwrap();

    let canonical_manifest_path = root.join("manifest.json");
    let mut canonical_manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&canonical_manifest_path).unwrap()).unwrap();
    canonical_manifest["assets"] = serde_json::json!([legacy_record]);
    fs::write(
        &canonical_manifest_path,
        serde_json::to_vec_pretty(&canonical_manifest).unwrap(),
    )
    .unwrap();
    drop(store);

    let reopened = open_kanji_store(&root);
    let records = reopened.verify_integrity().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].lifecycle, LifecycleState::Pending);
    assert!(!canonical_asset_path.exists());
    assert!(
        root.join(".runtime")
            .join(&records[0].storage_path)
            .exists()
    );
    assert!(AssetStore::verify_publishable_corpus(&root, &publishable_validator()).is_ok());
}

#[test]
fn revalidation_downgrade_atomically_removes_asset_from_publishable_tree() {
    let temp = TempDir::new("verified-demotion");
    let root = temp.path().join("store");
    let store = open_kanji_store(&root);
    let identity = AssetIdentity::new("kanji", "字").unwrap();
    let verified = store
        .ingest_verified(
            VerifiedIngestRequest {
                identity: identity.clone(),
                bytes: b"GIF89a verified fixture".to_vec(),
                provenance: Provenance {
                    source_kind: "fixture".into(),
                    source_name: "verified.gif".into(),
                },
                domain_metadata: None,
                replace_expected_sha256: None,
            },
            &FixedValidator::new(SemanticStatus::Verified),
        )
        .unwrap()
        .asset
        .unwrap();
    assert!(root.join(&verified.storage_path).exists());

    let report = store
        .validate(
            SelectionMode::Full,
            &FixedValidator::new(SemanticStatus::Uncertain),
        )
        .expect("неуверенная revalidation уводит bytes в quarantine");
    assert_eq!(report.changed, 1);
    let records = store.verify_integrity().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].identity, identity);
    assert_eq!(records[0].lifecycle, LifecycleState::Quarantined);
    assert!(!root.join(&verified.storage_path).exists());
    assert!(root.join(".runtime").join(&verified.storage_path).exists());
    assert!(AssetStore::verify_publishable_corpus(&root, &publishable_validator()).is_ok());
}

#[test]
fn publishable_gate_rejects_nonverified_records_and_unregistered_bytes() {
    let nonverified = TempDir::new("publishable-nonverified");
    let root = nonverified.path().join("store");
    let store = open_kanji_store(&root);
    store
        .ingest_verified(
            VerifiedIngestRequest {
                identity: AssetIdentity::new("kanji", "中").unwrap(),
                bytes: b"GIF89a verified fixture".to_vec(),
                provenance: Provenance {
                    source_kind: "fixture".into(),
                    source_name: "verified.gif".into(),
                },
                domain_metadata: None,
                replace_expected_sha256: None,
            },
            &FixedValidator::new(SemanticStatus::Verified),
        )
        .unwrap();
    let manifest_path = root.join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["assets"][0]["lifecycle"] = serde_json::json!("pending");
    manifest["assets"][0]["validation"] = serde_json::Value::Null;
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    assert_eq!(
        AssetStore::verify_publishable_corpus(&root, &publishable_validator())
            .unwrap_err()
            .code,
        ErrorCode::ManifestCorrupt
    );

    let orphaned = TempDir::new("publishable-orphan");
    let orphan_root = orphaned.path().join("store");
    open_store(&orphan_root);
    fs::write(orphan_root.join("assets/unregistered.png"), b"orphan bytes").unwrap();
    assert!(AssetStore::verify_publishable_corpus(&orphan_root, &publishable_validator()).is_err());

    assert!(
        AssetStore::verify_publishable_corpus(
            orphaned.path().join("absent"),
            &publishable_validator()
        )
        .is_ok()
    );
}

#[test]
fn publishable_gate_is_read_only_when_ignored_lock_file_is_absent() {
    let temp = TempDir::new("publishable-without-lock");
    let root = temp.path().join("store");
    let store = open_kanji_store(&root);
    store
        .ingest_verified(
            VerifiedIngestRequest {
                identity: AssetIdentity::new("kanji", "日").unwrap(),
                bytes: b"GIF89a verified fixture".to_vec(),
                provenance: Provenance {
                    source_kind: "fixture".into(),
                    source_name: "verified.gif".into(),
                },
                domain_metadata: None,
                replace_expected_sha256: None,
            },
            &FixedValidator::new(SemanticStatus::Verified),
        )
        .unwrap();

    fs::remove_file(root.join(".lock")).unwrap();
    fs::remove_dir_all(root.join(".tmp")).unwrap();
    fs::remove_dir_all(root.join(".runtime")).unwrap();
    assert!(AssetStore::verify_publishable_corpus(&root, &publishable_validator()).is_ok());
    assert!(!root.join(".lock").exists());
    assert!(!root.join(".tmp").exists());
    assert!(!root.join(".runtime").exists());
}

#[test]
fn publishable_gate_accepts_empty_manifest_without_assets_directory() {
    let temp = TempDir::new("publishable-empty-without-assets");
    let root = temp.path().join("store");
    open_kanji_store(&root);
    fs::remove_dir_all(root.join("assets")).unwrap();
    fs::remove_dir_all(root.join(".tmp")).unwrap();
    fs::remove_dir_all(root.join(".runtime")).unwrap();

    assert!(AssetStore::verify_publishable_corpus(&root, &publishable_validator()).is_ok());
    assert!(!root.join("assets").exists());
    assert!(!root.join(".tmp").exists());
    assert!(!root.join(".runtime").exists());
}

#[test]
fn empty_unregistered_directories_are_ignored_only_outside_publishable_gate() {
    let temp = TempDir::new("empty-unregistered-directories");
    let root = temp.path().join("store");
    let store = open_kanji_store(&root);
    let runtime_empty = root.join(".runtime/assets/unregistered-empty");
    fs::create_dir(&runtime_empty).unwrap();

    assert!(store.verify_integrity().unwrap().is_empty());
    assert!(AssetStore::verify_publishable_corpus(&root, &publishable_validator()).is_ok());

    let canonical_empty = root.join("assets/unregistered-empty");
    fs::create_dir(&canonical_empty).unwrap();
    assert!(store.verify_integrity().unwrap().is_empty());
    assert_eq!(
        AssetStore::verify_publishable_corpus(&root, &publishable_validator())
            .unwrap_err()
            .code,
        ErrorCode::UnexpectedPath
    );

    fs::write(runtime_empty.join("unexpected.bin"), b"unregistered bytes").unwrap();
    assert_eq!(
        store.verify_integrity().unwrap_err().code,
        ErrorCode::UnexpectedPath
    );
}

#[test]
fn publishable_gate_rejects_stale_validator_identity() {
    let temp = TempDir::new("publishable-stale-validator");
    let root = temp.path().join("store");
    let store = open_kanji_store(&root);
    store
        .ingest_verified(
            VerifiedIngestRequest {
                identity: AssetIdentity::new("kanji", "日").unwrap(),
                bytes: b"GIF89a verified fixture".to_vec(),
                provenance: Provenance {
                    source_kind: "fixture".into(),
                    source_name: "verified.gif".into(),
                },
                domain_metadata: None,
                replace_expected_sha256: None,
            },
            &FixedValidator::new(SemanticStatus::Verified),
        )
        .unwrap();

    assert_eq!(
        AssetStore::verify_publishable_corpus(
            &root,
            &ValidatorIdentity::new("synthetic", "2").unwrap(),
        )
        .unwrap_err()
        .code,
        ErrorCode::ManifestCorrupt
    );
}

#[test]
fn open_existing_recreates_ignored_service_files() {
    let temp = TempDir::new("reopen-without-service-files");
    let root = temp.path().join("store");
    let store = open_kanji_store(&root);
    store
        .ingest_verified(
            VerifiedIngestRequest {
                identity: AssetIdentity::new("kanji", "日").unwrap(),
                bytes: b"GIF89a verified fixture".to_vec(),
                provenance: Provenance {
                    source_kind: "fixture".into(),
                    source_name: "verified.gif".into(),
                },
                domain_metadata: None,
                replace_expected_sha256: None,
            },
            &FixedValidator::new(SemanticStatus::Verified),
        )
        .unwrap();
    drop(store);
    fs::remove_file(root.join(".lock")).unwrap();
    fs::remove_dir_all(root.join(".tmp")).unwrap();
    fs::remove_dir_all(root.join(".runtime")).unwrap();

    let reopened = AssetStore::open_kanji_existing(StoreOptions::new(&root)).unwrap();
    assert_eq!(reopened.verify_integrity().unwrap().len(), 1);
    assert!(root.join(".lock").exists());
    assert!(root.join(".tmp").exists());
    assert!(root.join(".runtime").exists());
}

#[test]
fn kanji_gif_and_png_are_independent_records_with_format_scoped_cas() {
    let temp = TempDir::new("kanji-stable-cas");
    let root = temp.path().join("store");
    let store = open_kanji_store(&root);
    let identity = AssetIdentity::new("kanji", "元").unwrap();
    let validator = FixedValidator::new(SemanticStatus::Verified);
    let publish = |bytes: Vec<u8>, expected: Option<String>, name: &str| {
        store.ingest_verified(
            VerifiedIngestRequest {
                identity: identity.clone(),
                bytes,
                provenance: Provenance {
                    source_kind: "fixture".into(),
                    source_name: name.into(),
                },
                domain_metadata: Some(serde_json::json!({ "character": "元" })),
                replace_expected_sha256: expected,
            },
            &validator,
        )
    };
    let ingest_verified = |bytes: Vec<u8>, expected: Option<String>, name: &str| {
        publish(bytes, expected, name)
            .expect("verified CAS publication succeeds")
            .asset
            .expect("verified asset is canonical")
    };

    let first = ingest_verified(b"GIF89a first bytes".to_vec(), None, "first.gif");
    assert_eq!(first.storage_path, "assets/gif/元.gif");
    let gif = ingest_verified(
        b"GIF89a replacement bytes".to_vec(),
        Some(first.sha256.clone()),
        "second.gif",
    );
    assert_eq!(gif.storage_path, first.storage_path);
    assert_ne!(gif.sha256, first.sha256);
    assert_eq!(
        gif.validation.as_ref().unwrap().content_sha256,
        gif.sha256,
        "new semantic evidence remains bound to replacement bytes"
    );

    // CAS с SHA записи другого формата — ошибка, а не разрешение заменить GIF.
    let png_bytes = [b"\x89PNG\r\n\x1a\n".as_slice(), b"independent png"].concat();
    let cross = publish(png_bytes.clone(), Some(gif.sha256.clone()), "cross.png")
        .expect_err("CAS другого формата отклоняется");
    assert_eq!(cross.code, ErrorCode::IdentityConflict);
    assert_eq!(cross.details["existing_format"], "gif");

    // Без CAS PNG становится отдельной записью; GIF и его SHA не меняются.
    let png = ingest_verified(png_bytes.clone(), None, "third.png");
    assert_eq!(png.format, DetectedFormat::Png);
    assert_eq!(png.storage_path, "assets/png/元.png");
    assert_eq!(png.consumer_filename, "元.png");
    assert_eq!(
        fs::read(root.join(&gif.storage_path)).unwrap(),
        b"GIF89a replacement bytes"
    );
    assert_eq!(fs::read(root.join(&png.storage_path)).unwrap(), png_bytes);

    // Повтор тех же PNG-байтов — no-op, а не вторая запись.
    let repeated = publish(png_bytes.clone(), None, "third.png").unwrap();
    assert!(!repeated.changed);

    let stale_cas = publish(
        b"GIF89a stale update".to_vec(),
        Some(first.sha256.clone()),
        "stale.gif",
    )
    .expect_err("устаревший expected hash блокируется CAS");
    assert_eq!(stale_cas.code, ErrorCode::IdentityConflict);

    let records = store
        .verify_integrity()
        .expect("canonical state remains valid");
    assert_eq!(records.len(), 2, "по одной записи на формат");
    let formats: Vec<_> = records.iter().map(|record| record.format).collect();
    assert!(formats.contains(&DetectedFormat::Gif) && formats.contains(&DetectedFormat::Png));
    let mut directories: Vec<_> = fs::read_dir(root.join("assets"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    directories.sort();
    assert_eq!(directories, ["gif", "png"]);

    // Чтение без формата предпочитает подлинный GIF; явный формат даёт PNG.
    let preferred = AssetStore::read_verified(
        &root,
        std::slice::from_ref(&identity),
        &validator.identity(),
    )
    .unwrap();
    assert_eq!(preferred[0].record.sha256, gif.sha256);
    let explicit = AssetStore::read_verified_variants_with_policy(
        &root,
        &[
            (identity.clone(), DetectedFormat::Png),
            (identity.clone(), DetectedFormat::Gif),
        ],
        &validator.identity(),
        &asset_store::KanjiDomainPolicy,
    )
    .unwrap();
    assert_eq!(explicit[0].bytes, png_bytes);
    assert_eq!(explicit[1].record.sha256, gif.sha256);

    // Потоковое чтение: ошибка отдельного запроса не прерывает пакет.
    let missing = AssetIdentity::new("kanji", "無").unwrap();
    let requests = [
        (identity.clone(), DetectedFormat::Png),
        (missing, DetectedFormat::Png),
        (identity.clone(), DetectedFormat::Gif),
    ];
    let mut visited = Vec::new();
    AssetStore::visit_verified_variants_with_policy(
        &root,
        &requests,
        &validator.identity(),
        &asset_store::KanjiDomainPolicy,
        |index, read| {
            visited.push((
                index,
                read.map(|read| read.record.sha256)
                    .map_err(|error| error.code),
            ));
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(
        visited,
        [
            (0, Ok(png.sha256.clone())),
            (1, Err(ErrorCode::MissingAssetFile)),
            (2, Ok(gif.sha256.clone())),
        ]
    );

    // Ошибка, которую вернул сам `visit`, прерывает чтение.
    let mut calls = 0;
    let aborted = AssetStore::visit_verified_variants_with_policy(
        &root,
        &requests,
        &validator.identity(),
        &asset_store::KanjiDomainPolicy,
        |_, _| {
            calls += 1;
            Err(asset_store::AssetError::new(
                ErrorCode::IntegrityMismatch,
                "стоп",
            ))
        },
    )
    .expect_err("ошибка visit прерывает пакет");
    assert_eq!((calls, aborted.message.as_str()), (1, "стоп"));

    // Повторное открытие сохраняет обе записи.
    drop(store);
    let reopened = open_kanji_store(&root);
    assert_eq!(reopened.verify_integrity().unwrap().len(), 2);
    drop(reopened);

    // Повреждение любого файла корпуса отклоняет весь снимок до первого `visit`.
    let tampered = root.join(&gif.storage_path);
    fs::remove_file(&tampered).unwrap();
    fs::write(&tampered, b"GIF89a tampered bytes!!").unwrap();
    let mut calls = 0;
    let corrupt = AssetStore::visit_verified_variants_with_policy(
        &root,
        &requests[..1],
        &validator.identity(),
        &asset_store::KanjiDomainPolicy,
        |_, _| {
            calls += 1;
            Ok(())
        },
    )
    .expect_err("повреждённый корпус отклоняется целиком");
    assert_eq!((calls, corrupt.code), (0, ErrorCode::IntegrityMismatch));
}

#[test]
fn second_pitch_accent_domain_publishes_its_own_verified_png_contract() {
    let temp = TempDir::new("pitch-domain");
    let root = temp.path().join("pitch-accent");
    let policy = PitchAccentDomainPolicy;
    let store = AssetStore::open_with_policy(StoreOptions::new(&root), policy).unwrap();
    let identity = AssetIdentity::new("pitch_accent", "幽霊").unwrap();
    let bytes = pitch_accent_contract_png();
    let markup_sha256 = asset_store::hashing::sha256_hex("<div>ゆうれい</div>".as_bytes());
    let graph_rect = PitchAccentCaptureRect {
        x: 100.0,
        y: 200.0,
        width: 20.0,
        height: 10.0,
    };
    let metadata = PitchAccentDomainMetadata {
        surface: "幽霊".into(),
        reading: "ゆうれい".into(),
        jpdb_vocabulary_id: 123,
        evidence: PitchAccentEvidence {
            provider: PitchAccentProvider::Jpdb,
            source_url: "https://jpdb.io/vocabulary/123/幽霊/ゆうれい#a".into(),
            resolved_forms: vec![PitchAccentResolvedForm {
                surface: "幽霊".into(),
                reading: "ゆうれい".into(),
            }],
            variant_selection: PitchAccentVariantSelection {
                basis: PitchAccentVariantSelectionBasis::SeededHashWithoutSourcePriority,
                detected_variant_count: 1,
                selected_index: 0,
                variants: vec![PitchAccentVariantEvidence {
                    index: 0,
                    selector: ".pitch-accent-graph".into(),
                    text: "ゆうれい".into(),
                    markup_sha256: markup_sha256.clone(),
                    selection_score: asset_store::pitch_accent_variant_selection_score(
                        123,
                        &markup_sha256,
                    ),
                    viewport_rect: PitchAccentCaptureRect {
                        x: 100.0,
                        y: 100.0,
                        width: 20.0,
                        height: 10.0,
                    },
                    document_rect: graph_rect,
                    paint_rect: graph_rect,
                }],
            },
            render: PitchAccentRenderEvidence {
                kind: PitchAccentRenderKind::BrowserRegionScreenshot,
                selector: ".pitch-accent-graph".into(),
                captured_graph_count: 1,
                coordinate_space: PitchAccentCoordinateSpace::Document,
                viewport_width: 1280,
                viewport_height: 900,
                document_width: 1280,
                document_height: 1800,
                scroll_x: 0.0,
                scroll_y: 100.0,
                pixel_width: 78,
                pixel_height: 48,
                device_scale_factor: 3.0,
                page_scale_factor: 1.0,
                dark_theme: PitchAccentDarkThemeProof {
                    document_element_classes: vec!["dark-mode".into()],
                    prefers_color_scheme: "dark".into(),
                    computed_color_scheme: "dark".into(),
                    background_selector: ".subsection-pitch-accent".into(),
                    background_rgb: [24, 36, 48],
                },
                padding_css_px: asset_store::PITCH_ACCENT_CAPTURE_PADDING_CSS_PX,
                // Отрисовка 20×10 CSS px с полем 3 CSS px с каждой стороны.
                capture_rect: PitchAccentCaptureRect {
                    x: 97.0,
                    y: 197.0,
                    width: 26.0,
                    height: 16.0,
                },
            },
            browser: BrowserRuntimeProvenance {
                product: "Chrome/140".into(),
                protocol_version: "1.3".into(),
                revision: "1234567".into(),
                user_agent: "Mozilla/5.0 Chrome/140".into(),
                js_version: "V8 14.0".into(),
                executable_source: BrowserExecutableSource::PathLookup,
            },
        },
    };
    let outcome = store
        .ingest_verified(
            VerifiedIngestRequest {
                identity: identity.clone(),
                bytes: bytes.clone(),
                provenance: Provenance {
                    source_kind: "jpdb-browser".into(),
                    source_name: "幽霊.pitch.png".into(),
                },
                domain_metadata: Some(serde_json::to_value(metadata).unwrap()),
                replace_expected_sha256: None,
            },
            &PitchAccentImageValidator,
        )
        .unwrap();
    let record = outcome.asset.expect("полное JPDB evidence публикует PNG");
    assert_eq!(record.identity, identity);
    assert_eq!(record.storage_path, "assets/png/幽霊.png");
    assert_eq!(record.consumer_filename, "幽霊.pitch.png");
    assert!(root.join("assets/png/幽霊.png").is_file());
    assert_eq!(store.verify_integrity().unwrap()[0].sha256, record.sha256);

    let png_without_evidence = store
        .ingest_verified(
            VerifiedIngestRequest {
                identity: AssetIdentity::new("pitch_accent", "猫").unwrap(),
                bytes,
                provenance: Provenance {
                    source_kind: "local_import".into(),
                    source_name: "unverified.png".into(),
                },
                domain_metadata: None,
                replace_expected_sha256: None,
            },
            &PitchAccentImageValidator,
        )
        .unwrap();
    assert_eq!(png_without_evidence.status, SemanticStatus::Uncertain);
    assert!(png_without_evidence.asset.is_none());

    drop(store);
    let validator = PitchAccentImageValidator::validator_identity();
    AssetStore::verify_publishable_corpus_with_policy(&root, &policy, &validator).unwrap();
    let read =
        AssetStore::read_verified_with_policy(&root, &[identity], &validator, &policy).unwrap();
    assert_eq!(read[0].record.consumer_filename, "幽霊.pitch.png");
    assert_eq!(read[0].bytes, pitch_accent_contract_png());
    assert!(AssetStore::open_kanji_existing(StoreOptions::new(&root)).is_err());
    assert!(AssetStore::verify_publishable_corpus(&root, &publishable_validator()).is_err());
}

#[test]
fn pitch_accent_policy_never_verifies_non_png_even_with_a_permissive_validator() {
    const GIF: &[u8] = b"GIF89a synthetic";

    let temp = TempDir::new("pitch-format-policy");
    let root = temp.path().join("pitch-accent");
    let store =
        AssetStore::open_with_policy(StoreOptions::new(&root), PitchAccentDomainPolicy).unwrap();
    let identity = AssetIdentity::new("pitch_accent", "猫").unwrap();
    let validator = FixedValidator::new(SemanticStatus::Verified);

    let error = store
        .ingest_verified(
            VerifiedIngestRequest {
                identity: identity.clone(),
                bytes: GIF.to_vec(),
                provenance: Provenance {
                    source_kind: "fixture".into(),
                    source_name: "candidate.gif".into(),
                },
                domain_metadata: None,
                replace_expected_sha256: None,
            },
            &validator,
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidTransition);
    assert!(store.verify_integrity().unwrap().is_empty());

    let candidate_path = temp.write("candidate.gif", GIF);
    store
        .ingest(IngestRequest {
            identity: identity.clone(),
            source_path: candidate_path,
            expected_source_sha256: None,
            domain_metadata: None,
            replace_expected_sha256: None,
        })
        .unwrap();
    assert_eq!(
        store
            .validate(SelectionMode::Full, &validator)
            .unwrap_err()
            .code,
        ErrorCode::InvalidTransition
    );
    assert!(!store.root().join("assets/unsupported").exists());
    let records = store.verify_integrity().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].lifecycle, LifecycleState::Pending);
}

#[test]
fn wrong_domain_schema_v5_open_does_not_create_runtime_or_poison_owner() {
    let temp = TempDir::new("wrong-domain-v5-open");
    let root = temp.path().join("pitch-accent");
    let store =
        AssetStore::open_with_policy(StoreOptions::new(&root), PitchAccentDomainPolicy).unwrap();
    drop(store);

    let manifest_path = root.join("manifest.json");
    let owner_path = root.join(".owner.json");
    let manifest_before = fs::read(&manifest_path).unwrap();
    let owner_before = fs::read(&owner_path).unwrap();
    fs::remove_dir_all(root.join(".runtime")).unwrap();
    fs::remove_file(root.join(".lock")).unwrap();
    assert_eq!(fs::read_dir(root.join(".tmp")).unwrap().count(), 0);

    let error = match AssetStore::open_with_policy(
        StoreOptions::new(&root),
        asset_store::KanjiDomainPolicy,
    ) {
        Ok(_) => panic!("несовместимая policy не должна открыть pitch-accent store"),
        Err(error) => error,
    };
    assert_eq!(error.code, ErrorCode::ManifestCorrupt);
    assert_eq!(fs::read(&manifest_path).unwrap(), manifest_before);
    assert_eq!(fs::read(&owner_path).unwrap(), owner_before);
    assert!(!root.join(".runtime").exists());
    assert!(!root.join(".lock").exists());
    assert_eq!(fs::read_dir(root.join(".tmp")).unwrap().count(), 0);

    let reopened =
        AssetStore::open_existing_with_policy(StoreOptions::new(&root), PitchAccentDomainPolicy)
            .unwrap();
    assert!(reopened.verify_integrity().unwrap().is_empty());
}

fn pitch_accent_contract_png() -> Vec<u8> {
    let mut image = image::RgbaImage::from_pixel(78, 48, image::Rgba([24, 36, 48, 255]));
    for x in 12..60 {
        image.put_pixel(x, 24, image::Rgba([235, 235, 235, 255]));
    }
    let mut output = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut output, image::ImageFormat::Png)
        .unwrap();
    output.into_inner()
}

#[test]
fn flat_kanji_schema_v3_v4_migrate_without_reacquisition_or_trust_loss() {
    const VALID_GIF: &[u8] = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff\x21\xf9\x04\x01\x00\x00\x00\x00\x2c\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02\x44\x01\x00\x3b";

    for legacy_schema in [3_u64, 4] {
        let temp = TempDir::new(&format!("kanji-schema-{legacy_schema}"));
        let root = temp.path().join("store");
        let identity = AssetIdentity::new("kanji", "元").unwrap();
        let validator = FixedValidator::new(SemanticStatus::Verified);
        let store = open_kanji_store(&root);
        let original = store
            .ingest_verified(
                VerifiedIngestRequest {
                    identity: identity.clone(),
                    bytes: VALID_GIF.to_vec(),
                    provenance: Provenance {
                        source_kind: "fixture".into(),
                        source_name: "trusted.gif".into(),
                    },
                    domain_metadata: Some(serde_json::json!({ "character": "元" })),
                    replace_expected_sha256: None,
                },
                &validator,
            )
            .unwrap()
            .asset
            .unwrap();
        let attested = if legacy_schema == 4 {
            store
                .attest(asset_store::HumanAttestationRequest {
                    identity: identity.clone(),
                    expected_sha256: original.sha256.clone(),
                    decision: asset_store::HumanDecision::Approve,
                    reason: "синтетическое human review перед migration".into(),
                })
                .unwrap()
                .asset
        } else {
            original.clone()
        };
        let expected_validation = attested.validation.clone().unwrap();
        let expected_attestation = attested.human_attestation.clone();
        drop(store);

        let nested_path = root.join("assets/gif/元.gif");
        let legacy_path = root.join("assets/元.gif");
        fs::rename(&nested_path, &legacy_path).unwrap();
        fs::remove_dir(root.join("assets/gif")).unwrap();
        fs::remove_dir(root.join("assets/png")).ok();
        let manifest_path = root.join("manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest["schema_version"] = serde_json::json!(legacy_schema);
        manifest.as_object_mut().unwrap().remove("domain_id");
        let record = &mut manifest["assets"][0];
        record["storage_path"] = serde_json::json!("assets/元.gif");
        record.as_object_mut().unwrap().remove("consumer_filename");
        if legacy_schema == 3 {
            record.as_object_mut().unwrap().remove("human_attestation");
        }
        let legacy_bytes = serde_json::to_vec_pretty(&manifest).unwrap();
        fs::write(&manifest_path, &legacy_bytes).unwrap();
        fs::remove_file(root.join(".lock")).unwrap();
        fs::remove_dir_all(root.join(".runtime")).unwrap();

        let owner_path = root.join(".owner.json");
        let owner_before = fs::read(&owner_path).unwrap();
        let error =
            match AssetStore::open_with_policy(StoreOptions::new(&root), PitchAccentDomainPolicy) {
                Ok(_) => panic!("новый domain не должен присваивать себе legacy Kanji store"),
                Err(error) => error,
            };
        assert_eq!(error.code, ErrorCode::UnsupportedSchemaVersion);
        assert_eq!(fs::read(&manifest_path).unwrap(), legacy_bytes);
        assert_eq!(fs::read(&owner_path).unwrap(), owner_before);
        assert!(!root.join(".runtime").exists());
        assert!(!root.join(".lock").exists());
        assert_eq!(fs::read_dir(root.join(".tmp")).unwrap().count(), 0);

        let read_only = AssetStore::read_verified_with_policy(
            &root,
            std::slice::from_ref(&identity),
            &validator.identity(),
            &asset_store::KanjiDomainPolicy,
        )
        .unwrap();
        assert_eq!(read_only[0].record.consumer_filename, "元.gif");
        assert_eq!(read_only[0].bytes, VALID_GIF);
        assert_eq!(fs::read(&manifest_path).unwrap(), legacy_bytes);
        assert!(legacy_path.exists());
        assert!(!nested_path.exists());
        assert!(!root.join(".lock").exists());
        assert!(!root.join(".runtime").exists());

        let migrated = AssetStore::open_kanji_existing(StoreOptions::new(&root)).unwrap();
        assert!(migrated.layout_migrated_on_open());
        let records = migrated.verify_integrity().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].sha256, expected_validation.content_sha256);
        assert_eq!(
            records[0].validation.as_ref().unwrap(),
            &expected_validation
        );
        assert_eq!(records[0].human_attestation, expected_attestation);
        assert_eq!(records[0].storage_path, "assets/gif/元.gif");
        assert_eq!(records[0].consumer_filename, "元.gif");
        assert!(!legacy_path.exists());
        assert_eq!(fs::read(nested_path).unwrap(), VALID_GIF);
        let migrated_manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        assert_eq!(migrated_manifest["schema_version"], 6);
        assert_eq!(migrated_manifest["domain_id"], "kanji");
    }
}

#[test]
fn empty_legacy_schema_v4_is_kanji_only_and_migrates_once() {
    let temp = TempDir::new("empty-legacy-v4");
    let root = temp.path().join("store");
    let store = open_kanji_store(&root);
    drop(store);

    let manifest_path = root.join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["schema_version"] = serde_json::json!(4);
    manifest.as_object_mut().unwrap().remove("domain_id");
    manifest["assets"] = serde_json::json!([]);
    let legacy_bytes = serde_json::to_vec_pretty(&manifest).unwrap();
    fs::write(&manifest_path, &legacy_bytes).unwrap();
    fs::remove_dir_all(root.join(".runtime")).unwrap();
    fs::remove_file(root.join(".lock")).unwrap();
    let owner_before = fs::read(root.join(".owner.json")).unwrap();

    let error =
        match AssetStore::open_with_policy(StoreOptions::new(&root), PitchAccentDomainPolicy) {
            Ok(_) => panic!("пустой legacy manifest остаётся Kanji compatibility path"),
            Err(error) => error,
        };
    assert_eq!(error.code, ErrorCode::UnsupportedSchemaVersion);
    assert_eq!(fs::read(&manifest_path).unwrap(), legacy_bytes);
    assert_eq!(fs::read(root.join(".owner.json")).unwrap(), owner_before);
    assert!(!root.join(".runtime").exists());
    assert!(!root.join(".lock").exists());

    let migrated = AssetStore::open_kanji_existing(StoreOptions::new(&root)).unwrap();
    assert!(migrated.layout_migrated_on_open());
    let migrated_manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    assert_eq!(migrated_manifest["schema_version"], serde_json::json!(6));
    assert_eq!(migrated_manifest["domain_id"], "kanji");
    drop(migrated);

    let reopened = AssetStore::open_kanji_existing(StoreOptions::new(&root)).unwrap();
    assert!(!reopened.layout_migrated_on_open());
    assert!(reopened.verify_integrity().unwrap().is_empty());
}

#[test]
fn legacy_runtime_overlay_v4_migrates_and_recovers_interrupted_target() {
    let temp = TempDir::new("legacy-runtime-v4");
    let root = temp.path().join("store");
    let identity = AssetIdentity::new("kanji", "裏").unwrap();
    let store = open_kanji_store(&root);
    let candidate = ingest(
        &store,
        identity.clone(),
        temp.write("candidate.gif", b"GIF89a runtime migration candidate"),
    )
    .asset;
    store
        .validate(
            SelectionMode::Full,
            &FixedValidator::new(SemanticStatus::Uncertain),
        )
        .unwrap();
    let quarantined = store
        .verify_integrity()
        .unwrap()
        .into_iter()
        .find(|record| record.identity == identity)
        .unwrap();
    assert_eq!(quarantined.lifecycle, LifecycleState::Quarantined);
    assert!(quarantined.validation.is_some());
    assert_eq!(quarantined.sha256, candidate.sha256);
    drop(store);

    let runtime = root.join(".runtime");
    let old_path = runtime.join("assets/裏.gif");
    let new_path = runtime.join("assets/gif/裏.gif");
    fs::rename(runtime.join(&quarantined.storage_path), &old_path).unwrap();
    fs::remove_dir(runtime.join("assets/gif")).unwrap();

    let runtime_manifest_path = runtime.join("manifest.json");
    let mut runtime_manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&runtime_manifest_path).unwrap()).unwrap();
    runtime_manifest["schema_version"] = serde_json::json!(4);
    runtime_manifest
        .as_object_mut()
        .unwrap()
        .remove("domain_id");
    runtime_manifest["assets"][0]["storage_path"] = serde_json::json!("assets/裏.gif");
    runtime_manifest["assets"][0]
        .as_object_mut()
        .unwrap()
        .remove("consumer_filename");
    fs::write(
        &runtime_manifest_path,
        serde_json::to_vec_pretty(&runtime_manifest).unwrap(),
    )
    .unwrap();

    // Имитируем прерывание после подготовки вложенной жёсткой ссылки, но до
    // сохранения manifest. Восстановление должно удалить целевой файл, после
    // чего миграция безопасно завершит перенос из прежнего плоского пути.
    fs::create_dir_all(new_path.parent().unwrap()).unwrap();
    fs::hard_link(&old_path, &new_path).unwrap();
    let marker = serde_json::json!({
        "schema_version": 1,
        "store_id": runtime_manifest["store_id"],
        "domain_id": "kanji",
        "source_schema_version": 4,
        "entries": [{
            "identity": identity,
            "sha256": quarantined.sha256,
            "format": "gif",
            "old_path": "assets/裏.gif",
            "new_path": "assets/gif/裏.gif",
            "consumer_filename": "裏.gif"
        }]
    });
    fs::write(
        runtime.join(".tmp/layout-migration.json"),
        serde_json::to_vec_pretty(&marker).unwrap(),
    )
    .unwrap();
    fs::remove_file(root.join(".lock")).unwrap();

    let migrated = AssetStore::open_kanji_existing(StoreOptions::new(&root)).unwrap();
    assert!(migrated.layout_migrated_on_open());
    let records = migrated.verify_integrity().unwrap();
    assert_eq!(records.as_slice(), std::slice::from_ref(&quarantined));
    assert_eq!(records[0].lifecycle, LifecycleState::Quarantined);
    assert_eq!(records[0].validation, quarantined.validation);
    assert!(!old_path.exists());
    assert!(new_path.is_file());
    assert_eq!(
        fs::read(&new_path).unwrap(),
        b"GIF89a runtime migration candidate"
    );
    assert!(!runtime.join(".tmp/layout-migration.json").exists());
    drop(migrated);

    let reopened = AssetStore::open_kanji_existing(StoreOptions::new(&root)).unwrap();
    assert!(!reopened.layout_migrated_on_open());
    assert_eq!(reopened.verify_integrity().unwrap(), records);
}

#[test]
fn cross_domain_identity_is_rejected() {
    let temp = TempDir::new("cross-domain-identity");
    let root = temp.path().join("store");
    let store = open_kanji_store(&root);
    for (character, name) in [('日', "first.gif"), ('月', "second.gif")] {
        store
            .ingest_verified(
                VerifiedIngestRequest {
                    identity: AssetIdentity::new("kanji", character.to_string()).unwrap(),
                    bytes: [b"GIF89a".as_slice(), name.as_bytes()].concat(),
                    provenance: Provenance {
                        source_kind: "fixture".into(),
                        source_name: name.into(),
                    },
                    domain_metadata: None,
                    replace_expected_sha256: None,
                },
                &FixedValidator::new(SemanticStatus::Verified),
            )
            .unwrap();
    }

    let cross_domain = store
        .ingest(IngestRequest {
            identity: AssetIdentity::new("pitch_accent", "幽霊").unwrap(),
            source_path: temp.write("pitch.png", b"not a png"),
            expected_source_sha256: None,
            domain_metadata: None,
            replace_expected_sha256: None,
        })
        .unwrap_err();
    assert_eq!(cross_domain.code, ErrorCode::InvalidIdentity);
    drop(store);
    assert!(AssetStore::open_kanji_existing(StoreOptions::new(&root)).is_ok());
}

#[test]
fn duplicate_consumer_filename_is_rejected_before_manifest_commit() {
    let temp = TempDir::new("duplicate-consumer-filename");
    let root = temp.path().join("store");
    let store =
        AssetStore::open_with_policy(StoreOptions::new(&root), DuplicateConsumerPolicy).unwrap();
    for key in ["first", "second"] {
        let result = store.ingest_verified(
            VerifiedIngestRequest {
                identity: AssetIdentity::new("duplicate-test", key).unwrap(),
                bytes: [b"GIF89a".as_slice(), key.as_bytes()].concat(),
                provenance: Provenance {
                    source_kind: "fixture".into(),
                    source_name: format!("{key}.gif"),
                },
                domain_metadata: None,
                replace_expected_sha256: None,
            },
            &FixedValidator::new(SemanticStatus::Verified),
        );
        if key == "first" {
            result.expect("первая consumer filename должна публиковаться");
        } else {
            let error = result.expect_err("повторный consumer filename отклоняется до commit");
            assert_eq!(error.code, ErrorCode::ManifestCorrupt);
            assert!(error.message.contains("повторяющиеся consumer_filename"));
        }
    }

    let assets = store.verify_integrity().unwrap();
    assert_eq!(assets.len(), 1);
    assert_eq!(assets[0].identity.key, "first");
    assert!(!root.join("assets/second.gif").exists());
}

#[test]
fn integrity_failures_are_explicit_and_traversal_is_rejected() {
    let temp = TempDir::new("integrity");
    let root = temp.path().join("store");
    let store = open_store(&root);
    let source = temp.write("asset.bin", b"integrity fixture");
    let record = ingest(&store, identity("one"), source).asset;
    let object = asset_path(&root, &record);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&object, fs::Permissions::from_mode(0o600))
            .expect("test делает owned object доступным для порчи");
    }
    fs::write(&object, b"tampered").expect("synthetic corruption записывается");
    assert_eq!(
        store.verify_integrity().unwrap_err().code,
        ErrorCode::IntegrityMismatch
    );

    let reopened = AssetStore::open(StoreOptions::new(&root));
    assert_eq!(reopened.unwrap_err().code, ErrorCode::IntegrityMismatch);
}

#[test]
fn orphan_asset_hash_is_checked_even_without_a_manifest_record() {
    let temp = TempDir::new("orphan-asset");
    let root = temp.path().join("store");
    let store = open_store(&root);
    let orphan = root
        .join("assets")
        .join(format!("orphan-{}.bin", "0".repeat(64)));
    fs::write(&orphan, b"unregistered bytes").expect("synthetic orphan object is written");

    assert_eq!(
        store.verify_integrity().unwrap_err().code,
        ErrorCode::IntegrityMismatch
    );
}

#[test]
fn stale_hash_suffixed_kanji_path_is_rejected_after_stable_layout_migration() {
    let temp = TempDir::new("stale-kanji-path");
    let root = temp.path().join("store");
    let store = open_kanji_store(&root);
    fs::write(
        root.join("assets")
            .join(format!("元-{}.gif", "0".repeat(64))),
        b"GIF89a stale bytes",
    )
    .expect("stale legacy path fixture is written");

    assert_eq!(
        store.verify_integrity().unwrap_err().code,
        ErrorCode::UnexpectedPath
    );
}

#[test]
fn absent_file_unsupported_schema_corrupt_json_and_traversal_fail_closed() {
    let temp = TempDir::new("manifest-errors");
    let root = temp.path().join("store");
    let store = open_store(&root);
    let record = ingest(&store, identity("one"), temp.write("asset.bin", b"payload")).asset;
    let path = asset_path(&root, &record);
    fs::remove_file(&path).expect("test удаляет owned file");
    assert_eq!(
        store.verify_integrity().unwrap_err().code,
        ErrorCode::MissingAssetFile
    );
    fs::write(&path, b"payload").expect("owned file восстанавливается");

    let manifest_path = root.join(".runtime/manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).expect("manifest читается"))
            .expect("manifest json valid");
    let supported_schema_version = manifest["schema_version"].clone();
    manifest["schema_version"] = serde_json::json!(999);
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .expect("schema mutation записывается");
    assert_eq!(
        store.verify_integrity().unwrap_err().code,
        ErrorCode::UnsupportedSchemaVersion
    );

    manifest["schema_version"] = supported_schema_version;
    manifest["assets"][0]["storage_path"] = serde_json::json!("../../decks/media/x.png");
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .expect("traversal mutation записывается");
    assert_eq!(
        store.verify_integrity().unwrap_err().code,
        ErrorCode::PathTraversal
    );

    fs::write(&manifest_path, b"{").expect("corrupt json записывается");
    assert_eq!(
        store.verify_integrity().unwrap_err().code,
        ErrorCode::ManifestCorrupt
    );
}

#[test]
fn store_root_must_not_overlap_protected_decks_tree_or_use_parent_aliases() {
    let temp = TempDir::new("boundary");
    let repository = temp.path().join("repository");
    let decks = repository.join("decks");
    fs::create_dir_all(decks.join("japanese/media")).expect("synthetic decks root");
    let inside = decks.join("assets");
    let error = AssetStore::open(StoreOptions::new(&inside).protect_from(&decks)).unwrap_err();
    assert_eq!(error.code, ErrorCode::BoundaryViolation);
    assert!(!inside.exists(), "запрещённый root не создаётся");

    let error = AssetStore::open(StoreOptions::new(&repository).protect_from(&decks)).unwrap_err();
    assert_eq!(error.code, ErrorCode::BoundaryViolation);

    let error = AssetStore::open(StoreOptions::new(repository.join("../outside"))).unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidStoreRoot);

    let unowned = temp.path().join("user-directory");
    fs::create_dir_all(&unowned).unwrap();
    let important = unowned.join("important.txt");
    fs::write(&important, b"user-owned").unwrap();
    let error = AssetStore::open(StoreOptions::new(&unowned)).unwrap_err();
    assert_eq!(error.code, ErrorCode::StoreNotOwned);
    assert_eq!(fs::read(&important).unwrap(), b"user-owned");
    assert!(
        !unowned.join(".lock").exists(),
        "отказ не оставляет lock-файл"
    );
    for name in ["assets", ".tmp", ".owner.json"] {
        assert!(!unowned.join(name).exists(), "отказ не создаёт {name}");
    }
    let remaining: Vec<_> = fs::read_dir(&unowned)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(remaining, ["important.txt"]);

    let manifest_only = temp.path().join("manifest-only");
    fs::create_dir(&manifest_only).unwrap();
    let manifest_path = manifest_only.join("manifest.json");
    let foreign_manifest = serde_json::to_vec_pretty(&serde_json::json!({
        "schema_version": 2,
        "store_id": "unowned-empty-state",
        "revision": 0,
        "assets": []
    }))
    .unwrap();
    fs::write(&manifest_path, &foreign_manifest).unwrap();
    let error = AssetStore::open(StoreOptions::new(&manifest_only)).unwrap_err();
    assert_eq!(error.code, ErrorCode::StoreNotOwned);
    assert_eq!(fs::read(&manifest_path).unwrap(), foreign_manifest);
    for name in [".lock", "assets", ".tmp", ".owner.json"] {
        assert!(
            !manifest_only.join(name).exists(),
            "manifest не усыновлен; {name} отсутствует"
        );
    }

    let interrupted_init = temp.path().join("owner-without-manifest");
    fs::create_dir(&interrupted_init).unwrap();
    let owner_path = interrupted_init.join(".owner.json");
    let owner_bytes = serde_json::to_vec_pretty(&serde_json::json!({
        "schema_version": 2,
        "store_id": "incomplete-initialization"
    }))
    .unwrap();
    fs::write(&owner_path, &owner_bytes).unwrap();
    let error = AssetStore::open(StoreOptions::new(&interrupted_init)).unwrap_err();
    assert_eq!(error.code, ErrorCode::ManifestMissing);
    assert_eq!(fs::read(&owner_path).unwrap(), owner_bytes);
    for name in [".lock", "assets", ".tmp", "manifest.json"] {
        assert!(
            !interrupted_init.join(name).exists(),
            "incomplete initialization fails closed without creating {name}"
        );
    }

    let new_root = temp.path().join("new-empty-store");
    let new_store = AssetStore::open(StoreOptions::new(&new_root)).unwrap();
    assert!(new_store.initialized_on_open());
    drop(new_store);
    let reopened = AssetStore::open(StoreOptions::new(&new_root)).unwrap();
    assert!(!reopened.initialized_on_open());

    let empty_root = temp.path().join("preexisting-empty-store");
    fs::create_dir(&empty_root).unwrap();
    let empty_store = AssetStore::open(StoreOptions::new(&empty_root)).unwrap();
    assert!(empty_store.initialized_on_open());
    drop(empty_store);
    let reopened = AssetStore::open(StoreOptions::new(&empty_root)).unwrap();
    assert!(!reopened.initialized_on_open());

    fs::write(decks.join("japanese/media/user.bin"), b"user-owned").unwrap();
    let store = open_store(&repository.join(".asset-store/kanji"));
    assert!(store.verify_integrity().unwrap().is_empty());
    let validator = ValidatorIdentity::new("fixture", "1").unwrap();
    assert!(
        store
            .select(SelectionMode::Full, &validator)
            .unwrap()
            .is_empty(),
        "full не индексирует соседний decks/media"
    );
}

#[cfg(unix)]
#[test]
fn symlink_store_root_and_object_symlink_are_rejected() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new("symlink-boundary");
    let root = temp.path().join("store");
    let store = open_kanji_store(&root);
    let record = ingest(
        &store,
        AssetIdentity::new("kanji", "髪").unwrap(),
        temp.write("asset.bin", b"asset"),
    )
    .asset;
    let object_path = asset_path(&root, &record);
    let external = temp.write("outside.bin", b"outside");
    fs::remove_file(&object_path).expect("owned object удалён для symlink fixture");
    symlink(&external, &object_path).expect("object symlink создаётся");
    assert_eq!(
        store.verify_integrity().unwrap_err().code,
        ErrorCode::BoundaryViolation
    );

    let target = temp.path().join("real-store");
    open_store(&target);
    let alias = temp.path().join("store-alias");
    symlink(&target, &alias).expect("root symlink создаётся");
    assert_eq!(
        AssetStore::open(StoreOptions::new(alias)).unwrap_err().code,
        ErrorCode::BoundaryViolation
    );

    let repository = temp.path().join("linked-repository");
    let actual_decks = temp.path().join("external-decks");
    fs::create_dir_all(&repository).unwrap();
    fs::create_dir_all(&actual_decks).unwrap();
    symlink(&actual_decks, repository.join("decks")).expect("decks symlink created");
    let outside_alias = actual_decks.join("candidate-store");
    let error =
        AssetStore::open(StoreOptions::new(&outside_alias).protect_from(repository.join("decks")))
            .unwrap_err();
    assert_eq!(error.code, ErrorCode::BoundaryViolation);
    assert!(!outside_alias.exists());
}

#[test]
fn failed_validator_never_publishes_verified_state_and_unicode_identity_roundtrips() {
    let temp = TempDir::new("failure-unicode");
    let root = temp.path().join("store");
    let store = open_kanji_store(&root);
    let unicode = AssetIdentity::new("kanji", "𠮷").unwrap();
    let source = temp.write("kanji.dat", b"unicode bytes");
    let record = ingest(&store, unicode.clone(), source).asset;
    let failed = FixedValidator {
        identity: ValidatorIdentity::new("broken", "1").unwrap(),
        status: SemanticStatus::Verified,
        fail: true,
    };
    let report = store.validate(SelectionMode::Full, &failed).unwrap();
    assert_eq!(report.changed, 0);
    let reopened = open_kanji_store(&root);
    let restored = reopened.verify_integrity().unwrap();
    assert_eq!(restored[0].identity, unicode);
    assert_eq!(restored[0].sha256, record.sha256);
    assert_eq!(restored[0].lifecycle, LifecycleState::Pending);
    assert!(restored[0].validation.is_none());
}

#[test]
fn verified_without_nonempty_evidence_is_rejected() {
    let temp = TempDir::new("empty-evidence");
    let root = temp.path().join("store");
    let store = open_store(&root);
    ingest(
        &store,
        identity("candidate"),
        temp.write("candidate.bin", b"bytes"),
    );

    let error = store
        .validate(SelectionMode::Full, &NoEvidenceValidator)
        .expect_err("VERIFIED без evidence запрещён");
    assert_eq!(error.code, ErrorCode::InvalidValidationEvidence);
    let record = open_store(&root).verify_integrity().unwrap().remove(0);
    assert_eq!(record.lifecycle, LifecycleState::Pending);
    assert!(record.validation.is_none());
}

fn publish_fixture(
    store: &AssetStore,
    identity: &AssetIdentity,
    bytes: &[u8],
    expected: Option<String>,
) -> Result<asset_store::store::VerifiedIngestOutcome, asset_store::AssetError> {
    store.ingest_verified(
        VerifiedIngestRequest {
            identity: identity.clone(),
            bytes: bytes.to_vec(),
            provenance: Provenance {
                source_kind: "fixture".into(),
                source_name: "fixture.bin".into(),
            },
            domain_metadata: None,
            replace_expected_sha256: expected,
        },
        &FixedValidator::new(SemanticStatus::Verified),
    )
}

#[test]
fn schema_v5_upgrades_to_format_keys_without_changing_bytes_or_decisions() {
    let temp = TempDir::new("schema-v5-upgrade");
    let root = temp.path().join("store");
    let store = open_kanji_store(&root);
    let identity = AssetIdentity::new("kanji", "元").unwrap();
    // Решение человека требует полностью декодируемого изображения.
    let gif_bytes = {
        let mut bytes = Vec::new();
        let mut encoder = gif::Encoder::new(&mut bytes, 2, 2, &[0, 0, 0, 255, 255, 255]).unwrap();
        encoder
            .write_frame(&gif::Frame {
                width: 2,
                height: 2,
                buffer: std::borrow::Cow::Borrowed(&[0, 1, 1, 0]),
                ..gif::Frame::default()
            })
            .unwrap();
        drop(encoder);
        bytes
    };
    let gif = publish_fixture(&store, &identity, &gif_bytes, None)
        .unwrap()
        .asset
        .unwrap();
    store
        .attest(asset_store::store::HumanAttestationRequest {
            identity: identity.clone(),
            expected_sha256: gif.sha256.clone(),
            decision: asset_store::HumanDecision::Approve,
            reason: "проверено человеком".into(),
        })
        .unwrap();
    drop(store);

    // Состояние предыдущей версии: та же раскладка, но схема 5.
    for manifest_path in [
        root.join("manifest.json"),
        root.join(".runtime/manifest.json"),
    ] {
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest["schema_version"] = serde_json::json!(5);
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
    }
    let manifest_path = root.join("manifest.json");
    let v5_bytes = fs::read(&manifest_path).unwrap();
    let v5: serde_json::Value = serde_json::from_slice(&v5_bytes).unwrap();

    // Чтение только для чтения не меняет манифест схемы 5.
    let read = AssetStore::read_verified(
        &root,
        std::slice::from_ref(&identity),
        &publishable_validator(),
    )
    .unwrap();
    assert_eq!(read[0].bytes, gif_bytes);
    assert_eq!(fs::read(&manifest_path).unwrap(), v5_bytes);

    let store = open_kanji_store(&root);
    assert!(store.layout_migrated_on_open());
    let v6: serde_json::Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    assert_eq!(v6["schema_version"], 6);
    assert_eq!(
        v6["assets"], v5["assets"],
        "записи, SHA и решения не меняются"
    );
    assert_eq!(
        v6["revision"].as_u64().unwrap(),
        v5["revision"].as_u64().unwrap() + 1
    );
    assert_eq!(fs::read(root.join("assets/gif/元.gif")).unwrap(), gif_bytes);

    // После перехода PNG того же символа — отдельная запись; GIF не меняется.
    let png_bytes = [b"\x89PNG\r\n\x1a\n".as_slice(), b"local render"].concat();
    let png = publish_fixture(&store, &identity, &png_bytes, None)
        .unwrap()
        .asset
        .unwrap();
    assert_eq!(png.consumer_filename, "元.png");
    let records = store.verify_integrity().unwrap();
    assert_eq!(records.len(), 2);
    let kept_gif = records
        .iter()
        .find(|record| record.format == DetectedFormat::Gif)
        .unwrap();
    assert_eq!(
        serde_json::to_value(kept_gif).unwrap(),
        v5["assets"][0],
        "GIF, его SHA и решение человека сохранены"
    );

    // Событие публикации PNG знает свой слот; журнал согласован.
    let events = store.journal_events(None, Some("元"), None).unwrap();
    let last = events.last().unwrap();
    assert_eq!(last.format, Some(DetectedFormat::Png));
    assert_eq!(last.new_sha256.as_deref(), Some(png.sha256.as_str()));
    assert!(store.journal_check().unwrap().is_clean());
    drop(store);
    assert!(!open_kanji_store(&root).layout_migrated_on_open());
}

#[test]
fn runtime_candidate_of_other_format_coexists_with_published_record() {
    let temp = TempDir::new("cross-format-runtime");
    let root = temp.path().join("store");
    let store = open_kanji_store(&root);
    let identity = AssetIdentity::new("kanji", "字").unwrap();
    let png_bytes = [b"\x89PNG\r\n\x1a\n".as_slice(), b"published png"].concat();
    publish_fixture(&store, &identity, &png_bytes, None).unwrap();
    let source = temp.path().join("candidate.gif");
    fs::write(&source, b"GIF89a pending candidate").unwrap();
    let candidate = store
        .ingest(IngestRequest {
            identity: identity.clone(),
            source_path: source,
            expected_source_sha256: None,
            domain_metadata: None,
            replace_expected_sha256: None,
        })
        .unwrap();
    assert!(
        candidate.previous.is_none(),
        "GIF-кандидат не вытесняет PNG"
    );
    assert_eq!(candidate.asset.lifecycle, LifecycleState::Pending);
    let records = store.verify_integrity().unwrap();
    assert_eq!(records.len(), 2);
    assert!(fs::read(root.join("assets/png/字.png")).is_ok());
    assert!(AssetStore::verify_publishable_corpus(&root, &publishable_validator()).is_ok());
    let read = AssetStore::read_verified(
        &root,
        std::slice::from_ref(&identity),
        &publishable_validator(),
    )
    .unwrap();
    assert_eq!(
        read[0].bytes, png_bytes,
        "недоверенный GIF не вытесняет доверенный PNG"
    );
}
