//! Общий снимок проверки кандидатов человеком.
//!
//! Снимок — производный артефакт сохранённого пакета, а не источник истины:
//! решения применяет только предметный адаптер после сверки каждой строки с
//! актуальным состоянием владельца. Общий слой отвечает за то, что у обоих
//! доменов одинаково:
//!
//! - адресация снимка SHA-256 его канонических метаданных (`review_id`);
//! - точные копии байтов кандидатов `<identity>-<SHA>.<ext>` рядом со снимком;
//! - редактируемый `decisions.tsv`, где человек меняет только первую колонку,
//!   а подпись, схема, пакет, идентичность и остальные колонки неизменны;
//! - повторный export того же снимка не перезаписывает заполненный файл решений;
//! - чтение отклоняет файл целиком при подмене снимка, копии изображения,
//!   подписи, колонки, порядка или числа строк.
//!
//! Слой не знает ни браузера, ни провайдера, ни уровня доверия. Необязательные
//! внешние предложения (OCR сегодня, иные проверки в будущем) прикрепляются к
//! строке только с привязкой к её идентичности и точному SHA (`ReviewRow::validate`)
//! и никогда не становятся решением: решение — только явная первая колонка.

use std::collections::BTreeSet;
use std::path::PathBuf;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::batch_runtime::ArtifactDirectory;
use crate::error::{AssetError, ErrorCode};
use crate::hashing::sha256_hex;
use crate::model::{AssetIdentity, ValidatorIdentity};

/// Строка снимка. Колонки, кроме первой, неизменны и сверяются побайтно.
pub(crate) trait ReviewRow: Serialize + DeserializeOwned {
    fn identity(&self) -> &AssetIdentity;
    /// Имя копии изображения в каталоге снимка и точный SHA её байтов.
    fn image(&self) -> Option<(&str, &str)>;
    /// Неизменяемые колонки строки после колонки решения.
    fn columns(&self) -> String;
    /// Предметная проверка целостности строки при чтении (например, что
    /// приложенное предложение относится к точным байтам этой строки).
    fn validate(&self) -> Result<(), AssetError> {
        Ok(())
    }
}

/// Метаданные снимка; их каноническая сериализация задаёт `review_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReviewSnapshot<R> {
    pub schema_version: u32,
    pub batch_id: String,
    pub validator: ValidatorIdentity,
    pub rows: Vec<R>,
}

/// Предметное оформление файла решений.
pub(crate) struct ReviewFormat {
    pub schema: u32,
    /// Строки комментариев перед подписью, каждая начинается с `#`.
    pub comments: &'static [&'static str],
}

pub(crate) struct ReviewBundle {
    pub id: String,
    pub directory: PathBuf,
    pub decisions: PathBuf,
}

/// Значение первой колонки строки решений.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReviewCell {
    /// Пустое поле: решения нет.
    Unspecified,
    /// `-`: явный пропуск.
    Skip,
    /// Предметное действие; его разбирает адаптер.
    Action(String),
}

fn invalid(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::InvalidTransition, message)
}

pub(crate) fn directory_name(id: &str) -> String {
    format!("review-{id}")
}

fn signature<R>(snapshot: &ReviewSnapshot<R>, format: &ReviewFormat, id: &str) -> String {
    format!("@review\t{}\t{}\t{id}", format.schema, snapshot.batch_id)
}

fn decisions_template<R: ReviewRow>(
    snapshot: &ReviewSnapshot<R>,
    format: &ReviewFormat,
    id: &str,
) -> String {
    let mut text = String::new();
    for comment in format.comments {
        text.push_str(comment);
        text.push('\n');
    }
    text.push_str(&signature(snapshot, format, id));
    text.push('\n');
    for row in &snapshot.rows {
        text.push_str(&format!("\t{}\n", row.columns()));
    }
    text
}

