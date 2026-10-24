//! Возобновляемый массовый конвейер локального рендера PNG кандзи.
//!
//! Этапы для каждой кодовой точки независимы и сохраняются атомарно в
//! `items/U+XXXX.json` рабочего каталога: рендер → семантическая проверка
//! `KanjiImageValidator` → слепой OCR → публикация через
//! [`AssetStore::ingest_verified`]. Повтор продолжает с первого
//! незавершённого этапа и не создаёт дубликатов. OCR — независимая
//! дополнительная проверка, а не источник доверия: публикуются только PNG,
//! прошедшие и штатный валидатор, и совпадение OCR с ожидаемым символом.
//!
//! Конвейер никогда не заменяет существующую PNG-запись символа: исторический
//! PNG остаётся, а новый рендер служит контрольной генерацией для сравнения.
//! Запись GIF того же символа — независимый слот и не затрагивается.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::domain::KanjiDomainPolicy;
use crate::error::{AssetError, ErrorCode};
use crate::hashing::sha256_hex;
use crate::inference::{InferenceProvenance, InferenceSession};
use crate::inference_config::TaskId;
use crate::kanji_ocr::{OcrOptions, TRANSFORM_VERSION, contract, interpret};
use crate::kanji_validator::{KanjiImageValidator, prepare_ocr_image};
use crate::local_render::{
    GeometryProfile, LOCAL_RENDER_ALGORITHM, LOCAL_RENDER_PROVIDER_VERSION,
    LOCAL_RENDER_SOURCE_KIND, LocalFont, RenderEvidence, RenderSettings, compare_images,
};
use crate::model::{
    AssetIdentity, AssetRecord, DetectedFormat, HumanDecision, LifecycleState, Provenance,
    SemanticStatus, ValidationEvidence, ValidatorIdentity,
};
use crate::store::{AssetStore, HumanAttestationRequest, IngestRequest, VerifiedIngestRequest};
use crate::validation::SemanticValidator;

const ITEM_SCHEMA: u32 = 1;
const RUN_SCHEMA: u32 = 1;
/// Имя файла источника в `Provenance`; путь рабочего каталога не сохраняется.
const SOURCE_NAME: &str = "local-render.png";

/// Откуда символ попал в очередь.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TargetOrigin {
    /// В хранилище есть доверенный GIF символа.
    CorpusGif,
    /// В хранилище уже есть PNG символа: генерация контрольная.
    ExistingPng,
    /// Символ передан явно.
    Explicit,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RenderState {
    pub sha256: String,
    pub raw_sha256: String,
    pub byte_length: u64,
    pub file: String,
    pub raw_file: String,
    pub evidence_file: String,
    pub glyph_area: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StageError {
    pub code: String,
    pub message: String,
    pub attempts: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ValidationState {
    pub sha256: String,
    pub status: SemanticStatus,
    pub validator: ValidatorIdentity,
    pub evidence: Vec<ValidationEvidence>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OcrState {
    pub sha256: String,
    pub observed: Option<String>,
    pub matches_expected: bool,
    pub failure: Option<String>,
    pub prepared_sha256: Option<String>,
    pub model: String,
    pub provenance_sha256: String,
    pub elapsed_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublicationState {
    pub sha256: String,
    pub outcome: String,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ControlState {
    /// SHA исторического PNG хранилища, с которым сравнивался рендер.
    pub original_sha256: String,
    pub original_provenance: String,
    pub reference_file: String,
    pub metrics: crate::local_render::ImageComparison,
}

/// Решение проверки по точным байтам рендера.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReviewDecision {
    Approve,
    Reject,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReviewState {
    pub review_id: String,
    pub sha256: String,
    pub decision: ReviewDecision,
    pub reason: String,
}

/// Состояние одной кодовой точки; переписывается атомарно после каждого этапа.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ItemState {
    pub schema_version: u32,
    pub character: String,
    pub unicode: String,
    pub origins: BTreeSet<TargetOrigin>,
    pub render: Option<RenderState>,
    pub render_error: Option<StageError>,
    pub validation: Option<ValidationState>,
    pub ocr: Option<OcrState>,
    pub publication: Option<PublicationState>,
    pub control: Option<ControlState>,
    #[serde(default)]
    pub review: Option<ReviewState>,
}

impl ItemState {
    fn new(character: &str) -> Self {
        Self {
            schema_version: ITEM_SCHEMA,
            character: character.into(),
            unicode: unicode_of(character),
            origins: BTreeSet::new(),
            render: None,
            render_error: None,
            validation: None,
            ocr: None,
            publication: None,
            control: None,
            review: None,
        }
    }

    fn identity(&self) -> AssetIdentity {
        AssetIdentity {
            namespace: "kanji".into(),
            key: self.character.clone(),
        }
    }

    fn rendered_sha(&self) -> Option<&str> {
        self.render.as_ref().map(|render| render.sha256.as_str())
    }

    fn validation_current(&self) -> Option<&ValidationState> {
        self.validation
            .as_ref()
            .filter(|state| Some(state.sha256.as_str()) == self.rendered_sha())
    }

    fn ocr_current(&self) -> Option<&OcrState> {
        self.ocr
            .as_ref()
            .filter(|state| Some(state.sha256.as_str()) == self.rendered_sha())
    }

    fn review_current(&self) -> Option<&ReviewState> {
        self.review
            .as_ref()
            .filter(|state| Some(state.sha256.as_str()) == self.rendered_sha())
    }

    /// Группа проверки для байтов, не прошедших автоматические ворота; `None` —
    /// проверка не нужна или ещё не выполнены валидатор и OCR этих байтов.
    fn review_group(&self) -> Option<&'static str> {
        let validation = self.validation_current()?;
        let ocr = self.ocr_current()?;
        if self.passes_gates() || validation.status == SemanticStatus::Corrupt {
            return None;
        }
        let verified = validation.status == SemanticStatus::Verified;
        Some(if ocr.failure.is_some() {
            "ocr_failed"
        } else if ocr.matches_expected {
            "validator_doubt_ocr_match"
        } else if ocr.observed.as_deref() == Some("?") {
            "ocr_unrecognized"
        } else if verified {
            "ocr_mismatch"
        } else {
            "validator_doubt_ocr_mismatch"
        })
    }

    fn passes_gates(&self) -> bool {
        self.validation_current()
            .is_some_and(|state| state.status == SemanticStatus::Verified)
            && self
                .ocr_current()
                .is_some_and(|state| state.matches_expected)
    }
}

/// Неизменяемая конфигурация рабочего каталога: повтор с другой обязан
/// начинаться в новом каталоге.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunConfig {
    schema_version: u32,
    algorithm: String,
    settings: RenderSettings,
    font_sha256: String,
    store_id: String,
}

pub(crate) struct RunOptions {
    pub font: LocalFont,
    pub profile: GeometryProfile,
    pub work_dir: PathBuf,
    pub browsers: usize,
    pub from_corpus: bool,
    pub explicit: Vec<String>,
    pub limit: Option<usize>,
    pub ocr: Option<OcrOptions>,
    pub rescan_ocr: bool,
    pub publish: bool,
}

pub(crate) fn unicode_of(character: &str) -> String {
    format!(
        "U+{:04X}",
        u32::from(character.chars().next().unwrap_or('\0'))
    )
}

fn invalid(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::InvalidTransition, message)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Атомарная запись файла рабочего каталога: временный файл, `fsync`, `rename`.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), AssetError> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("у пути результата нет каталога"))?;
    std::fs::create_dir_all(parent).map_err(|error| AssetError::io("создание каталога", error))?;
    let temp = parent.join(format!(
        ".{}.partial",
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    let mut file =
        std::fs::File::create(&temp).map_err(|error| AssetError::io("запись результата", error))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| AssetError::io("запись результата", error))?;
    drop(file);
    std::fs::rename(&temp, path).map_err(|error| AssetError::io("публикация результата", error))
}

/// Рабочий каталог конвейера.
pub(crate) struct WorkDir {
    root: PathBuf,
}

impl WorkDir {
    pub(crate) fn open(root: &Path) -> Result<Self, AssetError> {
        std::fs::create_dir_all(root.join("items"))
            .map_err(|error| AssetError::io("создание рабочего каталога", error))?;
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    fn item_path(&self, unicode: &str) -> PathBuf {
        self.root.join("items").join(format!("{unicode}.json"))
    }

    pub(crate) fn load_item(&self, character: &str) -> Result<Option<ItemState>, AssetError> {
        let path = self.item_path(&unicode_of(character));
        match std::fs::read(&path) {
            Ok(bytes) => {
                let state: ItemState = serde_json::from_slice(&bytes).map_err(|error| {
                    invalid(format!("повреждено состояние {}: {error}", path.display()))
                })?;
                if state.character != character || state.schema_version != ITEM_SCHEMA {
                    return Err(invalid(format!(
                        "состояние {} не относится к символу {character}",
                        path.display()
                    )));
                }
                Ok(Some(state))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(AssetError::io("чтение состояния символа", error)),
        }
    }

    pub(crate) fn save_item(&self, state: &ItemState) -> Result<(), AssetError> {
        write_atomic(
            &self.item_path(&state.unicode),
            &serde_json::to_vec_pretty(state).expect("состояние сериализуется"),
        )
    }

    /// Все сохранённые состояния в порядке кодовых точек.
    pub(crate) fn all_items(&self) -> Result<Vec<ItemState>, AssetError> {
        let mut items = Vec::new();
        let entries = std::fs::read_dir(self.root.join("items"))
            .map_err(|error| AssetError::io("чтение состояний", error))?;
        for entry in entries {
            let entry = entry.map_err(|error| AssetError::io("чтение состояний", error))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with("U+") || !name.ends_with(".json") {
                continue;
            }
            let bytes = std::fs::read(entry.path())
                .map_err(|error| AssetError::io("чтение состояния", error))?;
            let state: ItemState = serde_json::from_slice(&bytes)
                .map_err(|error| invalid(format!("повреждено состояние {name}: {error}")))?;
            items.push(state);
        }
        items.sort_by(|left, right| left.character.cmp(&right.character));
        Ok(items)
    }

    fn read(&self, relative: &str) -> Result<Vec<u8>, AssetError> {
        std::fs::read(self.root.join(relative))
            .map_err(|error| AssetError::io("чтение артефакта рабочего каталога", error))
    }

    fn log(&self, line: &str) {
        eprintln!("{line}");
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join("progress.log"))
        {
            let _ = writeln!(file, "{} {line}", now_unix());
        }
    }
}

/// Инвентарь хранилища, построенный только по проверенному `verify_integrity`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Inventory {
    pub captured_at_unix: u64,
    pub store_id: String,
    pub records_total: usize,
    pub verified_gif_records: usize,
    pub trusted_gif_characters: usize,
    pub untrusted_gif_records: usize,
    pub png_records: usize,
    pub png_local_render_records: usize,
    pub png_other_records: usize,
    pub characters_with_gif_and_png: usize,
    pub runtime_records: usize,
    /// SHA-256 каждого GIF: основание проверки неизменности после публикации.
    pub gif_sha256: BTreeMap<String, String>,
    /// SHA-256 и источник каждого PNG.
    pub png: BTreeMap<String, (String, String)>,
}

