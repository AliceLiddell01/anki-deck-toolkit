//! Политики доменов для канонического расположения ресурсов и имён для потребителей.

use std::path::{Component, Path};

use crate::error::{AssetError, ErrorCode};
use crate::hashing::sha256_hex;
use crate::kanji_domain::parse_kanji_character;
use crate::model::{AssetIdentity, AssetRecord, DetectedFormat, ValidatorIdentity};

/// Пространства имён, принадлежащие предметным доменам публикации.
///
/// Этот статический список используется общей политикой, чтобы она не могла
/// принять идентификатор, закреплённый за одной из предметных политик.
const PUBLISHABLE_DOMAIN_NAMESPACES: &[&str] = &["kanji", "pitch_accent"];

/// Внутреннее расположение ресурса и имя, которое получает конечный потребитель.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalAssetLocation {
    pub storage_path: String,
    pub consumer_filename: String,
}

/// Семантика доверия и обновления записи, которую предметный домен сообщает
/// общему хранилищу.
///
/// Общий слой не перечисляет известные домены и не распознаёт их по имени: он
/// применяет ровно ту семантику, которую объявила политика домена. Поэтому новый
/// домен не требует правок в `store.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustSemantics {
    /// Может ли явное решение человека заменить актуальное автоматическое
    /// решение ожидаемого валидатора при определении доверия потребителя.
    human_attestation_confers_trust: bool,
    /// Считает ли домен `domain_metadata` и `provenance` частью семантики
    /// ресурса: их изменение при неизменных байтах требует явного CAS.
    metadata_change_requires_explicit_cas: bool,
}

impl TrustSemantics {
    /// Доверие подтверждается либо актуальным решением ожидаемого валидатора,
    /// либо явным решением человека по точным текущим байтам.
    pub const HUMAN_ATTESTED: Self = Self {
        human_attestation_confers_trust: true,
        metadata_change_requires_explicit_cas: false,
    };

    /// Доверие требует актуального автоматического `verified` ожидаемого
    /// валидатора: решение человека его не заменяет, а изменение метаданных при
    /// неизменных байтах считается семантическим изменением ресурса.
    pub const AUTOMATED_VERIFIED_ONLY: Self = Self {
        human_attestation_confers_trust: false,
        metadata_change_requires_explicit_cas: true,
    };

    /// Считается ли запись доверенной для ожидаемого валидатора.
    pub fn confers_trust(
        &self,
        record: &AssetRecord,
        expected_validator: &ValidatorIdentity,
    ) -> bool {
        if self.human_attestation_confers_trust {
            record.is_trusted_for(expected_validator)
        } else {
            record.is_trusted_for_automated_validation(expected_validator)
        }
    }

    /// Требует ли запись новой проверки в режиме `SelectionMode::New`.
    pub fn requires_new_validation(
        &self,
        record: &AssetRecord,
        expected_validator: &ValidatorIdentity,
    ) -> bool {
        let already_decided = record.has_current_automated_decision(expected_validator)
            || (self.human_attestation_confers_trust && record.has_complete_human_approval());
        !already_decided
    }

    /// Требует ли повторная запись тех же байтов с другими метаданными явного CAS.
    pub const fn metadata_change_requires_explicit_cas(&self) -> bool {
        self.metadata_change_requires_explicit_cas
    }
}

/// Правила одного независимого домена ресурсов.
///
/// Реализации отвечают за идентичность, допустимый формат и детерминированное
/// каноническое расположение. `legacy_location` используется только для чтения
/// и миграции старых манифестов; новые записи всегда строятся через
/// `canonical_location`.
pub trait AssetDomainPolicy: std::fmt::Debug + Send + Sync {
    /// Стабильный идентификатор домена, сохраняемый вместе с хранилищем.
    fn domain_id(&self) -> &'static str;

    /// Проверяет принадлежность идентификатора этому домену.
    fn validate_identity(&self, identity: &AssetIdentity) -> Result<(), AssetError>;

    /// Возвращает канонический путь внутри хранилища и плоское имя для потребителя.
    fn canonical_location(
        &self,
        identity: &AssetIdentity,
        sha256: &str,
        format: DetectedFormat,
    ) -> Result<CanonicalAssetLocation, AssetError>;

