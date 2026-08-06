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

// ---------------------------------------------------------------------------
// WinZip AES (128/192/256) encryption/decryption
// ---------------------------------------------------------------------------
//
// On-disk layout of an AES-encrypted entry's data (from the data start):
//   [ salt ] [ 2-byte password-verification value ]
//   [ AES-CTR-encrypted compressed bytes ] [ 10-byte HMAC-SHA1 tag ]
//
// - salt length = key_length / 2 (8 / 12 / 16 for AES-128/192/256).
// - Key derivation: PBKDF2-HMAC-SHA1(password, salt, 1000 iterations) yields
//   `2*key_length + 2` bytes: `[aes_key][hmac_key][2-byte verify]`.
// - AES-CTR: 128-bit big-endian counter starting at value 1. libzip increments
//   only the low 8 bytes little-endian; for realistic data sizes this is
//   identical to a full 128-bit increment, so `Ctr128BE` matches byte-for-byte.
// - HMAC-SHA1 is computed over the CIPHERTEXT and the first 10 bytes (AE-2)
//   are stored as the authentication tag.

use crate::constant::{
    encryption, WINZIP_AES_HMAC_LENGTH, WINZIP_AES_PASSWORD_VERIFY_LENGTH,
    WINZIP_AES_PBKDF2_ITERATIONS,
};
use aes::cipher::generic_array::{typenum::U16, GenericArray};
use aes::cipher::{BlockEncrypt, BlockSizeUser, KeyInit};
use aes::{Aes128, Aes192, Aes256};
use hmac::{Hmac, Mac};
use pbkdf2::pbkdf2_hmac;
use sha1::Sha1;

type HmacSha1 = Hmac<Sha1>;

/// Whether `method` is a supported WinZip AES method.
pub fn is_aes_method(method: u16) -> bool {
    matches!(
        method,
        encryption::AES_128 | encryption::AES_192 | encryption::AES_256
    )
}

/// AES key length in bytes for an AES encryption method (16/24/32), or 0.
pub fn aes_key_length(method: u16) -> usize {
    match method {
        encryption::AES_128 => 16,
        encryption::AES_192 => 24,
        encryption::AES_256 => 32,
        _ => 0,
    }
}

/// AES salt length in bytes (key_length / 2), or 0 for non-AES methods.
pub fn aes_salt_length(method: u16) -> usize {
    aes_key_length(method) / 2
}

/// The strength byte stored in the WinZip AES extra field (1/2/3), or 0.
pub fn aes_strength(method: u16) -> u8 {
    match method {
        encryption::AES_128 => 1,
        encryption::AES_192 => 2,
        encryption::AES_256 => 3,
        _ => 0,
    }
}

/// Map a strength byte back to the AES encryption method.
pub fn aes_method_from_strength(strength: u8) -> u16 {
    match strength {
        1 => encryption::AES_128,
        2 => encryption::AES_192,
        3 => encryption::AES_256,
        _ => encryption::UNKNOWN,
    }
}

/// Derive the AES key, HMAC key, and 2-byte password-verification value from a
/// password + salt, exactly as libzip's `_zip_winzip_aes_new` does.
fn derive_aes_keys(
    method: u16,
    password: &[u8],
    salt: &[u8],
    aes_key: &mut [u8],
    hmac_key: &mut [u8],
    password_verify: &mut [u8; WINZIP_AES_PASSWORD_VERIFY_LENGTH],
) {
    let key_length = aes_key_length(method);
    let mut buf = [0u8; 2 * 32 + WINZIP_AES_PASSWORD_VERIFY_LENGTH]; // max AES-256
    let out_len = 2 * key_length + WINZIP_AES_PASSWORD_VERIFY_LENGTH;
    pbkdf2_hmac::<Sha1>(
        password,
        salt,
        WINZIP_AES_PBKDF2_ITERATIONS,
        &mut buf[..out_len],
    );
    aes_key.copy_from_slice(&buf[..key_length]);
    hmac_key.copy_from_slice(&buf[key_length..2 * key_length]);
    password_verify.copy_from_slice(&buf[2 * key_length..out_len]);
}

/// Apply the AES-CTR keystream to `data`, in place, mirroring libzip's
/// `_zip_winzip_aes` `aes_crypt` exactly: a 16-byte counter whose low 8 bytes
/// are a little-endian 64-bit counter starting at 1 (high 8 bytes stay 0); each
/// keystream block is `AES_encrypt(counter)` XORed with the data.
fn aes_ctr_apply(method: u16, key: &[u8], data: &mut [u8]) {
    match method {
        encryption::AES_128 => aes_ctr_apply_inner::<Aes128>(key, data),
        encryption::AES_192 => aes_ctr_apply_inner::<Aes192>(key, data),
        encryption::AES_256 => aes_ctr_apply_inner::<Aes256>(key, data),
        _ => {}
    }
}

