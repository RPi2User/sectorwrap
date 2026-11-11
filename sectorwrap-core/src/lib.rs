use reed_solomon::{Decoder, Encoder};
use std::io;
use thiserror::Error;

#[cfg(target_os = "linux")]
pub mod sgio;
pub mod ublk;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protection {
    Padding,
    Lba,
    LbaBch { ecc_bytes: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub physical_bytes: usize,
    pub payload_bytes: usize,
    pub protection: Protection,
}

#[derive(Debug, Error)]
pub enum FormatError {
    #[error("physical sector must be larger than 512 bytes")]
    TooSmall,
    #[error("physical sector size {0} is not supported")]
    Unsupported(usize),
    #[error("payload size must be a non-zero multiple of 512 bytes")]
    InvalidPayload,
    #[error("sector size arithmetic overflow")]
    Overflow,
}

impl Geometry {
    pub fn for_sector_size(physical_bytes: usize) -> Result<Self, FormatError> {
        if physical_bytes < 512 {
            return Err(FormatError::TooSmall);
        }
        if physical_bytes == 512 {
            return Ok(Self {
                physical_bytes,
                payload_bytes: physical_bytes,
                protection: Protection::Padding,
            });
        }
        let payload_bytes = physical_bytes / 512 * 512;
        if payload_bytes == 0 {
            return Err(FormatError::InvalidPayload);
        }
        let tail = physical_bytes - payload_bytes;
        let protection = match tail {
            0..=3 => Protection::Padding,
            4..=15 => Protection::Lba,
            16.. => Protection::LbaBch {
                ecc_bytes: tail - 4,
            },
        };
        if matches!(protection, Protection::LbaBch { ecc_bytes } if ecc_bytes < 12) {
            return Err(FormatError::Unsupported(physical_bytes));
        }
        Ok(Self {
            physical_bytes,
            payload_bytes,
            protection,
        })
    }

    pub fn tail_bytes(self) -> usize {
        self.physical_bytes - self.payload_bytes
    }
}

#[derive(Debug, Error)]
pub enum SectorError {
    #[error("buffer length {actual} does not match physical sector size {expected}")]
    InvalidSectorLength { actual: usize, expected: usize },
    #[error("LBA {lba} does not fit in the 32-bit protection word")]
    LbaOverflow { lba: u64 },
    #[error("stored LBA {stored} does not match requested LBA {expected}")]
    LbaMismatch { stored: u32, expected: u32 },
    #[error("BCH reference parity mismatch")]
    EccMismatch,
    #[error("reserved protection bytes are not valid padding")]
    PaddingMismatch,
    #[error("invalid request alignment")]
    Unaligned,
    #[error("request is outside the backend geometry")]
    OutOfRange,
    #[error("discard is not supported by this backend")]
    UnsupportedOperation,
    #[error("backend I/O error: {0}")]
    Io(#[from] io::Error),
}

/// Version 1 reference protection. Reed-Solomon is a BCH code over GF(2^8).
const PROFILE_V1: u8 = 1;

fn bch_chunks(data_len: usize, ecc_len: usize) -> usize {
    let total = data_len + ecc_len;
    total.div_ceil(200)
}

fn write_ecc(input: &[u8], out: &mut [u8]) {
    let chunks = bch_chunks(input.len(), out.len());
    let base_ecc = out.len() / chunks;
    let extra = out.len() % chunks;
    let mut input_pos = 0;
    let mut out_pos = 0;
    for i in 0..chunks {
        let ecc_len = base_ecc + usize::from(i < extra);
        let data_len = (input.len() - input_pos).min(255 - ecc_len);
        let encoded = Encoder::new(ecc_len).encode(&input[input_pos..input_pos + data_len]);
        out[out_pos..out_pos + ecc_len].copy_from_slice(encoded.ecc());
        input_pos += data_len;
        out_pos += ecc_len;
    }
}

fn correct_bch(input: &[u8], stored: &[u8], output: &mut [u8]) -> bool {
    let chunks = bch_chunks(input.len(), stored.len());
    let base_ecc = stored.len() / chunks;
    let extra = stored.len() % chunks;
    let mut input_pos = 0;
    let mut out_pos = 0;
    let mut ecc_pos = 0;
    for i in 0..chunks {
        let ecc_len = base_ecc + usize::from(i < extra);
        let data_len = (input.len() - input_pos).min(255 - ecc_len);
        let mut message = Vec::with_capacity(data_len + ecc_len);
        message.extend_from_slice(&input[input_pos..input_pos + data_len]);
        message.extend_from_slice(&stored[ecc_pos..ecc_pos + ecc_len]);
        let corrected = match Decoder::new(ecc_len).correct(&message, None) {
            Ok(value) => value,
            Err(_) => return false,
        };
        output[out_pos..out_pos + data_len].copy_from_slice(corrected.data());
        input_pos += data_len;
        out_pos += data_len;
        ecc_pos += ecc_len;
    }
    true
}

impl Geometry {
    pub fn encode_sector(&self, lba: u64, payload: &[u8]) -> Result<Vec<u8>, SectorError> {
        if payload.len() != self.payload_bytes {
            return Err(SectorError::InvalidSectorLength {
                actual: payload.len(),
                expected: self.payload_bytes,
            });
        }
        let lba32 = u32::try_from(lba).map_err(|_| SectorError::LbaOverflow { lba })?;
        let mut sector = vec![0xff; self.physical_bytes];
        sector[..self.payload_bytes].copy_from_slice(payload);
        match self.protection {
            Protection::Padding => {}
            Protection::Lba => {
                sector[self.payload_bytes..self.payload_bytes + 4]
                    .copy_from_slice(&lba32.to_le_bytes());
            }
            Protection::LbaBch { ecc_bytes } => {
                let tail = &mut sector[self.payload_bytes..];
                tail[..4].copy_from_slice(&lba32.to_le_bytes());
                let mut input = Vec::with_capacity(1 + 4 + self.payload_bytes);
                input.push(PROFILE_V1);
                input.extend_from_slice(&lba32.to_le_bytes());
                input.extend_from_slice(payload);
                write_ecc(&input, &mut tail[4..4 + ecc_bytes]);
            }
        }
        Ok(sector)
    }