    /// Доказывает, что прежнее имя потребителя принадлежит именно этой
    /// идентичности. Домен без явно объявленной прежней схемы имён отказывает.
    /// Эта проверка независима от расположения старого манифеста.
    fn proves_legacy_consumer_filename(&self, _identity: &AssetIdentity, _filename: &str) -> bool {
        false
    }

    /// Возвращает расположение, использовавшееся прежней схемой, если домен
    /// существовал в ней.
    fn legacy_location(
        &self,
        _identity: &AssetIdentity,
        _sha256: &str,
        _format: DetectedFormat,
    ) -> Option<CanonicalAssetLocation> {
        None
    }

    /// Поддерживает ли политика чтение прежних схем 3/4, исторически созданных
    /// до фиксации `domain_id`. По умолчанию такие схемы политике не принадлежат.
    fn supports_legacy_schema(&self) -> bool {
        false
    }

    /// Можно ли хранить этот фактический формат в каноническом корпусе домена.
    fn is_publishable_format(&self, format: DetectedFormat) -> bool;

    /// Может ли семантический валидатор поместить такой формат в состояние
    /// `verified`. Общий домен по умолчанию принимает любой определённый формат;
    /// предметные домены сужают этот набор.
    fn allows_verified_format(&self, _format: DetectedFormat) -> bool {
        true
    }

    /// Может ли одна идентичность иметь по независимой записи на каждый
    /// публикуемый формат. Тогда ключ записи — пара «идентичность, формат», а
    /// замена по CAS действует только внутри записи того же формата.
    fn allows_format_variants(&self) -> bool {
        false
    }

    /// Порядок предпочтения форматов, когда потребитель запрашивает
    /// идентичность без формата. Пустой список — формат не различается.
    fn format_preference(&self) -> &'static [DetectedFormat] {
        &[]
    }

    /// Доменный предел размера ресурса, если он установлен.
    fn max_asset_bytes(&self) -> Option<u64>;

    /// Использует ли домен неизменяемые объекты с хешем в имени вместо
    /// стабильного имени с публикацией по принципу compare-and-swap.
    fn content_addressed_storage(&self) -> bool;

    /// Проверяет полное каноническое расположение записи перед публикацией.
    fn validate_publishable_record(&self, record: &AssetRecord) -> Result<(), AssetError> {
        self.validate_identity(&record.identity)?;
        if !self.is_publishable_format(record.format) {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                format!(
                    "формат {:?} не разрешён для домена {}",
                    record.format,
                    self.domain_id()
                ),
            ));
        }
        let expected = self.canonical_location(&record.identity, &record.sha256, record.format)?;
        validate_safe_consumer_filename(&expected.consumer_filename, record.format)?;
        validate_safe_storage_path(&expected.storage_path)?;
        if record.storage_path != expected.storage_path
            || record.consumer_filename != expected.consumer_filename
        {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                format!(
                    "каноническое расположение записи {} не совпадает с политикой домена {}",
                    record.identity,
                    self.domain_id()
                ),
            ));
        }
        Ok(())
    }

    /// Семантика доверия и обновления записи для этого домена.
    ///
    /// Домен сам сообщает общему хранилищу, что подтверждает доверие потребителя
    /// и когда запись требует новой проверки. По умолчанию действует семантика с
    /// учётом явного решения человека.
    fn trust_semantics(&self) -> TrustSemantics {
        TrustSemantics::HUMAN_ATTESTED
    }
}

/// Политика для пространства имён без собственного домена публикации.
///
/// Для совместимости сохраняет прежнее размещение с хешем в имени файла.
/// Зарезервированные пространства имён должны обслуживаться своей предметной
/// политикой.
#[derive(Debug, Clone, Copy, Default)]
pub struct GenericDomainPolicy;

