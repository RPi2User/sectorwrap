#![cfg(target_os = "linux")]

use crate::{Backend, Geometry, SectorError};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;

const SG_IO: u64 = 0x2285;
const SG_DXFER_FROM_DEV: i32 = -3;
const SG_DXFER_TO_DEV: i32 = -2;
const SG_DXFER_NONE: i32 = -1;

#[repr(C)]
struct SgIoHdr {
    interface_id: i32,
    dxfer_direction: i32,
    cmd_len: u8,
    mx_sb_len: u8,
    iovec_count: u16,
    dxfer_len: u32,
    dxferp: *mut std::ffi::c_void,
    cmdp: *mut u8,
    sbp: *mut u8,
    timeout: u32,
    flags: u32,
    pack_id: i32,
    usr_ptr: *mut std::ffi::c_void,
    status: u8,
    masked_status: u8,
    msg_status: u8,
    sb_len_wr: u8,
    host_status: u16,
    driver_status: u16,
    resid: i32,
    duration: u32,
    info: u32,
}

pub struct SgIoBackend {
    file: File,
    geometry: Geometry,
    timeout_ms: u32,
    blocks_per_physical: u32,
}

impl SgIoBackend {
    pub fn open(path: &str, geometry: Geometry) -> Result<Self, SectorError> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Ok(Self {
            file,
            geometry,
            timeout_ms: 60_000,
            blocks_per_physical: 1,
        })
    }

    pub fn capacity(&mut self) -> Result<(u64, usize), SectorError> {
        let mut cdb = [0u8; 16];
        cdb[0] = 0x9e;
        cdb[1] = 0x10;
        cdb[10..14].copy_from_slice(&32u32.to_be_bytes());
        let mut response = [0u8; 32];
        self.command(&mut cdb, SG_DXFER_FROM_DEV, Some(&mut response))?;
        let last_lba = u64::from_be_bytes(response[0..8].try_into().unwrap());
        let block_size = u32::from_be_bytes(response[8..12].try_into().unwrap()) as usize;
        let exponent = response[13] & 0x0f;
        self.blocks_per_physical = 1u32 << exponent;
        Ok((
            last_lba.checked_add(1).ok_or(SectorError::OutOfRange)?,
            block_size,
        ))
    }

    pub fn format(&mut self, sectors: u64) -> Result<(), SectorError> {
        let payload = vec![0u8; self.geometry.payload_bytes];
        let group = u64::from(self.blocks_per_physical);
        if !sectors.is_multiple_of(group) {
            return Err(SectorError::Unaligned);
        }
        let counter_width = sectors.to_string().len();
        let mut lba = 0;
        while lba < sectors {
            let count = (sectors - lba).min(group);
            let mut data =
                Vec::with_capacity(usize::try_from(count).unwrap() * self.geometry.physical_bytes);
            for offset in 0..count {
                data.extend_from_slice(&self.geometry.encode_sector(lba + offset, &payload)?);
            }
            self.write_blocks(lba / group, 1, &data)?;
            lba += count;
            if lba % 65_536 == 0 || lba == sectors {
                let percent = lba as f64 * 100.0 / sectors as f64;
                eprint!("\rFormatting: {lba:>counter_width$} / {sectors} ({percent:.1}%)");
                io::stderr().flush()?;
            }
        }
        eprintln!();
        self.flush()
    }

    fn read_blocks(&mut self, lba: u64, count: u32, out: &mut [u8]) -> Result<(), SectorError> {
        let mut cdb = [0u8; 16];
        cdb[0] = 0x88;
        cdb[2..10].copy_from_slice(
            &lba.checked_mul(u64::from(self.blocks_per_physical))
                .ok_or(SectorError::OutOfRange)?
                .to_be_bytes(),
        );
        cdb[10..14].copy_from_slice(
            &count
                .checked_mul(self.blocks_per_physical)
                .ok_or(SectorError::OutOfRange)?
                .to_be_bytes(),
        );
        self.command(&mut cdb, SG_DXFER_FROM_DEV, Some(out))
    }

    fn write_blocks(&mut self, lba: u64, count: u32, data: &[u8]) -> Result<(), SectorError> {
        let mut cdb = [0u8; 16];
        cdb[0] = 0x8a;
        cdb[2..10].copy_from_slice(
            &lba.checked_mul(u64::from(self.blocks_per_physical))
                .ok_or(SectorError::OutOfRange)?
                .to_be_bytes(),
        );
        cdb[10..14].copy_from_slice(
            &count
                .checked_mul(self.blocks_per_physical)
                .ok_or(SectorError::OutOfRange)?
                .to_be_bytes(),
        );
        let mut data = data.to_vec();
        self.command(&mut cdb, SG_DXFER_TO_DEV, Some(&mut data))
    }

    fn command(
        &mut self,
        cdb: &mut [u8],
        direction: i32,
        data: Option<&mut [u8]>,
    ) -> Result<(), SectorError> {
        let mut sense = [0u8; 64];
        let (ptr, len) = data
            .map(|buffer| (buffer.as_mut_ptr(), buffer.len()))
            .unwrap_or((std::ptr::null_mut(), 0));
        let mut header = SgIoHdr {
            interface_id: i32::from(b'S'),
            dxfer_direction: direction,
            cmd_len: cdb.len() as u8,
            mx_sb_len: sense.len() as u8,
            iovec_count: 0,
            dxfer_len: len as u32,
            dxferp: ptr.cast(),
            cmdp: cdb.as_mut_ptr(),
            sbp: sense.as_mut_ptr(),
            timeout: self.timeout_ms,
            flags: 0,
            pack_id: 0,
            usr_ptr: std::ptr::null_mut(),
            status: 0,
            masked_status: 0,
            msg_status: 0,
            sb_len_wr: 0,
            host_status: 0,
            driver_status: 0,
            resid: 0,
            duration: 0,
            info: 0,
        };
        let result = unsafe { ioctl(self.file.as_raw_fd(), SG_IO, &mut header) };
        if result < 0 {
            return Err(SectorError::Io(io::Error::last_os_error()));
        }
        if header.status != 0 || header.host_status != 0 || header.driver_status != 0 {
            return Err(SectorError::Io(io::Error::other(format!(
                "SCSI command failed: status={} host={} driver={}",
                header.status, header.host_status, header.driver_status
            ))));
        }
        if header.resid != 0 && direction == SG_DXFER_FROM_DEV {
            return Err(SectorError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "short SCSI data transfer",
            )));
        }
        Ok(())
    }
}

