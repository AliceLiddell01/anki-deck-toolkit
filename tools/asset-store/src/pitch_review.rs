//! Детерминированный офлайн HTML-отчёт для предметного batch pitch-accent.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::Cursor;

use image::ImageDecoder;

use crate::domain::AssetDomainPolicy;
use crate::error::{AssetError, ErrorCode};
use crate::hashing::sha256_hex;
use crate::model::{AssetRecord, DetectedFormat, LifecycleState};
use crate::pitch_accent::{
    PITCH_ACCENT_MAX_ASSET_BYTES, PITCH_ACCENT_MAX_DECODE_ALLOCATION_BYTES,
    PITCH_ACCENT_MAX_IMAGE_DIMENSION, PitchAccentDomainMetadata, PitchAccentDomainPolicy,
};
use crate::pitch_batch::{
    PitchAccentBatch, PitchBatchCandidate, PitchBatchItem, PitchBatchItemStatus, PitchBatchOutcome,
};

const MAX_REVIEW_HTML_BYTES: usize = 64 * 1024 * 1024;

/// Собирает локальную страницу проверки без действий, меняющих batch или corpus.
///
/// Байты кандидата повторно сверяются с SHA, размером и декодером PNG.
/// Канонические ссылки выводятся только для записи хранилища владельца,
/// совпавшей с пакетом по идентичности, SHA, пути в хранилище и текущему
/// проверяющему модулю; вызывающая сторона передаёт только записи, чьи байты
/// уже проверены через `read_verified_with_policy`.
pub(crate) fn render(
    batch: &PitchAccentBatch,
    owner_records: &[AssetRecord],
    mut read_candidate: impl FnMut(&PitchBatchCandidate) -> Result<Vec<u8>, AssetError>,
) -> Result<String, AssetError> {
    let mut html = String::from(
        "<!doctype html><html lang=\"ru\"><head><meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; img-src 'self' file:; style-src 'unsafe-inline'\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Проверка pitch-accent</title><style>body{font-family:system-ui,sans-serif;max-width:1100px;margin:auto;padding:1rem;color:#202124;background:#fff}article{border:1px solid #9aa0a6;border-radius:.5rem;padding:1rem;margin:1rem 0}figure{margin:1rem 0;padding:1rem;background:#f5f6f7;border-radius:.35rem}img{display:block;max-width:min(100%,720px);max-height:480px;object-fit:contain;background:#202124}pre{white-space:pre-wrap;overflow-wrap:anywhere;background:#f5f6f7;padding:.75rem;border-radius:.35rem}code{overflow-wrap:anywhere}dt{font-weight:600;margin-top:.5rem}dd{margin-left:0;overflow-wrap:anywhere}.candidate{border-top:1px solid #dadce0;margin-top:1rem;padding-top:1rem}.state{font-weight:700}ul{padding-left:1.5rem}</style></head><body><h1>Проверка pitch-accent</h1>",
    );
    let validator = serde_json::to_value(&batch.validator).map_err(json_error)?;
    write!(
        &mut html,
        "<p>Пакет: <code>{}</code>; изменение: {}; проверяющий модуль: <code>{}</code></p>",
        escape_html(&batch.batch_id),
        batch.revision,
        escape_html(&json_text(&validator)?)
    )
    .expect("запись в String не завершается ошибкой");

    let mut candidate_cache = BTreeMap::<String, CandidateImage>::new();
    for item in &batch.items {
        render_item(
            &mut html,
            batch,
            item,
            owner_records,
            &mut candidate_cache,
            &mut read_candidate,
        )?;
        if html.len() > MAX_REVIEW_HTML_BYTES {
            return Err(invalid(
                "HTML-отчёт пакета pitch-accent превышает установленный предел",
            ));
        }
    }
    html.push_str("</body></html>");
    if html.len() > MAX_REVIEW_HTML_BYTES {
        return Err(invalid(
            "HTML-отчёт пакета pitch-accent превышает установленный предел",
        ));
    }
    Ok(html)
}

