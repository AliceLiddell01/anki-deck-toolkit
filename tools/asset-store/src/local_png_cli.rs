//! Команды `kanji-assets local-png`: локальный рендер PNG кандзи без Yarxi.
//!
//! `render` только рисует файлы в новый каталог и ничего не публикует.

use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use clap::Subcommand;
use serde_json::json;

use crate::error::{AssetError, ErrorCode};
use crate::kanji_domain::parse_kanji_character;
use crate::local_render::{GeometryProfile, LocalFont, LocalRenderer, RenderedKanji};

/// Подкоманды локального рендера.
#[derive(Debug, Subcommand)]
pub enum LocalPngCommand {
    /// Рисует PNG указанных символов в новый каталог; хранилище не открывается.
    Render {
        /// Файл шрифта; по умолчанию — откалиброванный шрифт репозитория.
        #[arg(long)]
        font: Option<PathBuf>,
        /// Разрешить другой файл шрифта; результат помечается неоткалиброванным.
        #[arg(long)]
        allow_uncalibrated_font: bool,
        /// Фаза захвата; `b` — штатная, `a` — только явный выбор.
        #[arg(long, value_enum, default_value_t = GeometryProfile::B)]
        profile: GeometryProfile,
        /// Новый или пустой каталог результатов вне `decks/`.
        #[arg(long)]
        out: PathBuf,
        /// Дополнительно сохранить сырой снимок без коррекции покрытия.
        #[arg(long)]
        raw: bool,
        /// Число одновременно работающих браузеров (1–4).
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=4))]
        browsers: u8,
        /// Символы; каждый аргумент может содержать несколько символов подряд.
        #[arg(required = true, num_args = 1..)]
        characters: Vec<String>,
    },
    /// Возобновляемый массовый прогон: рендер, штатная проверка, слепой OCR и
    /// публикация прошедших PNG через хранилище. Повтор продолжает с места остановки.
    Run {
        /// Файл откалиброванного шрифта; по умолчанию — шрифт репозитория.
        #[arg(long)]
        font: Option<PathBuf>,
        /// Фаза захвата; `b` — штатная, `a` — только явный выбор.
        #[arg(long, value_enum, default_value_t = GeometryProfile::B)]
        profile: GeometryProfile,
        /// Рабочий каталог прогона вне `decks/`; хранит состояние каждой кодовой точки.
        #[arg(long)]
        work_dir: PathBuf,
        /// Число одновременно работающих браузеров (1–4).
        #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u8).range(1..=4))]
        browsers: u8,
        /// Взять цели из хранилища: каждый символ с доверенным GIF и с историческим PNG.
        #[arg(long)]
        from_corpus: bool,
        /// Обработать в этом прогоне не больше N незавершённых символов.
        #[arg(long)]
        limit: Option<usize>,
        /// Выполнить слепой OCR каждого сгенерированного PNG.
        #[arg(long)]
        ocr: bool,
        #[command(flatten)]
        ocr_options: crate::kanji_ocr::OcrOptions,
        /// Опубликовать PNG, прошедшие штатный валидатор и совпадение OCR.
        #[arg(long, requires = "ocr")]
        publish: bool,
        /// Дополнительные символы.
        #[arg(num_args = 0..)]
        characters: Vec<String>,
    },
    /// HTML-отчёт, машиночитаемая сводка, реестр проблем и проверка
    /// целостности по финальному состоянию хранилища; хранилище не меняется.
    Report {
        /// Рабочий каталог прогона.
        #[arg(long)]
        work_dir: PathBuf,
    },
    /// Снимок проверки для символов, не прошедших автоматические ворота:
    /// `decisions.tsv` и `index.html` в `review/<review_id>/`; хранилище не меняется.
    ReviewExport {
        /// Рабочий каталог прогона.
        #[arg(long)]
        work_dir: PathBuf,
    },
    /// Проверяет целиком и применяет решения указанного снимка: `approve`
    /// публикует точные байты с решением человека, `reject` оставляет их вне хранилища.
    ReviewApply {
        /// Рабочий каталог прогона.
        #[arg(long)]
        work_dir: PathBuf,
        /// Идентификатор снимка из `review-export`.
        #[arg(long)]
        review_id: String,
    },
    /// Диагностическое попиксельное сравнение двух каталогов PNG 174×174.
    ///
    /// Файлы сопоставляются по символу: `<символ>.png` или `U+XXXX.png`.
    Compare {
        /// Каталог эталонов.
        #[arg(long)]
        reference: PathBuf,
        /// Каталог сравниваемых изображений.
        #[arg(long)]
        generated: PathBuf,
    },
}