pub(crate) fn inventory(store: &AssetStore) -> Result<(Inventory, Vec<AssetRecord>), AssetError> {
    let records = store.verify_integrity()?;
    let validator = KanjiImageValidator::validator_identity();
    let semantics = crate::domain::AssetDomainPolicy::trust_semantics(&KanjiDomainPolicy);
    let mut gif_sha256 = BTreeMap::new();
    let mut png = BTreeMap::new();
    let mut trusted_gif = BTreeSet::new();
    let mut untrusted_gif = 0;
    let mut verified_gif = 0;
    let mut runtime = 0;
    for record in &records {
        if record.lifecycle != LifecycleState::Verified {
            runtime += 1;
            continue;
        }
        match record.format {
            DetectedFormat::Gif => {
                verified_gif += 1;
                gif_sha256.insert(record.identity.key.clone(), record.sha256.clone());
                if semantics.confers_trust(record, &validator) {
                    trusted_gif.insert(record.identity.key.clone());
                } else {
                    untrusted_gif += 1;
                }
            }
            DetectedFormat::Png => {
                png.insert(
                    record.identity.key.clone(),
                    (record.sha256.clone(), record.provenance.source_kind.clone()),
                );
            }
            _ => {}
        }
    }
    let both = png
        .keys()
        .filter(|key| gif_sha256.contains_key(*key))
        .count();
    let local = png
        .values()
        .filter(|(_, kind)| kind == LOCAL_RENDER_SOURCE_KIND)
        .count();
    Ok((
        Inventory {
            captured_at_unix: now_unix(),
            store_id: store.store_id().to_owned(),
            records_total: records.len(),
            verified_gif_records: verified_gif,
            trusted_gif_characters: trusted_gif.len(),
            untrusted_gif_records: untrusted_gif,
            png_records: png.len(),
            png_local_render_records: local,
            png_other_records: png.len() - local,
            characters_with_gif_and_png: both,
            runtime_records: runtime,
            gif_sha256,
            png,
        },
        records,
    ))
}

fn trusted_gif_characters(records: &[AssetRecord]) -> BTreeSet<String> {
    let validator = KanjiImageValidator::validator_identity();
    let semantics = crate::domain::AssetDomainPolicy::trust_semantics(&KanjiDomainPolicy);
    records
        .iter()
        .filter(|record| {
            record.format == DetectedFormat::Gif
                && record.lifecycle == LifecycleState::Verified
                && semantics.confers_trust(record, &validator)
        })
        .map(|record| record.identity.key.clone())
        .collect()
}

/// Исторические PNG (не локальный рендер): для них генерация только контрольная.
fn existing_png_characters(records: &[AssetRecord]) -> BTreeSet<String> {
    records
        .iter()
        .filter(|record| {
            record.format == DetectedFormat::Png
                && record.lifecycle == LifecycleState::Verified
                && record.provenance.source_kind != LOCAL_RENDER_SOURCE_KIND
        })
        .map(|record| record.identity.key.clone())
        .collect()
}

fn glyph_area(png: &[u8]) -> u64 {
    image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .map(|image| {
            let gray = image.to_luma8();
            let (width, height) = gray.dimensions();
            let mut area = 0;
            for y in 8..height.saturating_sub(8) {
                for x in 8..width.saturating_sub(8) {
                    area += u64::from(gray.get_pixel(x, y)[0] >= 180);
                }
            }
            area
        })
        .unwrap_or(0)
}

fn local_metadata(state: &ItemState, evidence: &RenderEvidence) -> Value {
    json!({
        "domain": "kanji",
        "character": state.character,
        "unicode_codepoints": [state.unicode],
        "local_render": {
            "provider": LOCAL_RENDER_SOURCE_KIND,
            "provider_version": LOCAL_RENDER_PROVIDER_VERSION,
            "algorithm": evidence.settings.algorithm,
            "geometry_profile": evidence.settings.geometry_profile,
            "origin_css": evidence.settings.origin_css,
            "device_scale_factor": evidence.settings.device_scale_factor,
            "font_size_css": evidence.settings.font_size_css,
            "glyph_translate_css": evidence.settings.glyph_translate_css,
            "coverage_gamma": evidence.settings.coverage_correction.gamma,
            "font_sha256": evidence.font.sha256,
            "font_version": evidence.font.version,
            "font_calibrated": evidence.font.calibrated,
            "browser_product": evidence.browser_product,
            "browser_revision": evidence.browser_revision,
            "platform_font": evidence.platform_fonts.first().map(|font| font.post_script_name.clone()),
            "raw_sha256": evidence.raw_sha256,
            "not_yarxi_capture": true,
        },
    })
}

fn validate_bytes(character: &str, bytes: &[u8]) -> Result<ValidationState, String> {
    let validator = KanjiImageValidator::new();
    let sha256 = sha256_hex(bytes);
    let record = AssetRecord {
        identity: AssetIdentity {
            namespace: "kanji".into(),
            key: character.into(),
        },
        storage_path: format!("assets/png/{character}.png"),
        consumer_filename: format!("{character}.png"),
        sha256: sha256.clone(),
        byte_length: bytes.len() as u64,
        format: DetectedFormat::from_signature(bytes),
        provenance: Provenance {
            source_kind: LOCAL_RENDER_SOURCE_KIND.into(),
            source_name: SOURCE_NAME.into(),
        },
        lifecycle: LifecycleState::Pending,
        validation: None,
        human_attestation: None,
        domain_metadata: None,
    };
    let decision = validator
        .validate(&record, &mut std::io::Cursor::new(bytes))
        .map_err(|failure| format!("{}: {}", failure.code, failure.message))?;
    Ok(ValidationState {
        sha256,
        status: decision.status,
        validator: validator.identity(),
        evidence: decision.evidence,
    })
}

