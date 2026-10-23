use super::*;

fn snapshot() -> ReviewSnapshot {
    ReviewSnapshot {
        schema_version: SCHEMA,
        batch_id: "review-test".into(),
        validator: crate::kanji_validator::KanjiImageValidator::validator_identity(),
        rows: ['保', '倉', '点', '門']
            .into_iter()
            .map(|character| ReviewRow {
                identity: AssetIdentity::new("kanji", character.to_string()).unwrap(),
                generation: 3,
                attempts: 7,
                sha256: "a".repeat(64),
                candidate_path: format!("candidates/{}.gif", "a".repeat(64)),
                filename: format!("{character}-{}.gif", "a".repeat(64)),
                ocr: None,
            })
            .collect(),
    }
}
fn text(snapshot: &ReviewSnapshot, actions: &[&str]) -> String {
    let mut text = format!(
        "# комментарий\n@review\t1\t{}\t{}\n",
        snapshot.batch_id,
        "b".repeat(64)
    );
    for (row, action) in snapshot.rows.iter().zip(actions) {
        text.push_str(&format!("{action}\t{}\n", row.columns()));
    }
    text
}
#[test]
fn only_explicit_y_confirms_and_windows_text_is_accepted() {
    let snapshot = snapshot();
    let text = format!(
        "\u{feff}{}",
        text(&snapshot, &["y", "e", "侏", "-"]).replace('\n', "\r\n")
    );
    let parsed = parse(&snapshot, &"b".repeat(64), &text).unwrap();
    assert_eq!(
        parsed[0],
        ParsedDecision::Apply(HumanBatchDecision {
            identity: snapshot.rows[0].identity.clone(),
            candidate_sha256: snapshot.rows[0].sha256.clone(),
            action: HumanBatchAction::Confirm,
            reason: format!("review:{}; expected_character_confirmed", "b".repeat(64)),
        })
    );
    assert_eq!(
        parsed[1],
        ParsedDecision::Apply(HumanBatchDecision {
            identity: snapshot.rows[1].identity.clone(),
            candidate_sha256: snapshot.rows[1].sha256.clone(),
            action: HumanBatchAction::RejectAndHold,
            reason: format!(
                "review:{}; unusable_image; explicit retry required",
                "b".repeat(64)
            ),
        })
    );
    assert!(matches!(
        &parsed[2],
        ParsedDecision::Apply(decision) if decision.reason.contains("actual_character=侏")
    ));
    assert_eq!(parsed[3], ParsedDecision::Skip);
    let parsed = parse(&snapshot, &"b".repeat(64), &text.replace("y\t", "\t")).unwrap();
    assert_eq!(parsed[0], ParsedDecision::Unspecified);
}

#[test]
fn blank_decision_and_explicit_skip_are_distinct() {
    let snapshot = snapshot();
    let parsed = parse(
        &snapshot,
        &"b".repeat(64),
        &text(&snapshot, &["", "-", "", "-"]),
    )
    .unwrap();
    assert_eq!(parsed[0], ParsedDecision::Unspecified);
    assert_eq!(parsed[1], ParsedDecision::Skip);
    assert_eq!(parsed[2], ParsedDecision::Unspecified);
    assert_eq!(parsed[3], ParsedDecision::Skip);
}
#[test]
fn edited_metadata_duplicates_paths_and_unknown_decisions_reject_entire_file() {
    let snapshot = snapshot();
    let original = text(&snapshot, &["y", "-", "-", "-"]);
    let second = original
        .lines()
        .find(|line| line.starts_with("-\t倉"))
        .unwrap();
    let corruptions = [
        original.replacen("@review\t1", "@review\t2", 1),
        original.replacen("review-test", "other-batch", 1),
        original.replace("\t3\t7\t", "\t4\t7\t"),
        original.replace("\t3\t7\t", "\t3\t8\t"),
        original.replacen(&"a".repeat(64), &"c".repeat(64), 1),
        original.replace(
            second,
            original
                .lines()
                .find(|line| line.starts_with("y\t保"))
                .unwrap(),
        ),
        original.replacen("candidates/", "../", 1),
        original.replace("y\t", "yes\t"),
        original.replace("y\t", "保\t"),
        original.replace("y\t", "🙂\t"),
        original.replace("y\t", "侏保\t"),
        format!("{original}{second}\n"),
        original.lines().take(4).collect::<Vec<_>>().join("\n"),
    ];
    for text in corruptions {
        assert!(parse(&snapshot, &"b".repeat(64), &text).is_err(), "{text}");
    }
}

#[test]
fn preview_embeds_small_images_and_preserves_fallback_when_budget_is_exhausted() {
    let gif = preview_image_source("漢-abc.gif", b"GIF89a", &mut 8);
    assert_eq!(gif, "data:image/gif;base64,R0lGODlh");
    let mut budget = 8;
    let png = preview_image_source("字-abc.png", b"\x89PNG\r\n\x1a\n", &mut budget);
    assert!(png.starts_with("data:image/png;base64,"));
    assert_eq!(budget, 0);
    let fallback = preview_image_source("字-abc.png", b"123", &mut budget);
    assert_eq!(fallback, "字-abc.png");
}