fn render_item(
    html: &mut String,
    batch: &PitchAccentBatch,
    item: &PitchBatchItem,
    owner_records: &[AssetRecord],
    candidate_cache: &mut BTreeMap<String, CandidateImage>,
    read_candidate: &mut impl FnMut(&PitchBatchCandidate) -> Result<Vec<u8>, AssetError>,
) -> Result<(), AssetError> {
    let item_status = item.status();
    let status = serde_json::to_value(item_status).map_err(json_error)?;
    let status_text = status
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| status.to_string());
    let query = serde_json::to_value(&item.request).map_err(json_error)?;
    write!(
        html,
        "<article><h2>{}</h2><p class=\"state\">Состояние: {} (<code>{}</code>)</p><dl><dt>Запрос</dt><dd><pre>{}</pre></dd><dt>Поколение</dt><dd>{}</dd><dt>Изменение элемента</dt><dd>{}</dd><dt>SHA-256 текущего кандидата</dt><dd><code>{}</code></dd><dt>SHA-256 записи из снимка владельца</dt><dd><code>{}</code></dd><dt>SHA-256 текущего проверенного ресурса владельца</dt><dd><code>{}</code></dd><dt>SHA-256 успешной публикации</dt><dd><code>{}</code></dd><dt>SHA-256 канонического состояния</dt><dd><code>{}</code></dd><dt>Ожидаемый SHA для публикации с CAS</dt><dd><code>{}</code></dd><dt>Публикация</dt><dd><pre>{}</pre></dd><dt>Конфликт владельца</dt><dd><pre>{}</pre></dd><dt>Причина последнего действия</dt><dd><pre>{}</pre></dd><dt>Отклонённые SHA-256</dt><dd><pre>{}</pre></dd></dl>",
        escape_html(&item.identity.key),
        item_status_label(item_status),
        escape_html(&status_text),
        escape_html(&json_text(&query)?),
        item.generation,
        item.item_revision,
        escape_html(item.current_candidate_sha256.as_deref().unwrap_or("нет")),
        escape_html(item.owner_current_sha256.as_deref().unwrap_or("нет")),
        escape_html(item.existing_verified_sha256.as_deref().unwrap_or("нет")),
        escape_html(item.published_sha256.as_deref().unwrap_or("нет")),
        escape_html(item.canonical_sha256.as_deref().unwrap_or("нет")),
        escape_html(item.refresh_expected_sha256.as_deref().unwrap_or("нет")),
        escape_html(&json_text(&serde_json::to_value(&item.publication).map_err(json_error)?)?),
        escape_html(&json_text(&serde_json::to_value(&item.owner_conflict).map_err(json_error)?)?),
        escape_html(item.last_action_reason.as_deref().unwrap_or("нет")),
        escape_html(&json_text(&serde_json::to_value(&item.rejected_candidates).map_err(json_error)?)?),
    )
    .expect("запись в String не завершается ошибкой");

    if let Some(record) = matching_owner_snapshot_record(item, owner_records) {
        render_owner_snapshot_record(html, record)?;
    }
    let canonical_record = matching_owner_record(batch, item, owner_records)?;
    if let Some(record) = canonical_record {
        render_canonical(html, item, record)?;
    }

    let mut shown = BTreeSet::new();
    let current_candidate = item.current_candidate();
    if item.current_candidate_sha256.is_some() && current_candidate.is_none() {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            "текущая попытка не указывает на сохранённый pitch-кандидат с ожидаемым SHA",
        ));
    }
    for attempt in &item.attempts {
        write!(
            html,
            "<section class=\"candidate\"><h3>Попытка {} · поколение {}</h3>",
            attempt.index, attempt.generation
        )
        .expect("запись в String не завершается ошибкой");
        write_json_block(html, "Запрос этой попытки", &attempt.request)?;
        match &attempt.outcome {
            PitchBatchOutcome::Acquired { candidate } => {
                // SHA может повторяться; метку текущего получает только точная попытка.
                let is_current = item.current_candidate_attempt_index == Some(attempt.index)
                    && current_candidate.is_some_and(|current| current.sha256 == candidate.sha256);
                render_candidate(
                    html,
                    item,
                    candidate,
                    is_current,
                    item.rejected_candidates
                        .iter()
                        .any(|rejection| rejection.candidate_sha256 == candidate.sha256),
                    candidate_cache,
                    read_candidate,
                )?;
                shown.insert(candidate.sha256.clone());
            }
            PitchBatchOutcome::NoPitchAccentOnSource { evidence } => {
                html.push_str("<p>Типизированный исход: <code>no_pitch_accent_on_source</code>. PNG отсутствует: источник подтвердил страницу словарной записи, но в ней нет секции pitch-accent.</p>");
                write_json_block(html, "Свидетельства отсутствия", evidence)?;
            }
            PitchBatchOutcome::AmbiguousVocabulary {
                surface,
                reading,
                candidates,
            } => {
                write!(
                    html,
                    "<p>Типизированный исход: <code>ambiguous_vocabulary</code>. Запрос: {} · чтение: {}. Автоматический выбор не выполнен.</p><h4>Кандидаты JPDB ({})</h4>",
                    escape_html(surface),
                    escape_html(reading.as_deref().unwrap_or("не задано")),
                    candidates.len()
                )
                .expect("запись в String не завершается ошибкой");
                for candidate in candidates {
                    render_ambiguous_candidate(html, candidate)?;
                }
            }
            PitchBatchOutcome::VocabularyNotFound { surface, reading } => {
                write!(
                    html,
                    "<p>Типизированный исход: <code>vocabulary_not_found</code>. JPDB не нашёл словарную запись для формы {} и чтения {}. Это не является доказательством отсутствия pitch-accent.</p>",
                    escape_html(surface),
                    escape_html(reading.as_deref().unwrap_or("не задано"))
                )
                .expect("запись в String не завершается ошибкой");
            }
            PitchBatchOutcome::Failed { error } => {
                html.push_str("<p>Типизированный технический исход: <code>failed</code>. PNG не создаётся.</p>");
                write_json_block(html, "Техническая диагностика", error)?;
            }
        }
        html.push_str("</section>");
    }

    if let Some(current_sha) = item.current_candidate_sha256.as_deref()
        && !shown.contains(current_sha)
    {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            "текущий pitch-кандидат отсутствует среди сохранённых результатов получения",
        ));
    }
    if item.attempts.is_empty() && canonical_record.is_some() {
        html.push_str("<p>Для текущего канонического SHA уже есть действующий проверенный ресурс; получение для этого элемента не выполнялось.</p>");
    } else if item.attempts.is_empty() {
        html.push_str("<p>Получение ещё не запускалось; PNG кандидата отсутствует.</p>");
    }
    html.push_str("</article>");
    Ok(())
}