/// Выполняет все этапы конвейера и возвращает машиночитаемую сводку прогона.
pub(crate) fn run(store: &AssetStore, options: RunOptions) -> Result<Value, AssetError> {
    let work = WorkDir::open(&options.work_dir)?;
    let settings = RenderSettings::for_profile(options.profile);
    let config = RunConfig {
        schema_version: RUN_SCHEMA,
        algorithm: LOCAL_RENDER_ALGORITHM.into(),
        settings: settings.clone(),
        font_sha256: options.font.sha256.clone(),
        store_id: store.store_id().to_owned(),
    };
    let config_path = work.root().join("run.json");
    match std::fs::read(&config_path) {
        Ok(bytes) => {
            let saved: RunConfig = serde_json::from_slice(&bytes)
                .map_err(|error| invalid(format!("повреждён run.json: {error}")))?;
            if saved != config {
                return Err(invalid(
                    "рабочий каталог создан с другими параметрами рендера, шрифтом или хранилищем; \
                     начните новый каталог",
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            write_atomic(
                &config_path,
                &serde_json::to_vec_pretty(&config).expect("конфигурация сериализуется"),
            )?;
        }
        Err(error) => return Err(AssetError::io("чтение run.json", error)),
    }
    if options.publish && !options.font.calibrated {
        return Err(invalid(
            "публикация разрешена только для откалиброванного шрифта",
        ));
    }

    // 1. Инвентарь до любых изменений: фиксируется один раз и не переписывается.
    let (inventory_now, records) = inventory(store)?;
    let before_path = work.root().join("inventory-before.json");
    if !before_path.exists() {
        write_atomic(
            &before_path,
            &serde_json::to_vec_pretty(&inventory_now).expect("инвентарь сериализуется"),
        )?;
    }
    let mut targets: BTreeMap<String, BTreeSet<TargetOrigin>> = BTreeMap::new();
    if options.from_corpus {
        for character in trusted_gif_characters(&records) {
            targets
                .entry(character)
                .or_default()
                .insert(TargetOrigin::CorpusGif);
        }
        for character in existing_png_characters(&records) {
            targets
                .entry(character)
                .or_default()
                .insert(TargetOrigin::ExistingPng);
        }
    }
    for character in &options.explicit {
        targets
            .entry(character.clone())
            .or_default()
            .insert(TargetOrigin::Explicit);
    }
    let mut states = Vec::with_capacity(targets.len());
    for (character, origins) in targets {
        let mut state = work
            .load_item(&character)?
            .unwrap_or_else(|| ItemState::new(&character));
        let before = state.clone();
        state.origins.extend(origins);
        if existing_png_characters(&records).contains(&character) {
            state.origins.insert(TargetOrigin::ExistingPng);
        }
        if state != before {
            work.save_item(&state)?;
        }
        states.push(state);
    }
    if let Some(limit) = options.limit {
        // Предел ограничивает работу прогона, но не забывает остальные символы.
        let mut taken = 0;
        states.retain(|state| {
            let done = state.publication.is_some()
                || state
                    .render_error
                    .as_ref()
                    .is_some_and(|error| error.code == "glyph_missing");
            if done {
                return false;
            }
            taken += 1;
            taken <= limit
        });
    }
    // Сведения о составе хранилища остаются в инвентаре, а не в журнале хода.
    work.log(&format!(
        "local-png run: целей {}, браузеров {}",
        states.len(),
        options.browsers
    ));

    render_stage(&work, &options, &mut states)?;
    validation_stage(&work, &mut states)?;
    control_stage(&work, store, &mut states)?;
    let ocr_report = match &options.ocr {
        Some(ocr) => Some(ocr_stage(&work, ocr, options.rescan_ocr, &mut states)?),
        None => None,
    };
    if options.publish {
        publish_stage(&work, store, &mut states)?;
    }
    let (inventory_after, _) = inventory(store)?;
    write_atomic(
        &work.root().join("inventory-latest.json"),
        &serde_json::to_vec_pretty(&inventory_after).expect("инвентарь сериализуется"),
    )?;
    let counts = outcome_counts(&work.all_items()?, &inventory_after);
    Ok(json!({
        "schema_version": 1,
        "operation": "local-png run",
        "outcome": "completed",
        "work_dir": work.root().display().to_string(),
        "processed": states.len(),
        "counts": counts,
        "ocr": ocr_report,
    }))
}

fn render_stage(
    work: &WorkDir,
    options: &RunOptions,
    states: &mut [ItemState],
) -> Result<(), AssetError> {
    let mut pending = Vec::new();
    for state in states.iter() {
        let finished_missing = state
            .render_error
            .as_ref()
            .is_some_and(|error| error.code == "glyph_missing");
        let intact = state.render.as_ref().is_some_and(|render| {
            work.read(&render.file)
                .is_ok_and(|bytes| sha256_hex(&bytes) == render.sha256)
        });
        if !intact && !finished_missing {
            pending.push(state.character.clone());
        }
    }
    if pending.is_empty() {
        return Ok(());
    }
    // Отсутствие глифа устанавливается до запуска браузера и окончательно.
    let (missing, pending): (Vec<_>, Vec<_>) = pending.into_iter().partition(|character| {
        !options
            .font
            .has_glyph(character.chars().next().expect("символ непуст"))
    });
    let index: BTreeMap<String, usize> = states
        .iter()
        .enumerate()
        .map(|(position, state)| (state.character.clone(), position))
        .collect();
    for character in missing {
        let state = &mut states[index[&character]];
        state.render = None;
        state.render_error = Some(StageError {
            code: "glyph_missing".into(),
            message: format!(
                "в шрифте {} нет глифа для {}",
                options.font.sha256, state.unicode
            ),
            attempts: state
                .render_error
                .as_ref()
                .map_or(0, |error| error.attempts)
                + 1,
        });
        work.save_item(state)?;
        work.log(&format!("render {}: glyph_missing", state.unicode));
    }
    let total = pending.len();
    let mut done = 0usize;
    work.log(&format!("render: к генерации {total}"));
    super::local_png_cli::render_pool(
        &options.font,
        options.profile,
        pending,
        options.browsers,
        |(character, result)| {
            done += 1;
            let state = &mut states[index[&character]];
            match result {
                Ok(rendered) => {
                    let file = format!("png/{}.png", state.unicode);
                    let raw_file = format!("raw/{}.png", state.unicode);
                    let evidence_file = format!("evidence/{}.json", state.unicode);
                    write_atomic(&work.root().join(&file), &rendered.png)?;
                    write_atomic(&work.root().join(&raw_file), &rendered.raw_png)?;
                    write_atomic(
                        &work.root().join(&evidence_file),
                        &serde_json::to_vec_pretty(&rendered.evidence)
                            .expect("свидетельства сериализуются"),
                    )?;
                    let render = RenderState {
                        sha256: rendered.evidence.corrected_sha256.clone(),
                        raw_sha256: rendered.evidence.raw_sha256.clone(),
                        byte_length: rendered.png.len() as u64,
                        file,
                        raw_file,
                        evidence_file,
                        glyph_area: glyph_area(&rendered.png),
                    };
                    if state.rendered_sha() != Some(render.sha256.as_str()) {
                        // Новые байты не наследуют решений прежних байтов.
                        state.validation = None;
                        state.ocr = None;
                        state.publication = None;
                        state.control = None;
                    }
                    state.render = Some(render);
                    state.render_error = None;
                }
                Err(message) => {
                    let code = message
                        .split(':')
                        .next()
                        .unwrap_or("render_failed")
                        .trim()
                        .to_owned();
                    state.render_error = Some(StageError {
                        code,
                        message,
                        attempts: state
                            .render_error
                            .as_ref()
                            .map_or(0, |error| error.attempts)
                            + 1,
                    });
                }
            }
            work.save_item(state)?;
            if done.is_multiple_of(100) || done == total {
                work.log(&format!("render: {done}/{total}"));
            }
            Ok(())
        },
    )?;
    Ok(())
}

fn validation_stage(work: &WorkDir, states: &mut [ItemState]) -> Result<(), AssetError> {
    let validator = KanjiImageValidator::validator_identity();
    let total = states.iter().filter(|state| state.render.is_some()).count();
    let mut done = 0usize;
    for state in states.iter_mut() {
        let Some(render) = state.render.clone() else {
            continue;
        };
        if state
            .validation_current()
            .is_some_and(|validation| validation.validator == validator)
        {
            continue;
        }
        let bytes = work.read(&render.file)?;
        if sha256_hex(&bytes) != render.sha256 {
            return Err(AssetError::new(
                ErrorCode::IntegrityMismatch,
                format!("файл рендера {} изменился", render.file),
            ));
        }
        state.validation = Some(validate_bytes(&state.character, &bytes).map_err(|message| {
            AssetError::new(
                ErrorCode::ValidatorFailure,
                format!("{}: {message}", state.unicode),
            )
        })?);
        work.save_item(state)?;
        done += 1;
        if done.is_multiple_of(200) {
            work.log(&format!("validate: {done}/{total}"));
        }
    }
    work.log(&format!("validate: актуальных решений {total}"));
    Ok(())
}

/// Для символов с историческим PNG: копия оригинала и диагностические метрики.
fn control_stage(
    work: &WorkDir,
    store: &AssetStore,
    states: &mut [ItemState],
) -> Result<(), AssetError> {
    let validator = KanjiImageValidator::validator_identity();
    let pending: Vec<usize> = states
        .iter()
        .enumerate()
        .filter(|(_, state)| {
            state.origins.contains(&TargetOrigin::ExistingPng)
                && state.render.is_some()
                && state.control.is_none()
        })
        .map(|(index, _)| index)
        .collect();
    if pending.is_empty() {
        return Ok(());
    }
    let requests: Vec<_> = pending
        .iter()
        .map(|&index| (states[index].identity(), DetectedFormat::Png))
        .collect();
    AssetStore::visit_verified_variants_with_policy(
        store.root(),
        &requests,
        &validator,
        &KanjiDomainPolicy,
        |request, original| {
            let original = original?;
            let state = &mut states[pending[request]];
            let render = state.render.clone().expect("отобран рендер");
            if original.record.provenance.source_kind == LOCAL_RENDER_SOURCE_KIND {
                return Ok(());
            }
            let reference_file = format!("references/{}.png", state.unicode);
            write_atomic(&work.root().join(&reference_file), &original.bytes)?;
            let decode = |bytes: &[u8]| {
                image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
                    .map(|image| image.to_rgb8())
                    .map_err(|error| invalid(format!("декодирование PNG: {error}")))
            };
            let metrics = compare_images(
                &decode(&original.bytes)?,
                &decode(&work.read(&render.file)?)?,
            )
            .map_err(invalid)?;
            state.control = Some(ControlState {
                original_sha256: original.record.sha256.clone(),
                original_provenance: format!(
                    "{}:{}",
                    original.record.provenance.source_kind, original.record.provenance.source_name
                ),
                reference_file,
                metrics,
            });
            work.save_item(state)
        },
    )
}

/// Слепой OCR каждого сгенерированного PNG. Модели передаётся только кадр и
/// штатный промпт задачи; ожидаемый символ сравнивается после ответа.
fn ocr_stage(
    work: &WorkDir,
    options: &OcrOptions,
    rescan: bool,
    states: &mut [ItemState],
) -> Result<Value, AssetError> {
    let targets: Vec<usize> = states
        .iter()
        .enumerate()
        .filter(|(_, state)| {
            state.render.is_some()
                && match state.ocr_current() {
                    None => true,
                    // Технические отказы повторяются; ответ модели — только по --rescan-ocr.
                    Some(ocr) => rescan || ocr.failure.is_some(),
                }
        })
        .map(|(position, _)| position)
        .collect();
    if targets.is_empty() {
        work.log("ocr: нечего распознавать");
        return Ok(json!({"requests": 0}));
    }
    let task = options.inference.resolve(TaskId::KanjiOcr)?;
    task.require_ready()?;
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| AssetError::io("runtime OCR", error))?;
    work.log(&format!("ocr: подготовка модели, целей {}", targets.len()));
    let prepared = executor.block_on(InferenceSession::prepare(
        task,
        contract(),
        TRANSFORM_VERSION,
    ));
    let mut session = match prepared {
        Ok(session) => session,
        Err(error) => {
            let (failure, report) = *error;
            let stop_unproven = failure.stop_unproven();
            return Err(crate::inference::with_session_evidence(
                failure.into_asset_error(),
                stop_unproven,
                Some(&report),
            ));
        }
    };
    let provenance: InferenceProvenance = session.provenance().clone();
    let provenance_sha256 =
        sha256_hex(serde_json::to_vec(&provenance).expect("провенанс сериализуется"));
    let model = provenance.summary();
    write_atomic(
        &work.root().join("ocr-model.json"),
        &serde_json::to_vec_pretty(&json!({
            "provenance": provenance,
            "provenance_sha256": provenance_sha256,
            "summary": model,
        }))
        .expect("провенанс сериализуется"),
    )?;
    work.log(&format!("ocr: модель {model}"));
    let total = targets.len();
    let mut requests = 0usize;
    let mut interrupted = false;
    let mut failure: Option<AssetError> = None;
    for (done, position) in targets.into_iter().enumerate() {
        if session.interrupted() {
            interrupted = true;
            break;
        }
        let state = &mut states[position];
        let render = state.render.clone().expect("цель OCR отрендерена");
        let bytes = match work.read(&render.file) {
            Ok(bytes) => bytes,
            Err(error) => {
                failure = Some(error);
                break;
            }
        };
        let mut ocr = OcrState {
            sha256: render.sha256.clone(),
            observed: None,
            matches_expected: false,
            failure: None,
            prepared_sha256: None,
            model: model.clone(),
            provenance_sha256: provenance_sha256.clone(),
            elapsed_ms: None,
        };
        match prepare_ocr_image(&bytes) {
            Ok(image) => {
                ocr.prepared_sha256 = Some(sha256_hex(&image.png));
                requests += 1;
                match executor.block_on(session.run(Some(&image.png))) {
                    Err(error) if error.is_interrupted() => {
                        interrupted = true;
                        break;
                    }
                    Err(error) => ocr.failure = Some(error.code),
                    Ok(response) => {
                        ocr.elapsed_ms = Some(response.timings.elapsed_ms);
                        match interpret(&response.content) {
                            Ok(observed) => {
                                // Сравнение с ожидаемым — только после ответа модели.
                                ocr.matches_expected = observed == state.character;
                                ocr.observed = Some(observed);
                            }
                            Err(reason) => ocr.failure = Some(reason),
                        }
                    }
                }
            }
            Err(reason) => ocr.failure = Some(format!("image_prepare_failed: {reason}")),
        }
        state.ocr = Some(ocr);
        if let Err(error) = work.save_item(state) {
            failure = Some(error);
            break;
        }
        if (done + 1).is_multiple_of(50) || done + 1 == total {
            work.log(&format!("ocr: {}/{total}", done + 1));
        }
    }
    let finished = executor.block_on(session.finish());
    let stop_unproven = finished.stop.as_ref().is_some_and(|stop| !stop.proven());
    if let Some(error) = failure {
        return Err(crate::inference::with_session_evidence(
            error,
            stop_unproven,
            Some(&finished),
        ));
    }
    if interrupted {
        return Err(crate::inference::interrupted_error(
            "OCR прерван сигналом; уже сохранённые результаты остаются в рабочем каталоге",
            requests,
            stop_unproven,
            Some(&finished),
        ));
    }
    Ok(json!({
        "requests": requests,
        "model": model,
        "provenance_sha256": provenance_sha256,
        "stop_unproven": stop_unproven,
    }))
}

fn publish_stage(
    work: &WorkDir,
    store: &AssetStore,
    states: &mut [ItemState],
) -> Result<(), AssetError> {
    let validator = KanjiImageValidator::new();
    let (_, records) = inventory(store)?;
    let png_slots: BTreeMap<String, AssetRecord> = records
        .into_iter()
        .filter(|record| record.format == DetectedFormat::Png)
        .map(|record| (record.identity.key.clone(), record))
        .collect();
    let mut published = 0usize;
    for state in states.iter_mut() {
        let Some(render) = state.render.clone() else {
            continue;
        };
        if state
            .publication
            .as_ref()
            .is_some_and(|publication| publication.sha256 == render.sha256)
            && state
                .publication
                .as_ref()
                .is_some_and(|publication| publication.outcome != "publish_error")
        {
            continue;
        }
        if !state.passes_gates() {
            continue;
        }
        let outcome = match png_slots.get(&state.character) {
            Some(existing) if existing.sha256 == render.sha256 => PublicationState {
                sha256: render.sha256.clone(),
                outcome: "already_published".into(),
                message: None,
            },
            Some(existing) if existing.provenance.source_kind != LOCAL_RENDER_SOURCE_KIND => {
                PublicationState {
                    sha256: render.sha256.clone(),
                    outcome: "existing_original_kept".into(),
                    message: Some(format!(
                        "исторический PNG {} сохранён; рендер — контрольная генерация",
                        existing.sha256
                    )),
                }
            }
            Some(existing) => PublicationState {
                sha256: render.sha256.clone(),
                outcome: "png_slot_occupied".into(),
                message: Some(format!(
                    "в хранилище другой локальный рендер {}; автоматическая замена не выполняется",
                    existing.sha256
                )),
            },
            None => {
                let bytes = work.read(&render.file)?;
                let evidence: RenderEvidence =
                    serde_json::from_slice(&work.read(&render.evidence_file)?)
                        .map_err(|error| invalid(format!("свидетельства рендера: {error}")))?;
                let request = VerifiedIngestRequest {
                    identity: state.identity(),
                    bytes,
                    provenance: Provenance {
                        source_kind: LOCAL_RENDER_SOURCE_KIND.into(),
                        source_name: SOURCE_NAME.into(),
                    },
                    domain_metadata: Some(local_metadata(state, &evidence)),
                    // PNG-слот пуст: CAS не нужен, а GIF этого символа не затрагивается.
                    replace_expected_sha256: None,
                };
                match store.ingest_verified(request, &validator) {
                    Ok(outcome) if outcome.status == SemanticStatus::Verified => {
                        published += 1;
                        PublicationState {
                            sha256: outcome.sha256,
                            outcome: if outcome.changed {
                                "published".into()
                            } else {
                                "already_published".into()
                            },
                            message: None,
                        }
                    }
                    Ok(outcome) => PublicationState {
                        sha256: outcome.sha256,
                        outcome: format!("store_validator_{}", outcome.status.as_str()),
                        message: Some("штатная проверка при публикации не дала VERIFIED".into()),
                    },
                    Err(error) => PublicationState {
                        sha256: render.sha256.clone(),
                        outcome: "publish_error".into(),
                        message: Some(format!("{}: {}", error.code.as_str(), error.message)),
                    },
                }
            }
        };
        state.publication = Some(outcome);
        work.save_item(state)?;
        if published > 0 && published.is_multiple_of(100) {
            work.log(&format!("publish: опубликовано {published}"));
        }
    }
    work.log(&format!("publish: опубликовано в этом прогоне {published}"));
    Ok(())
}

const REVIEW_HEADER: &str = "unicode\tcharacter\tsha256\tgroup\tvalidator\tocr\tdecision\treason";

fn review_id_of(rows: &[(String, String)]) -> String {
    let mut text = String::new();
    for (unicode, sha256) in rows {
        text.push_str(&format!("{unicode}\t{sha256}\n"));
    }
    sha256_hex(text.as_bytes())[..32].to_owned()
}

/// Снимок третьего этапа: символы, у которых валидатор и слепой OCR не дали
/// совместного автоматического подтверждения. Хранилище не меняется.
pub(crate) fn review_export(store: &AssetStore, root: &Path) -> Result<Value, AssetError> {
    let work = WorkDir::open(root)?;
    let (after, _) = inventory(store)?;
    let mut rows = Vec::new();
    for state in work.all_items()? {
        let Some(group) = state.review_group() else {
            continue;
        };
        // Занятый PNG-слот этим путём не меняется, а решённое не переспрашивается.
        if state.review_current().is_some() || after.png.contains_key(&state.character) {
            continue;
        }
        rows.push((group, state));
    }
    rows.sort_by(|left, right| {
        left.0
            .cmp(right.0)
            .then(left.1.character.cmp(&right.1.character))
    });
    let keys: Vec<(String, String)> = rows
        .iter()
        .map(|(_, state)| {
            (
                state.unicode.clone(),
                state.rendered_sha().unwrap_or("").to_owned(),
            )
        })
        .collect();
    let review_id = review_id_of(&keys);
    let mut tsv = format!("# review_id={review_id}\n{REVIEW_HEADER}\n");
    let mut groups: BTreeMap<&str, usize> = BTreeMap::new();
    for (group, state) in &rows {
        *groups.entry(group).or_insert(0) += 1;
        let ocr = state.ocr_current().expect("группа требует OCR");
        tsv.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t\t\n",
            state.unicode,
            state.character,
            state.rendered_sha().unwrap_or(""),
            group,
            state
                .validation_current()
                .expect("группа требует проверки")
                .status
                .as_str(),
            ocr.observed.as_deref().unwrap_or("-"),
        ));
    }
    let directory = format!("review/{review_id}");
    write_atomic(&root.join(&directory).join("decisions.tsv"), tsv.as_bytes())?;
    write_atomic(
        &root.join(&directory).join("index.html"),
        review_html(&review_id, &rows).as_bytes(),
    )?;
    Ok(json!({
        "schema_version": 1,
        "operation": "local-png review-export",
        "outcome": "completed",
        "review_id": review_id,
        "counts": {"rows": rows.len(), "groups": groups},
        "decisions": format!("{directory}/decisions.tsv"),
        "html": format!("{directory}/index.html"),
    }))
}

