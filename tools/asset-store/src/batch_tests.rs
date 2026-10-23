use super::*;
use crate::temp_workspace::TempWorkspace;
use std::fs;
use std::os::unix::fs::symlink;

fn validator() -> ValidatorIdentity {
    ValidatorIdentity::new("kanjivg-pixel-chamfer", "synthetic-v1").unwrap()
}

fn identity(character: char) -> AssetIdentity {
    AssetIdentity::new("kanji", character.to_string()).unwrap()
}

// Историческая политика остаётся читаемой и проверяется независимо от новых пакетов.
fn batch(characters: &[char]) -> KanjiBatch {
    let mut state = KanjiBatch::new(
        "synthetic-batch".into(),
        characters.iter().copied().map(identity).collect(),
        validator(),
    )
    .unwrap();
    state.policy = AggregatePolicy::current(validator());
    for item in &mut state.items {
        item.aggregate.policy_version = state.policy.version.clone();
    }
    state
}

#[test]
fn new_batches_hold_a_valid_uncertain_candidate_without_aggregate_promotion_or_redownload() {
    let mut state =
        KanjiBatch::new("single-candidate".into(), vec![identity('漢')], validator()).unwrap();
    let uncertain = candidate("retained", SemanticStatus::Uncertain, 0.005, 0.010);
    acquire(&mut state, '漢', uncertain.clone());
    assert_eq!(state.items[0].status, BatchItemStatus::AwaitingHuman);
    assert!(!state.items[0].aggregate.accepted);
    assert_eq!(
        state.items[0].current_sha256.as_deref(),
        Some(uncertain.sha256.as_str())
    );
    assert!(state.next_round().is_empty());
    state.validate().unwrap();
}

#[test]
fn empty_aggregate_uses_the_policy_version_and_accepts_legacy_single_candidate_state() {
    let mut state = KanjiBatch::new(
        "single-candidate-empty".into(),
        vec![identity('漢')],
        validator(),
    )
    .unwrap();
    assert_eq!(
        state.items[0].aggregate.policy_version,
        SINGLE_CANDIDATE_POLICY_VERSION
    );

    // Старые пакеты записывали для пустого агрегата версию прежней политики.
    state.items[0].aggregate.policy_version = AGGREGATE_POLICY_VERSION.into();
    state.validate().unwrap();

    state
        .revalidate_candidates(validator(), |_, _| unreachable!("кандидатов нет"))
        .unwrap();
    assert_eq!(
        state.items[0].aggregate.policy_version,
        SINGLE_CANDIDATE_POLICY_VERSION
    );
    state.validate().unwrap();
}

#[test]
fn single_candidate_corrupt_bytes_are_reacquired_before_human_review() {
    let mut state = KanjiBatch::new(
        "single-candidate-corrupt".into(),
        vec![identity('漢')],
        validator(),
    )
    .unwrap();
    acquire(
        &mut state,
        '漢',
        candidate_with_validator("corrupt", SemanticStatus::Corrupt, validator(), true),
    );

    let item = &state.items[0];
    assert_eq!(item.status, BatchItemStatus::Unresolved);
    assert!(item.review_reason.is_none());
    assert_eq!(state.next_round(), vec![identity('漢')]);
    assert!(state.review_queue().is_empty());
    state.validate().unwrap();
}

fn bytes(label: &str) -> Vec<u8> {
    // Здесь проверяются границы состояния и хеша. Полное декодирование GIF проверяет владелец.
    let mut bytes = b"GIF89a".to_vec();
    bytes.extend_from_slice(label.as_bytes());
    bytes
}

fn record_with_validator(
    bytes: &[u8],
    status: SemanticStatus,
    distance: f64,
    margin: f64,
    validator: ValidatorIdentity,
) -> ValidationRecord {
    ValidationRecord {
        status,
        validator,
        content_sha256: sha256_hex(bytes),
        evidence: vec![ValidationEvidence {
            kind: "pixel_reference_comparison".into(),
            summary: "синтетическое семантическое свидетельство".into(),
            details: Some(
                serde_json::json!({"expected_distance": distance, "nearest_margin": margin, "nearest_other": "字"}),
            ),
        }],
    }
}

fn record(bytes: &[u8], status: SemanticStatus, distance: f64, margin: f64) -> ValidationRecord {
    record_with_validator(bytes, status, distance, margin, validator())
}

fn candidate_with_validator(
    label: &str,
    status: SemanticStatus,
    validator: ValidatorIdentity,
    technically_valid: bool,
) -> BatchCandidate {
    let bytes = bytes(label);
    let hash = sha256_hex(&bytes);
    BatchCandidate {
        storage_path: candidate_path(&hash, DetectedFormat::Gif).unwrap(),
        sha256: hash,
        format: DetectedFormat::Gif,
        technically_valid,
        automated: record_with_validator(&bytes, status, 0.005, 0.010, validator),
        acquisition: None,
    }
}

fn revalidation(
    candidate: &BatchCandidate,
    status: SemanticStatus,
    validator: ValidatorIdentity,
    technically_valid: bool,
) -> CandidateRevalidation {
    let mut automated = record_with_validator(&[], status, 0.005, 0.010, validator);
    automated.content_sha256.clone_from(&candidate.sha256);
    CandidateRevalidation {
        automated,
        technically_valid,
    }
}

