//! Little-endian byte reader helpers over a `Read`.
//!
//! libzip reads the ZIP structures with explicit, checked little-endian reads
//! (`_zip_buffer_get_{16,32,64}`). This module provides equivalent primitives
//! that map short reads / IO failures onto [`ZipErrorCode::Eof`] / [`ZipErrorCode::Read`].

use crate::error::{Result, ZipError, ZipErrorCode};
use std::io::Read;

/// Reads exactly `len` bytes from `r` into `buf` (which must be `len` long).
///
/// Maps a premature end-of-stream to [`ZipErrorCode::Eof`] and a genuine IO
/// error to [`ZipErrorCode::Read`] (preserving the system error).
pub fn read_exact(r: &mut impl Read, buf: &mut [u8]) -> Result<()> {
    match r.read_exact(buf) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            Err(ZipError::new(ZipErrorCode::Eof))
        }
        Err(e) => Err(ZipError::with_system(ZipErrorCode::Read, e)),
    }
}

/// Reads a little-endian `u16`.
pub fn read_u16(r: &mut impl Read) -> Result<u16> {
    let mut b = [0u8; 2];
    read_exact(r, &mut b)?;
    Ok(u16::from_le_bytes(b))
}

/// Reads a little-endian `u32`.
pub fn read_u32(r: &mut impl Read) -> Result<u32> {
    let mut b = [0u8; 4];
    read_exact(r, &mut b)?;
    Ok(u32::from_le_bytes(b))
}

/// Reads a little-endian `u64`.
pub fn read_u64(r: &mut impl Read) -> Result<u64> {
    let mut b = [0u8; 8];
    read_exact(r, &mut b)?;
    Ok(u64::from_le_bytes(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn reads_le_scalars() {
        let mut c = Cursor::new(vec![0x34, 0x12, 0x78, 0x56, 0x34, 0x12]);
        assert_eq!(read_u16(&mut c).unwrap(), 0x1234);
        assert_eq!(read_u32(&mut c).unwrap(), 0x12345678);
    }

    #[test]
    fn short_read_maps_to_eof() {
        let mut c = Cursor::new(vec![1u8]);
        assert_eq!(read_u32(&mut c).unwrap_err().code(), ZipErrorCode::Eof);
    }
}
