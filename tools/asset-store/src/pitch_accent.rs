//! Предметные правила и семантическая проверка pitch-accent PNG с отказом при неполных данных.

use std::io::{Cursor, Read};

use image::GenericImageView;
use image::ImageDecoder;
use serde::{Deserialize, Serialize};
use serde_json::json;
use url::Url;

use crate::browser_runtime::BrowserRuntimeProvenance;
use crate::domain::{
    AssetDomainPolicy, CanonicalAssetLocation, TrustSemantics, extension_for_format,
    validate_safe_consumer_filename,
};
use crate::error::{AssetError, ErrorCode};
use crate::hashing::sha256_hex;
use crate::model::{
    AssetRecord, DetectedFormat, SemanticDecision, SemanticStatus, ValidationEvidence,
    ValidatorIdentity,
};
use crate::validation::{SemanticValidator, ValidatorFailure};

/// Максимальный размер сжатого PNG одного браузерного снимка графика акцента.
/// Лимит оставляет запас для PNG-снимка элемента при масштабе 3×, сохраняя
/// конечную границу для недоверенного входного потока.
pub(crate) const PITCH_ACCENT_MAX_ASSET_BYTES: u64 = 8 * 1024 * 1024;

/// Геометрический допуск для согласования DOM, области снимка и сохранённого свидетельства.
pub(crate) const PITCH_ACCENT_CAPTURE_GEOMETRY_TOLERANCE_CSS_PX: f64 = 0.25;

/// Минимальное поле подлинного фона страницы вокруг отрисовки выбранного графика.
/// Область снимка расширяется на это значение и наружу до целых CSS-пикселей,
/// поэтому при DSF 3.0 каждое поле занимает от 9 до 11 физических пикселей.
pub const PITCH_ACCENT_CAPTURE_PADDING_CSS_PX: f64 = 3.0;

/// Верхняя граница числа вариантов графика у одной словарной записи.
pub(crate) const PITCH_ACCENT_MAX_VARIANT_COUNT: usize = 32;

/// Допуск сглаживания на границе отрисовки графика при проверке полей, в физических пикселях.
const PITCH_ACCENT_PAINT_EDGE_ANTIALIAS_PX: i64 = 1;

/// Доменная метка хеша, по которому выбирается вариант графика без приоритета источника.
const PITCH_ACCENT_VARIANT_SELECTION_DOMAIN: &str = "anki-decks/jpdb-pitch-variant-selection/v1";

/// Сравнивает чтение JPDB, считая хирагану и катакану эквивалентной записью.
/// Остальные символы сравниваются буквально; написание не нормализуется.
pub fn jpdb_readings_equivalent(left: &str, right: &str) -> bool {
    fn hiragana_equivalent(character: char) -> char {
        let code_point = u32::from(character);
        if matches!(code_point, 0x30A1..=0x30F6 | 0x30FD..=0x30FE) {
            char::from_u32(code_point - 0x60).unwrap_or(character)
        } else {
            character
        }
    }

    left.chars()
        .map(hiragana_equivalent)
        .eq(right.chars().map(hiragana_equivalent))
}

/// Область нативного снимка для отрисовки графика: поле `padding` с каждой стороны,
/// расширенное наружу до целых CSS-пикселей. Целочисленные границы при целом DSF
/// дают PNG точного размера без округления CDP.
pub(crate) fn pitch_accent_capture_rect_for(
    paint_rect: PitchAccentCaptureRect,
    padding: f64,
) -> PitchAccentCaptureRect {
    let left = (paint_rect.x - padding).floor();
    let top = (paint_rect.y - padding).floor();
    let right = (paint_rect.x + paint_rect.width + padding).ceil();
    let bottom = (paint_rect.y + paint_rect.height + padding).ceil();
    PitchAccentCaptureRect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    }
}

/// Оценка варианта для выбора без приоритета источника: SHA-256 от доменной метки,
/// ID словарной записи и SHA-256 разметки графика. Оценка не зависит от порядка
/// графиков на странице, запуска, параллельности или возобновления пакета.
pub fn pitch_accent_variant_selection_score(vocabulary_id: u64, markup_sha256: &str) -> String {
    sha256_hex(
        format!("{PITCH_ACCENT_VARIANT_SELECTION_DOMAIN}\0{vocabulary_id}\0{markup_sha256}")
            .as_bytes(),
    )
}

/// Выбирает индекс варианта с наименьшей оценкой; при одинаковой разметке (и потому
/// одинаковой оценке) — наименьший индекс DOM. `None` для пустого списка.
pub fn select_pitch_accent_variant<'a>(
    vocabulary_id: u64,
    markup_sha256: impl IntoIterator<Item = &'a str>,
) -> Option<usize> {
    markup_sha256
        .into_iter()
        .enumerate()
        .map(|(index, markup)| {
            (
                pitch_accent_variant_selection_score(vocabulary_id, markup),
                index,
            )
        })
        .min()
        .map(|(_, index)| index)
}

/// Верхняя граница ширины и высоты PNG pitch-accent.
pub const PITCH_ACCENT_MAX_IMAGE_DIMENSION: u32 = 4096;

/// Предельный объём памяти для декодирования PNG pitch-accent.
pub const PITCH_ACCENT_MAX_DECODE_ALLOCATION_BYTES: u64 = 48 * 1024 * 1024;

const PITCH_ACCENT_BACKGROUND_RGB_TOLERANCE: u8 = 8;
const PITCH_ACCENT_GRAPH_PIXEL_MIN_RGB_DIFFERENCE: u8 = 16;
const PITCH_ACCENT_MIN_CONTRASTING_GRAPH_PIXELS: usize = 8;

/// Независимая политика публикации канонического PNG pitch-accent.
#[derive(Debug, Clone, Copy, Default)]
pub struct PitchAccentDomainPolicy;

impl AssetDomainPolicy for PitchAccentDomainPolicy {
    fn domain_id(&self) -> &'static str {
        "pitch_accent"
    }

    fn validate_identity(&self, identity: &crate::model::AssetIdentity) -> Result<(), AssetError> {
        identity
            .validate()
            .map_err(|message| AssetError::new(ErrorCode::InvalidIdentity, message))?;
        if identity.namespace != "pitch_accent" {
            return Err(AssetError::new(
                ErrorCode::InvalidIdentity,
                "домен pitch-accent принимает только пространство имён `pitch_accent`",
            ));
        }
        let filename = format!("{}.pitch.png", identity.key);
        validate_safe_consumer_filename(&filename, DetectedFormat::Png)?;
        Ok(())
    }

    fn canonical_location(
        &self,
        identity: &crate::model::AssetIdentity,
        sha256: &str,
        format: DetectedFormat,
    ) -> Result<CanonicalAssetLocation, AssetError> {
        self.validate_identity(identity)?;
        validate_hash(sha256)?;
        let extension = extension_for_format(format);
        let (category, storage_filename) = if self.is_publishable_format(format) {
            ("png", format!("{}.png", identity.key))
        } else {
            let identity_hash = sha256_hex(identity.key.as_bytes());
            (
                "unsupported",
                format!("{identity_hash}-{sha256}.{extension}"),
            )
        };
        let consumer_filename = if format == DetectedFormat::Png {
            format!("{}.pitch.png", identity.key)
        } else {
            format!("{}.{extension}", identity.key)
        };
        validate_safe_consumer_filename(&consumer_filename, format)?;
        Ok(CanonicalAssetLocation {
            storage_path: format!("assets/{category}/{storage_filename}"),
            consumer_filename,
        })
    }

    fn proves_legacy_consumer_filename(
        &self,
        identity: &crate::model::AssetIdentity,
        filename: &str,
    ) -> bool {
        self.validate_identity(identity).is_ok()
            && filename == format!("{}.png", identity.key)
            && validate_safe_consumer_filename(filename, DetectedFormat::Png).is_ok()
    }

    fn legacy_location(
        &self,
        _identity: &crate::model::AssetIdentity,
        _sha256: &str,
        _format: DetectedFormat,
    ) -> Option<CanonicalAssetLocation> {
        // Домен `pitch_accent` отсутствовал в схемах 3/4.
        None
    }

    fn is_publishable_format(&self, format: DetectedFormat) -> bool {
        format == DetectedFormat::Png
    }

    fn allows_verified_format(&self, format: DetectedFormat) -> bool {
        format == DetectedFormat::Png
    }

    fn max_asset_bytes(&self) -> Option<u64> {
        Some(PITCH_ACCENT_MAX_ASSET_BYTES)
    }

    fn content_addressed_storage(&self) -> bool {
        false
    }

    /// Доверие pitch-ресурсу даёт только текущее автоматическое решение
    /// ожидаемого валидатора: подтверждение человека само по себе не делает
    /// отрисовку пригодной для карточки.
    fn trust_semantics(&self) -> TrustSemantics {
        TrustSemantics::AUTOMATED_VERIFIED_ONLY
    }
}

/// Метаданные словарной записи JPDB, не участвующие в имени канонического файла.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitchAccentDomainMetadata {
    /// Точное значение `surface`; оно должно совпадать с `AssetIdentity.key`.
    pub surface: String,
    /// Значение `reading` для карточки и сведений об источнике; оно не входит в имя файла.
    pub reading: String,
    /// Положительный идентификатор словарной записи JPDB.
    pub jpdb_vocabulary_id: u64,
    /// Структурные сведения об источнике, захвате и среде браузера.
    pub evidence: PitchAccentEvidence,
}

/// Одна реально наблюдённая JPDB пара написания и чтения.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitchAccentResolvedForm {
    pub surface: String,
    pub reading: String,
}

/// Минимальные структурные свидетельства, достаточные для автоматической проверки.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitchAccentEvidence {
    pub provider: PitchAccentProvider,
    pub source_url: String,
    /// Связанные пары написания и чтения, наблюдённые в словарной записи JPDB.
    pub resolved_forms: Vec<PitchAccentResolvedForm>,
    /// Все варианты графика выбранной словарной записи и основание выбора одного из них.
    pub variant_selection: PitchAccentVariantSelection,
    pub render: PitchAccentRenderEvidence,
    pub browser: BrowserRuntimeProvenance,
}

/// Основание выбора одного варианта графика внутри уже подтверждённой словарной записи.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PitchAccentVariantSelectionBasis {
    /// JPDB не сообщает распространённость отдельных вариантов ударения: выбран вариант
    /// с наименьшей оценкой `pitch_accent_variant_selection_score`.
    SeededHashWithoutSourcePriority,
}

/// Обнаруженные варианты графика и выбранный для PNG вариант.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitchAccentVariantSelection {
    pub basis: PitchAccentVariantSelectionBasis,
    /// Число вариантов графика, обнаруженных у словарной записи; в PNG входит один.
    pub detected_variant_count: u32,
    /// Индекс выбранного варианта в порядке DOM.
    pub selected_index: u32,
    /// Все обнаруженные варианты в порядке DOM.
    pub variants: Vec<PitchAccentVariantEvidence>,
}

