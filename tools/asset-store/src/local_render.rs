//! Локальный рендер PNG кандзи в визуальном стиле плитки Yarxi.
//!
//! Генератор не обращается к Yarxi и не использует эталонные изображения: он
//! строит одну и ту же DOM-плитку 80×80 CSS px для любого символа, рисует её
//! закреплённым локальным файлом шрифта в изолированном Chromium и снимает
//! точный clip в масштабе устройства 2,175, что даёт нативные 174×174 без
//! ресайза. После снимка к внутренней области `[8, 166)` применяется явная
//! общая коррекция покрытия; рамка остаётся нетронутой. Сырой снимок без
//! коррекции сохраняется отдельно для диагностики.
//!
//! Геометрия захвата — это фаза растровой сетки, а не свойство символа:
//! профиль `B` используется по умолчанию для любого символа, профиль `A`
//! доступен только как явный выбор вызывающего. Таблицы «символ → фаза» нет.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine as _;
use chromiumoxide::cdp::browser_protocol::css::{
    EnableParams as CssEnableParams, GetPlatformFontsForNodeParams,
};
use chromiumoxide::cdp::browser_protocol::dom::{
    EnableParams as DomEnableParams, GetDocumentParams, NodeId, QuerySelectorParams,
};
use chromiumoxide::cdp::browser_protocol::fetch::{
    EnableParams as FetchEnableParams, EventRequestPaused, FailRequestParams, RequestPattern,
};
use chromiumoxide::cdp::browser_protocol::network::ErrorReason;
use chromiumoxide::cdp::browser_protocol::page::{
    CaptureScreenshotFormat, CaptureScreenshotParams, Viewport,
};
use chromiumoxide::cdp::js_protocol::runtime::EvaluateParams;
use chromiumoxide::page::Page;
use futures::StreamExt;
use image::{ImageFormat, RgbImage};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use skrifa::{FontRef, MetadataProvider, string::StringId};

use crate::browser_runtime::{
    BrowserRuntimeConfig, BrowserRuntimeProvenance, BrowserSession, DeviceMetrics,
};
use crate::hashing::sha256_hex;
use crate::kanji_domain::parse_kanji_character;

/// Версия алгоритма рендера: меняется при любом изменении DOM, захвата или коррекции.
pub const LOCAL_RENDER_ALGORITHM: &str = "kanji-local-png-v1";
/// Тип источника в `Provenance`: локальный рендер, а не захват Yarxi.
pub const LOCAL_RENDER_SOURCE_KIND: &str = "local-render";
/// Версия провайдера в доменных метаданных.
pub const LOCAL_RENDER_PROVIDER_VERSION: &str = "kanji-local-png-v1";
/// SHA-256 откалиброванного файла Noto Serif JP Regular 1.001 (OTF).
pub const CALIBRATED_FONT_SHA256: &str =
    "c837828057fff744236d2df79f872d41b609cc011a8a7e768a2e4371c6a64a09";
/// Размер итогового PNG; размер не нормализуется ресайзом.
pub const OUTPUT_EDGE_PX: u32 = 174;
const TILE_CSS_PX: f64 = 80.0;
const VIEWPORT_WIDTH: i64 = 1280;
const VIEWPORT_HEIGHT: i64 = 900;
const DEVICE_SCALE_FACTOR: f64 = 2.175;
const FONT_SIZE_CSS_PX: f64 = 70.0;
const GLYPH_DX_CSS_PX: f64 = 0.3125;
const GLYPH_DY_CSS_PX: f64 = -4.0;
/// Общая коррекция покрытия для растеризатора Linux-Chromium.
///
/// Исследование на Windows (DirectWrite) подобрало 2,35. Linux-Chromium
/// (FreeType/Skia) рисует те же контуры с более светлыми краями, и перенос
/// 2,35 истончает глиф. По той же методике (подбор только на 10 замороженных
/// калибровочных символах, проверка на 15 отложенных, единые dx/dy/размер)
/// для Linux выбрано 1,10. Значение одно для всех символов.
const COVERAGE_GAMMA: f64 = 1.10;
const COVERAGE_INSET_PX: u32 = 8;
const COVERAGE_BACKGROUND: f64 = 34.0;
const COVERAGE_RANGE: f64 = 221.0;
const FONT_FAMILY: &str = "KanjiLocalRenderFont";
const MAX_FONT_BYTES: u64 = 64 * 1024 * 1024;
const RENDER_STEP_TIMEOUT: Duration = Duration::from_secs(30);

