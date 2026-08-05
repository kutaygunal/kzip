//! Traditional PKWARE (ZipCrypto) encryption/decryption.
//!
//! This is the classic ZIP encryption scheme (bit flag `0x0001`), used by
//! libzip's `ZIP_EM_TRAD_PKWARE`. It is a stream cipher built from three
//! 32-bit keys that are updated with the CRC-32 primitive. It is **not**
//! cryptographically strong (it is trivially breakable), but it is what the
//! ZIP format and libzip use for "traditional" encryption, so we implement it
//! for byte-level interoperability.
//!
//! Layout of an encrypted entry's data:
//!   - a 12-byte encryption header: 11 random bytes followed by the high byte
//!     of the file's CRC-32, all encrypted;
//!   - the compressed bytes, encrypted.
//!
//! On read, the 12-byte header is decrypted and its final byte is checked
//! against `(crc >> 24)` to detect a wrong password.

use crate::error::{Result, ZipError, ZipErrorCode};
use crate::source::Source;
use std::io::{self, Read, Seek, SeekFrom};

/// Number of bytes in the ZipCrypto encryption header (11 random + 1 CRC byte).
pub const ENCRYPTION_HEADER_LEN: usize = 12;

/// CRC-32 table (reflected, polynomial `0xEDB88320`) used for the key update.
/// This is the same CRC-32 primitive the ZIP format uses for the keystream.
static CRC_TABLE: [u32; 256] = build_crc_table();

const fn build_crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB88320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

/// Update a CRC-32 with a single byte (the PKWARE key-update primitive).
#[inline]
fn crc32_update(crc: u32, byte: u8) -> u32 {
    (crc >> 8) ^ CRC_TABLE[((crc ^ byte as u32) & 0xFF) as usize]
}

/// The ZipCrypto keystream state: three 32-bit keys.
#[derive(Debug, Clone)]
pub struct ZipCrypto {
    key0: u32,
    key1: u32,
    key2: u32,
}

impl ZipCrypto {
    /// Initialize the keys from a password.
    pub fn new(password: &[u8]) -> Self {
        let mut c = ZipCrypto {
            key0: 0x12345678,
            key1: 0x23456789,
            key2: 0x34567890,
        };
        for &b in password {
            c.update_keys(b);
        }
        c
    }

    fn update_keys(&mut self, byte: u8) {
        self.key0 = crc32_update(self.key0, byte);
        self.key1 = self
            .key1
            .wrapping_add(self.key0 & 0xFF)
            .wrapping_mul(134_775_813)
            .wrapping_add(1);
        self.key2 = crc32_update(self.key2, (self.key1 >> 24) as u8);
    }

    /// The keystream byte for the current key state.
    fn keystream_byte(&self) -> u8 {
        let temp = (self.key2 | 2) & 0xFFFF;
        (((temp.wrapping_mul(temp ^ 1)) >> 8) & 0xFF) as u8
    }

    /// Encrypt a single byte (updates the keys with the plaintext).
    pub fn encrypt_byte(&mut self, plain: u8) -> u8 {
        let cipher = plain ^ self.keystream_byte();
        self.update_keys(plain);
        cipher
    }

    /// Decrypt a single byte (updates the keys with the plaintext).
    pub fn decrypt_byte(&mut self, cipher: u8) -> u8 {
        let plain = cipher ^ self.keystream_byte();
        self.update_keys(plain);
        plain
    }

    /// Encrypt a buffer in place.
    pub fn encrypt(&mut self, data: &mut [u8]) {
        for b in data.iter_mut() {
            *b = self.encrypt_byte(*b);
        }
    }

    /// Decrypt a buffer in place.
    pub fn decrypt(&mut self, data: &mut [u8]) {
        for b in data.iter_mut() {
            *b = self.decrypt_byte(*b);
        }
    }
}

/// A deterministic PRNG (xorshift64) used to generate the encryption header.
/// The header bytes are arbitrary (readers only care about the decrypted
/// content), but making them deterministic keeps the write path reproducible.
struct XorShift64(u64);