    pub fn decode_sector(&self, lba: u64, sector: &[u8]) -> Result<Vec<u8>, SectorError> {
        if sector.len() != self.physical_bytes {
            return Err(SectorError::InvalidSectorLength {
                actual: sector.len(),
                expected: self.physical_bytes,
            });
        }
        let lba32 = u32::try_from(lba).map_err(|_| SectorError::LbaOverflow { lba })?;
        match self.protection {
            Protection::Padding => {
                if sector[self.payload_bytes..]
                    .iter()
                    .any(|byte| *byte != 0xff)
                {
                    return Err(SectorError::PaddingMismatch);
                }
            }
            Protection::Lba => {
                let stored = u32::from_le_bytes(
                    sector[self.payload_bytes..self.payload_bytes + 4]
                        .try_into()
                        .unwrap(),
                );
                if stored != lba32 {
                    return Err(SectorError::LbaMismatch {
                        stored,
                        expected: lba32,
                    });
                }
                if sector[self.payload_bytes + 4..]
                    .iter()
                    .any(|byte| *byte != 0xff)
                {
                    return Err(SectorError::PaddingMismatch);
                }
            }
            Protection::LbaBch { ecc_bytes } => {
                let tail = &sector[self.payload_bytes..];
                let stored = u32::from_le_bytes(tail[..4].try_into().unwrap());
                if stored != lba32 {
                    return Err(SectorError::LbaMismatch {
                        stored,
                        expected: lba32,
                    });
                }
                let mut input = Vec::with_capacity(1 + 4 + self.payload_bytes);
                input.push(PROFILE_V1);
                input.extend_from_slice(&lba32.to_le_bytes());
                input.extend_from_slice(&sector[..self.payload_bytes]);
                let mut corrected = vec![0; input.len()];
                if !correct_bch(&input, &tail[4..4 + ecc_bytes], &mut corrected) {
                    return Err(SectorError::EccMismatch);
                }
                if corrected[5..] != sector[..self.payload_bytes] {
                    return Ok(corrected[5..].to_vec());
                }
            }
        }
        Ok(sector[..self.payload_bytes].to_vec())
    }
}

pub trait Backend {
    fn read_sector(&mut self, lba: u64, out: &mut [u8]) -> Result<(), SectorError>;
    fn write_sector(&mut self, lba: u64, data: &[u8]) -> Result<(), SectorError>;
    fn flush(&mut self) -> Result<(), SectorError>;

    fn read_sectors(
        &mut self,
        lba: u64,
        out: &mut [u8],
        sector_bytes: usize,
    ) -> Result<(), SectorError> {
        if sector_bytes == 0 || !out.len().is_multiple_of(sector_bytes) {
            return Err(SectorError::Unaligned);
        }
        for (index, chunk) in out.chunks_exact_mut(sector_bytes).enumerate() {
            self.read_sector(lba + index as u64, chunk)?;
        }
        Ok(())
    }

    fn write_sectors(
        &mut self,
        lba: u64,
        data: &[u8],
        sector_bytes: usize,
    ) -> Result<(), SectorError> {
        if sector_bytes == 0 || !data.len().is_multiple_of(sector_bytes) {
            return Err(SectorError::Unaligned);
        }
        for (index, chunk) in data.chunks_exact(sector_bytes).enumerate() {
            self.write_sector(lba + index as u64, chunk)?;
        }
        Ok(())
    }
}

pub struct SectorMapper<B> {
    pub geometry: Geometry,
    pub backend: B,
}

impl<B: Backend> SectorMapper<B> {
    pub fn read(&mut self, offset: u64, out: &mut [u8]) -> Result<(), SectorError> {
        self.check_request(offset, out.len())?;
        self.backend.read_sectors(
            offset / self.geometry.payload_bytes as u64,
            out,
            self.geometry.payload_bytes,
        )
    }