/// Один вариант графика на странице словарной записи.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitchAccentVariantEvidence {
    /// Нулевой индекс в порядке графиков страницы.
    pub index: u32,
    /// CSS-селектор конкретного узла графика.
    pub selector: String,
    /// Наблюдаемый текст графика без пробелов.
    pub text: String,
    /// SHA-256 сериализованной разметки узла графика (`outerHTML`).
    pub markup_sha256: String,
    /// Оценка выбора, вычисленная из ID записи и `markup_sha256`.
    pub selection_score: String,
    /// Прямоугольник узла графика относительно области просмотра.
    pub viewport_rect: PitchAccentCaptureRect,
    /// Прямоугольник узла графика относительно начала документа.
    pub document_rect: PitchAccentCaptureRect,
    /// Объединение прямоугольников узла и всех его потомков в координатах документа:
    /// сегменты графика JPDB с отрицательным полем выходят за прямоугольник узла.
    pub paint_rect: PitchAccentCaptureRect,
}

/// Источник получения изображения для этого домена.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PitchAccentProvider {
    Jpdb,
}

/// Способ получения канонического PNG.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PitchAccentRenderKind {
    ElementScreenshot,
    SvgElementScreenshot,
    CanvasElementScreenshot,
    BrowserRegionScreenshot,
}

/// Состояние документа и цветовая схема браузера, наблюдённые перед снимком.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitchAccentDarkThemeProof {
    /// Токены `classList` элемента `document.documentElement`.
    pub document_element_classes: Vec<String>,
    /// Наблюдаемый результат `matchMedia('(prefers-color-scheme: dark)').matches`.
    pub prefers_color_scheme: String,
    /// Наблюдаемое `getComputedStyle(document.documentElement).colorScheme`.
    pub computed_color_scheme: String,
    /// CSS-селектор реально наблюдённого сплошного фона области pitch accent или её предка.
    pub background_selector: String,
    /// RGB вычисленного фона; один пиксель должен совпасть с допуском ±8 на каждый канал.
    pub background_rgb: [u8; 3],
}

/// Прямоугольник в CSS-пикселях; систему координат задаёт поле, в котором он хранится.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitchAccentCaptureRect {
    /// Координата X в CSS-пикселях.
    pub x: f64,
    /// Координата Y в CSS-пикселях.
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Координатная система снимка, зафиксированная в свидетельствах.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PitchAccentCoordinateSpace {
    /// Координаты относительно начала документа, а не текущей области просмотра.
    Document,
}

/// Фактические параметры отрисованной области.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitchAccentRenderEvidence {
    pub kind: PitchAccentRenderKind,
    /// Селектор выбранного узла графика, снятого нативным снимком.
    pub selector: String,
    /// Число графиков в PNG; допустимо только значение 1.
    pub captured_graph_count: u32,
    /// Система координат прямоугольников вариантов и итоговой области снимка.
    pub coordinate_space: PitchAccentCoordinateSpace,
    pub viewport_width: u32,
    pub viewport_height: u32,
    pub document_width: u32,
    pub document_height: u32,
    /// Наблюдённые `window.scrollX` и `window.scrollY` в CSS-пикселях.
    pub scroll_x: f64,
    pub scroll_y: f64,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub device_scale_factor: f64,
    /// `visualViewport.scale`; значение должно быть ровно 1.0.
    pub page_scale_factor: f64,
    pub dark_theme: PitchAccentDarkThemeProof,
    /// Минимальное поле вокруг `paint_rect` выбранного варианта в CSS-пикселях.
    pub padding_css_px: f64,
    /// Итоговая расширенная область в координатах документа, переданная в CDP `clip`.
    pub capture_rect: PitchAccentCaptureRect,
}

/// Семантический валидатор PNG pitch-accent.
///
/// PNG декодируется полностью, а `VERIFIED` возвращается только при наличии
/// структурных свидетельств об источнике JPDB и отрисовке. Одной сигнатуры или
/// успешного декодирования изображения недостаточно.
#[derive(Debug, Clone, Copy, Default)]
pub struct PitchAccentImageValidator;

impl PitchAccentImageValidator {
    pub const VALIDATOR_ID: &'static str = "jpdb-pitch-accent-render";
    pub const VALIDATOR_VERSION: &'static str = "6";
    pub const REQUIRED_DEVICE_SCALE_FACTOR: f64 = 3.0;

    /// Устойчивый идентификатор валидатора для манифеста и проверок потребителя.
    pub fn validator_identity() -> ValidatorIdentity {
        Self.identity()
    }
}

impl SemanticValidator for PitchAccentImageValidator {
    fn identity(&self) -> ValidatorIdentity {
        ValidatorIdentity::new(Self::VALIDATOR_ID, Self::VALIDATOR_VERSION)
            .expect("статический идентификатор валидатора pitch-accent корректен")
    }

    fn validate(
        &self,
        asset: &AssetRecord,
        bytes: &mut dyn Read,
    ) -> Result<SemanticDecision, ValidatorFailure> {
        let mut contents = Vec::new();
        bytes
            .take(PITCH_ACCENT_MAX_ASSET_BYTES + 1)
            .read_to_end(&mut contents)
            .map_err(|error| {
                ValidatorFailure::new(
                    "read_failed",
                    format!("ошибка чтения входных данных: {error}"),
                )
            })?;
        if contents.len() as u64 > PITCH_ACCENT_MAX_ASSET_BYTES {
            return Ok(decision(
                SemanticStatus::Corrupt,
                "pitch_accent_png_too_large",
                "размер PNG превышает установленный предел",
                json!({"max_bytes": PITCH_ACCENT_MAX_ASSET_BYTES}),
            ));
        }

        if asset.format != DetectedFormat::Png
            || DetectedFormat::from_signature(&contents) != DetectedFormat::Png
        {
            return Ok(decision(
                SemanticStatus::Corrupt,
                "pitch_accent_png_required",
                "канонический файл домена pitch-accent должен быть PNG",
                json!({"format": format!("{:?}", asset.format)}),
            ));
        }
        let decoded_png = match decode_png(&contents) {
            Ok(image) => image,
            Err(error) => {
                return Ok(decision(
                    SemanticStatus::Corrupt,
                    "pitch_accent_png_decode_failed",
                    "PNG не прошёл полное декодирование",
                    json!({"message": error}),
                ));
            }
        };

        let domain = PitchAccentDomainPolicy;
        if let Err(error) = domain.validate_identity(&asset.identity) {
            return Ok(decision(
                SemanticStatus::Rejected,
                "pitch_accent_identity_mismatch",
                "идентификатор не принадлежит домену pitch-accent",
                json!({"message": error.message}),
            ));
        }

        let Some(value) = asset.domain_metadata.clone() else {
            return Ok(incomplete_evidence("поле domain_metadata отсутствует"));
        };
        let metadata: PitchAccentDomainMetadata = match serde_json::from_value(value) {
            Ok(metadata) => metadata,
            Err(error) => return Ok(incomplete_evidence(error.to_string())),
        };
        match validate_evidence(&asset.identity.key, &metadata, &decoded_png) {
            Ok(()) => Ok(decision(
                SemanticStatus::Verified,
                "pitch_accent_source_render_verified",
                "PNG полностью декодирован, структурные данные рендеринга JPDB согласованы",
                serde_json::to_value(metadata).unwrap_or_else(|_| json!({})),
            )),
            Err(EvidenceFailure::Contradiction(message)) => Ok(decision(
                SemanticStatus::Rejected,
                "pitch_accent_evidence_mismatch",
                "данные источника и рендеринга противоречат идентификатору или источнику JPDB",
                json!({"message": message}),
            )),
            Err(EvidenceFailure::Incomplete(message)) => Ok(incomplete_evidence(message)),
        }
    }
}

pub(crate) enum EvidenceFailure {
    Incomplete(String),
    Contradiction(String),
}

impl EvidenceFailure {
    pub(crate) fn into_message(self) -> String {
        match self {
            Self::Incomplete(message) | Self::Contradiction(message) => message,
        }
    }
}

fn validate_evidence(
    surface: &str,
    metadata: &PitchAccentDomainMetadata,
    image: &image::DynamicImage,
) -> Result<(), EvidenceFailure> {
    if metadata.surface != surface {
        return Err(EvidenceFailure::Contradiction(
            "surface в метаданных не совпадает с ключом идентификатора".into(),
        ));
    }
    if metadata.jpdb_vocabulary_id == 0 {
        return Err(EvidenceFailure::Incomplete(
            "идентификатор словаря JPDB должен быть положительным".into(),
        ));
    }
    if metadata.reading.trim().is_empty() {
        return Err(EvidenceFailure::Incomplete(
            "поле reading не заполнено".into(),
        ));
    }

    let evidence = &metadata.evidence;
    if evidence.provider != PitchAccentProvider::Jpdb {
        return Err(EvidenceFailure::Contradiction(
            "поставщик в подтверждении не соответствует JPDB".into(),
        ));
    }
    let route = parse_jpdb_vocabulary_route(&evidence.source_url).map_err(|_| {
        EvidenceFailure::Contradiction(
            "URL источника должен вести на точный HTTPS-маршрут словарной записи `jpdb.io`".into(),
        )
    })?;
    if route.vocabulary_id != metadata.jpdb_vocabulary_id {
        return Err(EvidenceFailure::Contradiction(
            "ID маршрута JPDB не совпадает с ID словарной записи в метаданных".into(),
        ));
    }
    validate_resolved_forms(&evidence.resolved_forms)?;
    if !evidence
        .resolved_forms
        .iter()
        .any(|form| form.surface == metadata.surface && form.reading == metadata.reading)
    {
        return Err(EvidenceFailure::Contradiction(
            "пара значений `surface` и `reading` в метаданных отсутствует среди разрешённых форм JPDB"
                .into(),
        ));
    }
    if !evidence.resolved_forms.iter().any(|form| {
        form.surface == route.surface
            && route
                .reading
                .as_ref()
                .is_none_or(|reading| form.reading == *reading)
    }) {
        return Err(EvidenceFailure::Contradiction(
            "пара написания и чтения из маршрута JPDB отсутствует среди разрешённых форм".into(),
        ));
    }
    let selected = validate_variant_selection(metadata.jpdb_vocabulary_id, evidence)?;

    let render = &evidence.render;
    if render.kind != PitchAccentRenderKind::BrowserRegionScreenshot {
        return Err(EvidenceFailure::Incomplete(
            "выбранный график pitch accent должен быть получен нативным снимком области браузера"
                .into(),
        ));
    }
    if render.captured_graph_count != 1 {
        return Err(EvidenceFailure::Contradiction(format!(
            "PNG должен содержать ровно один график, в свидетельствах указано {}",
            render.captured_graph_count
        )));
    }
    if render.selector != selected.selector {
        return Err(EvidenceFailure::Contradiction(
            "селектор снимка не совпадает с селектором выбранного варианта графика".into(),
        ));
    }
    if render.padding_css_px != PITCH_ACCENT_CAPTURE_PADDING_CSS_PX {
        return Err(EvidenceFailure::Incomplete(format!(
            "поле снимка должно составлять ровно {PITCH_ACCENT_CAPTURE_PADDING_CSS_PX} CSS px"
        )));
    }
    if render.viewport_width == 0
        || render.viewport_height == 0
        || render.document_width == 0
        || render.document_height == 0
        || render.pixel_width == 0
        || render.pixel_height == 0
    {
        return Err(EvidenceFailure::Incomplete(
            "размеры области просмотра и изображения должны быть положительными".into(),
        ));
    }
    if render.pixel_width != image.width() || render.pixel_height != image.height() {
        return Err(EvidenceFailure::Contradiction(
            "размеры снимка не совпадают с размерами декодированного PNG".into(),
        ));
    }
    if render.device_scale_factor != PitchAccentImageValidator::REQUIRED_DEVICE_SCALE_FACTOR {
        return Err(EvidenceFailure::Incomplete(
            "масштаб устройства должен быть ровно 3.0".into(),
        ));
    }
    if render.page_scale_factor != 1.0 {
        return Err(EvidenceFailure::Incomplete(
            "масштаб страницы должен быть ровно 1.0".into(),
        ));
    }
    validate_capture_geometry(render, &evidence.variant_selection)?;
    validate_dark_theme(&render.dark_theme)?;
    validate_capture_background(&render.dark_theme, image)?;
    validate_capture_margins(render, selected.paint_rect, image)?;

    let browser = &evidence.browser;
    if [
        browser.product.as_str(),
        browser.protocol_version.as_str(),
        browser.revision.as_str(),
        browser.user_agent.as_str(),
        browser.js_version.as_str(),
    ]
    .iter()
    .any(|value| value.trim().is_empty() || value.chars().any(char::is_control))
    {
        return Err(EvidenceFailure::Incomplete(
            "сведения о среде выполнения браузера неполны".into(),
        ));
    }
    Ok(())
}