fn render_candidate(
    html: &mut String,
    item: &PitchBatchItem,
    candidate: &PitchBatchCandidate,
    is_current: bool,
    is_rejected: bool,
    cache: &mut BTreeMap<String, CandidateImage>,
    read_candidate: &mut impl FnMut(&PitchBatchCandidate) -> Result<Vec<u8>, AssetError>,
) -> Result<(), AssetError> {
    validate_candidate_reference(item, candidate)?;
    let image = if let Some(image) = cache.get(&candidate.sha256) {
        if image.byte_length != candidate.byte_length {
            return Err(AssetError::new(
                ErrorCode::IntegrityMismatch,
                "повторный кандидат с тем же SHA указал другой размер файла",
            ));
        }
        image.clone()
    } else {
        let bytes = read_candidate(candidate)?;
        let image = verify_candidate_bytes(candidate, &bytes)?;
        cache.insert(candidate.sha256.clone(), image.clone());
        image
    };
    let unavailable_reason = image.validation_error.clone().or_else(|| {
        let expected = (
            candidate.metadata.evidence.render.pixel_width,
            candidate.metadata.evidence.render.pixel_height,
        );
        let actual = (image.width?, image.height?);
        (expected != actual).then(|| {
            format!(
                "фактические размеры PNG {}×{} не совпали со свидетельством отрисовки {}×{}",
                actual.0, actual.1, expected.0, expected.1
            )
        })
    });
    let evidence = &candidate.metadata.evidence;
    let semantic_status = candidate.validation.status;
    write!(
        html,
        "<h4>{}кандидат · SHA-256 <code>{}</code></h4><p>Форма: {}; чтение: {}; ID словарной записи JPDB: {}; ссылка на страницу: <code>{}</code></p><p>Фактический размер файла: {} байт; размер PNG: {} × {}; {}; статус проверки: <strong><code>{}</code></strong>; проверяющий модуль: <code>{}@{}</code>; публикация: {}</p>",
        if is_current { "Текущий " } else { "" },
        escape_html(&candidate.sha256),
        escape_html(&candidate.metadata.surface),
        escape_html(&candidate.metadata.reading),
        candidate.metadata.jpdb_vocabulary_id,
        escape_html(&evidence.source_url),
        candidate.byte_length,
        image.width.map_or_else(|| "неизвестно".into(), |v| v.to_string()),
        image.height.map_or_else(|| "неизвестно".into(), |v| v.to_string()),
        escape_html(&variant_summary(evidence)),
        semantic_status.as_str(),
        escape_html(&candidate.validation.validator.id),
        escape_html(&candidate.validation.validator.version),
        if is_rejected { "отклонена для этого SHA" } else { "состояние указано в пакете" },
    )
    .expect("запись в String не завершается ошибкой");
    if let Some(reason) = &unavailable_reason {
        write!(
            html,
            "<p>Изображение не показано: {}</p>",
            escape_html(reason)
        )
        .expect("запись в String не завершается ошибкой");
    } else {
        let path = candidate_image_path(&candidate.sha256)?;
        write!(
            html,
            "<figure><img alt=\"Pitch-accent для {}\" src=\"{}\"><figcaption>Путь к файлу относительно страницы: <code>{}</code>. Отображение не меняет результат проверки или публикации.</figcaption></figure>",
            escape_html(&candidate.metadata.surface),
            escape_html(&path),
            escape_html(&path)
        )
        .expect("запись в String не завершается ошибкой");
    }
    write_json_block(html, "Данные источника", &evidence)?;
    write_json_block(html, "Данные отрисовки", &evidence.render)?;
    write_json_block(html, "Данные браузера", &evidence.browser)?;
    write_json_block(
        html,
        "Результат валидатора и диагностика",
        &candidate.validation,
    )?;
    Ok(())
}