/// Фаза растровой сетки захвата: координаты начала плитки в CSS px.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum GeometryProfile {
    /// Штатный профиль, начало плитки (21,0625; 0,0625) CSS px.
    B,
    /// Дополнительный явный профиль, начало плитки (5,0625; 4,9375) CSS px.
    A,
}

impl GeometryProfile {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::A => "a",
            Self::B => "b",
        }
    }

    /// Начало плитки в CSS px.
    pub const fn origin(self) -> (f64, f64) {
        match self {
            Self::A => (5.0625, 4.9375),
            Self::B => (21.0625, 0.0625),
        }
    }
}

/// Проверенный локальный файл шрифта, встраиваемый в страницу через data URL.
#[derive(Clone)]
pub struct LocalFont {
    bytes: Arc<Vec<u8>>,
    pub sha256: String,
    pub file_name: String,
    pub version: String,
    pub family_name: String,
    pub calibrated: bool,
}

impl std::fmt::Debug for LocalFont {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LocalFont")
            .field("sha256", &self.sha256)
            .field("file_name", &self.file_name)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

impl LocalFont {
    /// Читает файл шрифта и требует откалиброванный SHA, если не разрешено иное.
    ///
    /// Другой файл допускается только явно (`allow_uncalibrated`) и честно
    /// помечается неоткалиброванным в свидетельствах.
    pub fn load(path: &Path, allow_uncalibrated: bool) -> Result<Self, String> {
        let metadata = std::fs::metadata(path)
            .map_err(|error| format!("font_unavailable: {}: {error}", path.display()))?;
        if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_FONT_BYTES {
            return Err(format!(
                "font_unavailable: {} не является обычным файлом допустимого размера",
                path.display()
            ));
        }
        let bytes =
            std::fs::read(path).map_err(|error| format!("font_unavailable: чтение: {error}"))?;
        Self::from_bytes(
            bytes,
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            allow_uncalibrated,
        )
    }

    pub fn from_bytes(
        bytes: Vec<u8>,
        file_name: String,
        allow_uncalibrated: bool,
    ) -> Result<Self, String> {
        let sha256 = sha256_hex(&bytes);
        let calibrated = sha256 == CALIBRATED_FONT_SHA256;
        if !calibrated && !allow_uncalibrated {
            return Err(format!(
                "font_not_calibrated: SHA-256 шрифта {sha256} не совпадает с откалиброванным {CALIBRATED_FONT_SHA256}"
            ));
        }
        if !(bytes.starts_with(b"OTTO") || bytes.starts_with(&[0, 1, 0, 0])) {
            return Err("font_unsupported: ожидается один файл OpenType/TrueType (OTF/TTF)".into());
        }
        let font = FontRef::new(&bytes)
            .map_err(|error| format!("font_unsupported: разбор OpenType: {error}"))?;
        let name = |id| {
            font.localized_strings(id)
                .english_or_first()
                .map(|value| value.chars().collect::<String>())
                .unwrap_or_default()
        };
        let version = name(StringId::VERSION_STRING);
        let family_name = name(StringId::FAMILY_NAME);
        Ok(Self {
            bytes: Arc::new(bytes),
            sha256,
            file_name,
            version,
            family_name,
            calibrated,
        })
    }

    /// Есть ли у символа собственный глиф в `cmap` этого файла.
    pub fn has_glyph(&self, character: char) -> bool {
        FontRef::new(&self.bytes)
            .ok()
            .and_then(|font| font.charmap().map(character))
            .is_some_and(|glyph| glyph.to_u32() != 0)
    }