/// Generic AES-CTR over a 16-byte block cipher, matching libzip byte-for-byte.
fn aes_ctr_apply_inner<C>(key: &[u8], data: &mut [u8])
where
    C: BlockEncrypt + KeyInit,
    C: BlockSizeUser<BlockSize = U16>,
{
    let cipher = C::new_from_slice(key).expect("valid AES key length");
    // libzip counter: low 8 bytes = little-endian counter starting at 1; the
    // high 8 bytes remain zero.
    let mut counter: GenericArray<u8, U16> = GenericArray::default();
    counter[0] = 1;
    let mut pos = 0usize;
    while pos < data.len() {
        let mut pad: GenericArray<u8, U16> = GenericArray::default();
        cipher.encrypt_block_b2b(&counter, &mut pad);
        let n = 16usize.min(data.len() - pos);
        for j in 0..n {
            data[pos + j] ^= pad[j];
        }
        pos += 16;
        // Increment the low 8 bytes little-endian.
        for j in 0..8 {
            counter[j] = counter[j].wrapping_add(1);
            if counter[j] != 0 {
                break;
            }
        }
    }
}

/// Generate `len` pseudo-random bytes for the AES salt. The salt does not need
/// to be reproducible for interoperability (the C library reads any salt), so a
/// time/pid-seeded PRNG suffices and avoids an extra secure-RNG dependency.
pub fn random_salt(len: usize) -> Vec<u8> {
    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
    seed ^= std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    seed ^= (std::process::id() as u64).wrapping_shl(32);
    let mut rng = XorShift64(seed | 1);
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        out.extend_from_slice(&rng.next().to_le_bytes());
    }
    out.truncate(len);
    out
}

/// Encrypt (and authenticate) `data` with WinZip AES `method`, producing
/// `[salt][pwd_verify][ciphertext][hmac]`. `salt` must be `aes_salt_length(method)`
/// bytes. Returns the full encrypted region (the caller stores it as the entry's
/// data; its length is the entry's compressed size).
pub fn aes_encrypt_data(password: &[u8], method: u16, data: &[u8], salt: &[u8]) -> Vec<u8> {
    let key_length = aes_key_length(method);
    let salt_length = aes_salt_length(method);
    let mut aes_key = [0u8; 32];
    let mut hmac_key = [0u8; 32];
    let mut password_verify = [0u8; WINZIP_AES_PASSWORD_VERIFY_LENGTH];
    derive_aes_keys(
        method,
        password,
        salt,
        &mut aes_key[..key_length],
        &mut hmac_key[..key_length],
        &mut password_verify,
    );

    // Encrypt the compressed bytes with AES-CTR.
    let mut ciphertext = data.to_vec();
    aes_ctr_apply(method, &aes_key[..key_length], &mut ciphertext);

    // HMAC-SHA1 over the ciphertext; keep the first 10 bytes (AE-2).
    let mut mac =
        <HmacSha1 as Mac>::new_from_slice(&hmac_key[..key_length]).expect("valid HMAC key");
    mac.update(&ciphertext);
    let tag = mac.finalize().into_bytes();

    let mut out = Vec::with_capacity(
        salt_length + WINZIP_AES_PASSWORD_VERIFY_LENGTH + ciphertext.len() + WINZIP_AES_HMAC_LENGTH,
    );
    out.extend_from_slice(salt);
    out.extend_from_slice(&password_verify);
    out.extend_from_slice(&ciphertext);
    out.extend_from_slice(&tag[..WINZIP_AES_HMAC_LENGTH]);
    out
}