impl XorShift64 {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

/// Encrypt `data` (the compressed bytes) with ZipCrypto, returning the
/// 12-byte encryption header followed by the encrypted data.
///
/// `crc` is the CRC-32 of the *uncompressed* content; its high byte is placed
/// in the last header byte so a reader can verify the password.
pub fn encrypt_data(password: &[u8], crc: u32, data: &[u8]) -> Vec<u8> {
    let mut crypto = ZipCrypto::new(password);

    // Deterministic seed derived from the password and CRC.
    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
    for &b in password {
        seed = seed.wrapping_mul(31).wrapping_add(b as u64);
    }
    seed ^= crc as u64;
    let mut rng = XorShift64(seed | 1);

    let mut header = [0u8; ENCRYPTION_HEADER_LEN];
    for b in header[..11].iter_mut() {
        *b = (rng.next() & 0xFF) as u8;
    }
    header[11] = (crc >> 24) as u8;

    let mut out = Vec::with_capacity(ENCRYPTION_HEADER_LEN + data.len());
    crypto.encrypt(&mut header);
    out.extend_from_slice(&header);
    let mut enc = data.to_vec();
    crypto.encrypt(&mut enc);
    out.extend_from_slice(&enc);
    out
}

/// A `Source` that decrypts (ZipCrypto) the bytes it reads from an inner
/// source. Used on the read path to decrypt an entry's compressed data before
/// it reaches the decompressor.
pub(crate) struct DecryptingSource {
    inner: Box<dyn Source>,
    crypto: ZipCrypto,
}

impl DecryptingSource {
    pub(crate) fn new(inner: Box<dyn Source>, crypto: ZipCrypto) -> Self {
        DecryptingSource { inner, crypto }
    }
}

impl Read for DecryptingSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.crypto.decrypt(&mut buf[..n]);
        Ok(n)
    }
}

impl Seek for DecryptingSource {
    fn seek(&mut self, _pos: SeekFrom) -> io::Result<u64> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "seek not supported on decrypting source",
        ))
    }
}

impl Source for DecryptingSource {
    fn supports(&self) -> crate::source::Supports {
        crate::source::Supports::Readable
    }

    fn duplicate(&self) -> Result<Box<dyn Source>> {
        // A decrypting source is created fresh per entry and is never
        // duplicated; the keystream state is position-dependent.
        Err(ZipError::new(ZipErrorCode::Opnotsupp))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_then_decrypt_roundtrips() {
        let password = b"kzip-test-password";
        let data = b"the quick brown fox jumps over the lazy dog".repeat(10);
        let crc = crc32fast::hash(data.as_slice());

        let encrypted = encrypt_data(password, crc, &data);
        assert_eq!(encrypted.len(), ENCRYPTION_HEADER_LEN + data.len());

        // Decrypt the header and verify the CRC byte.
        let mut crypto = ZipCrypto::new(password);
        let mut header = [0u8; ENCRYPTION_HEADER_LEN];
        header.copy_from_slice(&encrypted[..ENCRYPTION_HEADER_LEN]);
        crypto.decrypt(&mut header);
        assert_eq!(header[11], (crc >> 24) as u8);

        // Decrypt the payload.
        let mut payload = encrypted[ENCRYPTION_HEADER_LEN..].to_vec();
        crypto.decrypt(&mut payload);
        assert_eq!(payload, data);
    }

    #[test]
    fn wrong_password_fails_crc_check() {
        let password = b"correct-password";
        let data = b"some secret content".repeat(5);
        let crc = crc32fast::hash(data.as_slice());
        let encrypted = encrypt_data(password, crc, &data);

        // Wrong password: the header's final byte will not match the CRC byte.
        let mut crypto = ZipCrypto::new(b"wrong-password");
        let mut header = [0u8; ENCRYPTION_HEADER_LEN];
        header.copy_from_slice(&encrypted[..ENCRYPTION_HEADER_LEN]);
        crypto.decrypt(&mut header);
        assert_ne!(header[11], (crc >> 24) as u8);
    }

    #[test]
    fn empty_password_works() {
        let data = b"data with empty password";
        let crc = crc32fast::hash(data);
        let encrypted = encrypt_data(b"", crc, data);
        let mut crypto = ZipCrypto::new(b"");
        let mut header = [0u8; ENCRYPTION_HEADER_LEN];
        header.copy_from_slice(&encrypted[..ENCRYPTION_HEADER_LEN]);
        crypto.decrypt(&mut header);
        let mut payload = encrypted[ENCRYPTION_HEADER_LEN..].to_vec();
        crypto.decrypt(&mut payload);
        assert_eq!(payload, data.as_slice());
    }

    #[test]
    fn decrypting_source_decrypts_stream() {
        use std::io::Cursor;
        let password = b"pw";
        let data = b"streaming decrypt test payload ".repeat(20);
        let crc = crc32fast::hash(data.as_slice());
        let encrypted = encrypt_data(password, crc, &data);

        // Consume the header first, then wrap the rest in a DecryptingSource.
        let mut crypto = ZipCrypto::new(password);
        let mut header = [0u8; ENCRYPTION_HEADER_LEN];
        header.copy_from_slice(&encrypted[..ENCRYPTION_HEADER_LEN]);
        crypto.decrypt(&mut header);

        let inner: Box<dyn Source> = Box::new(Cursor::new(encrypted[ENCRYPTION_HEADER_LEN..].to_vec()));
        let mut dec = DecryptingSource::new(inner, crypto);
        let mut out = Vec::new();
        dec.read_to_end(&mut out).unwrap();
        assert_eq!(out, data);
    }
}