fn review_html(review_id: &str, rows: &[(&str, ItemState)]) -> String {
    let mut html = format!(
        "<!doctype html><html lang=ru><meta charset=utf-8><meta name=viewport content='width=device-width,initial-scale=1'>\
         <title>Проверка local-png</title><style>\
         :root{{color-scheme:light dark;--bg:#fff;--fg:#111;--line:#ccc}}\
         @media (prefers-color-scheme:dark){{:root{{--bg:#151515;--fg:#eee;--line:#444}}}}\
         body{{margin:0;padding:16px;background:var(--bg);color:var(--fg);font:14px system-ui,sans-serif}}\
         .grid{{display:flex;flex-wrap:wrap;gap:12px}}.card{{border:1px solid var(--line);padding:8px;width:360px}}\
         .card img{{width:348px;height:348px;image-rendering:pixelated;display:block}}\
         .expected{{font:64px 'Noto Serif JP',serif}}</style><h1>Снимок {review_id}</h1>"
    );
    let mut current = "";
    for (group, state) in rows {
        if *group != current {
            if !current.is_empty() {
                html.push_str("</div>");
            }
            html.push_str(&format!("<h2>{}</h2><div class=grid>", html_escape(group)));
            current = group;
        }
        let file = state
            .render
            .as_ref()
            .map(|render| render.file.as_str())
            .unwrap_or("");
        let ocr = state
            .ocr_current()
            .and_then(|ocr| ocr.observed.clone())
            .unwrap_or_else(|| "-".into());
        html.push_str(&format!(
            "<div class=card><img src='../../{}' alt='{}'><div><span class=expected>{}</span> {} · OCR {} · валидатор {}</div></div>",
            html_escape(file),
            html_escape(&state.character),
            html_escape(&state.character),
            html_escape(&state.unicode),
            html_escape(&ocr),
            state.validation_current().map(|v| v.status.as_str()).unwrap_or("-"),
        ));
    }
    if !current.is_empty() {
        html.push_str("</div>");
    }
    html.push_str("</html>\n");
    html
}