impl LocalPngCommand {
    /// Пути команды, у предков которых нужно искать защищённое дерево `decks/`.
    pub(crate) fn paths(&self) -> Vec<&Path> {
        match self {
            Self::Render { out, .. } => vec![out.as_path()],
            Self::Run { work_dir, .. }
            | Self::Report { work_dir }
            | Self::ReviewExport { work_dir }
            | Self::ReviewApply { work_dir, .. } => vec![work_dir.as_path()],
            Self::Compare {
                reference,
                generated,
            } => vec![reference.as_path(), generated.as_path()],
        }
    }

    pub fn operation(&self) -> &'static str {
        match self {
            Self::Render { .. } => "local-png render",
            Self::Run { .. } => "local-png run",
            Self::Report { .. } => "local-png report",
            Self::ReviewExport { .. } => "local-png review-export",
            Self::ReviewApply { .. } => "local-png review-apply",
            Self::Compare { .. } => "local-png compare",
        }
    }
}

/// Откалиброванный шрифт, хранящийся в репозитории (SIL OFL 1.1, `fonts/OFL.txt`).
pub(crate) const BUNDLED_FONT: &str = "tools/asset-store/fonts/NotoSerifJP-Regular.otf";

fn font_path(font: &Option<PathBuf>, repository_root: &Path) -> PathBuf {
    font.clone()
        .unwrap_or_else(|| repository_root.join(BUNDLED_FONT))
}

/// Символ из имени `<символ>.png` или `U+XXXX.png`.
pub(crate) fn character_from_file_name(name: &str) -> Option<String> {
    let stem = name.strip_suffix(".png")?;
    let character = match stem.strip_prefix("U+") {
        Some(hex) => char::from_u32(u32::from_str_radix(hex, 16).ok()?)?.to_string(),
        None => stem.to_owned(),
    };
    parse_kanji_character(&character).ok()?;
    Some(character)
}

fn read_png_directory(
    directory: &Path,
) -> Result<std::collections::BTreeMap<String, image::RgbImage>, AssetError> {
    let mut images = std::collections::BTreeMap::new();
    let entries = std::fs::read_dir(directory)
        .map_err(|error| AssetError::io("чтение каталога PNG", error))?;
    for entry in entries {
        let entry = entry.map_err(|error| AssetError::io("чтение каталога PNG", error))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(character) = character_from_file_name(&name) else {
            continue;
        };
        let bytes =
            std::fs::read(entry.path()).map_err(|error| AssetError::io("чтение PNG", error))?;
        let image = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
            .map_err(|error| {
                AssetError::new(ErrorCode::IntegrityMismatch, format!("{name}: {error}"))
            })?
            .to_rgb8();
        images.insert(character, image);
    }
    Ok(images)
}

/// Разбирает аргументы в упорядоченный список уникальных символов.
pub(crate) fn parse_characters(values: &[String]) -> Result<Vec<String>, AssetError> {
    let mut seen = BTreeSet::new();
    let mut characters = Vec::new();
    for value in values {
        for character in value.chars().filter(|c| !c.is_whitespace()) {
            let text = character.to_string();
            parse_kanji_character(&text)
                .map_err(|message| AssetError::new(ErrorCode::InvalidIdentity, message))?;
            if seen.insert(text.clone()) {
                characters.push(text);
            }
        }
    }
    if characters.is_empty() {
        return Err(AssetError::new(
            ErrorCode::InvalidIdentity,
            "не передано ни одного символа",
        ));
    }
    Ok(characters)
}

/// Каталог результатов не может лежать в защищённом дереве `decks/`.
pub(crate) fn ensure_outside_protected(
    path: &Path,
    protected_roots: &BTreeSet<PathBuf>,
) -> Result<PathBuf, AssetError> {
    let absolute = super::normalize_absolute_path(path)?;
    for root in protected_roots {
        let lexical = super::normalize_absolute_path(root)?;
        let canonical = std::fs::canonicalize(root).ok();
        if absolute.starts_with(&lexical)
            || canonical
                .as_ref()
                .is_some_and(|root| absolute.starts_with(root))
        {
            return Err(AssetError::new(
                ErrorCode::BoundaryViolation,
                "каталог результатов локального рендера не может находиться в decks/",
            ));
        }
    }
    Ok(absolute)
}