fn render_ambiguous_candidate(
    html: &mut String,
    candidate: &crate::jpdb::JpdbVocabularyCandidate,
) -> Result<(), AssetError> {
    write!(
        html,
        "<section><h5>ID словарной записи JPDB: {}</h5><dl><dt>Фактический адрес страницы</dt><dd><code>{}</code></dd><dt>Формы слова</dt><dd><ul>",
        candidate.vocabulary_id,
        escape_html(&candidate.detail_url)
    )
    .expect("запись в String не завершается ошибкой");
    for surface in &candidate.surface_forms {
        write!(html, "<li>{}</li>", escape_html(surface))
            .expect("запись в String не завершается ошибкой");
    }
    html.push_str("</ul></dd><dt>Чтения</dt><dd><ul>");
    for reading in &candidate.readings {
        write!(html, "<li>{}</li>", escape_html(reading))
            .expect("запись в String не завершается ошибкой");
    }
    html.push_str("</ul></dd><dt>Связанные формы слова и чтения</dt><dd><ul>");
    for form in &candidate.resolved_forms {
        write!(
            html,
            "<li>{} · {}</li>",
            escape_html(&form.surface),
            escape_html(&form.reading)
        )
        .expect("запись в String не завершается ошибкой");
    }
    html.push_str("</ul></dd><dt>Части речи</dt><dd><ul>");
    for part_of_speech in &candidate.part_of_speech {
        write!(html, "<li>{}</li>", escape_html(part_of_speech))
            .expect("запись в String не завершается ошибкой");
    }
    html.push_str("</ul></dd><dt>Значения</dt><dd><ul>");
    for meaning in &candidate.meanings {
        write!(html, "<li>{}</li>", escape_html(meaning))
            .expect("запись в String не завершается ошибкой");
    }
    html.push_str("</ul></dd></dl></section>");
    Ok(())
}

