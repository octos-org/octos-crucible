//! Password-protected zip for submitters to download their outputs.
//!
//! WinZip AES-256 (AE-2): opens with 7-Zip, WinRAR, Keka, The Unarchiver,
//! Bandizip, libarchive's bsdtar, Python's pyzipper, etc. The legacy
//! "ZipCrypto" scheme that Windows Explorer and macOS Archive Utility also
//! open is broken (known-plaintext attacks), so it is not offered.
//! File names stay visible in a zip (the format encrypts contents only);
//! callers should not put secrets in names.

use std::io::{Cursor, Read, Seek, Write};

use zip::write::SimpleFileOptions;
use zip::{AesMode, CompressionMethod, ZipArchive, ZipWriter};

#[derive(Debug, thiserror::Error)]
pub enum ZipError {
    #[error("download password must not be empty")]
    EmptyPassword,
    #[error("zip: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("zip: {0}")]
    Io(#[from] std::io::Error),
}

/// Write `entries` (name, contents) as an AES-256 encrypted zip.
pub fn write_password_zip<'a, W: Write + Seek>(
    out: W,
    entries: impl IntoIterator<Item = (&'a str, &'a [u8])>,
    password: &str,
) -> Result<W, ZipError> {
    if password.is_empty() {
        return Err(ZipError::EmptyPassword);
    }
    let opts = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .with_aes_encryption(AesMode::Aes256, password);
    let mut zip = ZipWriter::new(out);
    for (name, data) in entries {
        zip.start_file(name, opts)?;
        zip.write_all(data)?;
    }
    Ok(zip.finish()?)
}

/// Read every file of a password zip (tests and the admin CLI).
pub fn read_password_zip(data: &[u8], password: &str) -> Result<Vec<(String, Vec<u8>)>, ZipError> {
    let mut archive = ZipArchive::new(Cursor::new(data))?;
    let mut out = Vec::with_capacity(archive.len());
    for i in 0..archive.len() {
        let mut f = archive.by_index_decrypt(i, password.as_bytes())?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        out.push((f.name().to_owned(), buf));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(password: &str) -> Vec<u8> {
        let files: [(&str, &[u8]); 2] = [
            ("out/index.html", b"<h1>the-secret-output</h1>"),
            ("logs/agent.log", b"the-secret-log line\n"),
        ];
        write_password_zip(Cursor::new(Vec::new()), files, password)
            .unwrap()
            .into_inner()
    }

    #[test]
    fn round_trip() {
        let z = make("correct horse");
        let files = read_password_zip(&z, "correct horse").unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].0, "out/index.html");
        assert_eq!(files[0].1, b"<h1>the-secret-output</h1>");
    }

    #[test]
    fn wrong_password_and_no_plaintext() {
        let z = make("correct horse");
        assert!(read_password_zip(&z, "wrong").is_err());
        assert!(!z.windows(10).any(|w| w == b"the-secret"));
        assert!(!z.windows(13).any(|w| w == b"correct horse"));
    }

    #[test]
    fn tamper_rejected() {
        let mut z = make("pw");
        // Flip a byte inside the first entry's encrypted data (after the
        // 30-byte local header, the name and the AES extra field + salt).
        z[80] ^= 1;
        assert!(read_password_zip(&z, "pw").is_err());
    }

    #[test]
    fn empty_password_refused() {
        let r = write_password_zip(Cursor::new(Vec::new()), [("a", &b"x"[..])], "");
        assert!(matches!(r, Err(ZipError::EmptyPassword)));
    }
}