    fn mime(&self) -> &'static str {
        if self.bytes.starts_with(b"OTTO") {
            "font/otf"
        } else {
            "font/ttf"
        }
    }

    fn css_rule(&self) -> String {
        format!(
            "@font-face{{font-family:'{FONT_FAMILY}';src:url('data:{};base64,{}');font-weight:400;font-style:normal;font-display:block;}}",
            self.mime(),
            base64::engine::general_purpose::STANDARD.encode(self.bytes.as_slice())
        )
    }
}

/// Сведения о шрифте в свидетельствах; содержимое файла не сохраняется.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FontEvidence {
    pub sha256: String,
    pub file_name: String,
    pub family_name: String,
    pub version: String,
    pub calibrated: bool,
}

impl From<&LocalFont> for FontEvidence {
    fn from(font: &LocalFont) -> Self {
        Self {
            sha256: font.sha256.clone(),
            file_name: font.file_name.clone(),
            family_name: font.family_name.clone(),
            version: font.version.clone(),
            calibrated: font.calibrated,
        }
    }
}

/// Неизменяемые параметры рендера, записываемые вместе с результатом.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderSettings {
    pub algorithm: String,
    pub geometry_profile: GeometryProfile,
    pub origin_css: [f64; 2],
    pub tile_css: [f64; 2],
    pub viewport_css: [i64; 2],
    pub device_scale_factor: f64,
    pub font_size_css: f64,
    pub glyph_translate_css: [f64; 2],
    pub line_height: String,
    pub font_weight: u16,
    pub background: String,
    pub foreground: String,
    pub border: String,
    pub capture: String,
    pub coverage_correction: CoverageCorrection,
}

/// Явная коррекция покрытия после снимка.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoverageCorrection {
    pub gamma: f64,
    pub inset_px: u32,
    pub formula: String,
}

impl RenderSettings {
    pub fn for_profile(profile: GeometryProfile) -> Self {
        let (x, y) = profile.origin();
        Self {
            algorithm: LOCAL_RENDER_ALGORITHM.into(),
            geometry_profile: profile,
            origin_css: [x, y],
            tile_css: [TILE_CSS_PX, TILE_CSS_PX],
            viewport_css: [VIEWPORT_WIDTH, VIEWPORT_HEIGHT],
            device_scale_factor: DEVICE_SCALE_FACTOR,
            font_size_css: FONT_SIZE_CSS_PX,
            glyph_translate_css: [GLYPH_DX_CSS_PX, GLYPH_DY_CSS_PX],
            line_height: "1".into(),
            font_weight: 400,
            background: "#222222".into(),
            foreground: "#ffffff".into(),
            border: "1px solid #808080; box-sizing: border-box".into(),
            capture: "CDP Page.captureScreenshot, точный clip плитки, scale=1, без ресайза".into(),
            coverage_correction: CoverageCorrection {
                gamma: COVERAGE_GAMMA,
                inset_px: COVERAGE_INSET_PX,
                formula: "round_half_even(34 + 221 * clamp((c - 34) / 221, 0, 1) ^ gamma) для RGB внутри [8, 166)".into(),
            },
        }
    }
}

/// Фактический шрифт платформы, которым Chromium нарисовал глиф.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformFontEvidence {
    pub family_name: String,
    pub post_script_name: String,
    pub is_custom_font: bool,
    pub glyph_count: f64,
}

/// Свидетельства одного рендера.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderEvidence {
    pub character: String,
    pub settings: RenderSettings,
    pub font: FontEvidence,
    pub browser_product: String,
    pub browser_revision: String,
    pub platform_fonts: Vec<PlatformFontEvidence>,
    pub tile_rect_css: [f64; 4],
    pub glyph_rect_css: [f64; 4],
    pub measured_device_pixel_ratio: f64,
    pub fonts_check: bool,
    pub raw_sha256: String,
    pub corrected_sha256: String,
    pub blocked_network_requests: u64,
}