fn candidate(label: &str, status: SemanticStatus, distance: f64, margin: f64) -> BatchCandidate {
    let mut candidate = candidate_with_validator(
        label,
        status,
        validator(),
        status != SemanticStatus::Corrupt,
    );
    candidate.automated = record(&bytes(label), status, distance, margin);
    candidate
}

fn acquire(state: &mut KanjiBatch, character: char, candidate: BatchCandidate) {
    state
        .record_attempt(
            &identity(character),
            BatchAttemptInput::Candidate { candidate },
        )
        .unwrap();
}

fn candidate_from_attempt(attempt: &BatchAttempt) -> &BatchCandidate {
    let BatchAttemptInput::Candidate { candidate } = &attempt.result else {
        panic!("ожидался сохранённый кандидат");
    };
    candidate
}

fn failed(state: &mut KanjiBatch, character: char) {
    state
        .record_attempt(
            &identity(character),
            BatchAttemptInput::Failed {
                code: "network".into(),
                message: "источник недоступен".into(),
            },
        )
        .unwrap();
}

struct Temporary {
    workspace: TempWorkspace,
}

impl Temporary {
    fn new() -> Self {
        Self {
            workspace: TempWorkspace::create("asset-store-kanji-batch-tests").unwrap(),
        }
    }

    fn root(&self) -> &std::path::Path {
        self.workspace.path()
    }
}

#[test]
fn large_batch_has_complete_breadth_first_frontier() {
    let characters: Vec<_> = (0x4E00..0x4E00 + 1000)
        .map(|code| char::from_u32(code).unwrap())
        .collect();
    let mut state = batch(&characters);
    let first = candidate("first", SemanticStatus::Uncertain, 0.030, 0.001);
    acquire(&mut state, characters[0], first.clone());
    assert!(!state.next_round().contains(&identity(characters[0])));
    assert!(
        state
            .record_attempt(
                &identity(characters[0]),
                BatchAttemptInput::Candidate { candidate: first }
            )
            .is_err()
    );
    assert_eq!(state.next_round().len(), 999);
    for &character in &characters[1..] {
        failed(&mut state, character);
    }
    assert_eq!(state.next_round().len(), 1000);
    assert!(state.next_round().contains(&identity(characters[0])));
    state.validate().unwrap();
}

#[test]
fn duplicate_hashes_do_not_inflate_semantic_count() {
    let mut state = batch(&['漢']);
    let uncertain = candidate("same", SemanticStatus::Uncertain, 0.030, 0.001);
    for _ in 0..5 {
        acquire(&mut state, '漢', uncertain.clone());
    }
    let item = &state.items[0];
    assert_eq!(item.status, BatchItemStatus::AwaitingHuman);
    assert_eq!(item.attempts.len(), 5);
    assert_eq!(
        item.attempts
            .iter()
            .filter(|attempt| attempt.duplicate_sha256)
            .count(),
        4
    );
    assert_eq!(item.aggregate.distinct_valid_hashes.len(), 1);
    assert_eq!(item.aggregate.expected_distance.as_ref().unwrap().count, 1);
    assert!(state.next_round().is_empty());
    assert_eq!(state.review_queue().len(), 1);
    state.validate().unwrap();
}

#[test]
fn complementary_distinct_samples_allow_versioned_exact_aggregate() {
    let mut state = batch(&['漢']);
    let farther = candidate("farther", SemanticStatus::Uncertain, 0.025, 0.010);
    let close = candidate("close", SemanticStatus::Uncertain, 0.005, 0.005);
    let selected = close.sha256.clone();
    acquire(&mut state, '漢', farther);
    acquire(&mut state, '漢', close);
    let item = &state.items[0];
    assert_eq!(item.status, BatchItemStatus::AutoVerified);
    assert!(!item.is_ready());
    assert!(!state.is_resolved());
    assert_eq!(item.current_sha256.as_deref(), Some(selected.as_str()));
    let metric = item.aggregate.expected_distance.as_ref().unwrap();
    assert_eq!(metric.count, 2);
    assert!((metric.mean - 0.015).abs() < 1e-12);
    assert_eq!(metric.minimum, 0.005);
    assert_eq!(metric.maximum, 0.025);
    let resolution = state.aggregate_decision(&identity('漢')).unwrap().unwrap();
    assert_eq!(resolution.candidate.sha256, selected);
    assert_eq!(
        resolution.candidate.automated.status,
        SemanticStatus::Uncertain
    );
    assert_eq!(resolution.decision.status, SemanticStatus::Verified);
    assert_eq!(
        resolution.decision.evidence[0].kind,
        "kanji_batch_aggregate"
    );
    assert_eq!(
        resolution.decision.evidence[0].details.as_ref().unwrap()["policy"]["version"],
        AGGREGATE_POLICY_VERSION
    );
    state
        .mark_published_ready(&identity('漢'), &selected, BatchTrustSource::Aggregate)
        .unwrap();
    assert!(state.is_resolved());
    state.validate().unwrap();
}