fn prepare_empty_directory(path: &Path) -> Result<(), AssetError> {
    match std::fs::read_dir(path) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Err(AssetError::new(
                    ErrorCode::InvalidTransition,
                    format!("каталог результатов не пуст: {}", path.display()),
                ));
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir_all(path)
            .map_err(|error| AssetError::io("создание каталога результатов", error)),
        Err(error) => Err(AssetError::io("чтение каталога результатов", error)),
    }
}

/// Результат рендера одного символа в пуле.
pub(crate) type RenderOutcome = (String, Result<RenderedKanji, String>);

/// Рисует символы ограниченным пулом браузеров; `on_result` вызывается в
/// вызывающем потоке по мере готовности. Один браузер обслуживает много символов.
pub(crate) fn render_pool(
    font: &LocalFont,
    profile: GeometryProfile,
    characters: Vec<String>,
    browsers: usize,
    mut on_result: impl FnMut(RenderOutcome) -> Result<(), AssetError>,
) -> Result<serde_json::Value, AssetError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(browsers.clamp(1, 4) + 1)
        .enable_all()
        .build()
        .map_err(|error| AssetError::io("runtime локального рендера", error))?;
    let queue = Arc::new(Mutex::new(characters.into_iter().collect::<VecDeque<_>>()));
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<RenderOutcome>(8);
    let mut workers = Vec::new();
    for _ in 0..browsers.max(1) {
        let queue = Arc::clone(&queue);
        let sender = sender.clone();
        let font = font.clone();
        workers.push(runtime.spawn(async move {
            let mut renderer = match LocalRenderer::launch(font, profile, None).await {
                Ok(renderer) => renderer,
                Err(message) => {
                    // Без браузера каждый оставшийся символ получает явную причину.
                    loop {
                        let next = queue.lock().expect("очередь доступна").pop_front();
                        let Some(character) = next else { break };
                        if sender
                            .send((character, Err(message.clone())))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    return None;
                }
            };
            let provenance = json!({
                "product": renderer.provenance().product,
                "revision": renderer.provenance().revision,
                "executable_source": renderer.provenance().executable_source,
            });
            loop {
                let next = queue.lock().expect("очередь доступна").pop_front();
                let Some(character) = next else { break };
                let result = renderer.render(&character).await;
                if sender.send((character, result)).await.is_err() {
                    break;
                }
            }
            let _ = renderer.close().await;
            Some(provenance)
        }));
    }
    drop(sender);
    let mut failure = None;
    while let Some(outcome) = runtime.block_on(receiver.recv()) {
        if let Err(error) = on_result(outcome) {
            // Остальным исполнителям больше нечего брать: очередь опустошается.
            queue.lock().expect("очередь доступна").clear();
            failure = Some(error);
            break;
        }
    }
    drop(receiver);
    let mut browsers_seen = Vec::new();
    for worker in workers {
        if let Ok(Some(provenance)) = runtime.block_on(worker) {
            browsers_seen.push(provenance);
        }
    }
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(json!(browsers_seen.into_iter().next()))
}

pub(crate) fn file_stem(character: &str) -> String {
    format!(
        "U+{:04X}",
        u32::from(character.chars().next().unwrap_or('\0'))
    )
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), AssetError> {
    let temp = path.with_extension("partial");
    std::fs::write(&temp, bytes).map_err(|error| AssetError::io("запись результата", error))?;
    std::fs::rename(&temp, path).map_err(|error| AssetError::io("публикация результата", error))
}