fn matching_owner_record<'a>(
    batch: &PitchAccentBatch,
    item: &PitchBatchItem,
    owner_records: &'a [AssetRecord],
) -> Result<Option<&'a AssetRecord>, AssetError> {
    let trusted_owner_shas = [
        item.existing_verified_sha256.as_deref(),
        item.published_sha256.as_deref(),
    ];
    let Some(current_owner_sha) = item.owner_current_sha256.as_deref() else {
        return Ok(None);
    };
    let record = owner_records.iter().find(|record| {
        record.identity == item.identity
            && record.sha256 == current_owner_sha
            && trusted_owner_shas
                .iter()
                .flatten()
                .any(|sha| *sha == record.sha256)
    });
    let Some(record) = record else {
        return Ok(None);
    };
    if record.lifecycle != LifecycleState::Verified
        || record.format != DetectedFormat::Png
        || !record.is_trusted_for_automated_validation(&batch.validator)
    {
        return Ok(None);
    }
    let expected = PitchAccentDomainPolicy.canonical_location(
        &item.identity,
        &record.sha256,
        record.format,
    )?;
    if record.storage_path != expected.storage_path
        || record.consumer_filename != expected.consumer_filename
        || record.byte_length == 0
        || record.byte_length > PITCH_ACCENT_MAX_ASSET_BYTES
    {
        return Ok(None);
    }
    let Some(value) = &record.domain_metadata else {
        return Ok(None);
    };
    let metadata: PitchAccentDomainMetadata =
        serde_json::from_value(value.clone()).map_err(|error| {
            invalid(format!(
                "метаданные канонического pitch-ресурса не читаются: {error}"
            ))
        })?;
    if metadata.surface != item.identity.key
        || metadata.jpdb_vocabulary_id == 0
        || metadata.evidence.variant_selection.detected_variant_count == 0
        || metadata.evidence.render.captured_graph_count != 1
        || metadata.evidence.source_url.is_empty()
    {
        return Ok(None);
    }
    Ok(Some(record))
}

fn matching_owner_snapshot_record<'a>(
    item: &PitchBatchItem,
    owner_records: &'a [AssetRecord],
) -> Option<&'a AssetRecord> {
    let sha256 = item.owner_current_sha256.as_deref()?;
    owner_records
        .iter()
        .find(|record| record.identity == item.identity && record.sha256 == sha256)
}

fn render_owner_snapshot_record(html: &mut String, record: &AssetRecord) -> Result<(), AssetError> {
    write!(
        html,
        "<section class=\"candidate\"><h3>Запись из снимка владельца, доверие не предполагается</h3><p>Состояние ресурса: <code>{}</code>; формат: <code>{}</code>; SHA-256: <code>{}</code>; размер: {} байт; путь в хранилище: <code>{}</code>; имя файла для потребителя: <code>{}</code></p>",
        record.lifecycle.as_str(),
        escape_html(&json_text(&serde_json::to_value(record.format).map_err(json_error)?)?),
        escape_html(&record.sha256),
        record.byte_length,
        escape_html(&record.storage_path),
        escape_html(&record.consumer_filename),
    )
    .expect("запись в String не завершается ошибкой");
    write_json_block(
        html,
        "Проверка записи из снимка владельца",
        &record.validation,
    )?;
    write_json_block(
        html,
        "Метаданные записи из снимка владельца",
        &record.domain_metadata,
    )?;
    write_json_block(
        html,
        "Источник данных записи из снимка владельца",
        &record.provenance,
    )?;
    write_json_block(
        html,
        "Подтверждение человеком записи из снимка владельца",
        &record.human_attestation,
    )?;
    html.push_str("</section>");
    Ok(())
}