#[test]
fn aggregate_waits_for_human_when_selected_sample_margin_fails() {
    let mut state = batch(&['漢']);
    let farther = candidate(
        "farther-low-margin",
        SemanticStatus::Uncertain,
        0.025,
        0.010,
    );
    let close = candidate("close-low-margin", SemanticStatus::Uncertain, 0.005, -0.001);
    let selected = close.sha256.clone();
    acquire(&mut state, '漢', farther);
    acquire(&mut state, '漢', close.clone());
    for _ in 0..3 {
        acquire(&mut state, '漢', close.clone());
    }

    let item = &state.items[0];
    assert_eq!(item.status, BatchItemStatus::AwaitingHuman);
    assert_eq!(item.current_sha256.as_deref(), Some(selected.as_str()));
    assert!(!item.aggregate.accepted);
    assert_eq!(item.aggregate.margin.as_ref().unwrap().count, 2);
    assert!((item.aggregate.margin.as_ref().unwrap().mean - 0.0045).abs() < 1e-12);
    assert!(state.aggregate_decision(&identity('漢')).unwrap().is_none());
    state.validate().unwrap();
}

#[test]
fn previous_aggregate_policy_version_is_rejected() {
    let mut state = batch(&['漢']);
    state.policy.version = "kanji-distinct-mean-v1".into();
    assert!(state.validate().is_err());
}

#[test]
fn individual_verified_waits_for_canonical_publication_and_skips_acquisition() {
    let mut state = batch(&['漢']);
    let good = candidate("good", SemanticStatus::Verified, 0.01, 0.01);
    let hash = good.sha256.clone();
    acquire(&mut state, '漢', good);
    assert_eq!(state.items[0].status, BatchItemStatus::AutoVerified);
    assert!(!state.is_resolved());
    assert!(state.next_round().is_empty());
    state
        .mark_published_ready(&identity('漢'), &hash, BatchTrustSource::Automated)
        .unwrap();
    assert!(state.items[0].is_ready());
    assert!(state.is_resolved());
    state.validate().unwrap();
}

#[test]
fn technically_invalid_candidates_are_not_semantic_samples() {
    let mut state = batch(&['漢']);
    let mut invalid = candidate("bad-technical", SemanticStatus::Uncertain, 0.0, 1.0);
    invalid.technically_valid = false;
    acquire(&mut state, '漢', invalid);
    acquire(
        &mut state,
        '漢',
        candidate("corrupt", SemanticStatus::Corrupt, 0.0, 1.0),
    );
    acquire(
        &mut state,
        '漢',
        candidate("uncertain", SemanticStatus::Uncertain, 0.04, 0.001),
    );
    assert_eq!(
        state.items[0]
            .aggregate
            .expected_distance
            .as_ref()
            .unwrap()
            .count,
        1
    );
    assert!(!state.items[0].aggregate.accepted);
    state.validate().unwrap();
}

#[test]
fn metrics_thresholds_and_unknown_policy_fail_closed() {
    let (maximum_distance, minimum_margin) =
        crate::kanji_validator::registered_aggregate_thresholds();
    let mut state = batch(&['漢']);
    acquire(
        &mut state,
        '漢',
        candidate(
            "border",
            SemanticStatus::Uncertain,
            maximum_distance + 0.0000001,
            minimum_margin,
        ),
    );
    assert!(!state.items[0].aggregate.accepted);
    state.policy.maximum_expected_distance = 0.03;
    assert!(state.validate().is_err());
    state.policy = AggregatePolicy::current(validator());
    state.policy.version = "unknown".into();
    assert!(state.validate().is_err());
}

#[test]
fn aggregate_policy_uses_registered_validator_thresholds() {
    let (maximum_distance, minimum_margin) =
        crate::kanji_validator::registered_aggregate_thresholds();
    let policy = AggregatePolicy::current(validator());
    assert_eq!(policy.maximum_expected_distance, maximum_distance);
    assert_eq!(policy.minimum_margin, minimum_margin);

    let mut state = batch(&['漢']);
    state.policy.maximum_expected_distance = maximum_distance + 0.001;
    assert!(state.validate().is_err());
}

#[test]
fn candidate_validator_and_hash_are_pinned() {
    let mut state = batch(&['漢']);
    let mut wrong = candidate("wrong-version", SemanticStatus::Uncertain, 0.03, 0.001);
    wrong.automated.validator.version = "different".into();
    assert!(
        state
            .record_attempt(
                &identity('漢'),
                BatchAttemptInput::Candidate { candidate: wrong }
            )
            .is_err()
    );
    let mut wrong = candidate("wrong-hash", SemanticStatus::Uncertain, 0.03, 0.001);
    wrong.automated.content_sha256 = "0".repeat(64);
    assert!(
        state
            .record_attempt(
                &identity('漢'),
                BatchAttemptInput::Candidate { candidate: wrong }
            )
            .is_err()
    );
    assert!(state.items[0].attempts.is_empty());
}