/// Результат рендера одного символа.
#[derive(Debug, Clone)]
pub struct RenderedKanji {
    /// PNG после явной коррекции покрытия — кандидат на публикацию.
    pub png: Vec<u8>,
    /// Сырой снимок Chromium без коррекции, только для диагностики.
    pub raw_png: Vec<u8>,
    pub evidence: RenderEvidence,
}

/// Изолированный Chromium с загруженной страницей плитки; переиспользуется для
/// многих символов.
pub struct LocalRenderer {
    session: BrowserSession,
    font: LocalFont,
    settings: RenderSettings,
    glyph_node: NodeId,
    blocked_requests: Arc<AtomicU64>,
    interceptor: tokio::task::JoinHandle<()>,
}

impl LocalRenderer {
    /// Запускает браузер, блокирует все сетевые запросы и загружает плитку со шрифтом.
    pub async fn launch(
        font: LocalFont,
        profile: GeometryProfile,
        workspace: Option<&Path>,
    ) -> Result<Self, String> {
        let config = BrowserRuntimeConfig {
            device_metrics: Some(DeviceMetrics::new(
                VIEWPORT_WIDTH,
                VIEWPORT_HEIGHT,
                DEVICE_SCALE_FACTOR,
            )?),
            prefers_color_scheme: Some("dark".into()),
            launch_timeout: Duration::from_secs(60),
            request_timeout: Duration::from_secs(60),
        };
        let session = match workspace {
            Some(workspace) => BrowserSession::launch_in_workspace(config, workspace).await,
            None => BrowserSession::launch(config).await,
        }
        .map_err(|error| format!("browser_unavailable: {error}"))?;
        match Self::prepare(session, font, profile).await {
            Ok(renderer) => Ok(renderer),
            Err(failure) => {
                let (session, message) = *failure;
                let _ = session.close().await;
                Err(message)
            }
        }
    }

    async fn prepare(
        session: BrowserSession,
        font: LocalFont,
        profile: GeometryProfile,
    ) -> Result<Self, Box<(BrowserSession, String)>> {
        let page = session.page().clone();
        let blocked_requests = Arc::new(AtomicU64::new(0));
        let interceptor = match block_network(&page, Arc::clone(&blocked_requests)).await {
            Ok(task) => task,
            Err(message) => return Err(Box::new((session, message))),
        };
        let settings = RenderSettings::for_profile(profile);
        let prepared = async {
            timed(page.set_content(page_html(&font, &settings)), "set_content").await?;
            page.execute(DomEnableParams::default())
                .await
                .map_err(|error| format!("CDP DOM.enable: {error}"))?;
            page.execute(CssEnableParams::default())
                .await
                .map_err(|error| format!("CDP CSS.enable: {error}"))?;
            let document = page
                .execute(GetDocumentParams::default())
                .await
                .map_err(|error| format!("CDP DOM.getDocument: {error}"))?;
            let node = page
                .execute(QuerySelectorParams::new(
                    document.result.root.node_id,
                    "#glyph",
                ))
                .await
                .map_err(|error| format!("CDP DOM.querySelector: {error}"))?;
            Ok::<_, String>(node.result.node_id)
        }
        .await;
        match prepared {
            Ok(glyph_node) => Ok(Self {
                session,
                font,
                settings,
                glyph_node,
                blocked_requests,
                interceptor,
            }),
            Err(message) => {
                interceptor.abort();
                Err(Box::new((
                    session,
                    format!("browser_render_setup: {message}"),
                )))
            }
        }
    }

    pub fn settings(&self) -> &RenderSettings {
        &self.settings
    }

    pub fn provenance(&self) -> &BrowserRuntimeProvenance {
        self.session.provenance()
    }