/// Выполняет команду без открытия хранилища; возвращает JSON и код выхода.
pub(crate) fn execute(
    command: &LocalPngCommand,
    store_path: &Path,
    protected_roots: &BTreeSet<PathBuf>,
    repository_root: &Path,
) -> Result<(serde_json::Value, u8), AssetError> {
    let open_store = || {
        let mut options = crate::store::StoreOptions::new(store_path);
        for root in protected_roots {
            options = options.protect_from(root);
        }
        crate::store::AssetStore::open_kanji_existing(options)
    };
    match command {
        LocalPngCommand::Run {
            font,
            profile,
            work_dir,
            browsers,
            from_corpus,
            limit,
            ocr,
            ocr_options,
            publish,
            characters,
        } => {
            ocr_options.validate()?;
            let explicit = if characters.is_empty() {
                Vec::new()
            } else {
                parse_characters(characters)?
            };
            if explicit.is_empty() && !from_corpus {
                return Err(AssetError::new(
                    ErrorCode::InvalidIdentity,
                    "укажите символы или --from-corpus",
                ));
            }
            let work_dir = ensure_outside_protected(work_dir, protected_roots)?;
            let font = LocalFont::load(&font_path(font, repository_root), false)
                .map_err(|message| AssetError::new(ErrorCode::InvalidTransition, message))?;
            let store = open_store()?;
            let mut ocr_options = ocr_options.clone();
            ocr_options.inference.repository_root = repository_root.to_path_buf();
            let summary = super::local_png_pipeline::run(
                &store,
                super::local_png_pipeline::RunOptions {
                    font,
                    profile: *profile,
                    work_dir,
                    browsers: usize::from(*browsers),
                    from_corpus: *from_corpus,
                    explicit,
                    limit: *limit,
                    rescan_ocr: ocr_options.rescan,
                    ocr: ocr.then_some(ocr_options),
                    publish: *publish,
                },
            )?;
            Ok((summary, 0))
        }
        LocalPngCommand::ReviewExport { work_dir } => {
            let work_dir = ensure_outside_protected(work_dir, protected_roots)?;
            let store = open_store()?;
            Ok((
                super::local_png_pipeline::review_export(&store, &work_dir)?,
                0,
            ))
        }
        LocalPngCommand::ReviewApply {
            work_dir,
            review_id,
        } => {
            let work_dir = ensure_outside_protected(work_dir, protected_roots)?;
            let store = open_store()?;
            let summary = super::local_png_pipeline::review_apply(&store, &work_dir, review_id)?;
            let failed = summary["counts"]["failed"].as_u64().unwrap_or(0);
            Ok((summary, if failed == 0 { 0 } else { 3 }))
        }
        LocalPngCommand::Report { work_dir } => {
            let work_dir = ensure_outside_protected(work_dir, protected_roots)?;
            let store = open_store()?;
            let (summary, healthy) = super::local_png_pipeline::report(&store, &work_dir)?;
            Ok((summary, if healthy { 0 } else { 3 }))
        }
        LocalPngCommand::Compare {
            reference,
            generated,
        } => {
            let references = read_png_directory(reference)?;
            let generated = read_png_directory(generated)?;
            let mut items = Vec::new();
            let mut sums = [0.0f64; 4];
            for (character, reference) in &references {
                let Some(candidate) = generated.get(character) else {
                    items.push(json!({"character": character, "error": "generated_missing"}));
                    continue;
                };
                match crate::local_render::compare_images(reference, candidate) {
                    Ok(metrics) => {
                        sums[0] += metrics.mae;
                        sums[1] += metrics.pixel_similarity;
                        sums[2] += metrics.glyph_iou;
                        sums[3] += metrics.exact_pixel_fraction;
                        items.push(json!({"character": character, "metrics": metrics}));
                    }
                    Err(message) => items.push(json!({"character": character, "error": message})),
                }
            }
            let compared = items
                .iter()
                .filter(|item| item.get("metrics").is_some())
                .count();
            let mean = |index: usize| {
                if compared == 0 {
                    0.0
                } else {
                    sums[index] / compared as f64
                }
            };
            Ok((
                json!({
                    "schema_version": 1,
                    "operation": "local-png compare",
                    "outcome": "completed",
                    "counts": {"references": references.len(), "compared": compared},
                    "mean": {
                        "mae": mean(0),
                        "pixel_similarity": mean(1),
                        "glyph_iou": mean(2),
                        "exact_pixel_fraction": mean(3),
                    },
                    "items": items,
                }),
                0,
            ))
        }
        LocalPngCommand::Render {
            font,
            allow_uncalibrated_font,
            profile,
            out,
            raw,
            browsers,
            characters,
        } => {
            let characters = parse_characters(characters)?;
            let out = ensure_outside_protected(out, protected_roots)?;
            let font = LocalFont::load(&font_path(font, repository_root), *allow_uncalibrated_font)
                .map_err(|message| AssetError::new(ErrorCode::InvalidTransition, message))?;
            prepare_empty_directory(&out)?;
            if *raw {
                std::fs::create_dir_all(out.join("raw"))
                    .map_err(|error| AssetError::io("создание каталога raw", error))?;
            }
            let mut items = Vec::new();
            let mut failed = 0usize;
            let browser = render_pool(
                &font,
                *profile,
                characters,
                usize::from(*browsers),
                |(character, result)| {
                    let stem = file_stem(&character);
                    match result {
                        Ok(rendered) => {
                            write_file(&out.join(format!("{stem}.png")), &rendered.png)?;
                            if *raw {
                                write_file(
                                    &out.join("raw").join(format!("{stem}.png")),
                                    &rendered.raw_png,
                                )?;
                            }
                            write_file(
                                &out.join(format!("{stem}.json")),
                                &serde_json::to_vec_pretty(&rendered.evidence)
                                    .expect("свидетельства сериализуются"),
                            )?;
                            items.push(json!({
                                "character": character,
                                "file": format!("{stem}.png"),
                                "sha256": rendered.evidence.corrected_sha256,
                                "raw_sha256": rendered.evidence.raw_sha256,
                            }));
                        }
                        Err(message) => {
                            failed += 1;
                            items.push(json!({"character": character, "error": message}));
                        }
                    }
                    Ok(())
                },
            )?;
            Ok((
                json!({
                    "schema_version": 1,
                    "operation": "local-png render",
                    "outcome": if failed == 0 { "completed" } else { "completed_with_errors" },
                    "out": out.display().to_string(),
                    "font": crate::local_render::FontEvidence::from(&font),
                    "browser": browser,
                    "profile": profile.as_str(),
                    "failed": failed,
                    "items": items,
                }),
                if failed == 0 { 0 } else { 3 },
            ))
        }
    }
}