#[test]
fn human_confirm_retains_automated_status_and_requires_exact_candidate() {
    let mut state = batch(&['漢']);
    let candidate = candidate("human", SemanticStatus::Rejected, 0.04, -0.02);
    let hash = candidate.sha256.clone();
    for _ in 0..MAX_ACQUISITION_ROUNDS {
        acquire(&mut state, '漢', candidate.clone());
    }
    let mut decision = HumanBatchDecision {
        identity: identity('漢'),
        candidate_sha256: hash.clone(),
        action: HumanBatchAction::Confirm,
        reason: "漢 - подтверждён".into(),
    };
    assert!(state.decide(decision.clone(), &bytes("different")).is_err());
    decision.candidate_sha256 = sha256_hex(bytes("stale"));
    assert!(state.decide(decision.clone(), &bytes("stale")).is_err());
    decision.candidate_sha256 = hash.clone();
    state.decide(decision.clone(), &bytes("human")).unwrap();
    assert_eq!(state.items[0].status, BatchItemStatus::HumanVerified);
    assert!(!state.is_resolved());
    assert_eq!(
        find_candidate(&state.items[0], &hash)
            .unwrap()
            .automated
            .status,
        SemanticStatus::Rejected
    );
    state
        .mark_published_ready(&identity('漢'), &hash, BatchTrustSource::Human)
        .unwrap();
    let revision = state.revision;
    state.decide(decision, &bytes("human")).unwrap();
    assert_eq!(state.revision, revision + 1);
    assert!(!state.items[0].is_ready());
    assert_eq!(state.items[0].human_decisions.len(), 1);
    state
        .mark_published_ready(&identity('漢'), &hash, BatchTrustSource::Human)
        .unwrap();
    state.validate().unwrap();
}

#[test]
fn human_reason_limit_is_shared_and_persisted_state_fails_closed() {
    let mut state = batch(&['漢']);
    let selected = candidate("reason-boundary", SemanticStatus::Uncertain, 0.03, 0.001);
    let bytes = bytes("reason-boundary");
    acquire(&mut state, '漢', selected.clone());
    let decision = |reason: String| HumanBatchDecision {
        identity: identity('漢'),
        candidate_sha256: selected.sha256.clone(),
        action: HumanBatchAction::Confirm,
        reason,
    };

    let mut accepted = state.clone();
    accepted
        .decide(decision("r".repeat(MAX_HUMAN_REASON_BYTES)), &bytes)
        .unwrap();
    assert_eq!(
        accepted.items[0].human_decisions[0].reason.len(),
        MAX_HUMAN_REASON_BYTES
    );
    accepted.validate().unwrap();

    assert!(
        state
            .decide(decision("r".repeat(MAX_HUMAN_REASON_BYTES + 1)), &bytes,)
            .is_err()
    );
    state.validate().unwrap();

    accepted.items[0].human_decisions[0].reason.push('r');
    assert!(accepted.validate().is_err());
}

#[test]
fn persisted_acquisition_round_limit_rejects_an_extra_attempt() {
    let mut state = batch(&['漢']);
    for _ in 0..MAX_ACQUISITION_ROUNDS {
        failed(&mut state, '漢');
    }
    assert_eq!(
        state.items[0].attempts.len(),
        MAX_ACQUISITION_ROUNDS as usize
    );
    state.validate().unwrap();

    state.items[0].attempts.push(BatchAttempt {
        index: MAX_ACQUISITION_ROUNDS + 1,
        generation: 0,
        round: MAX_ACQUISITION_ROUNDS + 1,
        result: BatchAttemptInput::Failed {
            code: "network".into(),
            message: "источник недоступен".into(),
        },
        duplicate_sha256: false,
    });
    assert!(state.validate().is_err());
}

#[test]
fn retry_reason_limit_is_enforced_in_domain_and_persisted_state() {
    let mut state = batch(&['漢']);
    for _ in 0..MAX_ACQUISITION_ROUNDS {
        failed(&mut state, '漢');
    }
    let maximum_reason = "r".repeat(MAX_HUMAN_REASON_BYTES);
    state
        .retry_acquisition(&identity('漢'), maximum_reason)
        .unwrap();
    state.validate().unwrap();

    let mut oversized_state = state.clone();
    oversized_state.items[0]
        .review_reason
        .as_mut()
        .unwrap()
        .push('r');
    assert!(oversized_state.validate().is_err());

    let mut rejected = batch(&['漢']);
    for _ in 0..MAX_ACQUISITION_ROUNDS {
        failed(&mut rejected, '漢');
    }
    assert!(
        rejected
            .retry_acquisition(&identity('漢'), "r".repeat(MAX_HUMAN_REASON_BYTES + 1),)
            .is_err()
    );
    rejected.validate().unwrap();
}

#[test]
fn corrupt_candidate_cannot_receive_human_confirmation() {
    let mut state = batch(&['漢']);
    let corrupt = candidate("corrupt", SemanticStatus::Corrupt, 0.0, 1.0);
    acquire(&mut state, '漢', corrupt.clone());
    let decision = HumanBatchDecision {
        identity: identity('漢'),
        candidate_sha256: corrupt.sha256,
        action: HumanBatchAction::Confirm,
        reason: "подтверждён".into(),
    };
    assert!(state.decide(decision, &bytes("corrupt")).is_err());
    assert_eq!(state.items[0].status, BatchItemStatus::Unresolved);
}