impl AssetDomainPolicy for GenericDomainPolicy {
    fn domain_id(&self) -> &'static str {
        "generic"
    }

    fn validate_identity(&self, identity: &AssetIdentity) -> Result<(), AssetError> {
        identity
            .validate()
            .map_err(|message| AssetError::new(ErrorCode::InvalidIdentity, message))?;
        if PUBLISHABLE_DOMAIN_NAMESPACES.contains(&identity.namespace.as_str()) {
            return Err(AssetError::new(
                ErrorCode::InvalidIdentity,
                format!(
                    "пространство имён {} должно использовать собственный домен ресурсов",
                    identity.namespace
                ),
            ));
        }
        Ok(())
    }

    fn canonical_location(
        &self,
        identity: &AssetIdentity,
        sha256: &str,
        format: DetectedFormat,
    ) -> Result<CanonicalAssetLocation, AssetError> {
        self.validate_identity(identity)?;
        validate_sha256(sha256)?;
        let key_hash = sha256_hex(identity.key.as_bytes());
        let prefix = format!("{}-{}", identity.namespace, &key_hash[..16]);
        let filename = format!("{prefix}-{sha256}.{}", extension_for_format(format));
        validate_safe_consumer_filename(&filename, format)?;
        Ok(CanonicalAssetLocation {
            storage_path: format!("assets/{filename}"),
            consumer_filename: filename,
        })
    }

    fn legacy_location(
        &self,
        identity: &AssetIdentity,
        sha256: &str,
        format: DetectedFormat,
    ) -> Option<CanonicalAssetLocation> {
        self.canonical_location(identity, sha256, format).ok()
    }

    fn is_publishable_format(&self, _format: DetectedFormat) -> bool {
        // Общие объекты могут пользоваться жизненным циклом хранилища, но
        // не входят в публикуемый корпус репозитория.
        false
    }

    fn max_asset_bytes(&self) -> Option<u64> {
        None
    }

    fn content_addressed_storage(&self) -> bool {
        true
    }
}

/// Политика канонического корпуса изображений кандзи.
#[derive(Debug, Clone, Copy, Default)]
pub struct KanjiDomainPolicy;

impl AssetDomainPolicy for KanjiDomainPolicy {
    fn domain_id(&self) -> &'static str {
        "kanji"
    }

    fn validate_identity(&self, identity: &AssetIdentity) -> Result<(), AssetError> {
        identity
            .validate()
            .map_err(|message| AssetError::new(ErrorCode::InvalidIdentity, message))?;
        if identity.namespace != "kanji" {
            return Err(AssetError::new(
                ErrorCode::InvalidIdentity,
                "домен кандзи принимает только пространство имён `kanji`",
            ));
        }
        parse_kanji_character(&identity.key)
            .map_err(|message| AssetError::new(ErrorCode::InvalidIdentity, message))?;
        Ok(())
    }

    fn canonical_location(
        &self,
        identity: &AssetIdentity,
        sha256: &str,
        format: DetectedFormat,
    ) -> Result<CanonicalAssetLocation, AssetError> {
        self.validate_identity(identity)?;
        validate_sha256(sha256)?;
        let filename = format!("{}.{}", identity.key, extension_for_format(format));
        validate_safe_consumer_filename(&filename, format)?;
        let directory = extension_for_format(format);
        Ok(CanonicalAssetLocation {
            storage_path: format!("assets/{directory}/{filename}"),
            consumer_filename: filename,
        })
    }

    fn legacy_location(
        &self,
        identity: &AssetIdentity,
        sha256: &str,
        format: DetectedFormat,
    ) -> Option<CanonicalAssetLocation> {
        self.validate_identity(identity).ok()?;
        validate_sha256(sha256).ok()?;
        let filename = format!("{}.{}", identity.key, extension_for_format(format));
        Some(CanonicalAssetLocation {
            storage_path: format!("assets/{filename}"),
            consumer_filename: filename,
        })
    }

    fn proves_legacy_consumer_filename(&self, identity: &AssetIdentity, filename: &str) -> bool {
        self.validate_identity(identity).is_ok()
            && [DetectedFormat::Gif, DetectedFormat::Png]
                .into_iter()
                .any(|format| {
                    filename == format!("{}.{}", identity.key, extension_for_format(format))
                        && validate_safe_consumer_filename(filename, format).is_ok()
                })
    }

    fn supports_legacy_schema(&self) -> bool {
        true
    }

    fn is_publishable_format(&self, format: DetectedFormat) -> bool {
        matches!(format, DetectedFormat::Gif | DetectedFormat::Png)
    }

    fn allows_verified_format(&self, format: DetectedFormat) -> bool {
        matches!(format, DetectedFormat::Gif | DetectedFormat::Png)
    }

    /// `漢.gif` и `漢.png` — независимые записи одного символа.
    fn allows_format_variants(&self) -> bool {
        true
    }

    /// Подлинный GIF предпочтительнее PNG, если вызывающий не указал формат.
    fn format_preference(&self) -> &'static [DetectedFormat] {
        &[DetectedFormat::Gif, DetectedFormat::Png]
    }

    fn max_asset_bytes(&self) -> Option<u64> {
        Some(crate::kanji_validator::MAX_MEDIA_BYTES as u64)
    }

    fn content_addressed_storage(&self) -> bool {
        false
    }
}