fn validate_resolved_forms(forms: &[PitchAccentResolvedForm]) -> Result<(), EvidenceFailure> {
    if forms.is_empty()
        || forms.iter().any(|form| {
            form.surface.trim().is_empty()
                || form.reading.trim().is_empty()
                || form.surface.chars().any(char::is_control)
                || form.reading.chars().any(char::is_control)
        })
    {
        return Err(EvidenceFailure::Incomplete(
            "список разрешённых пар написания и чтения JPDB пуст или некорректен".into(),
        ));
    }
    Ok(())
}

fn validate_dark_theme(proof: &PitchAccentDarkThemeProof) -> Result<(), EvidenceFailure> {
    if proof
        .document_element_classes
        .iter()
        .any(|class| class.trim().is_empty() || class.chars().any(char::is_control))
        || proof.prefers_color_scheme.chars().any(char::is_control)
        || proof.computed_color_scheme.trim().is_empty()
        || proof.computed_color_scheme.chars().any(char::is_control)
        || proof.background_selector.trim().is_empty()
        || proof.background_selector.chars().any(char::is_control)
    {
        return Err(EvidenceFailure::Incomplete(
            "наблюдаемое состояние тёмной темы неполно или некорректно".into(),
        ));
    }
    if !proof
        .document_element_classes
        .iter()
        .any(|class| class == "dark-mode")
        || proof.prefers_color_scheme != "dark"
    {
        return Err(EvidenceFailure::Incomplete(
            "документ или параметр темы браузера не подтверждают активную тёмную тему".into(),
        ));
    }
    if relative_luminance(proof.background_rgb) > 0.20 {
        return Err(EvidenceFailure::Contradiction(
            "наблюдённый сплошной фон RGB недостаточно тёмный для снимка JPDB".into(),
        ));
    }
    Ok(())
}

/// Проверяет перечень вариантов и воспроизводит выбор по сохранённым данным.
/// Возвращает выбранный вариант.
fn validate_variant_selection(
    vocabulary_id: u64,
    evidence: &PitchAccentEvidence,
) -> Result<&PitchAccentVariantEvidence, EvidenceFailure> {
    let selection = &evidence.variant_selection;
    if selection.variants.is_empty() || selection.detected_variant_count == 0 {
        return Err(EvidenceFailure::Incomplete(
            "подтверждение источника не содержит вариантов графика".into(),
        ));
    }
    if selection.variants.len() != selection.detected_variant_count as usize
        || selection.variants.len() > PITCH_ACCENT_MAX_VARIANT_COUNT
    {
        return Err(EvidenceFailure::Contradiction(
            "число обнаруженных вариантов не совпадает со списком вариантов или превышает предел"
                .into(),
        ));
    }
    let mut selectors = std::collections::BTreeSet::new();
    for (index, variant) in selection.variants.iter().enumerate() {
        if variant.index != index as u32
            || variant.selector.trim().is_empty()
            || variant.selector.chars().any(char::is_control)
            || !selectors.insert(variant.selector.as_str())
            || variant.text.trim().is_empty()
            || variant.text.chars().any(char::is_control)
            || !is_sha256_hex(&variant.markup_sha256)
        {
            return Err(EvidenceFailure::Contradiction(
                "вариант графика имеет неверный индекс DOM, селектор, текст или SHA-256 разметки"
                    .into(),
            ));
        }
        if variant.selection_score
            != pitch_accent_variant_selection_score(vocabulary_id, &variant.markup_sha256)
        {
            return Err(EvidenceFailure::Contradiction(
                "оценка выбора варианта не соответствует ID записи и разметке графика".into(),
            ));
        }
    }
    match selection.basis {
        PitchAccentVariantSelectionBasis::SeededHashWithoutSourcePriority => {}
    }
    let expected = select_pitch_accent_variant(
        vocabulary_id,
        selection
            .variants
            .iter()
            .map(|variant| variant.markup_sha256.as_str()),
    );
    if expected != Some(selection.selected_index as usize) {
        return Err(EvidenceFailure::Contradiction(
            "выбранный вариант не совпадает с воспроизводимым выбором по сохранённым вариантам"
                .into(),
        ));
    }
    Ok(&selection.variants[selection.selected_index as usize])
}

pub(crate) fn validate_capture_geometry(
    render: &PitchAccentRenderEvidence,
    selection: &PitchAccentVariantSelection,
) -> Result<(), EvidenceFailure> {
    if render.coordinate_space != PitchAccentCoordinateSpace::Document {
        return Err(EvidenceFailure::Incomplete(
            "снимок браузера должен явно использовать координаты документа".into(),
        ));
    }
    if !render.scroll_x.is_finite()
        || !render.scroll_y.is_finite()
        || render.scroll_x < 0.0
        || render.scroll_y < 0.0
        || render.scroll_x > f64::from(render.document_width)
        || render.scroll_y > f64::from(render.document_height)
    {
        return Err(EvidenceFailure::Incomplete(
            "значения прокрутки документа должны быть конечными и неотрицательными".into(),
        ));
    }
    let Some(selected) = selection.variants.get(selection.selected_index as usize) else {
        return Err(EvidenceFailure::Incomplete(
            "свидетельства захвата не содержат выбранного варианта графика".into(),
        ));
    };

    let geometry_tolerance = PITCH_ACCENT_CAPTURE_GEOMETRY_TOLERANCE_CSS_PX;
    let document_width = f64::from(render.document_width);
    let document_height = f64::from(render.document_height);
    for variant in &selection.variants {
        validate_positive_rect(variant.viewport_rect, "график в области просмотра")?;
        validate_positive_rect(variant.document_rect, "график в координатах документа")?;
        validate_positive_rect(variant.paint_rect, "отрисовка графика")?;
        if variant.viewport_rect.x < -geometry_tolerance
            || variant.viewport_rect.y < -geometry_tolerance
            || variant.viewport_rect.x + variant.viewport_rect.width
                > f64::from(render.viewport_width) + geometry_tolerance
            || variant.viewport_rect.y + variant.viewport_rect.height
                > f64::from(render.viewport_height) + geometry_tolerance
        {
            return Err(EvidenceFailure::Contradiction(
                "прямоугольник графика выходит за границы области просмотра".into(),
            ));
        }
        if (variant.document_rect.x - (variant.viewport_rect.x + render.scroll_x)).abs()
            > geometry_tolerance
            || (variant.document_rect.y - (variant.viewport_rect.y + render.scroll_y)).abs()
                > geometry_tolerance
            || (variant.document_rect.width - variant.viewport_rect.width).abs()
                > geometry_tolerance
            || (variant.document_rect.height - variant.viewport_rect.height).abs()
                > geometry_tolerance
            || !rect_within(variant.document_rect, document_width, document_height)
        {
            return Err(EvidenceFailure::Contradiction(
                "прямоугольник графика в документе не соответствует области просмотра и прокрутке"
                    .into(),
            ));
        }
        if !rect_contains(
            variant.paint_rect,
            variant.document_rect,
            geometry_tolerance,
        ) || !rect_within(variant.paint_rect, document_width, document_height)
        {
            return Err(EvidenceFailure::Contradiction(
                "область отрисовки графика не охватывает его узел или выходит за пределы документа"
                    .into(),
            ));
        }
    }

    let rect = render.capture_rect;
    validate_positive_rect(rect, "снимок")?;
    let expected = pitch_accent_capture_rect_for(selected.paint_rect, render.padding_css_px);
    if rect != expected {
        return Err(EvidenceFailure::Contradiction(
            "область снимка должна совпадать с отрисовкой выбранного графика, расширенной на поле до целых CSS-пикселей".into(),
        ));
    }
    if rect.x < 0.0
        || rect.y < 0.0
        || rect.x + rect.width > document_width
        || rect.y + rect.height > document_height
    {
        return Err(EvidenceFailure::Contradiction(
            "область снимка с полями выходит за пределы страницы документа".into(),
        ));
    }
    if selection
        .variants
        .iter()
        .filter(|variant| variant.index != selected.index)
        .any(|variant| rects_intersect(rect, variant.paint_rect))
    {
        return Err(EvidenceFailure::Contradiction(
            "область снимка с полями захватывает отрисовку соседнего варианта графика".into(),
        ));
    }

    let expected_width = rect.width * render.device_scale_factor;
    let expected_height = rect.height * render.device_scale_factor;
    if f64::from(render.pixel_width) != expected_width
        || f64::from(render.pixel_height) != expected_height
    {
        return Err(EvidenceFailure::Contradiction(
            "размеры PNG не равны области снимка в CSS-пикселях, умноженной на масштаб устройства"
                .into(),
        ));
    }
    Ok(())
}

fn rect_within(rect: PitchAccentCaptureRect, width: f64, height: f64) -> bool {
    let tolerance = PITCH_ACCENT_CAPTURE_GEOMETRY_TOLERANCE_CSS_PX;
    rect.x >= -tolerance
        && rect.y >= -tolerance
        && rect.x + rect.width <= width + tolerance
        && rect.y + rect.height <= height + tolerance
}