#[test]
fn revalidation_recomputes_technical_validity_instead_of_inheriting_old_false() {
    let old_validator = ValidatorIdentity::new("legacy-validator", "v0").unwrap();
    let mut state = KanjiBatch::new(
        "revalidate-valid-now".into(),
        vec![identity('漢')],
        old_validator.clone(),
    )
    .unwrap();
    let stale = candidate_with_validator(
        "old-corrupt-flag",
        SemanticStatus::Corrupt,
        old_validator,
        false,
    );
    let sha256 = stale.sha256.clone();
    acquire(&mut state, '漢', stale);
    assert!(!candidate_from_attempt(&state.items[0].attempts[0]).technically_valid);

    state
        .revalidate_candidates(validator(), |_, candidate| {
            Ok(revalidation(
                candidate,
                SemanticStatus::Uncertain,
                validator(),
                true,
            ))
        })
        .unwrap();

    let item = &state.items[0];
    let candidate = candidate_from_attempt(&item.attempts[0]);
    assert_eq!(candidate.sha256, sha256);
    assert!(candidate.technically_valid);
    assert_eq!(candidate.automated.status, SemanticStatus::Uncertain);
    assert_eq!(item.status, BatchItemStatus::AwaitingHuman);
    assert_eq!(item.attempts.len(), 1);
    state.validate().unwrap();
}

#[test]
fn undecodable_candidate_stays_blocked_even_if_new_semantic_result_is_uncertain() {
    let old_validator = ValidatorIdentity::new("legacy-validator", "v0").unwrap();
    let mut state = KanjiBatch::new(
        "revalidate-undecodable".into(),
        vec![identity('漢')],
        old_validator.clone(),
    )
    .unwrap();
    let stale = candidate_with_validator(
        "old-undecodable",
        SemanticStatus::Uncertain,
        old_validator,
        true,
    );
    let candidate_bytes = bytes("old-undecodable");
    let sha256 = stale.sha256.clone();
    acquire(&mut state, '漢', stale);

    state
        .revalidate_candidates(validator(), |_, candidate| {
            Ok(revalidation(
                candidate,
                SemanticStatus::Uncertain,
                validator(),
                false,
            ))
        })
        .unwrap();

    let item = &state.items[0];
    let candidate = candidate_from_attempt(&item.attempts[0]);
    assert_eq!(candidate.sha256, sha256);
    assert!(!candidate.technically_valid);
    assert_eq!(candidate.automated.status, SemanticStatus::Uncertain);
    assert_ne!(item.status, BatchItemStatus::AutoVerified);
    assert!(
        state
            .decide(
                HumanBatchDecision {
                    identity: identity('漢'),
                    candidate_sha256: sha256,
                    action: HumanBatchAction::Confirm,
                    reason: "недекодируемые байты нельзя подтвердить".into(),
                },
                &candidate_bytes,
            )
            .is_err()
    );
    state.validate().unwrap();
}

#[test]
fn revalidation_demotes_corrupt_human_confirmation_but_keeps_its_history() {
    let old_validator = ValidatorIdentity::new("legacy-validator", "v0").unwrap();
    let mut state = KanjiBatch::new(
        "revalidate-human-corrupt".into(),
        vec![identity('漢')],
        old_validator.clone(),
    )
    .unwrap();
    let stale = candidate_with_validator(
        "human-corrupt",
        SemanticStatus::Uncertain,
        old_validator,
        true,
    );
    let candidate_bytes = bytes("human-corrupt");
    let sha256 = stale.sha256.clone();
    acquire(&mut state, '漢', stale);
    let decision = HumanBatchDecision {
        identity: identity('漢'),
        candidate_sha256: sha256.clone(),
        action: HumanBatchAction::Confirm,
        reason: "подтверждены прежние точные байты".into(),
    };
    state.decide(decision.clone(), &candidate_bytes).unwrap();
    state
        .mark_published_ready(&identity('漢'), &sha256, BatchTrustSource::Human)
        .unwrap();
    let attempts_before = state.items[0].attempts.len();

    state
        .revalidate_candidates(validator(), |_, candidate| {
            Ok(revalidation(
                candidate,
                SemanticStatus::Corrupt,
                validator(),
                false,
            ))
        })
        .unwrap();

    let item = &state.items[0];
    assert_eq!(item.status, BatchItemStatus::Unresolved);
    assert_eq!(item.current_sha256.as_deref(), Some(sha256.as_str()));
    assert_eq!(item.human_decisions, vec![decision]);
    assert_eq!(item.attempts.len(), attempts_before);
    assert!(item.published_sha256.is_none());
    assert!(item.publication_source.is_none());
    assert!(!candidate_from_attempt(&item.attempts[0]).technically_valid);
    state.validate().unwrap();
}

