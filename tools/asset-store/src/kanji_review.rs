//! Производный текстовый пакет проверки. Источник истины — KanjiBatch;
//! редактируемый TSV никогда не исполняется и не сканируется по маске файлов.
//! Снимок, копии байтов и файл решений принадлежат общему `review_snapshot`;
//! здесь — строки Kanji, их действия и предпросмотр групп OCR.

use serde::{Deserialize, Serialize};

use crate::batch::{
    BatchCandidate, BatchRuntime, HumanBatchAction, HumanBatchDecision, KanjiBatch,
};
use crate::error::{AssetError, ErrorCode};
use crate::kanji_domain::parse_kanji_character;
use crate::model::AssetIdentity;
pub(crate) use crate::review_snapshot::ReviewBundle;
#[cfg(test)]
use crate::review_snapshot::preview_image_source;
use crate::review_snapshot::{ReviewCell, ReviewFormat};

const SCHEMA: u32 = 1;

const FORMAT: ReviewFormat = ReviewFormat {
    schema: SCHEMA,
    comments: &[
        "# Проверка изображений кандзи Yarxi, формат v1",
        "# y = ожидаемый символ; e = непригодное изображение; другой один Han = фактический символ; пусто = без решения; - = пропуск",
        "# Только первая колонка; разделитель TAB. Сохраните файл перед применением.",
    ],
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReviewRow {
    pub identity: AssetIdentity,
    pub generation: u32,
    pub attempts: usize,
    pub sha256: String,
    pub candidate_path: String,
    pub filename: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ocr: Option<crate::kanji_ocr::OcrSuggestion>,
}

impl ReviewRow {
    fn columns(&self) -> String {
        let mut columns = format!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            self.identity.key,
            self.filename,
            self.sha256,
            self.generation,
            self.attempts,
            self.candidate_path
        );
        if let Some(ocr) = &self.ocr {
            columns.push_str(&format!(
                "\t{}\t{}\t{}\t{}",
                ocr.observed.as_deref().unwrap_or("-"),
                ocr.matches_expected,
                ocr.failure.as_deref().unwrap_or("-"),
                ocr.key()
            ));
        }
        columns
    }
}

impl crate::review_snapshot::ReviewRow for ReviewRow {
    fn identity(&self) -> &AssetIdentity {
        &self.identity
    }

    fn image(&self) -> Option<(&str, &str)> {
        Some((&self.filename, &self.sha256))
    }

    fn columns(&self) -> String {
        ReviewRow::columns(self)
    }

    fn validate(&self) -> Result<(), AssetError> {
        if self
            .ocr
            .as_ref()
            .is_some_and(|ocr| ocr.identity != self.identity || ocr.source_sha256 != self.sha256)
        {
            return Err(invalid(
                "предложение OCR не принадлежит точным байтам review",
            ));
        }
        Ok(())
    }
}

pub(crate) type ReviewSnapshot = crate::review_snapshot::ReviewSnapshot<ReviewRow>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ParsedDecision {
    Unspecified,
    Skip,
    Apply(HumanBatchDecision),
}

fn invalid(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::InvalidTransition, message)
}

pub(crate) fn candidate<'a>(
    state: &'a KanjiBatch,
    row: &ReviewRow,
) -> Result<&'a BatchCandidate, AssetError> {
    let item = state
        .items
        .iter()
        .find(|item| item.identity == row.identity)
        .ok_or_else(|| invalid("строка review не принадлежит пакету"))?;
    item.attempts
        .iter()
        .rev()
        .find_map(|attempt| match &attempt.result {
            crate::batch::BatchAttemptInput::Candidate { candidate }
                if candidate.sha256 == row.sha256 =>
            {
                Some(candidate)
            }
            _ => None,
        })
        .or_else(|| {
            item.existing_candidate
                .as_ref()
                .filter(|candidate| candidate.sha256 == row.sha256)
        })
        .filter(|candidate| candidate.storage_path == row.candidate_path)
        .ok_or_else(|| invalid("кандидат review отсутствует или заменён"))
}

pub(crate) fn prepare(
    runtime: &BatchRuntime,
    state: &KanjiBatch,
) -> Result<ReviewBundle, AssetError> {
    state.validate()?;
    let mut rows = Vec::new();
    let suggestions = crate::kanji_ocr::saved_suggestions_or_empty(runtime);
    for item in state.review_queue() {
        let Some(hash) = &item.current_sha256 else {
            continue;
        };
        let mut row = ReviewRow {
            identity: item.identity.clone(),
            generation: item.generation,
            attempts: item.attempts.len(),
            sha256: hash.clone(),
            candidate_path: String::new(),
            filename: String::new(),
            ocr: suggestions
                .get(&item.identity)
                .filter(|ocr| &ocr.source_sha256 == hash)
                .cloned(),
        };
        let selected = item
            .attempts
            .iter()
            .rev()
            .find_map(|attempt| match &attempt.result {
                crate::batch::BatchAttemptInput::Candidate { candidate }
                    if &candidate.sha256 == hash =>
                {
                    Some(candidate)
                }
                _ => None,
            })
            .ok_or_else(|| invalid("нет текущего кандидата для review"))?;
        row.candidate_path = selected.storage_path.clone();
        row.filename = format!(
            "{}-{}.{}",
            item.identity.key,
            hash,
            crate::domain::extension_for_format(selected.format)
        );
        rows.push(row);
    }
    if rows.is_empty() {
        return Err(invalid("нет кандидатов, ожидающих решения человека"));
    }
    rows.sort_by_key(|row| row.ocr.as_ref().map_or(3, |ocr| ocr.group()));
    let snapshot = ReviewSnapshot {
        schema_version: SCHEMA,
        batch_id: state.batch_id.clone(),
        validator: state.policy.validator.clone(),
        rows,
    };
    let (bundle, directory) = crate::review_snapshot::export(
        |name| runtime.artifact_directory(name),
        &snapshot,
        &FORMAT,
        |row| runtime.read_candidate(candidate(state, row)?),
    )?;
    write_preview(&directory, &snapshot)?;
    Ok(bundle)
}