/// Decrypt and authenticate a WinZip AES region `data` of the form
/// `[salt][pwd_verify][ciphertext][hmac]`. Returns the plaintext (compressed
/// bytes) on success, or `Wrongpasswd` (password mismatch) / `Crc` (HMAC
/// authentication failure) / `TruncatedZip` (too short) — never a panic.
pub fn aes_decrypt_data(password: &[u8], method: u16, data: &[u8]) -> Result<Vec<u8>> {
    let key_length = aes_key_length(method);
    let salt_length = aes_salt_length(method);
    let aux = salt_length + WINZIP_AES_PASSWORD_VERIFY_LENGTH + WINZIP_AES_HMAC_LENGTH;
    if data.len() < aux {
        return Err(ZipError::new(ZipErrorCode::TruncatedZip));
    }
    let salt = &data[..salt_length];
    let stored_verify = &data[salt_length..salt_length + WINZIP_AES_PASSWORD_VERIFY_LENGTH];
    let ciphertext =
        &data[salt_length + WINZIP_AES_PASSWORD_VERIFY_LENGTH..data.len() - WINZIP_AES_HMAC_LENGTH];
    let stored_hmac = &data[data.len() - WINZIP_AES_HMAC_LENGTH..];

    let mut aes_key = [0u8; 32];
    let mut hmac_key = [0u8; 32];
    let mut password_verify = [0u8; WINZIP_AES_PASSWORD_VERIFY_LENGTH];
    derive_aes_keys(
        method,
        password,
        salt,
        &mut aes_key[..key_length],
        &mut hmac_key[..key_length],
        &mut password_verify,
    );

    // Reject wrong password before doing any crypto on the payload.
    if stored_verify != password_verify {
        return Err(ZipError::new(ZipErrorCode::Wrongpasswd));
    }

    // Verify HMAC-SHA1 over the ciphertext.
    let mut mac = <HmacSha1 as Mac>::new_from_slice(&hmac_key[..key_length])
        .map_err(|_| ZipError::new(ZipErrorCode::Internal))?;
    mac.update(ciphertext);
    let tag = mac.finalize().into_bytes();
    if &tag[..WINZIP_AES_HMAC_LENGTH] != stored_hmac {
        return Err(ZipError::new(ZipErrorCode::Crc));
    }

    // Decrypt the ciphertext (AES-CTR is symmetric).
    let mut plaintext = ciphertext.to_vec();
    aes_ctr_apply(method, &aes_key[..key_length], &mut plaintext);
    Ok(plaintext)
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

        let inner: Box<dyn Source> =
            Box::new(Cursor::new(encrypted[ENCRYPTION_HEADER_LEN..].to_vec()));
        let mut dec = DecryptingSource::new(inner, crypto);
        let mut out = Vec::new();
        dec.read_to_end(&mut out).unwrap();
        assert_eq!(out, data);
    }

    // ---- Phase 2: WinZip AES ----

    /// Known-answer vector extracted from libzip's regress archive
    /// `encrypt-aes128.zip` (password `foofoofoo`, entry `encrypted`).
    /// salt/pwdverify/ciphertext/hmac verified against libzip's own output.
    #[test]
    fn aes_known_answer_libzip_regress() {
        let password = b"foofoofoo";
        let method = encryption::AES_128;
        let salt = [0x7d, 0x7a, 0x78, 0x41, 0x12, 0x73, 0xc2, 0x69];
        let region = [
            // salt
            0x7d, 0x7a, 0x78, 0x41, 0x12, 0x73, 0xc2, 0x69, // password verify
            0x1d, 0xcc, // ciphertext
            0x99, 0x9a, 0x8d, 0x75, 0x92, 0x4d, 0x67, 0xa5, 0xd5, 0x86, // hmac
            0x94, 0xe5, 0x72, 0x1c, 0xf7, 0xea, 0x31, 0xa0, 0x61, 0x3b,
        ];
        let plain = aes_decrypt_data(password, method, &region).unwrap();
        assert_eq!(plain, b"encrypted\n");

        // Round-trip: re-encrypt with the same salt reproduces the same region.
        let re_enc = aes_encrypt_data(password, method, b"encrypted\n", &salt);
        assert_eq!(re_enc, region);
    }

    #[test]
    fn aes_wrong_password_is_wrongpass() {
        let password = b"correct-password";
        let method = encryption::AES_256;
        let data = b"secret aes payload ".repeat(16);
        let salt = random_salt(aes_salt_length(method));
        let region = aes_encrypt_data(password, method, &data, &salt);
        let err = aes_decrypt_data(b"wrong-password", method, &region).unwrap_err();
        assert_eq!(err.code(), ZipErrorCode::Wrongpasswd);
    }

    #[test]
    fn aes_corrupted_ciphertext_is_crc() {
        let password = b"correct-password";
        let method = encryption::AES_256;
        let data = b"some aes payload that must be authenticated ".repeat(8);
        let salt = random_salt(aes_salt_length(method));
        let mut region = aes_encrypt_data(password, method, &data, &salt);
        // Flip a ciphertext byte (past the header, before the hmac).
        let cipher_pos = aes_salt_length(method) + WINZIP_AES_PASSWORD_VERIFY_LENGTH;
        region[cipher_pos] ^= 0xFF;
        let err = aes_decrypt_data(password, method, &region).unwrap_err();
        assert_eq!(err.code(), ZipErrorCode::Crc);
    }

    #[test]
    fn aes_truncated_region_is_truncated_zip() {
        let password = b"pw";
        let method = encryption::AES_256;
        // Too short to even hold salt+verify+hmac.
        let err = aes_decrypt_data(password, method, &[0u8; 4]).unwrap_err();
        assert_eq!(err.code(), ZipErrorCode::TruncatedZip);
    }

    #[test]
    fn aes_roundtrip_all_key_sizes() {
        for (method, salt_len) in [
            (encryption::AES_128, 8usize),
            (encryption::AES_192, 12usize),
            (encryption::AES_256, 16usize),
        ] {
            let password = b"kzip-test-password";
            let data = format!("winzip aes roundtrip payload for {method:#06x}").repeat(8);
            let salt = random_salt(salt_len);
            let region = aes_encrypt_data(password, method, data.as_bytes(), &salt);
            let dec = aes_decrypt_data(password, method, &region).unwrap();
            assert_eq!(dec, data.as_bytes());
            // Region length = salt + 2 + data + 10.
            assert_eq!(
                region.len(),
                salt_len + WINZIP_AES_PASSWORD_VERIFY_LENGTH + data.len() + WINZIP_AES_HMAC_LENGTH
            );
        }
    }
}