    /// Рендерит один символ. Отсутствие глифа, подстановка системного шрифта,
    /// неверная геометрия или размер дают ошибку, а не «успешную» картинку.
    pub async fn render(&mut self, character: &str) -> Result<RenderedKanji, String> {
        parse_kanji_character(character).map_err(|error| format!("invalid_character: {error}"))?;
        let scalar = character.chars().next().expect("проверен один символ");
        if !self.font.has_glyph(scalar) {
            return Err(format!(
                "glyph_missing: в шрифте {} нет глифа для U+{:04X}",
                self.font.sha256,
                u32::from(scalar)
            ));
        }
        let page = self.session.page().clone();
        let blocked_before = self.blocked_requests.load(Ordering::SeqCst);
        let [origin_x, origin_y] = self.settings.origin_css;
        let script = format!(
            r#"(async () => {{
                const character = {character};
                const glyph = document.getElementById('glyph');
                const tile = document.getElementById('tile');
                glyph.textContent = character;
                tile.style.marginLeft = '{origin_x}px';
                tile.style.marginTop = '{origin_y}px';
                const font = getComputedStyle(glyph).font;
                await document.fonts.load(font, character);
                await document.fonts.ready;
                await new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)));
                const rect = node => {{ const r = node.getBoundingClientRect(); return [r.x, r.y, r.width, r.height]; }};
                return {{
                    text: glyph.textContent,
                    check: document.fonts.check(`${{getComputedStyle(glyph).fontSize}} '{FONT_FAMILY}'`, character),
                    tile: rect(tile),
                    glyph: rect(glyph),
                    dpr: window.devicePixelRatio,
                    scale: window.visualViewport ? window.visualViewport.scale : 1,
                    family: getComputedStyle(glyph).fontFamily,
                }};
            }})()"#,
            character = serde_json::to_string(character).expect("строка сериализуется"),
        );
        let evaluation = EvaluateParams::builder()
            .expression(script)
            .await_promise(true)
            .return_by_value(true)
            .build()
            .map_err(|error| format!("browser_render_state: {error}"))?;
        let state: Value = timed(page.evaluate(evaluation), "render_prepare")
            .await?
            .into_value()
            .map_err(|error| format!("browser_render_state: {error}"))?;
        if state["text"].as_str() != Some(character) {
            return Err(format!(
                "browser_render_state: текст плитки не совпал с символом: {state}"
            ));
        }
        let fonts_check = state["check"].as_bool().unwrap_or(false);
        if !fonts_check {
            return Err(
                "font_not_loaded: document.fonts.check не подтвердил локальный шрифт".into(),
            );
        }
        let tile = rect4(&state["tile"])?;
        let glyph = rect4(&state["glyph"])?;
        let dpr = state["dpr"].as_f64().unwrap_or(0.0);
        if (dpr - DEVICE_SCALE_FACTOR).abs() > 1e-6
            || (state["scale"].as_f64().unwrap_or(0.0) - 1.0).abs() > 1e-9
        {
            return Err(format!(
                "browser_scale_mismatch: devicePixelRatio={dpr}, ожидалось {DEVICE_SCALE_FACTOR}"
            ));
        }
        if (tile[0] - origin_x).abs() > 1e-9
            || (tile[1] - origin_y).abs() > 1e-9
            || (tile[2] - TILE_CSS_PX).abs() > 1e-9
            || (tile[3] - TILE_CSS_PX).abs() > 1e-9
        {
            return Err(format!(
                "geometry_mismatch: фактическая плитка {tile:?} не совпала с профилем {}",
                self.settings.geometry_profile.as_str()
            ));
        }
        let fonts = page
            .execute(GetPlatformFontsForNodeParams::new(self.glyph_node))
            .await
            .map_err(|error| format!("CDP CSS.getPlatformFontsForNode: {error}"))?;
        let platform_fonts: Vec<PlatformFontEvidence> = fonts
            .result
            .fonts
            .iter()
            .map(|font| PlatformFontEvidence {
                family_name: font.family_name.clone(),
                post_script_name: font.post_script_name.clone(),
                is_custom_font: font.is_custom_font,
                glyph_count: font.glyph_count,
            })
            .collect();
        if platform_fonts.len() != 1
            || !platform_fonts[0].is_custom_font
            || platform_fonts[0].glyph_count != 1.0
        {
            return Err(format!(
                "font_fallback: глиф нарисован не только локальным шрифтом: {}",
                serde_json::to_string(&platform_fonts).unwrap_or_default()
            ));
        }
        let screenshot = CaptureScreenshotParams {
            format: Some(CaptureScreenshotFormat::Png),
            clip: Some(Viewport {
                x: tile[0],
                y: tile[1],
                width: TILE_CSS_PX,
                height: TILE_CSS_PX,
                scale: 1.0,
            }),
            from_surface: Some(true),
            capture_beyond_viewport: Some(false),
            ..CaptureScreenshotParams::default()
        };
        let captured = timed(page.execute(screenshot), "capture_screenshot").await?;
        let raw_bytes = base64::engine::general_purpose::STANDARD
            .decode(captured.result.data.as_ref() as &str)
            .map_err(|error| format!("browser_capture_decode: {error}"))?;
        let raw = decode_capture(&raw_bytes)?;
        let blocked = self.blocked_requests.load(Ordering::SeqCst) - blocked_before;
        if blocked != 0 {
            return Err(format!(
                "network_attempt: страница рендера пыталась выполнить {blocked} сетевых запросов"
            ));
        }
        let mut corrected = raw.clone();
        apply_coverage_correction(&mut corrected);
        let raw_png = encode_png(&raw)?;
        let png = encode_png(&corrected)?;
        let provenance = self.session.provenance();
        let evidence = RenderEvidence {
            character: character.into(),
            settings: self.settings.clone(),
            font: FontEvidence::from(&self.font),
            browser_product: provenance.product.clone(),
            browser_revision: provenance.revision.clone(),
            platform_fonts,
            tile_rect_css: tile,
            glyph_rect_css: glyph,
            measured_device_pixel_ratio: dpr,
            fonts_check,
            raw_sha256: sha256_hex(&raw_png),
            corrected_sha256: sha256_hex(&png),
            blocked_network_requests: blocked,
        };
        Ok(RenderedKanji {
            png,
            raw_png,
            evidence,
        })
    }

    pub async fn close(self) -> Result<(), String> {
        self.interceptor.abort();
        self.session.close().await
    }
}