#[test]
fn revalidation_preserves_human_confirmation_for_same_technically_valid_sha() {
    let old_validator = ValidatorIdentity::new("legacy-validator", "v0").unwrap();
    let mut state = KanjiBatch::new(
        "revalidate-human-same-sha".into(),
        vec![identity('漢')],
        old_validator.clone(),
    )
    .unwrap();
    let candidate = candidate_with_validator(
        "human-same-sha",
        SemanticStatus::Uncertain,
        old_validator,
        true,
    );
    let candidate_bytes = bytes("human-same-sha");
    let sha256 = candidate.sha256.clone();
    acquire(&mut state, '漢', candidate);
    let decision = HumanBatchDecision {
        identity: identity('漢'),
        candidate_sha256: sha256.clone(),
        action: HumanBatchAction::Confirm,
        reason: "подтверждены неизменные байты".into(),
    };
    state.decide(decision.clone(), &candidate_bytes).unwrap();
    state
        .mark_published_ready(&identity('漢'), &sha256, BatchTrustSource::Human)
        .unwrap();
    let attempts_before = state.items[0].attempts.len();

    state
        .revalidate_candidates(validator(), |_, candidate| {
            Ok(revalidation(
                candidate,
                SemanticStatus::Uncertain,
                validator(),
                true,
            ))
        })
        .unwrap();

    let item = &state.items[0];
    assert_eq!(item.status, BatchItemStatus::HumanVerified);
    assert_eq!(item.current_sha256.as_deref(), Some(sha256.as_str()));
    assert_eq!(item.human_decisions, vec![decision]);
    assert_eq!(item.attempts.len(), attempts_before);
    assert_eq!(item.published_sha256.as_deref(), Some(sha256.as_str()));
    assert_eq!(item.publication_source, Some(BatchTrustSource::Human));
    state.validate().unwrap();
}

#[test]
fn revalidation_preserves_confirmation_for_existing_candidate_without_attempts() {
    let old_validator = ValidatorIdentity::new("legacy-validator", "v0").unwrap();
    let current_validator = validator();
    let mut state = KanjiBatch::new(
        "revalidate-existing-human-same-sha".into(),
        vec![identity('漢')],
        old_validator.clone(),
    )
    .unwrap();
    let candidate = candidate_with_validator(
        "existing-human-same-sha",
        SemanticStatus::Uncertain,
        old_validator,
        true,
    );
    let candidate_bytes = bytes("existing-human-same-sha");
    let sha256 = candidate.sha256.clone();
    state.mark_existing_ready(&identity('漢'), &sha256).unwrap();
    state
        .retain_existing_candidate(&identity('漢'), candidate)
        .unwrap();
    let decision = HumanBatchDecision {
        identity: identity('漢'),
        candidate_sha256: sha256.clone(),
        action: HumanBatchAction::Confirm,
        reason: "подтверждены точные байты канонического кандидата".into(),
    };
    state.decide(decision.clone(), &candidate_bytes).unwrap();
    state
        .mark_published_ready(&identity('漢'), &sha256, BatchTrustSource::Human)
        .unwrap();

    state
        .revalidate_candidates(current_validator.clone(), |_, candidate| {
            Ok(revalidation(
                candidate,
                SemanticStatus::Uncertain,
                current_validator.clone(),
                true,
            ))
        })
        .unwrap();

    let item = &state.items[0];
    assert_eq!(item.status, BatchItemStatus::HumanVerified);
    assert_eq!(item.current_sha256.as_deref(), Some(sha256.as_str()));
    assert_eq!(item.human_decisions, vec![decision]);
    assert!(item.attempts.is_empty());
    assert_eq!(
        item.existing_candidate
            .as_ref()
            .unwrap()
            .automated
            .validator,
        current_validator
    );
    assert_eq!(item.published_sha256.as_deref(), Some(sha256.as_str()));
    assert_eq!(item.publication_source, Some(BatchTrustSource::Human));
    state.validate().unwrap();
}

#[test]
fn revalidation_keeps_the_original_state_when_a_candidate_cannot_be_checked() {
    let old_validator = ValidatorIdentity::new("legacy-validator", "v0").unwrap();
    let mut state = KanjiBatch::new(
        "revalidate-atomic".into(),
        vec![identity('漢')],
        old_validator.clone(),
    )
    .unwrap();
    acquire(
        &mut state,
        '漢',
        candidate_with_validator(
            "atomic-candidate",
            SemanticStatus::Uncertain,
            old_validator,
            true,
        ),
    );
    let before = state.clone();

    let result = state.revalidate_candidates(validator(), |_, _| {
        Err(AssetError::new(
            ErrorCode::ValidatorFailure,
            "тестовая ошибка повторной проверки",
        ))
    });

    assert_eq!(result.unwrap_err().code, ErrorCode::ValidatorFailure);
    assert_eq!(state, before);
    state.validate().unwrap();
}

#[test]
fn rejection_of_auto_verified_bytes_overrides_and_only_reacquires_target() {
    let mut state = batch(&['漢', '字']);
    let good = candidate("good", SemanticStatus::Verified, 0.01, 0.01);
    let rejected_hash = good.sha256.clone();
    acquire(&mut state, '漢', good.clone());
    state
        .mark_published_ready(&identity('漢'), &good.sha256, BatchTrustSource::Automated)
        .unwrap();
    state
        .mark_existing_ready(&identity('字'), &"a".repeat(64))
        .unwrap();
    let decision = HumanBatchDecision {
        identity: identity('漢'),
        candidate_sha256: good.sha256.clone(),
        action: HumanBatchAction::Reject,
        reason: "артефакт битый".into(),
    };
    state.decide(decision, &bytes("good")).unwrap();
    assert_eq!(state.next_round(), vec![identity('漢')]);
    assert!(state.items[1].is_ready());
    acquire(&mut state, '漢', good);
    assert_eq!(state.items[0].status, BatchItemStatus::Reacquire);
    assert_eq!(state.items[0].current_sha256, None);
    assert!(state.items[0].aggregate.distinct_valid_hashes.is_empty());
    assert!(state.items[0].attempts[1].duplicate_sha256);
    acquire(
        &mut state,
        '漢',
        candidate("new-good", SemanticStatus::Verified, 0.01, 0.01),
    );
    assert_ne!(
        state.items[0].current_sha256.as_ref().unwrap(),
        &rejected_hash
    );
    assert_eq!(state.items[0].attempts.len(), 3);
    state.validate().unwrap();
}