struct ReviewRow {
    unicode: String,
    sha256: String,
    decision: Option<ReviewDecision>,
    reason: String,
}

fn parse_review(text: &str, review_id: &str) -> Result<Vec<ReviewRow>, AssetError> {
    let mut lines = text.lines();
    if lines.next() != Some(format!("# review_id={review_id}").as_str()) {
        return Err(invalid("decisions.tsv не относится к указанному снимку"));
    }
    if lines.next() != Some(REVIEW_HEADER) {
        return Err(invalid("неизвестный заголовок decisions.tsv"));
    }
    let mut rows = Vec::new();
    for (index, line) in lines.enumerate() {
        if line.is_empty() {
            continue;
        }
        let columns: Vec<&str> = line.split('\t').collect();
        if columns.len() != 8 {
            return Err(invalid(format!(
                "строка {}: ожидается 8 столбцов",
                index + 3
            )));
        }
        let decision = match columns[6].trim() {
            "" | "-" => None,
            "approve" => Some(ReviewDecision::Approve),
            "reject" => Some(ReviewDecision::Reject),
            other => {
                return Err(invalid(format!(
                    "строка {}: неизвестное решение {other:?}; допустимы approve, reject, - или пусто",
                    index + 3
                )));
            }
        };
        let reason = columns[7].trim().to_owned();
        if decision.is_some() && reason.is_empty() {
            return Err(invalid(format!(
                "строка {}: решение требует основания",
                index + 3
            )));
        }
        rows.push(ReviewRow {
            unicode: columns[0].into(),
            sha256: columns[2].into(),
            decision,
            reason,
        });
    }
    let keys: Vec<(String, String)> = rows
        .iter()
        .map(|row| (row.unicode.clone(), row.sha256.clone()))
        .collect();
    if review_id_of(&keys) != review_id {
        return Err(invalid(
            "набор строк decisions.tsv изменён после review-export",
        ));
    }
    Ok(rows)
}

/// Применяет решения снимка. Весь файл проверяется до первого изменения;
/// повтор уже применённых решений ничего не меняет.
pub(crate) fn review_apply(
    store: &AssetStore,
    root: &Path,
    review_id: &str,
) -> Result<Value, AssetError> {
    if review_id.len() != 32 || !review_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid("review_id — 32 шестнадцатеричных символа"));
    }
    let work = WorkDir::open(root)?;
    let text = String::from_utf8(work.read(&format!("review/{review_id}/decisions.tsv"))?)
        .map_err(|_| invalid("decisions.tsv не в UTF-8"))?;
    let rows = parse_review(&text, review_id)?;
    let mut decided = Vec::new();
    for row in &rows {
        let Some(decision) = row.decision else {
            continue;
        };
        let character = char::from_u32(
            u32::from_str_radix(row.unicode.trim_start_matches("U+"), 16)
                .map_err(|_| invalid(format!("неверная кодовая точка {}", row.unicode)))?,
        )
        .ok_or_else(|| invalid(format!("неверная кодовая точка {}", row.unicode)))?
        .to_string();
        let state = work
            .load_item(&character)?
            .ok_or_else(|| invalid(format!("нет состояния {}", row.unicode)))?;
        if state.rendered_sha() != Some(row.sha256.as_str()) {
            return Err(invalid(format!(
                "{}: SHA-256 строки не совпадает с текущим рендером",
                row.unicode
            )));
        }
        if let Some(previous) = state.review_current()
            && (previous.decision != decision || previous.review_id != review_id)
        {
            return Err(invalid(format!(
                "{}: для этих байтов уже применено другое решение",
                row.unicode
            )));
        }
        if state.review_current().is_none() && state.review_group().is_none() {
            return Err(invalid(format!(
                "{}: символ больше не требует проверки",
                row.unicode
            )));
        }
        decided.push((state, decision, row.reason.clone()));
    }
    let (_, records) = inventory(store)?;
    let mut png_slots: BTreeMap<String, AssetRecord> = records
        .into_iter()
        .filter(|record| record.format == DetectedFormat::Png)
        .map(|record| (record.identity.key.clone(), record))
        .collect();
    let (mut approved, mut rejected, mut failed) = (0usize, 0usize, 0usize);
    let mut items = Vec::new();
    for (mut state, decision, reason) in decided {
        state.review = Some(ReviewState {
            review_id: review_id.into(),
            sha256: state.rendered_sha().unwrap_or("").to_owned(),
            decision,
            reason: reason.clone(),
        });
        work.save_item(&state)?;
        let outcome = match decision {
            ReviewDecision::Reject => {
                rejected += 1;
                "review_rejected".to_owned()
            }
            ReviewDecision::Approve => {
                let slot = png_slots.get(&state.character).cloned();
                match publish_reviewed(store, &work, &state, slot, review_id, &reason) {
                    Ok((outcome, record)) => {
                        if let Some(record) = record {
                            png_slots.insert(state.character.clone(), record);
                        }
                        if outcome == "published_after_review" || outcome == "already_published" {
                            approved += 1;
                        } else {
                            failed += 1;
                        }
                        state.publication = Some(PublicationState {
                            sha256: state.rendered_sha().unwrap_or("").to_owned(),
                            outcome: outcome.into(),
                            message: None,
                        });
                        outcome.to_owned()
                    }
                    Err(error) => {
                        failed += 1;
                        state.publication = Some(PublicationState {
                            sha256: state.rendered_sha().unwrap_or("").to_owned(),
                            outcome: "publish_error".into(),
                            message: Some(format!("{}: {}", error.code.as_str(), error.message)),
                        });
                        "publish_error".to_owned()
                    }
                }
            }
        };
        work.save_item(&state)?;
        items.push(json!({
            "character": state.character,
            "unicode": state.unicode,
            "sha256": state.rendered_sha(),
            "outcome": outcome,
        }));
    }
    work.log(&format!(
        "review-apply {review_id}: одобрено {approved}, отклонено {rejected}, ошибок {failed}"
    ));
    Ok(json!({
        "schema_version": 1,
        "operation": "local-png review-apply",
        "outcome": if failed == 0 { "completed" } else { "completed_with_errors" },
        "review_id": review_id,
        "counts": {"rows": rows.len(), "approved": approved, "rejected": rejected, "failed": failed},
        "items": items,
    }))
}

/// Тот же путь хранилища, что у решения человека в пакетах: кандидат `pending`
/// с происхождением локального рендера, точная автоматическая проверка, затем
/// `Approve` по точному SHA. GIF символа не затрагивается.
fn publish_reviewed(
    store: &AssetStore,
    work: &WorkDir,
    state: &ItemState,
    slot: Option<AssetRecord>,
    review_id: &str,
    reason: &str,
) -> Result<(&'static str, Option<AssetRecord>), AssetError> {
    let render = state
        .render
        .as_ref()
        .ok_or_else(|| invalid("нет рендера для публикации"))?;
    let identity = state.identity();
    let validator_identity = KanjiImageValidator::validator_identity();
    let record = match slot {
        Some(record) if record.sha256 != render.sha256 => return Ok(("png_slot_occupied", None)),
        Some(record) if record.is_trusted_for(&validator_identity) => {
            return Ok(("already_published", Some(record)));
        }
        Some(record) => record,
        None => {
            let evidence: RenderEvidence =
                serde_json::from_slice(&work.read(&render.evidence_file)?)
                    .map_err(|error| invalid(format!("свидетельства рендера: {error}")))?;
            store
                .ingest_with_provenance(
                    IngestRequest {
                        identity: identity.clone(),
                        source_path: work.root().join(&render.file),
                        expected_source_sha256: Some(render.sha256.clone()),
                        domain_metadata: Some(local_metadata(state, &evidence)),
                        replace_expected_sha256: None,
                    },
                    Provenance {
                        source_kind: LOCAL_RENDER_SOURCE_KIND.into(),
                        source_name: SOURCE_NAME.into(),
                    },
                )?
                .asset
        }
    };
    if record.validation.is_none() {
        let report =
            store.validate_exact(&identity, &render.sha256, &KanjiImageValidator::new())?;
        if !report.blockers.is_empty() {
            return Err(AssetError::new(
                ErrorCode::ValidatorFailure,
                "точная автоматическая проверка завершилась техническим отказом",
            ));
        }
    }
    let outcome = store.attest(HumanAttestationRequest {
        identity: identity.clone(),
        expected_sha256: render.sha256.clone(),
        decision: HumanDecision::Approve,
        reason: format!("local-png review {review_id}: {reason}"),
    })?;
    if outcome.asset.sha256 != render.sha256
        || outcome.asset.format != DetectedFormat::Png
        || outcome.asset.lifecycle != LifecycleState::Verified
        || !outcome.asset.is_trusted_for(&validator_identity)
    {
        return Err(invalid(
            "после решения человека хранилище не подтверждает доверие к точным байтам",
        ));
    }
    Ok(("published_after_review", Some(outcome.asset)))
}