fn render_canonical(
    html: &mut String,
    item: &PitchBatchItem,
    record: &AssetRecord,
) -> Result<(), AssetError> {
    let location = PitchAccentDomainPolicy.canonical_location(
        &item.identity,
        &record.sha256,
        DetectedFormat::Png,
    )?;
    let expected = location.storage_path;
    if record.storage_path != expected {
        return Err(AssetError::new(
            ErrorCode::PathTraversal,
            "путь канонического pitch-ресурса отличается от пути политики предметной области",
        ));
    }
    let metadata: PitchAccentDomainMetadata = serde_json::from_value(
        record
            .domain_metadata
            .clone()
            .ok_or_else(|| invalid("метаданные канонического pitch-ресурса отсутствуют"))?,
    )
    .map_err(|error| {
        invalid(format!(
            "метаданные канонического pitch-ресурса не читаются: {error}"
        ))
    })?;
    let relative_path = format!("../../../{}", percent_encode_path(&record.storage_path));
    write!(
        html,
        "<section class=\"candidate\"><h3>Канонический PNG</h3><p>Форма: {}; чтение: {}; ID словарной записи JPDB: {}; ссылка на страницу: <code>{}</code></p><p>{}; SHA-256: <code>{}</code>; размер: {} байт; размер PNG: {} × {}; статус проверки: <strong><code>{}</code></strong>; проверяющий модуль: <code>{}@{}</code>; публикация: подтверждена</p><figure><img alt=\"Канонический pitch-accent для {}\" src=\"{}\"><figcaption>Канонический путь в хранилище: <code>{}</code>; имя файла для потребителя: <code>{}</code></figcaption></figure>",
        escape_html(&metadata.surface),
        escape_html(&metadata.reading),
        metadata.jpdb_vocabulary_id,
        escape_html(&metadata.evidence.source_url),
        escape_html(&variant_summary(&metadata.evidence)),
        escape_html(&record.sha256),
        record.byte_length,
        metadata.evidence.render.pixel_width,
        metadata.evidence.render.pixel_height,
        record
            .validation
            .as_ref()
            .map_or("неизвестно", |v| v.status.as_str()),
        escape_html(
            record
                .validation
                .as_ref()
                .map_or("неизвестно", |v| v.validator.id.as_str()),
        ),
        escape_html(
            record
                .validation
                .as_ref()
                .map_or("неизвестно", |v| v.validator.version.as_str()),
        ),
        escape_html(&metadata.surface),
        escape_html(&relative_path),
        escape_html(&record.storage_path),
        escape_html(&record.consumer_filename),
    )
    .expect("запись в String не завершается ошибкой");
    write_json_block(
        html,
        "Данные канонического источника и отрисовки",
        &metadata.evidence,
    )?;
    write_json_block(
        html,
        "Данные канонического браузера",
        &metadata.evidence.browser,
    )?;
    html.push_str("</section>");
    Ok(())
}

/// Различает число вариантов на странице источника и число графиков в самом PNG.
fn variant_summary(evidence: &crate::pitch_accent::PitchAccentEvidence) -> String {
    let selection = &evidence.variant_selection;
    let selected = selection
        .variants
        .get(selection.selected_index as usize)
        .map_or_else(
            || "отсутствует в свидетельствах".to_owned(),
            |variant| format!("№{} «{}»", variant.index + 1, variant.text),
        );
    let basis = match selection.basis {
        crate::pitch_accent::PitchAccentVariantSelectionBasis::SeededHashWithoutSourcePriority => {
            "воспроизводимый хеш; JPDB не сообщает распространённость вариантов"
        }
    };
    format!(
        "вариантов графика у записи: {}; выбран вариант {selected}; основание выбора: {basis}; графиков в PNG: {}",
        selection.detected_variant_count, evidence.render.captured_graph_count
    )
}

fn validate_candidate_reference(
    item: &PitchBatchItem,
    candidate: &PitchBatchCandidate,
) -> Result<(), AssetError> {
    let expected_path = candidate_image_path(&candidate.sha256)?;
    if candidate.blob.storage_path != expected_path {
        return Err(AssetError::new(
            ErrorCode::PathTraversal,
            "путь кандидата не совпадает с безопасным путём PNG, вычисленным из SHA",
        ));
    }
    if candidate.blob.sha256 != candidate.sha256
        || candidate.validation.content_sha256 != candidate.sha256
        || candidate.metadata.surface != item.identity.key
        || candidate.byte_length == 0
        || candidate.byte_length > PITCH_ACCENT_MAX_ASSET_BYTES
    {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            "ссылка, идентичность, размер или проверка pitch-кандидата не совпадает",
        ));
    }
    Ok(())
}