pub(crate) fn read(
    runtime: &BatchRuntime,
    id: &str,
) -> Result<(ReviewSnapshot, Vec<ParsedDecision>), AssetError> {
    let (snapshot, cells) = crate::review_snapshot::read(
        |name| runtime.artifact_directory(name),
        runtime.batch_id(),
        id,
        &FORMAT,
    )?;
    let decisions = decide(&snapshot, id, cells)?;
    Ok((snapshot, decisions))
}

#[cfg(test)]
fn parse(
    snapshot: &ReviewSnapshot,
    id: &str,
    text: &str,
) -> Result<Vec<ParsedDecision>, AssetError> {
    let cells = crate::review_snapshot::parse_cells(snapshot, &FORMAT, id, text)?;
    decide(snapshot, id, cells)
}

fn decide(
    snapshot: &ReviewSnapshot,
    id: &str,
    cells: Vec<ReviewCell>,
) -> Result<Vec<ParsedDecision>, AssetError> {
    let mut decisions = Vec::with_capacity(cells.len());
    for (row, cell) in snapshot.rows.iter().zip(cells) {
        let action = match cell {
            ReviewCell::Unspecified => {
                decisions.push(ParsedDecision::Unspecified);
                continue;
            }
            ReviewCell::Skip => {
                decisions.push(ParsedDecision::Skip);
                continue;
            }
            ReviewCell::Action(action) => action,
        };
        let (action, reason) = match action.as_str() {
            "y" => (
                HumanBatchAction::Confirm,
                "expected_character_confirmed".to_owned(),
            ),
            "e" => (
                HumanBatchAction::RejectAndHold,
                "unusable_image; explicit retry required".to_owned(),
            ),
            other => {
                let character = parse_kanji_character(other)
                    .map_err(|_| invalid("решение: y, e, -, либо один другой Han"))?;
                if other == row.identity.key {
                    return Err(invalid(
                        "для подтверждения ожидаемого Unicode используйте y",
                    ));
                }
                (
                    HumanBatchAction::RejectAndHold,
                    format!("actual_character={character}; explicit retry required"),
                )
            }
        };
        decisions.push(ParsedDecision::Apply(HumanBatchDecision {
            identity: row.identity.clone(),
            candidate_sha256: row.sha256.clone(),
            action,
            reason: format!("review:{id}; {reason}"),
        }));
    }
    Ok(decisions)
}

fn write_preview(
    directory: &crate::batch_runtime::ArtifactDirectory,
    snapshot: &ReviewSnapshot,
) -> Result<(), AssetError> {
    use crate::batch::html_escape;
    let mut html = String::from(
        "<!doctype html><html lang=\"ru\"><meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; img-src 'self' file: data:; style-src 'unsafe-inline'\"><title>Ручная проверка</title><style>body{font-family:system-ui;margin:2rem}article{border:1px solid #999;padding:1rem;margin:1rem 0}img{max-width:300px}pre{white-space:pre-wrap;overflow-wrap:anywhere}</style><h1>Предложения OCR и решения человека</h1><p>Просмотрите изображения. OCR может ошибаться при совпадении метки. Только явное решение человека подтверждает байты.</p>",
    );
    let mut group = None;
    let mut inline_budget = crate::review_snapshot::PREVIEW_INLINE_BUDGET;
    for row in &snapshot.rows {
        let next_group = row.ocr.as_ref().map_or(3, |ocr| ocr.group());
        if group != Some(next_group) {
            html.push_str(match next_group {
                0 => "<h2>Совпадения OCR — требуют просмотра</h2>",
                1 => "<h2>Несовпадения Unicode</h2>",
                2 => "<h2>Не распознано / отказ</h2>",
                _ => "<h2>Без OCR</h2>",
            });
            group = Some(next_group);
        }
        let image = directory.read(&row.filename)?;
        let source =
            crate::review_snapshot::preview_image_source(&row.filename, &image, &mut inline_budget);
        html.push_str(&format!("<article><h3>{}</h3><img src=\"{}\" alt=\"{}\"><pre>SHA: {}\nПоколение: {}\n{}</pre></article>",
            html_escape(&row.identity.key), source, html_escape(&row.identity.key), row.sha256, row.generation,
            html_escape(&row.ocr.as_ref().map_or_else(|| "Без OCR".into(), |ocr| ocr.summary()))));
    }
    html.push_str("</html>");
    directory.write("index.html", html.as_bytes())
}

#[cfg(test)]
#[path = "kanji_review_tests.rs"]
mod tests;