impl Backend for SgIoBackend {
    fn read_sector(&mut self, lba: u64, out: &mut [u8]) -> Result<(), SectorError> {
        if out.len() != self.geometry.payload_bytes {
            return Err(SectorError::InvalidSectorLength {
                actual: out.len(),
                expected: self.geometry.payload_bytes,
            });
        }
        let group = self.blocks_per_physical as usize;
        let index = usize::try_from(lba % u64::from(self.blocks_per_physical)).unwrap();
        let mut data = vec![0u8; self.geometry.physical_bytes * group];
        self.read_blocks(lba / u64::from(self.blocks_per_physical), 1, &mut data)?;
        let payload = self.geometry.decode_sector(
            lba,
            &data[index * self.geometry.physical_bytes..(index + 1) * self.geometry.physical_bytes],
        )?;
        out.copy_from_slice(&payload);
        Ok(())
    }

    fn write_sector(&mut self, lba: u64, data: &[u8]) -> Result<(), SectorError> {
        if data.len() != self.geometry.payload_bytes {
            return Err(SectorError::InvalidSectorLength {
                actual: data.len(),
                expected: self.geometry.payload_bytes,
            });
        }
        let encoded = self.geometry.encode_sector(lba, data)?;
        let group = self.blocks_per_physical as usize;
        let index = usize::try_from(lba % u64::from(self.blocks_per_physical)).unwrap();
        let mut group_data = vec![0u8; self.geometry.physical_bytes * group];
        if self.blocks_per_physical > 1 {
            self.read_blocks(
                lba / u64::from(self.blocks_per_physical),
                1,
                &mut group_data,
            )?;
        }
        group_data
            [index * self.geometry.physical_bytes..(index + 1) * self.geometry.physical_bytes]
            .copy_from_slice(&encoded);
        self.write_blocks(lba / u64::from(self.blocks_per_physical), 1, &group_data)
    }

    fn flush(&mut self) -> Result<(), SectorError> {
        let mut cdb = [0u8; 10];
        cdb[0] = 0x35;
        self.command(&mut cdb, SG_DXFER_NONE, None)
    }
}

unsafe fn ioctl(fd: i32, request: u64, arg: *mut SgIoHdr) -> i32 {
    unsafe extern "C" {
        fn ioctl(fd: i32, request: u64, ...) -> i32;
    }
    ioctl(fd, request, arg)
}
