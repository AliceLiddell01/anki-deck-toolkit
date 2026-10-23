//! SHA-256 и стабильное lowercase-hex представление hash-значений.

use std::fmt::Write as _;

use sha2::{Digest, Sha256};

/// Вычисляет SHA-256 и возвращает его каноническое lowercase-hex представление.
pub fn sha256_hex(bytes: impl AsRef<[u8]>) -> String {
    encode_lower_hex(Sha256::digest(bytes.as_ref()))
}

/// Потоковый SHA-256 без загрузки файла в память (многогигабайтные веса моделей).
pub(crate) fn sha256_reader(reader: &mut impl std::io::Read) -> std::io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(encode_lower_hex(hasher.finalize()))
}

/// Кодирует байты в lowercase hex независимо от formatter API digest-типа.
pub(crate) fn encode_lower_hex(bytes: impl AsRef<[u8]>) -> String {
    let bytes = bytes.as_ref();
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("запись в String не может завершиться ошибкой");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lower_hex_preserves_leading_zeroes() {
        assert_eq!(encode_lower_hex([0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
    }

    #[test]
    fn sha256_reader_retries_interrupted_reads() {
        struct InterruptedOnce {
            interrupted: bool,
            data: &'static [u8],
        }
        impl std::io::Read for InterruptedOnce {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                let length = self.data.len().min(buffer.len());
                buffer[..length].copy_from_slice(&self.data[..length]);
                self.data = &self.data[length..];
                Ok(length)
            }
        }
        let mut reader = InterruptedOnce {
            interrupted: false,
            data: b"abc",
        };
        assert_eq!(sha256_reader(&mut reader).unwrap(), sha256_hex(b"abc"));
    }

    #[test]
    fn sha256_hex_matches_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