fn verify_candidate_bytes(
    candidate: &PitchBatchCandidate,
    bytes: &[u8],
) -> Result<CandidateImage, AssetError> {
    if bytes.len() as u64 != candidate.byte_length
        || bytes.len() as u64 > PITCH_ACCENT_MAX_ASSET_BYTES
        || sha256_hex(bytes) != candidate.sha256
        || DetectedFormat::from_signature(bytes) != DetectedFormat::Png
    {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            "прочитанные байты pitch-кандидата не совпали с размером, SHA-256 или PNG",
        ));
    }
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(PITCH_ACCENT_MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(PITCH_ACCENT_MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(PITCH_ACCENT_MAX_DECODE_ALLOCATION_BYTES);
    let decoder = match image::codecs::png::PngDecoder::with_limits(Cursor::new(bytes), limits) {
        Ok(decoder) => decoder,
        Err(error) => {
            return Ok(CandidateImage::unavailable(
                candidate.byte_length,
                format!("декодер PNG отклонил данные: {error}"),
            ));
        }
    };
    let (width, height) = decoder.dimensions();
    let bytes_per_pixel = u64::from(decoder.color_type().bytes_per_pixel());
    let allocation = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(bytes_per_pixel));
    if width == 0
        || height == 0
        || allocation.is_none_or(|value| value > PITCH_ACCENT_MAX_DECODE_ALLOCATION_BYTES)
    {
        return Ok(CandidateImage::unavailable(
            candidate.byte_length,
            "размеры PNG превышают предел безопасного декодирования".into(),
        ));
    }
    if let Err(error) = image::DynamicImage::from_decoder(decoder) {
        return Ok(CandidateImage::unavailable(
            candidate.byte_length,
            format!("PNG не прошёл полное декодирование: {error}"),
        ));
    }
    Ok(CandidateImage {
        width: Some(width),
        height: Some(height),
        byte_length: candidate.byte_length,
        validation_error: None,
    })
}

#[derive(Debug, Clone)]
struct CandidateImage {
    width: Option<u32>,
    height: Option<u32>,
    byte_length: u64,
    validation_error: Option<String>,
}

impl CandidateImage {
    fn unavailable(byte_length: u64, reason: String) -> Self {
        Self {
            width: None,
            height: None,
            byte_length,
            validation_error: Some(reason),
        }
    }
}

fn candidate_image_path(sha256: &str) -> Result<String, AssetError> {
    if !is_sha256(sha256) {
        return Err(AssetError::new(
            ErrorCode::PathTraversal,
            "SHA-256 кандидата нельзя использовать как относительный путь",
        ));
    }
    Ok(format!("candidates/{sha256}.png"))
}

fn item_status_label(status: PitchBatchItemStatus) -> &'static str {
    match status {
        PitchBatchItemStatus::Pending => "ожидает получения",
        PitchBatchItemStatus::AcquiredVerified => "кандидат проверен валидатором",
        PitchBatchItemStatus::CandidateRejected => "кандидат отклонён или не прошёл проверку",
        PitchBatchItemStatus::NoPitchAccentOnSource => "на источнике нет pitch-accent",
        PitchBatchItemStatus::AmbiguousVocabulary => "несколько словарных кандидатов",
        PitchBatchItemStatus::VocabularyNotFound => "словарная запись не найдена",
        PitchBatchItemStatus::TechnicalFailure => "техническая ошибка получения",
        PitchBatchItemStatus::PublicationPending => "публикация ожидает сверки",
        PitchBatchItemStatus::Published => "канонический ресурс опубликован",
        PitchBatchItemStatus::ExistingVerified => "канонический ресурс уже подтверждён",
        PitchBatchItemStatus::Conflict => "конфликт состояния владельца",
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn percent_encode_path(path: &str) -> String {
    let mut output = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            output.push(char::from(byte));
        } else {
            write!(&mut output, "%{byte:02X}").expect("запись в String не завершается ошибкой");
        }
    }
    output
}

fn write_json_block<T: serde::Serialize>(
    html: &mut String,
    heading: &str,
    value: &T,
) -> Result<(), AssetError> {
    let serialized = serde_json::to_string_pretty(value).map_err(json_error)?;
    write!(
        html,
        "<h4>{}</h4><pre>{}</pre>",
        escape_html(heading),
        escape_html(&serialized)
    )
    .expect("запись в String не завершается ошибкой");
    Ok(())
}

fn json_text(value: &serde_json::Value) -> Result<String, AssetError> {
    serde_json::to_string(value).map_err(json_error)
}

fn escape_html(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&#39;"),
            _ => output.push(character),
        }
    }
    output
}

fn json_error(error: serde_json::Error) -> AssetError {
    invalid(format!(
        "не удалось сериализовать данные проверки pitch-accent: {error}"
    ))
}

fn invalid(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::InvalidTransition, message)
}

#[cfg(test)]
#[path = "pitch_review_tests.rs"]
mod tests;