/// Итоговое состояние символа по его этапам и финальному хранилищу.
pub(crate) fn item_outcome(state: &ItemState, after: &Inventory) -> &'static str {
    if let Some(error) = &state.render_error
        && state.render.is_none()
    {
        return if error.code == "glyph_missing" {
            "glyph_missing"
        } else {
            "render_error"
        };
    }
    let Some(render) = &state.render else {
        return "not_rendered";
    };
    let in_store = after
        .png
        .get(&state.character)
        .is_some_and(|(sha256, kind)| sha256 == &render.sha256 && kind == LOCAL_RENDER_SOURCE_KIND);
    if in_store {
        return match state.review_current() {
            Some(review) if review.decision == ReviewDecision::Approve => "published_after_review",
            _ => "published",
        };
    }
    if state
        .review_current()
        .is_some_and(|review| review.decision == ReviewDecision::Reject)
    {
        return "review_rejected";
    }
    if after
        .png
        .get(&state.character)
        .is_some_and(|(_, kind)| kind != LOCAL_RENDER_SOURCE_KIND)
        && state.origins.contains(&TargetOrigin::ExistingPng)
    {
        return "existing_original_kept";
    }
    match state
        .validation_current()
        .map(|validation| validation.status)
    {
        None => return "awaiting_validation",
        Some(SemanticStatus::Rejected) => return "validator_rejected",
        Some(SemanticStatus::Uncertain) => return "validator_uncertain",
        Some(SemanticStatus::Corrupt) => return "validator_corrupt",
        Some(SemanticStatus::Verified) => {}
    }
    match state.ocr_current() {
        None => "awaiting_ocr",
        Some(ocr) if ocr.failure.is_some() => "ocr_failed",
        Some(ocr) if ocr.observed.as_deref() == Some("?") => "ocr_unrecognized",
        Some(ocr) if !ocr.matches_expected => "ocr_mismatch",
        Some(_) => match state.publication.as_ref().map(|p| p.outcome.as_str()) {
            Some("png_slot_occupied") => "png_slot_occupied",
            Some("publish_error") => "publish_error",
            Some(outcome) if outcome.starts_with("store_validator_") => "store_validator_refused",
            _ => "awaiting_publication",
        },
    }
}

pub(crate) fn outcome_counts(items: &[ItemState], after: &Inventory) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for item in items {
        *counts
            .entry(item_outcome(item, after).to_owned())
            .or_insert(0) += 1;
    }
    counts
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Разница двух PNG, усиленная ×4: только для визуальной диагностики.
fn diff_png(reference: &[u8], generated: &[u8]) -> Result<Vec<u8>, AssetError> {
    let decode = |bytes: &[u8]| {
        image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
            .map(|image| image.to_rgb8())
            .map_err(|error| invalid(format!("декодирование PNG: {error}")))
    };
    let (left, right) = (decode(reference)?, decode(generated)?);
    if left.dimensions() != right.dimensions() {
        return Err(invalid("размеры PNG различаются"));
    }
    let diff = image::RgbImage::from_fn(left.width(), left.height(), |x, y| {
        let (a, b) = (left.get_pixel(x, y), right.get_pixel(x, y));
        image::Rgb(std::array::from_fn(|channel| {
            a[channel].abs_diff(b[channel]).saturating_mul(4)
        }))
    });
    let mut bytes = std::io::Cursor::new(Vec::new());
    diff.write_to(&mut bytes, image::ImageFormat::Png)
        .map_err(|error| invalid(format!("кодирование PNG: {error}")))?;
    Ok(bytes.into_inner())
}

/// Проверяет, что PNG — нормальное изображение 174×174.
fn png_is_valid(bytes: &[u8]) -> bool {
    DetectedFormat::from_signature(bytes) == DetectedFormat::Png
        && image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
            .is_ok_and(|image| image.width() == 174 && image.height() == 174)
}

/// Итог чтения опубликованных вариантов одного символа в отчёте.
#[derive(Clone, Default)]
struct PublishedReadCheck {
    png_ok: bool,
    gif_read: bool,
    gif_ok: bool,
    error: Option<String>,
}

fn describe_error(error: &AssetError) -> String {
    format!("{}: {}", error.code.as_str(), error.message)
}