fn page_html(font: &LocalFont, settings: &RenderSettings) -> String {
    let [dx, dy] = settings.glyph_translate_css;
    format!(
        "<!doctype html><html lang='ja'><head><meta charset='utf-8'><style>{}\
         html{{color-scheme:dark}}body{{margin:0;background:#222;color:#fff;}}\
         #tile{{box-sizing:border-box;display:flex;align-items:center;justify-content:center;\
         width:80px;height:80px;border:1px solid #808080;background:#222;color:#fff;overflow:hidden;}}\
         #glyph{{display:block;white-space:pre;font-family:'{FONT_FAMILY}';font-size:{}px;\
         font-style:normal;font-weight:400;line-height:1;transform:translate({dx}px,{dy}px);}}\
         </style></head><body><div id='tile' class='font-sample'><span id='glyph'>一</span></div></body></html>",
        font.css_rule(),
        settings.font_size_css,
    )
}

async fn block_network(
    page: &Page,
    counter: Arc<AtomicU64>,
) -> Result<tokio::task::JoinHandle<()>, String> {
    let mut events = page
        .event_listener::<EventRequestPaused>()
        .await
        .map_err(|error| format!("CDP Fetch.requestPaused: {error}"))?;
    page.execute(
        FetchEnableParams::builder()
            .pattern(RequestPattern::builder().url_pattern("*").build())
            .build(),
    )
    .await
    .map_err(|error| format!("CDP Fetch.enable: {error}"))?;
    let page = page.clone();
    Ok(tokio::spawn(async move {
        while let Some(event) = events.next().await {
            counter.fetch_add(1, Ordering::SeqCst);
            let _ = page
                .execute(FailRequestParams::new(
                    event.request_id.clone(),
                    ErrorReason::BlockedByClient,
                ))
                .await;
        }
    }))
}

async fn timed<T, E: std::fmt::Display>(
    future: impl std::future::Future<Output = Result<T, E>>,
    stage: &str,
) -> Result<T, String> {
    match tokio::time::timeout(RENDER_STEP_TIMEOUT, future).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(format!("browser_{stage}: {error}")),
        Err(_) => Err(format!("browser_timeout: этап {stage} превысил предел")),
    }
}