fn rect_contains(
    outer: PitchAccentCaptureRect,
    inner: PitchAccentCaptureRect,
    tolerance: f64,
) -> bool {
    outer.x <= inner.x + tolerance
        && outer.y <= inner.y + tolerance
        && outer.x + outer.width >= inner.x + inner.width - tolerance
        && outer.y + outer.height >= inner.y + inner.height - tolerance
}

fn rects_intersect(left: PitchAccentCaptureRect, right: PitchAccentCaptureRect) -> bool {
    left.x < right.x + right.width
        && right.x < left.x + left.width
        && left.y < right.y + right.height
        && right.y < left.y + left.height
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_positive_rect(
    rect: PitchAccentCaptureRect,
    label: &str,
) -> Result<(), EvidenceFailure> {
    if ![rect.x, rect.y, rect.width, rect.height]
        .iter()
        .all(|value| value.is_finite())
        || rect.width <= 0.0
        || rect.height <= 0.0
    {
        return Err(EvidenceFailure::Incomplete(format!(
            "{label}: прямоугольник должен иметь конечные положительные размеры"
        )));
    }
    Ok(())
}

pub(crate) fn validate_capture_background(
    proof: &PitchAccentDarkThemeProof,
    image: &image::DynamicImage,
) -> Result<(), EvidenceFailure> {
    let (width, height) = image.dimensions();
    if width == 0 || height == 0 {
        return Err(EvidenceFailure::Incomplete(
            "в PNG нет пикселей для проверки сплошного фона".into(),
        ));
    }
    let mut matching_background_pixels = 0usize;
    let mut contrasting_graph_pixels = 0usize;
    // Проверяется сам снимок: искусственная рамка вокруг графика не требуется.
    // В области графика JPDB могут присутствовать и тёмный фон, и линии графика.
    for pixel in image.pixels().map(|(_, _, pixel)| pixel.0) {
        if pixel[3] != 255 {
            return Err(EvidenceFailure::Contradiction(
                "в области графиков найден пиксель, который не является полностью непрозрачным"
                    .into(),
            ));
        }
        let matches_background =
            pixel[..3]
                .iter()
                .zip(proof.background_rgb)
                .all(|(actual, expected)| {
                    actual.abs_diff(expected) <= PITCH_ACCENT_BACKGROUND_RGB_TOLERANCE
                });
        if matches_background {
            matching_background_pixels += 1;
        }
        if pixel[..3]
            .iter()
            .zip(proof.background_rgb)
            .any(|(actual, expected)| {
                actual.abs_diff(expected) > PITCH_ACCENT_GRAPH_PIXEL_MIN_RGB_DIFFERENCE
            })
        {
            contrasting_graph_pixels += 1;
        }
    }
    if matching_background_pixels == 0
        || contrasting_graph_pixels < PITCH_ACCENT_MIN_CONTRASTING_GRAPH_PIXELS
    {
        return Err(EvidenceFailure::Contradiction(format!(
            "PNG должен содержать наблюдённый фон и не менее {PITCH_ACCENT_MIN_CONTRASTING_GRAPH_PIXELS} контрастных пикселей графика"
        )));
    }
    Ok(())
}

/// Проверяет по фактическим пикселям, что всё вне отрисовки выбранного графика —
/// наблюдённый фон. Так доказывается, что поля не пусты от чужого содержимого
/// (соседних вариантов, кнопок, подписей), а линии и символы графика не касаются
/// краёв PNG и потому не обрезаны областью снимка.
pub(crate) fn validate_capture_margins(
    render: &PitchAccentRenderEvidence,
    paint_rect: PitchAccentCaptureRect,
    image: &image::DynamicImage,
) -> Result<(), EvidenceFailure> {
    let scale = render.device_scale_factor;
    let capture = render.capture_rect;
    let to_pixels = |value: f64| value * scale;
    let inner_left =
        to_pixels(paint_rect.x - capture.x).floor() as i64 - PITCH_ACCENT_PAINT_EDGE_ANTIALIAS_PX;
    let inner_top =
        to_pixels(paint_rect.y - capture.y).floor() as i64 - PITCH_ACCENT_PAINT_EDGE_ANTIALIAS_PX;
    let inner_right = to_pixels(paint_rect.x + paint_rect.width - capture.x).ceil() as i64
        + PITCH_ACCENT_PAINT_EDGE_ANTIALIAS_PX;
    let inner_bottom = to_pixels(paint_rect.y + paint_rect.height - capture.y).ceil() as i64
        + PITCH_ACCENT_PAINT_EDGE_ANTIALIAS_PX;
    let (width, height) = image.dimensions();
    if inner_left < 1
        || inner_top < 1
        || inner_right >= i64::from(width)
        || inner_bottom >= i64::from(height)
    {
        return Err(EvidenceFailure::Contradiction(
            "в PNG нет свободного поля фона с каждой стороны отрисовки графика".into(),
        ));
    }
    for (x, y, pixel) in image.pixels() {
        let (x, y) = (i64::from(x), i64::from(y));
        if (inner_left..inner_right).contains(&x) && (inner_top..inner_bottom).contains(&y) {
            continue;
        }
        let matches_background = pixel.0[3] == 255
            && pixel.0[..3]
                .iter()
                .zip(render.dark_theme.background_rgb)
                .all(|(actual, expected)| {
                    actual.abs_diff(expected) <= PITCH_ACCENT_BACKGROUND_RGB_TOLERANCE
                });
        if !matches_background {
            return Err(EvidenceFailure::Contradiction(format!(
                "поле PNG вне отрисовки выбранного графика содержит не фон в пикселе ({x}, {y})"
            )));
        }
    }
    Ok(())
}

pub(crate) fn relative_luminance(rgb: [u8; 3]) -> f64 {
    let linear = rgb.map(|channel| {
        let value = f64::from(channel) / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    });
    0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2]
}

/// Разобранный и проверенный маршрут словарной записи `jpdb.io`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JpdbVocabularyRoute {
    pub(crate) vocabulary_id: u64,
    pub(crate) surface: String,
    pub(crate) reading: Option<String>,
}

/// Ошибка разбора точного маршрута словарной записи JPDB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JpdbVocabularyRouteError {
    Url,
    Origin,
    Path,
    VocabularyId,
    Encoding,
}

/// Разбирает только точный путь словарной записи JPDB, игнорируя строку запроса и фрагмент.
pub(crate) fn parse_jpdb_vocabulary_route(
    value: &str,
) -> Result<JpdbVocabularyRoute, JpdbVocabularyRouteError> {
    let (vocabulary_id, segments) = parse_jpdb_vocabulary_path(value)?;
    let surface = segments[0].clone();
    let reading = segments.get(1).cloned();
    if surface.eq_ignore_ascii_case("used-in")
        || reading
            .as_ref()
            .is_some_and(|value| value.eq_ignore_ascii_case("used-in"))
    {
        return Err(JpdbVocabularyRouteError::Path);
    }
    Ok(JpdbVocabularyRoute {
        vocabulary_id,
        surface,
        reading,
    })
}

/// ID и написание из ссылки `Used in` строки JPDB.
///
/// Ссылка ведёт на список употреблений, а не на словарную запись, поэтому
/// маршрутом карточки не является; она лишь свидетельствует, к какой записи
/// относится строка поиска.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JpdbUsedInRoute {
    pub(crate) vocabulary_id: u64,
    pub(crate) surface: String,
}

/// Разбирает только точный путь `/vocabulary/<id>/<surface>/used-in` на `jpdb.io`.
pub(crate) fn parse_jpdb_used_in_route(
    value: &str,
) -> Result<JpdbUsedInRoute, JpdbVocabularyRouteError> {
    let (vocabulary_id, segments) = parse_jpdb_vocabulary_path(value)?;
    if segments.len() != 2
        || segments[1] != "used-in"
        || segments[0].eq_ignore_ascii_case("used-in")
    {
        return Err(JpdbVocabularyRouteError::Path);
    }
    Ok(JpdbUsedInRoute {
        vocabulary_id,
        surface: segments[0].clone(),
    })
}

/// Общая часть разбора: origin, `vocabulary`, ID и декодированные сегменты после ID.
fn parse_jpdb_vocabulary_path(value: &str) -> Result<(u64, Vec<String>), JpdbVocabularyRouteError> {
    let url = Url::parse(value).map_err(|_| JpdbVocabularyRouteError::Url)?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url
            .host_str()
            .is_none_or(|host| !host.eq_ignore_ascii_case("jpdb.io"))
        || url.port_or_known_default() != Some(443)
    {
        return Err(JpdbVocabularyRouteError::Origin);
    }
    let encoded_segments: Vec<_> = url
        .path_segments()
        .ok_or(JpdbVocabularyRouteError::Path)?
        .collect();
    if !(encoded_segments.len() == 3 || encoded_segments.len() == 4)
        || encoded_segments.iter().any(|segment| segment.is_empty())
        || encoded_segments[0] != "vocabulary"
    {
        return Err(JpdbVocabularyRouteError::Path);
    }
    if encoded_segments[1]
        .bytes()
        .any(|byte| !byte.is_ascii_digit())
    {
        return Err(JpdbVocabularyRouteError::VocabularyId);
    }
    let vocabulary_id = encoded_segments[1]
        .parse::<u64>()
        .ok()
        .filter(|id| *id > 0 && id.to_string() == encoded_segments[1])
        .ok_or(JpdbVocabularyRouteError::VocabularyId)?;
    let segments = encoded_segments[2..]
        .iter()
        .map(|segment| decode_path_segment(segment))
        .collect::<Result<Vec<_>, _>>()?;
    if segments.iter().any(|segment| segment.trim().is_empty()) {
        return Err(JpdbVocabularyRouteError::Path);
    }
    Ok((vocabulary_id, segments))
}

fn decode_path_segment(segment: &str) -> Result<String, JpdbVocabularyRouteError> {
    let input = segment.as_bytes();
    let mut decoded = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index] == b'%' {
            if index + 2 >= input.len() {
                return Err(JpdbVocabularyRouteError::Encoding);
            }
            let high = (input[index + 1] as char)
                .to_digit(16)
                .ok_or(JpdbVocabularyRouteError::Encoding)?;
            let low = (input[index + 2] as char)
                .to_digit(16)
                .ok_or(JpdbVocabularyRouteError::Encoding)?;
            decoded.push(((high << 4) | low) as u8);
            index += 3;
        } else {
            decoded.push(input[index]);
            index += 1;
        }
    }
    let value = String::from_utf8(decoded).map_err(|_| JpdbVocabularyRouteError::Encoding)?;
    if value.chars().any(char::is_control) {
        return Err(JpdbVocabularyRouteError::Encoding);
    }
    Ok(value)
}

fn validate_hash(hash: &str) -> Result<(), AssetError> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "SHA-256 должен состоять из 64 строчных hex-символов",
        ));
    }
    Ok(())
}