#[test]
fn failures_are_item_local_and_candidate_less_queue_can_restart() {
    let mut state = batch(&['漢', '字']);
    for round in 0..5 {
        failed(&mut state, '漢');
        if round == 0 {
            acquire(
                &mut state,
                '字',
                candidate("verified-neighbor", SemanticStatus::Verified, 0.01, 0.01),
            );
        }
    }
    assert_eq!(state.items[0].status, BatchItemStatus::AwaitingHuman);
    assert_eq!(state.items[1].status, BatchItemStatus::AutoVerified);
    state
        .retry_acquisition(
            &identity('漢'),
            "повторить после восстановления сети".into(),
        )
        .unwrap();
    assert_eq!(state.next_round(), vec![identity('漢')]);
    assert_eq!(state.items[0].attempts.len(), 5);
    state.validate().unwrap();
}

#[test]
fn state_and_exact_bytes_resume_after_process_restart() {
    let temporary = Temporary::new();
    let mut state = batch(&['漢']);
    {
        let mut runtime = BatchRuntime::open(temporary.root(), &state.batch_id).unwrap();
        assert!(runtime.load().unwrap().is_none());
        let bytes = bytes("resume");
        let candidate = runtime
            .persist_candidate(
                &bytes,
                record(&bytes, SemanticStatus::Verified, 0.01, 0.01),
                true,
            )
            .unwrap();
        acquire(&mut state, '漢', candidate);
        runtime.save(&state).unwrap();
    }
    let mut runtime = BatchRuntime::open(temporary.root(), &state.batch_id).unwrap();
    let mut recovered = runtime.load().unwrap().unwrap();
    assert_eq!(recovered, state);
    assert!(!recovered.is_resolved());
    assert!(recovered.next_round().is_empty());
    let resolution = recovered
        .aggregate_decision(&identity('漢'))
        .unwrap()
        .unwrap();
    assert_eq!(
        runtime.read_candidate(&resolution.candidate).unwrap(),
        bytes("resume")
    );
    recovered
        .mark_published_ready(
            &identity('漢'),
            &resolution.candidate.sha256,
            BatchTrustSource::Automated,
        )
        .unwrap();
    runtime.save(&recovered).unwrap();
    assert!(recovered.is_resolved());
}

#[test]
fn load_rejects_candidate_format_mismatch_before_materialization() {
    let temporary = Temporary::new();
    let state = batch(&['漢']);
    let mut runtime = BatchRuntime::open(temporary.root(), &state.batch_id).unwrap();
    let data = bytes("format-mismatch");
    let candidate = runtime
        .persist_candidate(
            &data,
            record(&data, SemanticStatus::Uncertain, 0.03, 0.001),
            true,
        )
        .unwrap();
    let original_path = temporary
        .root()
        .join(".runtime/batches")
        .join(&state.batch_id)
        .join(&candidate.storage_path);
    let mut inconsistent = state;
    let mut wrong_format = candidate;
    wrong_format.format = DetectedFormat::Png;
    wrong_format.storage_path = candidate_path(&wrong_format.sha256, wrong_format.format).unwrap();
    let mismatched_path = temporary
        .root()
        .join(".runtime/batches")
        .join(&inconsistent.batch_id)
        .join(&wrong_format.storage_path);
    fs::copy(original_path, &mismatched_path).unwrap();
    acquire(&mut inconsistent, '漢', wrong_format);
    inconsistent.validate().unwrap();
    fs::write(
        temporary
            .root()
            .join(".runtime/batches")
            .join(&inconsistent.batch_id)
            .join("state.json"),
        serde_json::to_vec_pretty(&inconsistent).unwrap(),
    )
    .unwrap();

    assert_eq!(
        runtime.load().unwrap_err().code,
        ErrorCode::IntegrityMismatch
    );
}

#[test]
fn runtime_rejects_path_traversal_symlinks_and_changed_bytes() {
    let temporary = Temporary::new();
    assert!(BatchRuntime::open(temporary.root(), "../escape").is_err());
    let runtime = BatchRuntime::open(temporary.root(), "safe").unwrap();
    let data = bytes("safe");
    let mut candidate = runtime
        .persist_candidate(
            &data,
            record(&data, SemanticStatus::Uncertain, 0.03, 0.001),
            true,
        )
        .unwrap();
    let original_path = candidate.storage_path.clone();
    candidate.storage_path = "../outside.gif".into();
    assert_eq!(
        runtime.read_candidate(&candidate).unwrap_err().code,
        ErrorCode::PathTraversal
    );
    candidate.storage_path = original_path;
    let actual_path = temporary
        .root()
        .join(".runtime/batches/safe")
        .join(&candidate.storage_path);
    fs::write(&actual_path, bytes("changed")).unwrap();
    assert_eq!(
        runtime.read_candidate(&candidate).unwrap_err().code,
        ErrorCode::IntegrityMismatch
    );
    fs::remove_file(&actual_path).unwrap();
    let outside = temporary.root().join("outside.gif");
    fs::write(&outside, &data).unwrap();
    symlink(&outside, &actual_path).unwrap();
    assert_eq!(
        runtime.read_candidate(&candidate).unwrap_err().code,
        ErrorCode::BoundaryViolation
    );
}