/// Проверяет, что имя для потребителя является безопасным одноуровневым именем
/// и его расширение соответствует фактическому формату байтов.
pub fn validate_safe_consumer_filename(
    filename: &str,
    format: DetectedFormat,
) -> Result<(), AssetError> {
    let invalid = || {
        AssetError::new(
            ErrorCode::PathTraversal,
            "имя для потребителя должно быть безопасным одноуровневым именем",
        )
    };
    if filename.is_empty()
        || filename.len() > 255
        || filename == "."
        || filename == ".."
        || filename.contains(['/', '\\', ':'])
        || filename.chars().any(char::is_control)
        || filename.ends_with(['.', ' '])
    {
        return Err(invalid());
    }
    let path = Path::new(filename);
    if path.components().count() != 1
        || !matches!(path.components().next(), Some(Component::Normal(_)))
    {
        return Err(invalid());
    }
    let (stem, extension) = filename.rsplit_once('.').ok_or_else(invalid)?;
    if stem.is_empty() || extension != extension_for_format(format) {
        return Err(invalid());
    }
    let device_name = stem.split('.').next().unwrap_or(stem).to_ascii_uppercase();
    if matches!(device_name.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (device_name.len() == 4
            && (device_name.starts_with("COM") || device_name.starts_with("LPT"))
            && device_name.as_bytes()[3].is_ascii_digit()
            && device_name.as_bytes()[3] != b'0')
    {
        return Err(invalid());
    }
    Ok(())
}

/// Проверяет лексически безопасный относительный путь внутри `assets/`.
pub fn validate_safe_storage_path(storage_path: &str) -> Result<(), AssetError> {
    let invalid = || {
        AssetError::new(
            ErrorCode::PathTraversal,
            "storage_path должен оставаться внутри каталога `assets`",
        )
    };
    if storage_path.is_empty()
        || storage_path.contains('\\')
        || storage_path.chars().any(char::is_control)
    {
        return Err(invalid());
    }
    let mut components = storage_path.split('/');
    if components.next() != Some("assets") {
        return Err(invalid());
    }
    let mut count = 1;
    for component in components {
        count += 1;
        if component.is_empty() || matches!(component, "." | "..") || component.contains(':') {
            return Err(invalid());
        }
    }
    if count < 2 || Path::new(storage_path).is_absolute() {
        return Err(invalid());
    }
    Ok(())
}

/// Возвращает каноническое расширение для обнаруженного формата.
///
/// Это единый контракт asset-store для имён внутри хранилища и плоских имён,
/// которые получает потребитель. Для `Unknown` используется `bin`; потребители,
/// которым нужны только известные media-форматы, должны отдельно отклонить его.
pub fn extension_for_format(format: DetectedFormat) -> &'static str {
    match format {
        DetectedFormat::Png => "png",
        DetectedFormat::Jpeg => "jpg",
        DetectedFormat::Gif => "gif",
        DetectedFormat::Webp => "webp",
        DetectedFormat::Bmp => "bmp",
        DetectedFormat::Tiff => "tiff",
        DetectedFormat::Unknown => "bin",
    }
}