/// Отчёт по рабочему каталогу и финальному состоянию хранилища.
///
/// Возвращает сводку и признак успешной проверки целостности.
pub(crate) fn report(store: &AssetStore, root: &Path) -> Result<(Value, bool), AssetError> {
    let work = WorkDir::open(root)?;
    let items = work.all_items()?;
    let before: Inventory = serde_json::from_slice(&work.read("inventory-before.json")?)
        .map_err(|error| invalid(format!("inventory-before.json: {error}")))?;
    let (after, _) = inventory(store)?;
    let validator = KanjiImageValidator::validator_identity();
    let report_dir = work.root().join("report");
    std::fs::create_dir_all(report_dir.join("diff"))
        .map_err(|error| AssetError::io("создание каталога отчёта", error))?;

    // Неизменность каждого исходного GIF по SHA из инвентаря до запуска.
    let mut gif_changed = Vec::new();
    for (key, sha256) in &before.gif_sha256 {
        if after.gif_sha256.get(key) != Some(sha256) {
            gif_changed.push(json!({
                "character": key,
                "before": sha256,
                "after": after.gif_sha256.get(key),
            }));
        }
    }
    // Исторические PNG не заменены.
    let mut original_png_changed = Vec::new();
    for (key, (sha256, kind)) in &before.png {
        if kind != LOCAL_RENDER_SOURCE_KIND
            && after.png.get(key).map(|(sha, _)| sha) != Some(sha256)
        {
            original_png_changed.push(json!({"character": key, "before": sha256}));
        }
    }
    let journal = store.journal_check()?;

    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut issues = Vec::new();
    let mut published = Vec::new();
    let mut controls = Vec::new();
    let mut read_failures = Vec::new();
    let mut ocr_stats: BTreeMap<&str, usize> = BTreeMap::new();
    let mut validator_stats: BTreeMap<String, usize> = BTreeMap::new();
    let mut read_requests = Vec::new();
    let mut read_owners = Vec::new();
    for item in &items {
        let outcome = item_outcome(item, &after);
        *counts.entry(outcome.to_owned()).or_insert(0) += 1;
        if let Some(validation) = item.validation_current() {
            *validator_stats
                .entry(validation.status.as_str().to_owned())
                .or_insert(0) += 1;
        }
        if let Some(ocr) = item.ocr_current() {
            let key = if ocr.failure.is_some() {
                "failed"
            } else if ocr.observed.as_deref() == Some("?") {
                "unrecognized"
            } else if ocr.matches_expected {
                "matched"
            } else {
                "mismatch"
            };
            *ocr_stats.entry(key).or_insert(0) += 1;
        } else if item.render.is_some() {
            *ocr_stats.entry("not_run").or_insert(0) += 1;
        }
        if matches!(outcome, "published" | "published_after_review") {
            let index = published.len();
            read_requests.push((item.identity(), DetectedFormat::Png));
            read_owners.push(index);
            if after.gif_sha256.contains_key(&item.character) {
                read_requests.push((item.identity(), DetectedFormat::Gif));
                read_owners.push(index);
            }
            published.push(item);
        }
        if let Some(control) = &item.control {
            let render = item.render.as_ref().expect("контроль требует рендера");
            let diff_file = format!("diff/{}.png", item.unicode);
            let diff = diff_png(
                &work.read(&control.reference_file)?,
                &work.read(&render.file)?,
            )?;
            write_atomic(&report_dir.join(&diff_file), &diff)?;
            controls.push((item, control, diff_file));
        }
        let problem = !matches!(
            outcome,
            "published" | "published_after_review" | "existing_original_kept"
        );
        if problem {
            issues.push(json!({
                "character": item.character,
                "unicode": item.unicode,
                "outcome": outcome,
                "expected": item.character,
                "observed": item.ocr_current().and_then(|ocr| ocr.observed.clone()),
                "ocr_failure": item.ocr_current().and_then(|ocr| ocr.failure.clone()),
                "sha256": item.rendered_sha(),
                "validator_status": item.validation_current().map(|v| v.status.as_str()),
                "validator_summary": item.validation_current()
                    .and_then(|v| v.evidence.first().map(|e| e.summary.clone())),
                "render_error": item.render_error,
                "publication": item.publication,
                "artifact": item.render.as_ref().map(|render| render.file.clone()),
                "state_file": format!("items/{}.json", item.unicode),
            }));
        }
    }

    // Все опубликованные варианты читаются одним согласованным снимком: манифест и
    // корпус проверяются один раз, байты каждого запроса — по своей записи.
    let mut checks = vec![PublishedReadCheck::default(); published.len()];
    let snapshot = AssetStore::visit_verified_variants_with_policy(
        store.root(),
        &read_requests,
        &validator,
        &KanjiDomainPolicy,
        |request, read| {
            let item = published[read_owners[request]];
            let check = &mut checks[read_owners[request]];
            let read = match read {
                Ok(read) => read,
                Err(error) => {
                    check.error.get_or_insert_with(|| describe_error(&error));
                    return Ok(());
                }
            };
            match read_requests[request].1 {
                DetectedFormat::Png => {
                    let render = item.render.as_ref().expect("опубликован рендер");
                    check.png_ok = read.record.sha256 == render.sha256
                        && read.record.consumer_filename == format!("{}.png", item.character)
                        && png_is_valid(&read.bytes);
                }
                _ => {
                    check.gif_read = true;
                    check.gif_ok = Some(&read.record.sha256)
                        == before.gif_sha256.get(&item.character)
                        && read.record.consumer_filename == format!("{}.gif", item.character);
                }
            }
            Ok(())
        },
    );
    if let Err(error) = snapshot {
        let error = describe_error(&error);
        for check in &mut checks {
            check.error = Some(error.clone());
        }
    }
    let mut dual_read_checked = 0usize;
    for (item, check) in published.iter().zip(&checks) {
        if let Some(error) = &check.error {
            read_failures.push(json!({"character": item.character, "error": error}));
            continue;
        }
        let gif_ok = !check.gif_read || check.gif_ok;
        if check.gif_read {
            dual_read_checked += 1;
        }
        if !check.png_ok || !gif_ok {
            read_failures.push(
                json!({"character": item.character, "png_ok": check.png_ok, "gif_ok": gif_ok}),
            );
        }
    }

    let mean = |selector: fn(&crate::local_render::ImageComparison) -> f64| {
        if controls.is_empty() {
            0.0
        } else {
            controls
                .iter()
                .map(|(_, control, _)| selector(&control.metrics))
                .sum::<f64>()
                / controls.len() as f64
        }
    };
    let control_mean = json!({
        "count": controls.len(),
        "mae": mean(|m| m.mae),
        "pixel_similarity": mean(|m| m.pixel_similarity),
        "glyph_iou": mean(|m| m.glyph_iou),
        "exact_pixel_fraction": mean(|m| m.exact_pixel_fraction),
    });
    let healthy = gif_changed.is_empty()
        && original_png_changed.is_empty()
        && read_failures.is_empty()
        && journal.is_clean();
    let ocr_model: Option<Value> = work
        .read("ocr-model.json")
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let run_config: Option<Value> = work
        .read("run.json")
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let summary = json!({
        "schema_version": 1,
        "operation": "local-png report",
        "outcome": if healthy { "completed" } else { "integrity_problems" },
        "work_dir": work.root().display().to_string(),
        "items": items.len(),
        "counts": counts,
        "validator": validator_stats,
        "ocr": ocr_stats,
        "ocr_model": ocr_model.as_ref().and_then(|model| model.get("summary").cloned()),
        "controls": control_mean,
        "integrity": {
            "healthy": healthy,
            "store_verify_integrity": true,
            "journal_clean": journal.is_clean(),
            "gif_before": before.gif_sha256.len(),
            "gif_after": after.gif_sha256.len(),
            "gif_changed": gif_changed,
            "original_png_changed": original_png_changed,
            "published_read_failures": read_failures,
            "published_with_gif_read_together": dual_read_checked,
        },
        "store_before": {
            "records": before.records_total,
            "gif": before.verified_gif_records,
            "trusted_gif_characters": before.trusted_gif_characters,
            "png": before.png_records,
            "png_local_render": before.png_local_render_records,
            "gif_and_png": before.characters_with_gif_and_png,
        },
        "store_after": {
            "records": after.records_total,
            "gif": after.verified_gif_records,
            "trusted_gif_characters": after.trusted_gif_characters,
            "png": after.png_records,
            "png_local_render": after.png_local_render_records,
            "png_other": after.png_other_records,
            "gif_and_png": after.characters_with_gif_and_png,
            "runtime_records": after.runtime_records,
        },
        "run": run_config,
    });
    write_atomic(
        &report_dir.join("summary.json"),
        &serde_json::to_vec_pretty(&summary).expect("сводка сериализуется"),
    )?;
    write_atomic(
        &report_dir.join("issues.json"),
        &serde_json::to_vec_pretty(&issues).expect("реестр сериализуется"),
    )?;
    let html = report_html(&summary, &controls, &published, &issues);
    write_atomic(&report_dir.join("index.html"), html.as_bytes())?;
    Ok((
        json!({
            "schema_version": 1,
            "operation": "local-png report",
            "outcome": summary["outcome"],
            "report": report_dir.join("index.html").display().to_string(),
            "counts": summary["counts"],
            "integrity": summary["integrity"]["healthy"],
        }),
        healthy,
    ))
}

fn zoom_box(src: &str, x: u32, y: u32) -> String {
    // Фрагмент 48×48 px исходника при увеличении ×4 без сглаживания.
    format!(
        "<div class=zoom style=\"background-image:url('{src}');background-position:-{}px -{}px\"></div>",
        x * 4,
        y * 4
    )
}