#[test]
fn runtime_directory_and_state_symlinks_fail_closed() {
    let temporary = Temporary::new();
    let outside = temporary.root().join("outside");
    fs::create_dir(&outside).unwrap();
    symlink(&outside, temporary.root().join(".runtime")).unwrap();
    assert_eq!(
        BatchRuntime::open(temporary.root(), "safe")
            .unwrap_err()
            .code,
        ErrorCode::BoundaryViolation
    );
    fs::remove_file(temporary.root().join(".runtime")).unwrap();
    let mut runtime = BatchRuntime::open(temporary.root(), "safe").unwrap();
    let outside_state = outside.join("state.json");
    fs::write(&outside_state, "{}").unwrap();
    symlink(
        &outside_state,
        temporary.root().join(".runtime/batches/safe/state.json"),
    )
    .unwrap();
    assert_eq!(
        runtime.load().unwrap_err().code,
        ErrorCode::BoundaryViolation
    );
}

#[test]
fn interrupted_temporary_write_does_not_replace_durable_state() {
    let temporary = Temporary::new();
    let state = batch(&['漢']);
    {
        let mut runtime = BatchRuntime::open(temporary.root(), &state.batch_id).unwrap();
        runtime.save(&state).unwrap();
        fs::write(
            temporary
                .root()
                .join(".runtime/batches/synthetic-batch/.tmp-interrupted"),
            "{truncated",
        )
        .unwrap();
    }
    let mut runtime = BatchRuntime::open(temporary.root(), &state.batch_id).unwrap();
    assert_eq!(runtime.load().unwrap().unwrap(), state);
    let mut altered = state.clone();
    altered.schema_version = 999;
    assert!(runtime.save(&altered).is_err());
    assert_eq!(runtime.load().unwrap().unwrap(), state);
}

#[test]
fn review_references_exact_gif_and_escapes_machine_evidence() {
    let temporary = Temporary::new();
    let mut state = batch(&['漢']);
    let mut runtime = BatchRuntime::open(temporary.root(), &state.batch_id).unwrap();
    let data = bytes("review");
    let mut automated = record(&data, SemanticStatus::Uncertain, 0.03, 0.001);
    automated.evidence[0].summary = "<script>alert('unsafe')</script>".into();
    let candidate = runtime.persist_candidate(&data, automated, true).unwrap();
    let other_data = bytes("review-other");
    let other_candidate = runtime
        .persist_candidate(
            &other_data,
            record(&other_data, SemanticStatus::Uncertain, 0.03, 0.001),
            true,
        )
        .unwrap();
    for _ in 0..4 {
        acquire(&mut state, '漢', candidate.clone());
    }
    acquire(&mut state, '漢', other_candidate);
    runtime.save(&state).unwrap();
    let path = runtime.write_review(&state).unwrap();
    let html = fs::read_to_string(path).unwrap();
    assert!(html.contains(&format!("src=\"{}\"", candidate.storage_path)));
    assert!(html.contains(&candidate.sha256));
    assert!(html.contains("<html lang=\"ru\">"));
    assert!(html.contains("Пакет:"));
    assert!(html.contains("Состояние: ожидает решения человека"));
    assert!(html.contains("Точный SHA-256:"));
    assert!(html.contains("Причина проверки:"));
    assert!(html.contains("Среднее, минимум, максимум и количество по агрегату"));
    assert!(html.contains("Все попытки получения"));
    assert!(html.contains("Попытка 1 · раунд 1"));
    assert!(html.contains("Другой кандидат SHA-256:"));
    assert!(!html.contains("<script>"));
    assert!(html.contains("&lt;script&gt;"));
    assert!(html.contains("разных допустимых SHA-256: 2"));
    for label in [
        "Batch:",
        "policy:",
        "revision:",
        "State:",
        "acquisition attempts:",
        "independent valid SHA:",
        "Aggregate mean",
        "Attempt 1",
        "round 1",
        "duplicate SHA:",
    ] {
        assert!(
            !html.contains(label),
            "в HTML осталась английская метка {label}"
        );
    }
    assert_eq!(runtime.load().unwrap().unwrap(), state);
}

#[test]
fn tampered_aggregate_and_acquisition_counts_are_rejected() {
    let mut state = batch(&['漢']);
    acquire(
        &mut state,
        '漢',
        candidate("uncertain", SemanticStatus::Uncertain, 0.03, 0.001),
    );
    let mut tampered = state.clone();
    tampered.items[0].aggregate.accepted = true;
    assert!(tampered.validate().is_err());
    let mut tampered = state.clone();
    tampered.items[0].attempts[0].round = 5;
    assert!(tampered.validate().is_err());
    state.items[0].attempts[0].duplicate_sha256 = true;
    assert!(state.validate().is_err());
}