    pub fn write(&mut self, offset: u64, data: &[u8]) -> Result<(), SectorError> {
        self.check_request(offset, data.len())?;
        self.backend.write_sectors(
            offset / self.geometry.payload_bytes as u64,
            data,
            self.geometry.payload_bytes,
        )
    }

    pub fn flush(&mut self) -> Result<(), SectorError> {
        self.backend.flush()
    }

    fn check_request(&self, offset: u64, len: usize) -> Result<(), SectorError> {
        if !offset.is_multiple_of(self.geometry.payload_bytes as u64)
            || len == 0
            || !len.is_multiple_of(self.geometry.payload_bytes)
        {
            return Err(SectorError::Unaligned);
        }
        Ok(())
    }
}

pub struct MockBackend {
    geometry: Geometry,
    image: Vec<u8>,
}

impl MockBackend {
    pub fn new(geometry: Geometry, sectors: usize) -> Self {
        Self {
            geometry,
            image: vec![0xff; geometry.physical_bytes * sectors],
        }
    }

    pub fn raw_sector_mut(&mut self, lba: usize) -> Option<&mut [u8]> {
        let start = lba.checked_mul(self.geometry.physical_bytes)?;
        self.image
            .get_mut(start..start + self.geometry.physical_bytes)
    }
}

impl Backend for MockBackend {
    fn read_sector(&mut self, lba: u64, out: &mut [u8]) -> Result<(), SectorError> {
        let start = usize::try_from(lba)
            .ok()
            .and_then(|n| n.checked_mul(self.geometry.physical_bytes))
            .ok_or(SectorError::OutOfRange)?;
        let raw = self
            .image
            .get(start..start + self.geometry.physical_bytes)
            .ok_or(SectorError::OutOfRange)?;
        let payload = self.geometry.decode_sector(lba, raw)?;
        if out.len() != payload.len() {
            return Err(SectorError::InvalidSectorLength {
                actual: out.len(),
                expected: payload.len(),
            });
        }
        out.copy_from_slice(&payload);
        Ok(())
    }

    fn write_sector(&mut self, lba: u64, data: &[u8]) -> Result<(), SectorError> {
        let encoded = self.geometry.encode_sector(lba, data)?;
        let start = usize::try_from(lba)
            .ok()
            .and_then(|n| n.checked_mul(self.geometry.physical_bytes))
            .ok_or(SectorError::OutOfRange)?;
        let raw = self
            .image
            .get_mut(start..start + self.geometry.physical_bytes)
            .ok_or(SectorError::OutOfRange)?;
        raw.copy_from_slice(&encoded);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), SectorError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_reference_geometries() {
        assert_eq!(
            Geometry::for_sector_size(512).unwrap(),
            Geometry {
                physical_bytes: 512,
                payload_bytes: 512,
                protection: Protection::Padding
            }
        );
        assert_eq!(
            Geometry::for_sector_size(520).unwrap().protection,
            Protection::Lba
        );
        assert_eq!(
            Geometry::for_sector_size(528).unwrap().protection,
            Protection::LbaBch { ecc_bytes: 12 }
        );
        assert_eq!(
            Geometry::for_sector_size(4224).unwrap().protection,
            Protection::LbaBch { ecc_bytes: 124 }
        );
        assert_eq!(
            Geometry::for_sector_size(513).unwrap().protection,
            Protection::Padding
        );
    }

    #[test]
    fn mock_round_trip_and_lba_detection() {
        let g = Geometry::for_sector_size(528).unwrap();
        let mut b = MockBackend::new(g, 2);
        let data = vec![0x42; g.payload_bytes];
        b.write_sector(1, &data).unwrap();
        let mut out = vec![0; g.payload_bytes];
        b.read_sector(1, &mut out).unwrap();
        assert_eq!(out, data);
        b.raw_sector_mut(1).unwrap()[g.payload_bytes] ^= 1;
        assert!(matches!(
            b.read_sector(1, &mut out),
            Err(SectorError::LbaMismatch { .. })
        ));
    }

    #[test]
    fn detects_payload_corruption() {
        let g = Geometry::for_sector_size(4224).unwrap();
        let mut b = MockBackend::new(g, 1);
        b.write_sector(0, &vec![0; g.payload_bytes]).unwrap();
        b.raw_sector_mut(0).unwrap()[0] = 1;
        let mut out = vec![0; g.payload_bytes];
        assert!(b.read_sector(0, &mut out).is_ok());
        assert_eq!(out[0], 0);
    }
}