/// Краткий человекочитаемый вывод.
pub(crate) fn human_summary(value: &serde_json::Value) -> String {
    let mut text = format!(
        "{}: {}\n",
        value["operation"].as_str().unwrap_or("local-png"),
        value["outcome"].as_str().unwrap_or("")
    );
    if let Some(error) = value.get("error") {
        text.push_str(&format!(
            "  ошибка {}: {}\n",
            error["code"].as_str().unwrap_or(""),
            error["message"].as_str().unwrap_or("")
        ));
    }
    if let Some(counts) = value.get("counts") {
        text.push_str(&format!("  {counts}\n"));
    }
    for item in value["items"].as_array().into_iter().flatten() {
        text.push_str(&format!(
            "  {} {}\n",
            item["character"].as_str().unwrap_or("?"),
            item.get("error")
                .and_then(|error| error.as_str())
                .or_else(|| item["sha256"].as_str())
                .unwrap_or("")
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn characters_are_unique_in_argument_order() {
        let parsed = parse_characters(&["漢字 漢".into(), "一".into()]).unwrap();
        assert_eq!(parsed, ["漢", "字", "一"]);
        assert!(parse_characters(&["a".into()]).is_err());
        assert!(parse_characters(&[" ".into()]).is_err());
    }

    #[test]
    fn png_file_names_map_to_characters() {
        assert_eq!(character_from_file_name("漢.png").as_deref(), Some("漢"));
        assert_eq!(
            character_from_file_name("U+4E00.png").as_deref(),
            Some("一")
        );
        assert_eq!(character_from_file_name("漢.gif"), None);
        assert_eq!(character_from_file_name("readme.png"), None);
    }

    #[test]
    fn output_directories_inside_decks_are_refused() {
        let root = std::env::temp_dir().join("local-png-boundary");
        let decks = root.join("decks");
        let protected = BTreeSet::from([decks.clone()]);
        assert_eq!(
            ensure_outside_protected(&decks.join("out"), &protected)
                .unwrap_err()
                .code,
            ErrorCode::BoundaryViolation
        );
        assert!(ensure_outside_protected(&root.join("out"), &protected).is_ok());
    }
}