/// Создаёт или повторно открывает снимок. Каталог открывает владелец runtime
/// пакета под своей блокировкой; `read_image` возвращает проверенные байты
/// кандидата строки из сохранённого состояния.
pub(crate) fn export<R: ReviewRow>(
    open_directory: impl FnOnce(&str) -> Result<ArtifactDirectory, AssetError>,
    snapshot: &ReviewSnapshot<R>,
    format: &ReviewFormat,
    mut read_image: impl FnMut(&R) -> Result<Vec<u8>, AssetError>,
) -> Result<(ReviewBundle, ArtifactDirectory), AssetError> {
    if snapshot.schema_version != format.schema {
        return Err(invalid("схема снимка review не совпадает с форматом"));
    }
    let encoded = serde_json::to_vec(snapshot).map_err(|error| invalid(error.to_string()))?;
    let id = sha256_hex(&encoded);
    let directory = open_directory(&directory_name(&id))?;
    // Повторный export не перезаписывает уже заполненный файл редактора.
    match directory.read("snapshot.json") {
        Ok(existing) if existing == encoded => {
            for row in &snapshot.rows {
                let Some((filename, sha256)) = row.image() else {
                    continue;
                };
                match directory.read(filename) {
                    Ok(bytes) if sha256_hex(&bytes) == sha256 => {}
                    Ok(_) => return Err(invalid("копия изображения review изменена")),
                    Err(error) if error.code == ErrorCode::MissingAssetFile => {
                        let bytes = read_image(row)?;
                        if sha256_hex(&bytes) != sha256 {
                            return Err(invalid("исходный кандидат review изменён"));
                        }
                        directory.write_if_absent(filename, &bytes)?;
                        if sha256_hex(&directory.read(filename)?) != sha256 {
                            return Err(invalid("копия изображения review изменена"));
                        }
                    }
                    Err(error) => return Err(error),
                }
            }
            match directory.read("decisions.tsv") {
                Ok(_) => {}
                Err(error) if error.code == ErrorCode::MissingAssetFile => {
                    directory.write_if_absent(
                        "decisions.tsv",
                        decisions_template(snapshot, format, &id).as_bytes(),
                    )?;
                    directory.read("decisions.tsv")?;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(_) => return Err(invalid("содержимое снимка review не совпадает с его SHA")),
        Err(error) if error.code == ErrorCode::MissingAssetFile => {
            for row in &snapshot.rows {
                let Some((filename, sha256)) = row.image() else {
                    continue;
                };
                let bytes = read_image(row)?;
                if sha256_hex(&bytes) != sha256 {
                    return Err(invalid("исходный кандидат review изменён"));
                }
                directory.write(filename, &bytes)?;
            }
            directory.write(
                "decisions.tsv",
                decisions_template(snapshot, format, &id).as_bytes(),
            )?;
            // Снимок записывается последним: неполный export безопасно повторяется.
            directory.write("snapshot.json", &encoded)?;
        }
        Err(error) => return Err(error),
    }
    let bundle = ReviewBundle {
        id,
        decisions: directory.path.join("decisions.tsv"),
        directory: directory.path.clone(),
    };
    Ok((bundle, directory))
}

/// Читает снимок по `review_id` и разбирает файл решений целиком.
pub(crate) fn read<R: ReviewRow>(
    open_directory: impl FnOnce(&str) -> Result<ArtifactDirectory, AssetError>,
    batch_id: &str,
    id: &str,
    format: &ReviewFormat,
) -> Result<(ReviewSnapshot<R>, Vec<ReviewCell>), AssetError> {
    crate::batch_runtime::validate_hash(id)?;
    let directory = open_directory(&directory_name(id))?;
    let encoded = directory.read("snapshot.json")?;
    if sha256_hex(&encoded) != id {
        return Err(invalid("SHA снимка review не совпадает"));
    }
    let snapshot: ReviewSnapshot<R> =
        serde_json::from_slice(&encoded).map_err(|error| invalid(error.to_string()))?;
    if snapshot.schema_version != format.schema || snapshot.batch_id != batch_id {
        return Err(invalid("неверная схема или batch_id файла review"));
    }
    for row in &snapshot.rows {
        row.validate()?;
        if let Some((filename, sha256)) = row.image()
            && sha256_hex(&directory.read(filename)?) != sha256
        {
            return Err(invalid("SHA копии изображения review не совпадает"));
        }
    }
    let bytes = directory.read("decisions.tsv")?;
    let text =
        std::str::from_utf8(&bytes).map_err(|_| invalid("файл решений должен быть UTF-8"))?;
    let cells = parse_cells(&snapshot, format, id, text)?;
    Ok((snapshot, cells))
}

/// Разбор первой колонки при неизменных остальных; BOM/CRLF и `#` допускаются.
pub(crate) fn parse_cells<R: ReviewRow>(
    snapshot: &ReviewSnapshot<R>,
    format: &ReviewFormat,
    id: &str,
    text: &str,
) -> Result<Vec<ReviewCell>, AssetError> {
    let mut lines = text
        .trim_start_matches('\u{feff}')
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'));
    if lines.next() != Some(signature(snapshot, format, id).as_str()) {
        return Err(invalid("изменена подпись, схема или идентичность review"));
    }
    let mut cells = Vec::with_capacity(snapshot.rows.len());
    let mut seen = BTreeSet::new();
    for (index, row) in snapshot.rows.iter().enumerate() {
        if !seen.insert(row.identity()) {
            return Err(invalid("повторяющаяся идентичность в review"));
        }
        let line = lines
            .next()
            .ok_or_else(|| invalid("в review отсутствуют строки"))?;
        let (action, columns) = line
            .split_once('\t')
            .ok_or_else(|| invalid("в review требуется TAB"))?;
        if columns != row.columns() {
            return Err(invalid(format!("изменены метаданные строки {}", index + 1)));
        }
        cells.push(match action {
            "" => ReviewCell::Unspecified,
            "-" => ReviewCell::Skip,
            action => ReviewCell::Action(action.to_owned()),
        });
    }
    if lines.next().is_some() {
        return Err(invalid("лишние или повторяющиеся строки review"));
    }
    Ok(cells)
}

/// HTML для ручного просмотра остаётся переносимым при длинных UNC-путях WSL.
/// Объём встроенных изображений ограничен, чтобы крупный пакет не создавал
/// неограниченный HTML; после исчерпания бюджета остаются относительные ссылки.
pub(crate) fn preview_image_source(filename: &str, bytes: &[u8], remaining: &mut usize) -> String {
    let mime = match filename.rsplit_once('.').map(|(_, ext)| ext) {
        Some("gif") => "image/gif",
        Some("png") => "image/png",
        _ => return crate::batch::html_escape(filename),
    };
    if bytes.len() > *remaining {
        return crate::batch::html_escape(filename);
    }
    *remaining -= bytes.len();
    format!("data:{mime};base64,{}", STANDARD.encode(bytes))
}

/// 32 МиБ исходных изображений: бюджет применяется к данным до base64.
pub(crate) const PREVIEW_INLINE_BUDGET: usize = 32 * 1024 * 1024;