fn rect4(value: &Value) -> Result<[f64; 4], String> {
    let values: Vec<f64> = value
        .as_array()
        .map(|items| items.iter().filter_map(Value::as_f64).collect())
        .unwrap_or_default();
    values
        .try_into()
        .map_err(|_| "browser_render_state: прямоугольник DOM не получен".into())
}

fn decode_capture(bytes: &[u8]) -> Result<RgbImage, String> {
    let decoded = image::load_from_memory_with_format(bytes, ImageFormat::Png)
        .map_err(|error| format!("browser_capture_decode: {error}"))?;
    if decoded.width() != OUTPUT_EDGE_PX || decoded.height() != OUTPUT_EDGE_PX {
        return Err(format!(
            "capture_size_mismatch: Chromium вернул {}×{} вместо {OUTPUT_EDGE_PX}×{OUTPUT_EDGE_PX}; размер не нормализуется",
            decoded.width(),
            decoded.height()
        ));
    }
    let rgba = decoded.to_rgba8();
    if rgba.pixels().any(|pixel| pixel[3] != 255) {
        return Err("capture_transparent: снимок содержит прозрачные пиксели".into());
    }
    Ok(image::DynamicImage::ImageRgba8(rgba).to_rgb8())
}

/// Явная коррекция покрытия внутри `[8, 166)`; рамка и внешняя область не меняются.
pub fn apply_coverage_correction(image: &mut RgbImage) {
    let inset = COVERAGE_INSET_PX;
    let (width, height) = image.dimensions();
    if width <= inset * 2 || height <= inset * 2 {
        return;
    }
    for y in inset..height - inset {
        for x in inset..width - inset {
            let pixel = image.get_pixel_mut(x, y);
            for channel in pixel.0.iter_mut() {
                *channel = correct_channel(*channel);
            }
        }
    }
}

fn correct_channel(value: u8) -> u8 {
    let alpha = ((f64::from(value) - COVERAGE_BACKGROUND) / COVERAGE_RANGE).clamp(0.0, 1.0);
    let corrected =
        (COVERAGE_BACKGROUND + COVERAGE_RANGE * alpha.powf(COVERAGE_GAMMA)).round_ties_even();
    corrected.clamp(0.0, 255.0) as u8
}

fn encode_png(image: &RgbImage) -> Result<Vec<u8>, String> {
    let mut bytes = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut bytes, ImageFormat::Png)
        .map_err(|error| format!("png_encode: {error}"))?;
    Ok(bytes.into_inner())
}

/// Диагностические метрики сравнения двух PNG 174×174 (как в исследовательском отчёте).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageComparison {
    pub mae: f64,
    pub pixel_similarity: f64,
    pub glyph_iou: f64,
    pub exact_pixel_fraction: f64,
    pub reference_area: u64,
    pub generated_area: u64,
    pub border_mae: f64,
}