fn report_html(
    summary: &Value,
    controls: &[(&ItemState, &ControlState, String)],
    published: &[&ItemState],
    issues: &[Value],
) -> String {
    let mut html = String::from(
        "<!doctype html><html lang=ru><meta charset=utf-8><meta name=viewport content='width=device-width,initial-scale=1'>\
         <title>Локальные PNG кандзи</title><style>\
         :root{--bg:#f6f6f4;--fg:#1d1d1b;--muted:#6a6a66;--card:#fff;--line:#d9d9d4;--bad:#a8321e;--ok:#2d6a3e}\
         @media (prefers-color-scheme:dark){:root{--bg:#161615;--fg:#ececea;--muted:#9a9a95;--card:#20201f;--line:#3a3a37;--bad:#e27a63;--ok:#7cc48f}}\
         body{margin:0;background:var(--bg);color:var(--fg);font:15px/1.5 system-ui,sans-serif}\
         main{max-width:1200px;margin:0 auto;padding:16px}h1{font-size:22px}h2{font-size:18px;margin-top:32px}\
         table{border-collapse:collapse;width:100%;font-size:13px}td,th{border-bottom:1px solid var(--line);padding:4px 6px;text-align:left;vertical-align:middle}\
         .grid{display:flex;flex-wrap:wrap;gap:12px}.card{background:var(--card);border:1px solid var(--line);border-radius:6px;padding:8px}\
         .card img{display:block;width:174px;height:174px;image-rendering:pixelated}\
         .zoom{width:192px;height:192px;background-size:696px 696px;image-rendering:pixelated;border:1px solid var(--line)}\
         .row{display:flex;gap:8px;flex-wrap:wrap;align-items:flex-start}.muted{color:var(--muted)}.bad{color:var(--bad)}.ok{color:var(--ok)}\
         pre{white-space:pre-wrap;font-size:12px;background:var(--card);border:1px solid var(--line);padding:8px;overflow:auto}\
         .tbl{overflow-x:auto}</style><main>",
    );
    html.push_str("<h1>Локальная генерация PNG кандзи</h1>");
    let integrity = &summary["integrity"];
    let healthy = integrity["healthy"].as_bool().unwrap_or(false);
    html.push_str(&format!(
        "<p class={}>Целостность: {}. Журнал согласован: {}. GIF до/после: {}/{}; изменённых GIF: {}; изменённых исторических PNG: {}; ошибок чтения опубликованных: {}.</p>",
        if healthy { "ok" } else { "bad" },
        if healthy { "проверена" } else { "есть проблемы" },
        integrity["journal_clean"],
        integrity["gif_before"],
        integrity["gif_after"],
        integrity["gif_changed"].as_array().map_or(0, Vec::len),
        integrity["original_png_changed"].as_array().map_or(0, Vec::len),
        integrity["published_read_failures"].as_array().map_or(0, Vec::len),
    ));
    html.push_str("<h2>Итоги</h2><pre>");
    html.push_str(&html_escape(
        &serde_json::to_string_pretty(&json!({
            "counts": summary["counts"],
            "validator": summary["validator"],
            "ocr": summary["ocr"],
            "ocr_model": summary["ocr_model"],
            "store_before": summary["store_before"],
            "store_after": summary["store_after"],
            "controls_mean": summary["controls"],
        }))
        .unwrap_or_default(),
    ));
    html.push_str("</pre>");

    html.push_str("<h2>Контроль: исторические PNG и новый Rust-рендер</h2>\
        <p class=muted>Слева оригинал из хранилища, затем рендер, разница ×4 и увеличенный ×4 фрагмент 48×48 (верхний левый угол рамки и центр глифа). Геометрия — общий профиль из настроек прогона; таблицы «символ → фаза» нет.</p>");
    for (item, control, diff_file) in controls {
        let render = item.render.as_ref().expect("контроль требует рендера");
        let reference = format!("../{}", control.reference_file);
        let generated = format!("../{}", render.file);
        html.push_str(&format!(
            "<div class=card><b>{} {}</b> <span class=muted>IoU {:.4}%, similarity {:.4}%, MAE {:.4}, точные пиксели {:.4}%</span>\
             <div class=row><img src='{}' alt=оригинал><img src='{}' alt=рендер><img src='{}' alt=разница>{}{}{}{}</div></div>",
            html_escape(&item.character),
            item.unicode,
            control.metrics.glyph_iou * 100.0,
            control.metrics.pixel_similarity * 100.0,
            control.metrics.mae,
            control.metrics.exact_pixel_fraction * 100.0,
            html_escape(&reference),
            html_escape(&generated),
            html_escape(diff_file),
            zoom_box(&html_escape(&reference), 0, 0),
            zoom_box(&html_escape(&generated), 0, 0),
            zoom_box(&html_escape(&reference), 63, 63),
            zoom_box(&html_escape(&generated), 63, 63),
        ));
    }

    let mut by_area: Vec<&&ItemState> = published.iter().collect();
    by_area.sort_by_key(|item| item.render.as_ref().map_or(0, |render| render.glyph_area));
    let sample = |title: &str, items: &[&&ItemState], html: &mut String| {
        html.push_str(&format!("<h2>{title}</h2><div class=grid>"));
        for item in items {
            let render = item.render.as_ref().expect("опубликован рендер");
            html.push_str(&format!(
                "<div class=card><img src='../{}' alt='{}'><div>{} {} · OCR {}</div><div class=muted>{}</div></div>",
                html_escape(&render.file),
                html_escape(&item.character),
                html_escape(&item.character),
                item.unicode,
                html_escape(item.ocr_current().and_then(|ocr| ocr.observed.as_deref()).unwrap_or("—")),
                &render.sha256[..16],
            ));
        }
        html.push_str("</div>");
    };
    let count = by_area.len().min(12);
    sample(
        "Простые опубликованные (меньшая площадь глифа)",
        &by_area[..count],
        &mut html,
    );
    sample(
        "Сложные опубликованные (большая площадь глифа)",
        &by_area[by_area.len() - count..],
        &mut html,
    );

    html.push_str(&format!(
        "<h2>Реестр проблем ({})</h2><p class=muted>Полный машиночитаемый список — issues.json. Такие кандидаты не публиковались.</p><div class=tbl><table><tr><th>Символ</th><th>Unicode</th><th>Исход</th><th>Ожидался</th><th>OCR</th><th>Валидатор</th><th>SHA-256</th><th>Артефакт</th></tr>",
        issues.len()
    ));
    for issue in issues {
        let artifact = issue["artifact"].as_str().unwrap_or("");
        html.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td class=bad>{}</td><td>{}</td><td>{}</td><td>{}</td><td><code>{}</code></td><td>{}</td></tr>",
            html_escape(issue["character"].as_str().unwrap_or("")),
            html_escape(issue["unicode"].as_str().unwrap_or("")),
            html_escape(issue["outcome"].as_str().unwrap_or("")),
            html_escape(issue["expected"].as_str().unwrap_or("")),
            html_escape(
                issue["observed"]
                    .as_str()
                    .or_else(|| issue["ocr_failure"].as_str())
                    .unwrap_or("—")
            ),
            html_escape(issue["validator_status"].as_str().unwrap_or("—")),
            html_escape(issue["sha256"].as_str().map_or("—", |sha| &sha[..16.min(sha.len())])),
            if artifact.is_empty() {
                "—".to_owned()
            } else {
                format!("<a href='../{0}'>{0}</a>", html_escape(artifact))
            },
        ));
    }
    html.push_str("</table></div></main></html>\n");
    html
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inventory_with(png: &[(&str, &str, &str)]) -> Inventory {
        Inventory {
            captured_at_unix: 0,
            store_id: "store".into(),
            records_total: png.len(),
            verified_gif_records: 0,
            trusted_gif_characters: 0,
            untrusted_gif_records: 0,
            png_records: png.len(),
            png_local_render_records: 0,
            png_other_records: 0,
            characters_with_gif_and_png: 0,
            runtime_records: 0,
            gif_sha256: BTreeMap::new(),
            png: png
                .iter()
                .map(|(character, sha256, kind)| {
                    ((*character).into(), ((*sha256).into(), (*kind).into()))
                })
                .collect(),
        }
    }

    fn rendered(character: &str, sha256: &str) -> ItemState {
        let mut state = ItemState::new(character);
        state.origins.insert(TargetOrigin::CorpusGif);
        state.render = Some(RenderState {
            sha256: sha256.into(),
            raw_sha256: "raw".into(),
            byte_length: 1,
            file: "png/U+4E00.png".into(),
            raw_file: "raw/U+4E00.png".into(),
            evidence_file: "evidence/U+4E00.json".into(),
            glyph_area: 1,
        });
        state
    }

    fn validated(mut state: ItemState, status: SemanticStatus) -> ItemState {
        state.validation = Some(ValidationState {
            sha256: state.render.as_ref().unwrap().sha256.clone(),
            status,
            validator: KanjiImageValidator::validator_identity(),
            evidence: Vec::new(),
        });
        state
    }

    fn recognized(mut state: ItemState, observed: &str) -> ItemState {
        state.ocr = Some(OcrState {
            sha256: state.render.as_ref().unwrap().sha256.clone(),
            observed: Some(observed.into()),
            matches_expected: observed == state.character,
            failure: None,
            prepared_sha256: None,
            model: "model".into(),
            provenance_sha256: "provenance".into(),
            elapsed_ms: None,
        });
        state
    }

    #[test]
    fn publication_requires_verified_validator_and_matching_blind_ocr() {
        let empty = inventory_with(&[]);
        let passing = recognized(
            validated(rendered("一", "a"), SemanticStatus::Verified),
            "一",
        );
        assert!(passing.passes_gates());
        assert_eq!(item_outcome(&passing, &empty), "awaiting_publication");

        let mismatch = recognized(
            validated(rendered("丞", "a"), SemanticStatus::Verified),
            "承",
        );
        assert!(!mismatch.passes_gates());
        assert_eq!(item_outcome(&mismatch, &empty), "ocr_mismatch");

        let unknown = recognized(
            validated(rendered("一", "a"), SemanticStatus::Verified),
            "?",
        );
        assert!(!unknown.passes_gates());
        assert_eq!(item_outcome(&unknown, &empty), "ocr_unrecognized");

        for (status, outcome) in [
            (SemanticStatus::Uncertain, "validator_uncertain"),
            (SemanticStatus::Rejected, "validator_rejected"),
        ] {
            let state = recognized(validated(rendered("一", "a"), status), "一");
            assert!(!state.passes_gates());
            assert_eq!(item_outcome(&state, &empty), outcome);
        }
    }

    #[test]
    fn decisions_for_other_bytes_do_not_count_after_rerender() {
        let mut state = recognized(
            validated(rendered("一", "a"), SemanticStatus::Verified),
            "一",
        );
        state.render.as_mut().unwrap().sha256 = "b".into();
        assert!(!state.passes_gates());
        assert_eq!(
            item_outcome(&state, &inventory_with(&[])),
            "awaiting_validation"
        );
    }

    #[test]
    fn only_local_render_with_the_same_sha_counts_as_published() {
        let state = recognized(
            validated(rendered("一", "a"), SemanticStatus::Verified),
            "一",
        );
        let published = inventory_with(&[("一", "a", LOCAL_RENDER_SOURCE_KIND)]);
        assert_eq!(item_outcome(&state, &published), "published");
        let other_bytes = inventory_with(&[("一", "b", LOCAL_RENDER_SOURCE_KIND)]);
        assert_eq!(item_outcome(&state, &other_bytes), "awaiting_publication");

        let mut control = state.clone();
        control.origins.insert(TargetOrigin::ExistingPng);
        let historical = inventory_with(&[("一", "a", "local-directory")]);
        assert_eq!(
            item_outcome(&control, &historical),
            "existing_original_kept"
        );
    }

    #[test]
    fn review_groups_cover_only_bytes_that_missed_the_automatic_gates() {
        let passing = recognized(
            validated(rendered("一", "a"), SemanticStatus::Verified),
            "一",
        );
        assert_eq!(passing.review_group(), None);
        let doubt = recognized(
            validated(rendered("一", "a"), SemanticStatus::Uncertain),
            "一",
        );
        assert_eq!(doubt.review_group(), Some("validator_doubt_ocr_match"));
        let mismatch = recognized(
            validated(rendered("丞", "a"), SemanticStatus::Verified),
            "承",
        );
        assert_eq!(mismatch.review_group(), Some("ocr_mismatch"));
        let corrupt = recognized(
            validated(rendered("一", "a"), SemanticStatus::Corrupt),
            "一",
        );
        assert_eq!(corrupt.review_group(), None);
        assert_eq!(
            validated(rendered("一", "a"), SemanticStatus::Uncertain).review_group(),
            None
        );
    }

    #[test]
    fn review_file_is_bound_to_its_exact_row_set() {
        let rows = vec![("U+4E00".to_owned(), "a".repeat(64))];
        let id = review_id_of(&rows);
        let line = format!("U+4E00\t一\t{}\tocr_mismatch\tverified\t二", "a".repeat(64));
        let file = |decision: &str, reason: &str| {
            format!("# review_id={id}\n{REVIEW_HEADER}\n{line}\t{decision}\t{reason}\n")
        };
        let parsed = parse_review(&file("approve", "глиф верный"), &id).unwrap();
        assert_eq!(parsed[0].decision, Some(ReviewDecision::Approve));
        assert!(
            parse_review(&file("", ""), &id).unwrap()[0]
                .decision
                .is_none()
        );
        assert!(parse_review(&file("approve", ""), &id).is_err());
        assert!(parse_review(&file("maybe", "x"), &id).is_err());
        let foreign = file("approve", "x").replace(&"a".repeat(64), &"b".repeat(64));
        assert!(parse_review(&foreign, &id).is_err());
    }

    #[test]
    fn missing_glyph_is_reported_without_an_image() {
        let mut state = ItemState::new("一");
        state.render_error = Some(StageError {
            code: "glyph_missing".into(),
            message: "нет глифа".into(),
            attempts: 1,
        });
        assert_eq!(item_outcome(&state, &inventory_with(&[])), "glyph_missing");
        assert_eq!(state.unicode, "U+4E00");
    }
}