fn validate_sha256(sha256: &str) -> Result<(), AssetError> {
    if sha256.len() != 64
        || !sha256
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

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn kanji_locations_separate_format_and_keep_flat_consumer_names() {
        let policy = KanjiDomainPolicy;
        let gif = AssetIdentity::new("kanji", "漢").unwrap();
        let png = AssetIdentity::new("kanji", "饅").unwrap();

        assert_eq!(
            policy
                .canonical_location(&gif, HASH, DetectedFormat::Gif)
                .unwrap(),
            CanonicalAssetLocation {
                storage_path: "assets/gif/漢.gif".into(),
                consumer_filename: "漢.gif".into(),
            }
        );
        assert_eq!(
            policy
                .canonical_location(&png, HASH, DetectedFormat::Png)
                .unwrap(),
            CanonicalAssetLocation {
                storage_path: "assets/png/饅.png".into(),
                consumer_filename: "饅.png".into(),
            }
        );
        assert_eq!(
            policy
                .legacy_location(&gif, HASH, DetectedFormat::Gif)
                .unwrap()
                .storage_path,
            "assets/漢.gif"
        );
        let unsupported = policy
            .canonical_location(&gif, HASH, DetectedFormat::Jpeg)
            .unwrap();
        assert_eq!(unsupported.storage_path, "assets/jpg/漢.jpg");
        assert_eq!(unsupported.consumer_filename, "漢.jpg");
        assert!(!policy.is_publishable_format(DetectedFormat::Jpeg));
        assert!(!policy.allows_verified_format(DetectedFormat::Jpeg));
        assert!(policy.allows_verified_format(DetectedFormat::Gif));
        assert!(policy.allows_verified_format(DetectedFormat::Png));
    }

    #[test]
    fn generic_locations_preserve_hash_suffixed_layout() {
        let policy = GenericDomainPolicy;
        let identity = AssetIdentity::new("generic", "one").unwrap();
        let location = policy
            .canonical_location(&identity, HASH, DetectedFormat::Png)
            .unwrap();
        let key_hash = sha256_hex(b"one");
        assert_eq!(
            location.storage_path,
            format!("assets/generic-{}-{HASH}.png", &key_hash[..16])
        );
        assert_eq!(
            location.storage_path,
            format!("assets/{}", location.consumer_filename)
        );
        assert!(policy.content_addressed_storage());
        assert!(!policy.is_publishable_format(DetectedFormat::Png));
        assert!(policy.allows_verified_format(DetectedFormat::Png));
    }

    #[test]
    fn reserved_namespaces_and_unsafe_consumer_names_fail_closed() {
        let generic = GenericDomainPolicy;
        for &namespace in PUBLISHABLE_DOMAIN_NAMESPACES {
            let identity = AssetIdentity::new(namespace, "漢").unwrap();
            assert!(generic.validate_identity(&identity).is_err());
        }
        for filename in [
            "../漢.png",
            "dir/漢.png",
            r"dir\漢.png",
            "C:漢.png",
            "CON.png",
            "漢.gif",
            "漢.png\n",
        ] {
            assert!(
                validate_safe_consumer_filename(filename, DetectedFormat::Png).is_err(),
                "имя {filename:?} должно быть отклонено"
            );
        }
    }

    #[test]
    fn legacy_schema_ownership_and_kanji_size_limit_are_explicit() {
        assert!(KanjiDomainPolicy.supports_legacy_schema());
        assert!(!GenericDomainPolicy.supports_legacy_schema());
        assert_eq!(
            KanjiDomainPolicy.max_asset_bytes(),
            Some(crate::kanji_validator::MAX_MEDIA_BYTES as u64)
        );
        assert_eq!(GenericDomainPolicy.max_asset_bytes(), None);
    }

    #[test]
    fn format_extension_mapping_is_the_canonical_table() {
        assert_eq!(extension_for_format(DetectedFormat::Png), "png");
        assert_eq!(extension_for_format(DetectedFormat::Jpeg), "jpg");
        assert_eq!(extension_for_format(DetectedFormat::Gif), "gif");
        assert_eq!(extension_for_format(DetectedFormat::Webp), "webp");
        assert_eq!(extension_for_format(DetectedFormat::Bmp), "bmp");
        assert_eq!(extension_for_format(DetectedFormat::Tiff), "tiff");
        assert_eq!(extension_for_format(DetectedFormat::Unknown), "bin");
    }
}