/// Сравнивает эталон и результат. Маска глифа — яркость ≥ 180 внутри `[8, 166)`.
pub fn compare_images(
    reference: &RgbImage,
    generated: &RgbImage,
) -> Result<ImageComparison, String> {
    if reference.dimensions() != generated.dimensions() {
        return Err("размеры изображений различаются".into());
    }
    let (width, height) = reference.dimensions();
    let inset = COVERAGE_INSET_PX;
    let mut absolute_sum = 0u64;
    let mut exact = 0u64;
    let mut both = 0u64;
    let mut either = 0u64;
    let mut reference_area = 0u64;
    let mut generated_area = 0u64;
    let mut border_sum = 0u64;
    let mut border_count = 0u64;
    let luma = |pixel: &image::Rgb<u8>| {
        // Совпадает с Pillow `convert("L")`: ITU-R 601-2 в целых с округлением.
        (u32::from(pixel[0]) * 19595
            + u32::from(pixel[1]) * 38470
            + u32::from(pixel[2]) * 7471
            + 0x8000)
            >> 16
    };
    for y in 0..height {
        for x in 0..width {
            let a = reference.get_pixel(x, y);
            let b = generated.get_pixel(x, y);
            let difference: u64 = (0..3)
                .map(|channel| u64::from(a[channel].abs_diff(b[channel])))
                .sum();
            absolute_sum += difference;
            exact += u64::from(a == b);
            let inner = x >= inset && y >= inset && x < width - inset && y < height - inset;
            if inner {
                let in_a = luma(a) >= 180;
                let in_b = luma(b) >= 180;
                both += u64::from(in_a && in_b);
                either += u64::from(in_a || in_b);
                reference_area += u64::from(in_a);
                generated_area += u64::from(in_b);
            } else {
                border_sum += difference;
                border_count += 3;
            }
        }
    }
    let channels = f64::from(width * height * 3);
    let mae = absolute_sum as f64 / channels;
    Ok(ImageComparison {
        mae,
        pixel_similarity: 1.0 - mae / 255.0,
        glyph_iou: if either == 0 {
            0.0
        } else {
            both as f64 / either as f64
        },
        exact_pixel_fraction: exact as f64 / f64::from(width * height),
        reference_area,
        generated_area,
        border_mae: if border_count == 0 {
            0.0
        } else {
            border_sum as f64 / border_count as f64
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_font_is_the_calibrated_file() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fonts/NotoSerifJP-Regular.otf");
        let font = LocalFont::load(&path, false).expect("шрифт репозитория откалиброван");
        assert_eq!(font.sha256, CALIBRATED_FONT_SHA256);
        assert!(font.has_glyph('漢'));
    }

    #[test]
    fn coverage_correction_keeps_frame_and_extremes() {
        assert_eq!(correct_channel(34), 34);
        assert_eq!(correct_channel(0), 34);
        assert_eq!(correct_channel(255), 255);
        // 34 + 221 * (110/221)^1.1 = 34 + 221 * 0.46420… = 136.59…
        assert_eq!(correct_channel(144), 137);
        let mut image = RgbImage::from_pixel(174, 174, image::Rgb([128, 128, 128]));
        apply_coverage_correction(&mut image);
        assert_eq!(image.get_pixel(0, 0).0, [128, 128, 128]);
        assert_eq!(image.get_pixel(7, 90).0, [128, 128, 128]);
        assert_eq!(image.get_pixel(166, 166).0, [128, 128, 128]);
        assert_ne!(image.get_pixel(8, 8).0, [128, 128, 128]);
        assert_ne!(image.get_pixel(165, 165).0, [128, 128, 128]);
    }

    #[test]
    fn profiles_are_fixed_capture_phases_without_character_table() {
        assert_eq!(GeometryProfile::B.origin(), (21.0625, 0.0625));
        assert_eq!(GeometryProfile::A.origin(), (5.0625, 4.9375));
        let settings = RenderSettings::for_profile(GeometryProfile::B);
        assert_eq!(settings.device_scale_factor * settings.tile_css[0], 174.0);
    }

    #[test]
    fn uncalibrated_font_requires_explicit_permission() {
        let error = LocalFont::from_bytes(b"OTTO".to_vec(), "x.otf".into(), false).unwrap_err();
        assert!(error.starts_with("font_not_calibrated"));
        let error =
            LocalFont::from_bytes(b"not a font".to_vec(), "x.otf".into(), true).unwrap_err();
        assert!(error.starts_with("font_unsupported"));
    }

    #[test]
    fn comparison_of_identical_images_is_exact() {
        let mut image = RgbImage::from_pixel(174, 174, image::Rgb([34, 34, 34]));
        for x in 40..120 {
            image.put_pixel(x, 80, image::Rgb([255, 255, 255]));
        }
        let comparison = compare_images(&image, &image).unwrap();
        assert_eq!(comparison.mae, 0.0);
        assert_eq!(comparison.glyph_iou, 1.0);
        assert_eq!(comparison.exact_pixel_fraction, 1.0);
        assert_eq!(comparison.reference_area, 80);
    }
}