fn decode_png(bytes: &[u8]) -> Result<image::DynamicImage, String> {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(PITCH_ACCENT_MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(PITCH_ACCENT_MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(PITCH_ACCENT_MAX_DECODE_ALLOCATION_BYTES);
    let decoder = image::codecs::png::PngDecoder::with_limits(Cursor::new(bytes), limits)
        .map_err(|error| error.to_string())?;
    let (width, height) = decoder.dimensions();
    let bytes_per_pixel = u64::from(decoder.color_type().bytes_per_pixel());
    let output_allocation = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(bytes_per_pixel))
        .ok_or_else(|| "объём декодируемого PNG выходит за числовой предел".to_owned())?;
    if output_allocation > PITCH_ACCENT_MAX_DECODE_ALLOCATION_BYTES {
        return Err(format!(
            "декодированное изображение потребует {output_allocation} байт при пределе {PITCH_ACCENT_MAX_DECODE_ALLOCATION_BYTES} байт"
        ));
    }
    image::DynamicImage::from_decoder(decoder).map_err(|error| error.to_string())
}

fn incomplete_evidence(message: impl Into<String>) -> SemanticDecision {
    decision(
        SemanticStatus::Uncertain,
        "pitch_accent_evidence_incomplete",
        "PNG не подтверждён полными данными источника и рендеринга",
        json!({"message": message.into()}),
    )
}

fn decision(
    status: SemanticStatus,
    kind: &str,
    summary: &str,
    details: serde_json::Value,
) -> SemanticDecision {
    SemanticDecision::new(
        status,
        vec![ValidationEvidence {
            kind: kind.to_owned(),
            summary: summary.to_owned(),
            details: Some(details),
        }],
    )
}

/// Согласованные фикстуры одного варианта для тестов пакетов, CLI и отчёта.
#[cfg(test)]
pub(crate) mod test_support {
    use std::io::Cursor;

    use super::*;

    /// Отрисовка 20×10 CSS px в точке (100, 200) документа при прокрутке 100 CSS px;
    /// область снимка с полями — (97, 197) 26×16 CSS px, PNG — 78×48.
    pub(crate) fn single_variant_metadata(
        surface: &str,
        reading: &str,
        vocabulary_id: u64,
        source_url: String,
        browser: BrowserRuntimeProvenance,
    ) -> PitchAccentDomainMetadata {
        let document_rect = PitchAccentCaptureRect {
            x: 100.0,
            y: 200.0,
            width: 20.0,
            height: 10.0,
        };
        let markup_sha256 = sha256_hex(format!("<div>{reading}</div>").as_bytes());
        let selector = ".subsection-pitch-accent .subsection > div > div:nth-child(1) > div:nth-child(2)[style*=\"word-break: keep-all\"]".to_owned();
        let capture_rect =
            pitch_accent_capture_rect_for(document_rect, PITCH_ACCENT_CAPTURE_PADDING_CSS_PX);
        PitchAccentDomainMetadata {
            surface: surface.into(),
            reading: reading.into(),
            jpdb_vocabulary_id: vocabulary_id,
            evidence: PitchAccentEvidence {
                provider: PitchAccentProvider::Jpdb,
                source_url,
                resolved_forms: vec![PitchAccentResolvedForm {
                    surface: surface.into(),
                    reading: reading.into(),
                }],
                variant_selection: PitchAccentVariantSelection {
                    basis: PitchAccentVariantSelectionBasis::SeededHashWithoutSourcePriority,
                    detected_variant_count: 1,
                    selected_index: 0,
                    variants: vec![PitchAccentVariantEvidence {
                        index: 0,
                        selector: selector.clone(),
                        text: reading.into(),
                        selection_score: pitch_accent_variant_selection_score(
                            vocabulary_id,
                            &markup_sha256,
                        ),
                        markup_sha256,
                        viewport_rect: PitchAccentCaptureRect {
                            y: 100.0,
                            ..document_rect
                        },
                        document_rect,
                        paint_rect: document_rect,
                    }],
                },
                render: PitchAccentRenderEvidence {
                    kind: PitchAccentRenderKind::BrowserRegionScreenshot,
                    selector,
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
                    padding_css_px: PITCH_ACCENT_CAPTURE_PADDING_CSS_PX,
                    capture_rect,
                },
                browser,
            },
        }
    }

    /// PNG 78×48 для `single_variant_metadata`: фон и линия внутри отрисовки графика.
    /// `alternate` сдвигает линию и даёт другие байты с той же геометрией.
    pub(crate) fn single_variant_png(alternate: bool) -> Vec<u8> {
        let mut image = image::RgbaImage::from_pixel(78, 48, image::Rgba([24, 36, 48, 255]));
        let start = if alternate { 14 } else { 12 };
        for x in start..(start + 48) {
            image.put_pixel(x, 24, image::Rgba([235, 235, 235, 255]));
        }
        let mut output = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut output, image::ImageFormat::Png)
            .unwrap();
        output.into_inner()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    const BACKGROUND: [u8; 3] = [24, 36, 48];

    fn rect(x: f64, y: f64, width: f64, height: f64) -> PitchAccentCaptureRect {
        PitchAccentCaptureRect {
            x,
            y,
            width,
            height,
        }
    }

    /// Вариант с прямоугольником узла `document_rect`; прокрутка документа — 100 CSS px.
    fn variant(
        index: u32,
        markup: &str,
        document_rect: PitchAccentCaptureRect,
    ) -> PitchAccentVariantEvidence {
        let markup_sha256 = sha256_hex(markup.as_bytes());
        PitchAccentVariantEvidence {
            index,
            selector: format!(".pitch-row-{index} > .pitch-graph"),
            text: "ゆうれい".into(),
            selection_score: pitch_accent_variant_selection_score(123, &markup_sha256),
            markup_sha256,
            viewport_rect: rect(
                document_rect.x,
                document_rect.y - 100.0,
                document_rect.width,
                document_rect.height,
            ),
            document_rect,
            paint_rect: document_rect,
        }
    }

    /// Согласованные метаданные: область снимка вычисляется из выбранного варианта.
    fn metadata_with_variants(
        surface: &str,
        variants: Vec<PitchAccentVariantEvidence>,
    ) -> PitchAccentDomainMetadata {
        let selected_index = select_pitch_accent_variant(
            123,
            variants
                .iter()
                .map(|variant| variant.markup_sha256.as_str()),
        )
        .unwrap();
        let selected = &variants[selected_index];
        let capture_rect =
            pitch_accent_capture_rect_for(selected.paint_rect, PITCH_ACCENT_CAPTURE_PADDING_CSS_PX);
        PitchAccentDomainMetadata {
            surface: surface.to_owned(),
            reading: "ゆうれい".into(),
            jpdb_vocabulary_id: 123,
            evidence: PitchAccentEvidence {
                provider: PitchAccentProvider::Jpdb,
                source_url: "https://jpdb.io/vocabulary/123/幽霊/ゆうれい#a".into(),
                resolved_forms: vec![PitchAccentResolvedForm {
                    surface: "幽霊".into(),
                    reading: "ゆうれい".into(),
                }],
                render: PitchAccentRenderEvidence {
                    kind: PitchAccentRenderKind::BrowserRegionScreenshot,
                    selector: selected.selector.clone(),
                    captured_graph_count: 1,
                    coordinate_space: PitchAccentCoordinateSpace::Document,
                    viewport_width: 1280,
                    viewport_height: 900,
                    document_width: 1280,
                    document_height: 1800,
                    scroll_x: 0.0,
                    scroll_y: 100.0,
                    pixel_width: (capture_rect.width * 3.0) as u32,
                    pixel_height: (capture_rect.height * 3.0) as u32,
                    device_scale_factor: 3.0,
                    page_scale_factor: 1.0,
                    dark_theme: PitchAccentDarkThemeProof {
                        document_element_classes: vec!["dark-mode".into()],
                        prefers_color_scheme: "dark".into(),
                        computed_color_scheme: "dark".into(),
                        background_selector: ".subsection-pitch-accent".into(),
                        background_rgb: BACKGROUND,
                    },
                    padding_css_px: PITCH_ACCENT_CAPTURE_PADDING_CSS_PX,
                    capture_rect,
                },
                variant_selection: PitchAccentVariantSelection {
                    basis: PitchAccentVariantSelectionBasis::SeededHashWithoutSourcePriority,
                    detected_variant_count: variants.len() as u32,
                    selected_index: selected_index as u32,
                    variants,
                },
                browser: BrowserRuntimeProvenance {
                    product: "Chrome/140".into(),
                    protocol_version: "1.3".into(),
                    revision: "1234567".into(),
                    user_agent: "Mozilla/5.0 Chrome/140".into(),
                    js_version: "V8 14.0".into(),
                    executable_source: crate::browser_runtime::BrowserExecutableSource::PathLookup,
                },
            },
        }
    }

    fn valid_metadata(surface: &str) -> PitchAccentDomainMetadata {
        metadata_with_variants(
            surface,
            vec![variant(
                0,
                "<div>ゆうれい</div>",
                rect(100.0, 200.0, 20.0, 10.0),
            )],
        )
    }

    /// Три варианта в столбик с естественными промежутками в 10 CSS px.
    fn multi_variant_metadata() -> PitchAccentDomainMetadata {
        metadata_with_variants(
            "幽霊",
            (0..3)
                .map(|index| {
                    variant(
                        index,
                        &format!("<div>variant-{index}</div>"),
                        rect(100.0, 200.0 + f64::from(index) * 20.0, 20.0, 10.0),
                    )
                })
                .collect(),
        )
    }

    /// PNG по геометрии метаданных: подлинный фон и горизонтальная линия внутри
    /// отрисовки выбранного графика на `line_offset` физических пикселей от её края.
    fn png_for(metadata: &PitchAccentDomainMetadata, line_offset: i64) -> Vec<u8> {
        let render = &metadata.evidence.render;
        let selection = &metadata.evidence.variant_selection;
        let paint = selection.variants[selection.selected_index as usize].paint_rect;
        let mut image = image::RgbaImage::from_pixel(
            render.pixel_width,
            render.pixel_height,
            image::Rgba([BACKGROUND[0], BACKGROUND[1], BACKGROUND[2], 255]),
        );
        let left = ((paint.x - render.capture_rect.x) * 3.0) as i64;
        let right = ((paint.x + paint.width - render.capture_rect.x) * 3.0) as i64;
        let middle = ((paint.y + paint.height / 2.0 - render.capture_rect.y) * 3.0) as u32;
        for x in (left + line_offset)..(right - line_offset) {
            if (0..i64::from(render.pixel_width)).contains(&x) {
                image.put_pixel(x as u32, middle, image::Rgba([235, 235, 235, 255]));
            }
        }
        encode(image)
    }

    fn encode(image: image::RgbaImage) -> Vec<u8> {
        let mut output = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut output, image::ImageFormat::Png)
            .unwrap();
        output.into_inner()
    }

    fn valid_png() -> Vec<u8> {
        png_for(&valid_metadata("幽霊"), 3)
    }

    fn rgb_png(width: u32, height: u32) -> Vec<u8> {
        let mut image = image::RgbImage::from_pixel(width, height, image::Rgb(BACKGROUND));
        if width > 0 && height > 0 {
            for x in (width / 4)..((width * 3 / 4).max(1)) {
                image.put_pixel(x, height / 2, image::Rgb([235, 235, 235]));
            }
        }
        let mut output = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut output, image::ImageFormat::Png)
            .unwrap();
        output.into_inner()
    }

    fn rgba_png(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([24, 36, 48, 128]));
        encode(image)
    }

    fn white_png(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbImage::from_pixel(width, height, image::Rgb([255, 255, 255]));
        let mut output = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut output, image::ImageFormat::Png)
            .unwrap();
        output.into_inner()
    }

    #[test]
    fn pitch_policy_keeps_surface_filename_and_isolates_unsupported_runtime_formats() {
        let policy = PitchAccentDomainPolicy;
        let identity = crate::model::AssetIdentity::new("pitch_accent", "飴").unwrap();
        let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

        let png = policy
            .canonical_location(&identity, hash, DetectedFormat::Png)
            .unwrap();
        assert_eq!(png.storage_path, "assets/png/飴.png");
        assert_eq!(png.consumer_filename, "飴.pitch.png");

        let kanji = crate::domain::KanjiDomainPolicy
            .canonical_location(
                &crate::model::AssetIdentity::new("kanji", "飴").unwrap(),
                hash,
                DetectedFormat::Png,
            )
            .unwrap();
        assert_eq!(kanji.storage_path, "assets/png/飴.png");
        assert_eq!(kanji.consumer_filename, "飴.png");

        let gif = policy
            .canonical_location(&identity, hash, DetectedFormat::Gif)
            .unwrap();
        let identity_hash = sha256_hex("飴".as_bytes());
        assert_eq!(
            gif.storage_path,
            format!("assets/unsupported/{identity_hash}-{hash}.gif")
        );
        assert_eq!(gif.consumer_filename, "飴.gif");
        assert_eq!(policy.max_asset_bytes(), Some(PITCH_ACCENT_MAX_ASSET_BYTES));
        assert!(!policy.is_publishable_format(DetectedFormat::Gif));
        assert!(!policy.allows_verified_format(DetectedFormat::Gif));
        assert!(policy.allows_verified_format(DetectedFormat::Png));
        assert!(
            policy
                .legacy_location(&identity, hash, DetectedFormat::Png)
                .is_none()
        );
    }

    fn record(metadata: Option<PitchAccentDomainMetadata>) -> AssetRecord {
        AssetRecord {
            identity: crate::model::AssetIdentity::new("pitch_accent", "幽霊").unwrap(),
            storage_path: "assets/png/幽霊.png".into(),
            consumer_filename: "幽霊.pitch.png".into(),
            sha256: "0".repeat(64),
            byte_length: 0,
            format: DetectedFormat::Png,
            provenance: crate::model::Provenance {
                source_kind: "jpdb-browser".into(),
                source_name: "幽霊.png".into(),
            },
            lifecycle: crate::model::LifecycleState::Pending,
            validation: None,
            human_attestation: None,
            domain_metadata: metadata.and_then(|value| serde_json::to_value(value).ok()),
        }
    }

    fn validate(asset: &AssetRecord, png: &[u8]) -> Result<SemanticDecision, ValidatorFailure> {
        PitchAccentImageValidator.validate(asset, &mut Cursor::new(png))
    }

    #[test]
    fn decodable_png_with_structured_jpdb_evidence_is_verified() {
        let metadata = valid_metadata("幽霊");
        assert_eq!(
            metadata.evidence.render.capture_rect,
            rect(97.0, 197.0, 26.0, 16.0),
            "отрисовка 20×10 расширяется на 3 CSS px с каждой стороны"
        );
        let asset = record(Some(metadata));
        let decision = validate(&asset, &valid_png()).unwrap();

        assert_eq!(PitchAccentImageValidator::validator_identity().version, "6");
        assert_eq!(decision.status, SemanticStatus::Verified);
        assert_eq!(
            decision.evidence[0].kind,
            "pitch_accent_source_render_verified"
        );
    }

    #[test]
    fn valid_png_without_evidence_is_uncertain() {
        let asset = record(None);
        let decision = validate(&asset, &valid_png()).unwrap();

        assert_eq!(decision.status, SemanticStatus::Uncertain);
        assert_ne!(decision.status, SemanticStatus::Verified);
    }

    #[test]
    fn mismatched_surface_or_capture_settings_cannot_be_verified() {
        let asset = record(Some(valid_metadata("ゆうれい")));
        assert_eq!(
            validate(&asset, &valid_png()).unwrap().status,
            SemanticStatus::Rejected
        );

        let mut metadata = valid_metadata("幽霊");
        metadata.evidence.render.pixel_width = 3;
        let asset = record(Some(metadata));
        assert_eq!(
            validate(&asset, &valid_png()).unwrap().status,
            SemanticStatus::Rejected
        );

        let mut metadata = valid_metadata("幽霊");
        metadata.evidence.render.device_scale_factor = 2.0;
        let asset = record(Some(metadata));
        assert_eq!(
            validate(&asset, &valid_png()).unwrap().status,
            SemanticStatus::Uncertain
        );

        let mut metadata = valid_metadata("幽霊");
        metadata.evidence.resolved_forms = vec![PitchAccentResolvedForm {
            surface: "幽霊".into(),
            reading: "げんき".into(),
        }];
        let asset = record(Some(metadata));
        assert_eq!(
            validate(&asset, &valid_png()).unwrap().status,
            SemanticStatus::Rejected
        );
    }

    #[test]
    fn source_url_and_typed_route_parser_share_the_exact_jpdb_contract() {
        for source_url in [
            "https://jpdb.io:443/vocabulary/123/幽霊/ゆうれい?tab=1#pitch",
            "https://jpdb.io/vocabulary/123/幽霊?tab=1#pitch",
        ] {
            assert!(parse_jpdb_vocabulary_route(source_url).is_ok());
            let mut metadata = valid_metadata("幽霊");
            metadata.evidence.source_url = source_url.into();
            let asset = record(Some(metadata));
            assert_eq!(
                validate(&asset, &valid_png()).unwrap().status,
                SemanticStatus::Verified,
                "валидатор должен принимать маршрут, допустимый общим разборщиком: {source_url}"
            );
        }

        for source_url in [
            "https://subdomain.jpdb.io/vocabulary/123/幽霊/ゆうれい",
            "https://jpdb.io/prefix/vocabulary/123/幽霊/ゆうれい",
            "https://jpdb.io/vocabulary/123/幽霊/used-in",
            "http://jpdb.io/vocabulary/123/幽霊/ゆうれい",
            "https://user:pass@jpdb.io/vocabulary/123/幽霊/ゆうれい",
            "https://jpdb.io:444/vocabulary/123/幽霊/ゆうれい",
            "https://jpdb.io/vocabulary/123/幽霊/ゆうれい/",
            "https://jpdb.io/vocabulary/123//ゆうれい",
            "https://jpdb.io/vocabulary/123/幽霊/ゆうれい/extra",
            "https://jpdb.io.evil.example/vocabulary/123/幽霊/ゆうれい",
            "https://jpdb.io/vocabulary/0123/幽霊/ゆうれい",
        ] {
            assert!(
                parse_jpdb_vocabulary_route(source_url).is_err(),
                "разборщик должен отвергать маршрут: {source_url}"
            );
            let mut metadata = valid_metadata("幽霊");
            metadata.evidence.source_url = source_url.into();
            let asset = record(Some(metadata));
            assert_eq!(
                validate(&asset, &valid_png()).unwrap().status,
                SemanticStatus::Rejected,
                "валидатор должен отвергать неверный URL источника: {source_url}"
            );
        }

        let mismatched_id = "https://jpdb.io/vocabulary/124/幽霊/ゆうれい";
        assert!(parse_jpdb_vocabulary_route(mismatched_id).is_ok());
        let mut metadata = valid_metadata("幽霊");
        metadata.evidence.source_url = mismatched_id.into();
        let asset = record(Some(metadata));
        assert_eq!(
            validate(&asset, &valid_png()).unwrap().status,
            SemanticStatus::Rejected,
            "структурно допустимый маршрут с неверным ID в метаданных отвергается"
        );
    }

    #[test]
    fn used_in_link_is_evidence_of_id_and_surface_but_never_a_vocabulary_route() {
        let used_in = "https://jpdb.io/vocabulary/123/%E5%B9%BD%E9%9C%8A/used-in";
        assert_eq!(
            parse_jpdb_used_in_route(used_in),
            Ok(JpdbUsedInRoute {
                vocabulary_id: 123,
                surface: "幽霊".into(),
            })
        );
        assert!(parse_jpdb_vocabulary_route(used_in).is_err());

        for value in [
            "https://jpdb.io/vocabulary/123/幽霊/ゆうれい",
            "https://jpdb.io/vocabulary/123/幽霊",
            "https://jpdb.io/vocabulary/123/used-in/used-in",
            "https://jpdb.io/vocabulary/123/幽霊/USED-IN",
            "https://jpdb.io/vocabulary/123/幽霊/used-in/extra",
            "https://jpdb.io/vocabulary/0123/幽霊/used-in",
            "https://jpdb.io/vocabulary/123//used-in",
            "https://jpdb.io.evil.example/vocabulary/123/幽霊/used-in",
            "http://jpdb.io/vocabulary/123/幽霊/used-in",
            "https://jpdb.io/conjugate/123/幽霊/used-in",
        ] {
            assert!(
                parse_jpdb_used_in_route(value).is_err(),
                "разборщик `Used in` должен отвергать: {value}"
            );
        }
    }

    #[test]
    fn metadata_surface_and_reading_must_be_an_observed_pair() {
        let mut metadata = valid_metadata("幽霊");
        metadata.reading = "げんき".into();
        metadata
            .evidence
            .resolved_forms
            .push(PitchAccentResolvedForm {
                surface: "元気".into(),
                reading: "げんき".into(),
            });
        let asset = record(Some(metadata));

        assert_eq!(
            validate(&asset, &valid_png()).unwrap().status,
            SemanticStatus::Rejected
        );
    }

    #[test]
    fn dark_theme_render_kind_and_page_scale_are_required() {
        let mut metadata = valid_metadata("幽霊");
        metadata.evidence.render.kind = PitchAccentRenderKind::ElementScreenshot;
        let asset = record(Some(metadata));
        assert_eq!(
            validate(&asset, &valid_png()).unwrap().status,
            SemanticStatus::Uncertain
        );

        let mut metadata = valid_metadata("幽霊");
        metadata
            .evidence
            .render
            .dark_theme
            .document_element_classes
            .clear();
        let asset = record(Some(metadata));
        assert_eq!(
            validate(&asset, &valid_png()).unwrap().status,
            SemanticStatus::Uncertain
        );

        let mut metadata = valid_metadata("幽霊");
        metadata.evidence.render.dark_theme.prefers_color_scheme = "light".into();
        let asset = record(Some(metadata));
        assert_eq!(
            validate(&asset, &valid_png()).unwrap().status,
            SemanticStatus::Uncertain
        );

        let mut metadata = valid_metadata("幽霊");
        metadata.evidence.render.page_scale_factor = 1.25;
        let asset = record(Some(metadata));
        assert_eq!(
            validate(&asset, &valid_png()).unwrap().status,
            SemanticStatus::Uncertain
        );
    }

    #[test]
    fn jpdb_reading_comparison_only_equates_hiragana_and_katakana() {
        assert!(jpdb_readings_equivalent("ネコ", "ねこ"));
        assert!(jpdb_readings_equivalent("スーパー", "すーぱー"));
        assert!(!jpdb_readings_equivalent("ネコ", "しょうじょ"));
        assert!(!jpdb_readings_equivalent("NEKO", "ねこ"));
    }

    #[test]
    fn version_5_metadata_with_graph_union_is_not_verified() {
        // Форма свидетельств валидатора v5: все графики, `graph_count` и объединение.
        let mut legacy = serde_json::to_value(valid_metadata("幽霊")).unwrap();
        let evidence = legacy["evidence"].as_object_mut().unwrap();
        evidence.remove("variant_selection");
        evidence.insert("graph_count".into(), json!(1));
        let render = evidence["render"].as_object_mut().unwrap();
        render.remove("captured_graph_count");
        render.remove("padding_css_px");
        let paint = json!({"x": 100.0, "y": 200.0, "width": 20.0, "height": 10.0});
        render.insert("graph_union_rect".into(), paint.clone());
        render.insert("capture_rect".into(), paint.clone());
        render.insert(
            "graphs".into(),
            json!([{
                "index": 0,
                "selector": ".pitch-row-0 > .pitch-graph",
                "viewport_rect": {"x": 100.0, "y": 100.0, "width": 20.0, "height": 10.0},
                "document_rect": paint,
            }]),
        );
        render.insert("pixel_width".into(), json!(60));
        render.insert("pixel_height".into(), json!(30));

        let mut asset = record(None);
        asset.domain_metadata = Some(legacy);
        let decision = validate(&asset, &rgb_png(60, 30)).unwrap();

        assert_eq!(decision.status, SemanticStatus::Uncertain);
        assert_eq!(
            decision.evidence[0].kind,
            "pitch_accent_evidence_incomplete"
        );
    }

    #[test]
    fn page_coordinate_geometry_offsets_graphs_by_document_scroll() {
        let mut metadata = valid_metadata("幽霊");
        metadata.evidence.render.scroll_y = 120.0;
        metadata.evidence.variant_selection.variants[0]
            .viewport_rect
            .y = 80.0;
        let asset = record(Some(metadata));
        assert_eq!(
            validate(&asset, &valid_png()).unwrap().status,
            SemanticStatus::Verified
        );

        let mut metadata = valid_metadata("幽霊");
        metadata.evidence.variant_selection.variants[0]
            .document_rect
            .y += 1.0;
        let asset = record(Some(metadata));
        assert_eq!(
            validate(&asset, &valid_png()).unwrap().status,
            SemanticStatus::Rejected
        );

        let mut metadata = valid_metadata("幽霊");
        metadata.evidence.variant_selection.variants[0]
            .document_rect
            .width += 1.0;
        let asset = record(Some(metadata));
        assert_eq!(
            validate(&asset, &valid_png()).unwrap().status,
            SemanticStatus::Rejected,
            "ширина графика в координатах документа должна совпадать с областью просмотра"
        );
    }

    #[test]
    fn paint_overflow_beyond_the_node_is_captured_inside_the_padding() {
        // Сегмент JPDB с `margin-right: -2px` выходит за прямоугольник узла.
        let mut overflowing = variant(0, "<div>overflow</div>", rect(100.0, 200.0, 20.0, 10.0));
        overflowing.paint_rect = rect(100.0, 200.0, 22.0, 10.0);
        let metadata = metadata_with_variants("幽霊", vec![overflowing]);
        assert_eq!(
            metadata.evidence.render.capture_rect,
            rect(97.0, 197.0, 28.0, 16.0)
        );
        let bytes = png_for(&metadata, 3);
        let asset = record(Some(metadata.clone()));
        assert_eq!(
            validate(&asset, &bytes).unwrap().status,
            SemanticStatus::Verified
        );

        let mut node_only = metadata;
        node_only.evidence.variant_selection.variants[0].paint_rect =
            node_only.evidence.variant_selection.variants[0].document_rect;
        let asset = record(Some(node_only));
        assert_eq!(
            validate(&asset, &bytes).unwrap().status,
            SemanticStatus::Rejected,
            "область снимка должна строиться от полной отрисовки, а не от узла"
        );

        let mut shrunken = valid_metadata("幽霊");
        shrunken.evidence.variant_selection.variants[0]
            .paint_rect
            .width = 10.0;
        let asset = record(Some(shrunken));
        assert_eq!(
            validate(&asset, &valid_png()).unwrap().status,
            SemanticStatus::Rejected,
            "отрисовка обязана охватывать узел графика"
        );
    }

    #[test]
    fn capture_rect_must_be_the_padded_selected_paint_inside_the_document() {
        let mut unpadded = valid_metadata("幽霊");
        unpadded.evidence.render.capture_rect = rect(100.0, 200.0, 20.0, 10.0);
        unpadded.evidence.render.pixel_width = 60;
        unpadded.evidence.render.pixel_height = 30;
        let asset = record(Some(unpadded));
        assert_eq!(
            validate(&asset, &rgb_png(60, 30)).unwrap().status,
            SemanticStatus::Rejected,
            "прежняя семантика v5 без полей больше не подтверждается"
        );

        let mut zero_padding = valid_metadata("幽霊");
        zero_padding.evidence.render.padding_css_px = 0.0;
        let asset = record(Some(zero_padding));
        assert_eq!(
            validate(&asset, &valid_png()).unwrap().status,
            SemanticStatus::Uncertain
        );

        let mut near_top = metadata_with_variants(
            "幽霊",
            vec![variant(0, "<div>top</div>", rect(100.0, 101.0, 20.0, 10.0))],
        );
        near_top.evidence.render.scroll_y = 100.0;
        assert_eq!(near_top.evidence.render.capture_rect.y, 98.0);
        let asset = record(Some(near_top.clone()));
        assert_eq!(
            validate(&asset, &png_for(&near_top, 3)).unwrap().status,
            SemanticStatus::Verified
        );
        let mut beyond_document = metadata_with_variants(
            "幽霊",
            vec![variant(0, "<div>edge</div>", rect(1.0, 101.0, 20.0, 10.0))],
        );
        beyond_document.evidence.render.document_width = 1280;
        assert_eq!(beyond_document.evidence.render.capture_rect.x, -2.0);
        let asset = record(Some(beyond_document.clone()));
        assert_eq!(
            validate(&asset, &png_for(&beyond_document, 3))
                .unwrap()
                .status,
            SemanticStatus::Rejected,
            "поле не может выходить за левую границу документа"
        );

        let mut shifted = valid_metadata("幽霊");
        shifted.evidence.render.capture_rect.x += 1.0;
        let asset = record(Some(shifted));
        assert_eq!(
            validate(&asset, &valid_png()).unwrap().status,
            SemanticStatus::Rejected
        );
    }

    #[test]
    fn png_dimensions_must_equal_the_integer_clip_times_device_scale() {
        let metadata = valid_metadata("幽霊");
        let mut image = image::RgbaImage::from_pixel(
            metadata.evidence.render.pixel_width + 1,
            metadata.evidence.render.pixel_height,
            image::Rgba([BACKGROUND[0], BACKGROUND[1], BACKGROUND[2], 255]),
        );
        for x in 12..60 {
            image.put_pixel(x, 24, image::Rgba([235, 235, 235, 255]));
        }
        let bytes = encode(image);

        let mut claimed = metadata.clone();
        claimed.evidence.render.pixel_width += 1;
        let asset = record(Some(claimed));
        assert_eq!(
            validate(&asset, &bytes).unwrap().status,
            SemanticStatus::Rejected,
            "даже на один пиксель шире целочисленной области при DSF 3 — противоречие"
        );
        let asset = record(Some(metadata));
        assert_eq!(
            validate(&asset, &bytes).unwrap().status,
            SemanticStatus::Rejected
        );
    }

    #[test]
    fn margins_must_contain_only_the_observed_background() {
        let metadata = valid_metadata("幽霊");
        let asset = record(Some(metadata.clone()));

        // Линия начинается внутри поля: графику не хватило места, край мог быть обрезан.
        assert_eq!(
            validate(&asset, &png_for(&metadata, -6)).unwrap().status,
            SemanticStatus::Rejected
        );

        let mut touching_edge = image::load_from_memory(&valid_png()).unwrap().to_rgba8();
        touching_edge.put_pixel(0, 20, image::Rgba([235, 235, 235, 255]));
        assert_eq!(
            validate(&asset, &encode(touching_edge)).unwrap().status,
            SemanticStatus::Rejected,
            "пиксель рисунка у края PNG означает возможное обрезание"
        );

        let mut foreign = image::load_from_memory(&valid_png()).unwrap().to_rgba8();
        let last = foreign.height() - 2;
        foreign.put_pixel(30, last, image::Rgba([200, 40, 40, 255]));
        assert_eq!(
            validate(&asset, &encode(foreign)).unwrap().status,
            SemanticStatus::Rejected,
            "чужое содержимое в нижнем поле не принимается"
        );

        let mut antialias = image::load_from_memory(&valid_png()).unwrap().to_rgba8();
        antialias.put_pixel(8, 20, image::Rgba([60, 70, 80, 255]));
        assert_eq!(
            validate(&asset, &encode(antialias)).unwrap().status,
            SemanticStatus::Verified,
            "один физический пиксель сглаживания на границе отрисовки допустим"
        );
    }

    #[test]
    fn multi_variant_entry_publishes_exactly_one_reproducibly_selected_graph() {
        let metadata = multi_variant_metadata();
        let selection = &metadata.evidence.variant_selection;
        assert_eq!(selection.detected_variant_count, 3);
        assert_eq!(metadata.evidence.render.captured_graph_count, 1);
        let selected = &selection.variants[selection.selected_index as usize];
        assert_eq!(metadata.evidence.render.selector, selected.selector);
        let bytes = png_for(&metadata, 3);
        let asset = record(Some(metadata.clone()));
        assert_eq!(
            validate(&asset, &bytes).unwrap().status,
            SemanticStatus::Verified
        );

        let other = (selection.selected_index + 1) % 3;
        let mut wrong_choice = metadata.clone();
        wrong_choice.evidence.variant_selection.selected_index = other;
        wrong_choice.evidence.render.selector = wrong_choice.evidence.variant_selection.variants
            [other as usize]
            .selector
            .clone();
        let asset = record(Some(wrong_choice));
        assert_eq!(
            validate(&asset, &bytes).unwrap().status,
            SemanticStatus::Rejected,
            "выбор, не совпадающий с воспроизводимым, отвергается"
        );

        let mut forged_score = metadata.clone();
        forged_score.evidence.variant_selection.variants[0].selection_score = "0".repeat(64);
        let asset = record(Some(forged_score));
        assert_eq!(
            validate(&asset, &bytes).unwrap().status,
            SemanticStatus::Rejected
        );

        let mut forged_markup = metadata.clone();
        forged_markup.evidence.variant_selection.variants[1].markup_sha256 = "XYZ".into();
        let asset = record(Some(forged_markup));
        assert_eq!(
            validate(&asset, &bytes).unwrap().status,
            SemanticStatus::Rejected
        );

        let mut miscounted = metadata.clone();
        miscounted.evidence.variant_selection.detected_variant_count = 2;
        let asset = record(Some(miscounted));
        assert_eq!(
            validate(&asset, &bytes).unwrap().status,
            SemanticStatus::Rejected
        );

        // Удаление невыбранного варианта не в конце списка разрывает индексы DOM.
        let dropped = (0..3)
            .find(|index| *index != selection.selected_index && *index != 2)
            .unwrap();
        let mut dropped_variant = metadata.clone();
        dropped_variant
            .evidence
            .variant_selection
            .variants
            .retain(|variant| variant.index != dropped);
        dropped_variant
            .evidence
            .variant_selection
            .detected_variant_count = 2;
        let asset = record(Some(dropped_variant));
        assert_eq!(
            validate(&asset, &bytes).unwrap().status,
            SemanticStatus::Rejected,
            "удаление варианта из свидетельств нарушает порядок DOM"
        );

        let mut two_graphs = metadata.clone();
        two_graphs.evidence.render.captured_graph_count = 2;
        let asset = record(Some(two_graphs));
        assert_eq!(
            validate(&asset, &bytes).unwrap().status,
            SemanticStatus::Rejected
        );

        let mut other_selector = metadata.clone();
        other_selector.evidence.render.selector = metadata.evidence.variant_selection.variants
            [other as usize]
            .selector
            .clone();
        let asset = record(Some(other_selector));
        assert_eq!(
            validate(&asset, &bytes).unwrap().status,
            SemanticStatus::Rejected
        );

        let mut duplicate_selector = metadata.clone();
        duplicate_selector.evidence.variant_selection.variants[other as usize].selector =
            selected.selector.clone();
        let asset = record(Some(duplicate_selector));
        assert_eq!(
            validate(&asset, &bytes).unwrap().status,
            SemanticStatus::Rejected
        );

        let mut empty = metadata;
        empty.evidence.variant_selection.variants.clear();
        empty.evidence.variant_selection.detected_variant_count = 0;
        let asset = record(Some(empty));
        assert_eq!(
            validate(&asset, &bytes).unwrap().status,
            SemanticStatus::Uncertain
        );
    }

    #[test]
    fn capture_padding_must_not_reach_a_neighbouring_variant() {
        // Соседний график в 2 CSS px ниже: поле в 3 CSS px его задело бы.
        let metadata = metadata_with_variants(
            "幽霊",
            vec![
                variant(0, "<div>a</div>", rect(100.0, 200.0, 20.0, 10.0)),
                variant(1, "<div>b</div>", rect(100.0, 212.0, 20.0, 10.0)),
            ],
        );
        let bytes = png_for(&metadata, 3);
        let asset = record(Some(metadata));
        assert_eq!(
            validate(&asset, &bytes).unwrap().status,
            SemanticStatus::Rejected
        );
    }

    #[test]
    fn variant_selection_is_order_independent_seeded_and_resolves_ties_by_dom_order() {
        let markups: Vec<String> = (0..4)
            .map(|index| sha256_hex(format!("variant-{index}").as_bytes()))
            .collect();
        let choose = |id: u64, order: &[usize]| {
            let ordered: Vec<&str> = order.iter().map(|index| markups[*index].as_str()).collect();
            order[select_pitch_accent_variant(id, ordered.iter().copied()).unwrap()]
        };
        for id in 1..64 {
            let chosen = choose(id, &[0, 1, 2, 3]);
            assert_eq!(
                chosen,
                choose(id, &[3, 2, 1, 0]),
                "порядок DOM не влияет на выбор"
            );
            assert_eq!(chosen, choose(id, &[2, 0, 3, 1]));
            assert_eq!(
                chosen,
                choose(id, &[0, 1, 2, 3]),
                "повтор даёт тот же выбор"
            );
            // Добавление варианта либо сохраняет прежний выбор, либо выбирает новый.
            let extended = choose(id, &[0, 1, 2]);
            assert!(chosen == extended || chosen == 3);
        }
        let first_positions: BTreeSet<usize> =
            (1..64).map(|id| choose(id, &[0, 1, 2, 3])).collect();
        assert!(
            first_positions.len() > 1,
            "выбор не сводится к первому элементу: {first_positions:?}"
        );
        assert_eq!(
            select_pitch_accent_variant(7, [markups[1].as_str(), markups[1].as_str()]),
            Some(0),
            "одинаковая разметка даёт равные оценки; выбирается меньший индекс DOM"
        );
        assert_eq!(select_pitch_accent_variant(7, std::iter::empty()), None);
        assert_ne!(
            pitch_accent_variant_selection_score(1, &markups[0]),
            pitch_accent_variant_selection_score(2, &markups[0]),
            "оценка зависит от ID словарной записи"
        );
    }

    #[test]
    fn dark_capture_must_contain_observed_background_and_graph_pixels() {
        let asset = record(Some(valid_metadata("幽霊")));
        assert_eq!(
            validate(&asset, &valid_png()).unwrap().status,
            SemanticStatus::Verified
        );

        let metadata = valid_metadata("幽霊");
        let (width, height) = (
            metadata.evidence.render.pixel_width,
            metadata.evidence.render.pixel_height,
        );
        let mut light = metadata;
        light.evidence.render.dark_theme.background_rgb = [255, 255, 255];
        let asset = record(Some(light));
        assert_eq!(
            validate(&asset, &white_png(width, height)).unwrap().status,
            SemanticStatus::Rejected,
            "флаги тёмной темы не могут подтвердить светлый вычисленный фон"
        );

        let asset = record(Some(valid_metadata("幽霊")));
        assert_eq!(
            validate(&asset, &white_png(width, height)).unwrap().status,
            SemanticStatus::Rejected,
            "содержимое снимка должно включать наблюдённый тёмный фон и пиксели графика"
        );

        let mut image = image::RgbaImage::from_pixel(
            width,
            height,
            image::Rgba([BACKGROUND[0], BACKGROUND[1], BACKGROUND[2], 255]),
        );
        image.put_pixel(30, 24, image::Rgba([235, 235, 235, 255]));
        assert_eq!(
            validate(&asset, &encode(image)).unwrap().status,
            SemanticStatus::Rejected,
            "один случайный контрастный пиксель не подтверждает содержимое графика"
        );
    }

    #[test]
    fn transparent_pixel_anywhere_is_rejected() {
        let mut image = image::load_from_memory(&valid_png()).unwrap().to_rgba8();
        let (width, height) = image.dimensions();
        image.put_pixel(width - 1, height - 1, image::Rgba([24, 36, 48, 0]));

        let asset = record(Some(valid_metadata("幽霊")));
        assert_eq!(
            validate(&asset, &encode(image)).unwrap().status,
            SemanticStatus::Rejected
        );
    }

    #[test]
    fn alpha_254_is_not_fully_opaque() {
        let mut image = image::load_from_memory(&valid_png()).unwrap().to_rgba8();
        for pixel in image.pixels_mut() {
            pixel.0[3] = 254;
        }

        let asset = record(Some(valid_metadata("幽霊")));
        assert_eq!(
            validate(&asset, &encode(image)).unwrap().status,
            SemanticStatus::Rejected
        );
    }

    #[test]
    fn one_background_pixel_and_eight_contrasting_pixels_satisfy_capture_evidence() {
        let proof = valid_metadata("幽霊").evidence.render.dark_theme;
        let mut image = image::RgbaImage::from_pixel(60, 30, image::Rgba([100, 100, 100, 255]));
        image.put_pixel(0, 0, image::Rgba([24, 36, 48, 255]));

        assert!(
            validate_capture_background(&proof, &image::DynamicImage::ImageRgba8(image)).is_ok(),
            "исходный контракт требует один совпавший пиксель фона и не менее восьми контрастных"
        );
    }

    #[test]
    fn invalid_png_bytes_are_corrupt_even_with_valid_evidence() {
        let asset = record(Some(valid_metadata("幽霊")));
        let decision = validate(&asset, b"\x89PNG\r\n\x1a\nnot a complete png").unwrap();

        assert_eq!(decision.status, SemanticStatus::Corrupt);
    }

    #[test]
    fn oversized_input_is_rejected_after_reading_only_limit_plus_one_byte() {
        struct CountingReader {
            remaining: usize,
            bytes_read: usize,
        }

        impl Read for CountingReader {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                let count = output.len().min(self.remaining);
                output[..count].fill(0);
                self.remaining -= count;
                self.bytes_read += count;
                Ok(count)
            }
        }

        let asset = record(Some(valid_metadata("幽霊")));
        let mut reader = CountingReader {
            remaining: PITCH_ACCENT_MAX_ASSET_BYTES as usize + 128,
            bytes_read: 0,
        };

        let decision = PitchAccentImageValidator
            .validate(&asset, &mut reader)
            .unwrap();

        assert_eq!(decision.status, SemanticStatus::Corrupt);
        assert_eq!(decision.evidence[0].kind, "pitch_accent_png_too_large");
        assert_eq!(reader.bytes_read, PITCH_ACCENT_MAX_ASSET_BYTES as usize + 1);
    }

    #[test]
    fn png_dimensions_above_limit_are_corrupt() {
        let asset = record(Some(valid_metadata("幽霊")));
        let oversized = rgb_png(PITCH_ACCENT_MAX_IMAGE_DIMENSION + 1, 1);

        let decision = validate(&asset, &oversized).unwrap();

        assert_eq!(decision.status, SemanticStatus::Corrupt);
        assert_eq!(decision.evidence[0].kind, "pitch_accent_png_decode_failed");
    }

    #[test]
    fn png_decode_allocation_above_limit_is_corrupt() {
        let asset = record(Some(valid_metadata("幽霊")));
        let oversized = rgba_png(
            PITCH_ACCENT_MAX_IMAGE_DIMENSION,
            PITCH_ACCENT_MAX_IMAGE_DIMENSION,
        );

        let decision = validate(&asset, &oversized).unwrap();

        assert_eq!(decision.status, SemanticStatus::Corrupt);
        assert_eq!(decision.evidence[0].kind, "pitch_accent_png_decode_failed");
        let message = decision.evidence[0].details.as_ref().unwrap()["message"]
            .as_str()
            .unwrap();
        assert!(message.contains("декодированное изображение потребует"));
    }
}
