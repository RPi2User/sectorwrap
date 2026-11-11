use clap::Parser;
use libublk::io::{BufDescList, UblkDev, UblkQueue};
use libublk::{ctrl::UblkCtrl, BufDesc, UblkError, UblkFlags, UblkIORes};
use sectorwrap_core::sgio::SgIoBackend;
use sectorwrap_core::ublk::{RequestOp, UblkRequest, UblkTarget};
use sectorwrap_core::{Backend, Geometry, MockBackend, SectorMapper};
use std::sync::Arc;

#[derive(Parser, Debug)]
struct Args {
    #[arg(long, help = "Real SCSI generic device, for example /dev/sg5")]
    device: Option<String>,
    #[arg(long, default_value_t = 528)]
    sector_size: usize,
    #[arg(long)]
    sectors: Option<u64>,
    #[arg(long, default_value_t = -1)]
    id: i32,
    #[arg(long, default_value_t = 4)]
    queues: u16,
    #[arg(long, default_value_t = 64)]
    depth: u16,
    #[arg(
        long,
        help = "Skip LBA/ECC validation on reads; writes remain protected"
    )]
    raw_payload: bool,
}

enum HardwareBackend {
    Mock(MockBackend),
    Sg(SgIoBackend),
}

enum TargetFactory {
    Mock {
        geometry: Geometry,
        sectors: usize,
    },
    Sg {
        backend: Arc<SgIoBackend>,
        raw_payload: bool,
    },
}

impl TargetFactory {
    fn build(&self) -> Result<UblkTarget<HardwareBackend>, sectorwrap_core::SectorError> {
        let backend = match self {
            Self::Mock { geometry, sectors } => {
                HardwareBackend::Mock(MockBackend::new(*geometry, *sectors))
            }
            Self::Sg {
                backend,
                raw_payload,
            } => {
                let mut backend = backend.try_clone()?;
                backend.set_raw_payload(*raw_payload);
                HardwareBackend::Sg(backend)
            }
        };
        Ok(UblkTarget {
            mapper: SectorMapper {
                geometry: match self {
                    Self::Mock { geometry, .. } => *geometry,
                    Self::Sg { backend, .. } => backend.geometry(),
                },
                backend,
            },
        })
    }
}

impl Backend for HardwareBackend {
    fn read_sector(
        &mut self,
        lba: u64,
        out: &mut [u8],
    ) -> Result<(), sectorwrap_core::SectorError> {
        match self {
            Self::Mock(backend) => backend.read_sector(lba, out),
            Self::Sg(backend) => backend.read_sector(lba, out),
        }
    }

    fn write_sector(&mut self, lba: u64, data: &[u8]) -> Result<(), sectorwrap_core::SectorError> {
        match self {
            Self::Mock(backend) => backend.write_sector(lba, data),
            Self::Sg(backend) => backend.write_sector(lba, data),
        }
    }

    fn read_sectors(
        &mut self,
        lba: u64,
        out: &mut [u8],
        sector_bytes: usize,
    ) -> Result<(), sectorwrap_core::SectorError> {
        match self {
            Self::Mock(backend) => backend.read_sectors(lba, out, sector_bytes),
            Self::Sg(backend) => backend.read_sectors(lba, out, sector_bytes),
        }
    }

    fn write_sectors(
        &mut self,
        lba: u64,
        data: &[u8],
        sector_bytes: usize,
    ) -> Result<(), sectorwrap_core::SectorError> {
        match self {
            Self::Mock(backend) => backend.write_sectors(lba, data, sector_bytes),
            Self::Sg(backend) => backend.write_sectors(lba, data, sector_bytes),
        }
    }

    fn flush(&mut self) -> Result<(), sectorwrap_core::SectorError> {
        match self {
            Self::Mock(backend) => backend.flush(),
            Self::Sg(backend) => backend.flush(),
        }
    }
}

fn errno(error: &sectorwrap_core::SectorError) -> i32 {
    match error {
        sectorwrap_core::SectorError::Unaligned
        | sectorwrap_core::SectorError::LbaOverflow { .. }
        | sectorwrap_core::SectorError::OutOfRange
        | sectorwrap_core::SectorError::UnsupportedOperation => libc::EINVAL,
        sectorwrap_core::SectorError::LbaMismatch { .. }
        | sectorwrap_core::SectorError::EccMismatch
        | sectorwrap_core::SectorError::PaddingMismatch
        | sectorwrap_core::SectorError::InvalidSectorLength { .. }
        | sectorwrap_core::SectorError::Io(_) => libc::EIO,
    }
}

fn queue_handler(qid: u16, dev: &UblkDev, factory: Arc<TargetFactory>) {
    let mut target = match factory.build() {
        Ok(target) => target,
        Err(error) => {
            eprintln!("cannot create backend for queue {qid}: {error}");
            return;
        }
    };
    let buffers = Arc::new(dev.alloc_queue_io_bufs());
    let fetch_buffers = buffers.clone();
    let handler = move |q: &UblkQueue, tag: u16, _io: &libublk::io::UblkIOCtx| {
        let iod = q.get_iod(tag);
        let bytes = (iod.nr_sectors << 9) as usize;
        let offset = iod.start_sector << 9;
        let op = match iod.op_flags & 0xff {
            libublk::sys::UBLK_IO_OP_READ => RequestOp::Read,
            libublk::sys::UBLK_IO_OP_WRITE => RequestOp::Write,
            libublk::sys::UBLK_IO_OP_FLUSH => RequestOp::Flush,
            _ => {
                q.complete_io_cmd_unified(
                    tag,
                    BufDesc::Slice(fetch_buffers[tag as usize].as_slice()),
                    Err(UblkError::OtherError(-libc::EINVAL)),
                )
                .unwrap();
                return;
            }
        };
        let buffer = &fetch_buffers[tag as usize];
        // libublk owns the queue buffer for the duration of this callback.
        // Its safe immutable view is converted only for READ completion.
        let slice =
            unsafe { std::slice::from_raw_parts_mut(buffer.as_slice().as_ptr() as *mut u8, bytes) };
        let result = target
            .handle(UblkRequest {
                op,
                offset,
                data: slice,
            })
            .map(|_| bytes as i32)
            .map_err(|e| UblkError::OtherError(-errno(&e)));
        q.complete_io_cmd_unified(
            tag,
            BufDesc::Slice(fetch_buffers[tag as usize].as_slice()),
            result.map(UblkIORes::Result),
        )
        .unwrap();
    };
    UblkQueue::new(qid, dev)
        .unwrap()
        .submit_fetch_commands_unified(BufDescList::Slices(Some(&buffers)))
        .unwrap()
        .wait_and_handle_io(handler);
}

fn main() {
    env_logger::init();
    let args = Args::parse();
    let geometry = Geometry::for_sector_size(args.sector_size).unwrap_or_else(|e| {
        eprintln!("invalid sector geometry: {e}");
        std::process::exit(2);
    });
    let (factory, sectors) = if let Some(path) = args.device.as_deref() {
        let mut backend = SgIoBackend::open(path, geometry).unwrap_or_else(|e| {
            eprintln!("cannot open SG_IO device {path}: {e}");
            std::process::exit(2);
        });
        let (capacity, block_size) = backend.capacity().unwrap_or_else(|e| {
            eprintln!("cannot read capacity from {path}: {e}");
            std::process::exit(2);
        });
        if block_size != geometry.physical_bytes {
            eprintln!(
                "device reports {block_size}-byte sectors, but --sector-size is {}",
                geometry.physical_bytes
            );
            std::process::exit(2);
        }
        let sectors = args.sectors.unwrap_or(capacity);
        if sectors > capacity {
            eprintln!("--sectors exceeds the device capacity ({capacity})");
            std::process::exit(2);
        }
        eprintln!(
            "WARNING: writes through this ublk device modify {path}; \
             do not use the underlying device simultaneously"
        );
        (
            Arc::new(TargetFactory::Sg {
                backend: Arc::new(backend),
                raw_payload: args.raw_payload,
            }),
            sectors,
        )
    } else {
        let sectors = args.sectors.unwrap_or(1024);
        let sector_count = usize::try_from(sectors).unwrap_or_else(|_| {
            eprintln!("--sectors is too large for the mock backend");
            std::process::exit(2);
        });
        (
            Arc::new(TargetFactory::Mock {
                geometry,
                sectors: sector_count,
            }),
            sectors,
        )
    };
    let ctrl = UblkCtrl::new(
        Some("sectorwrap".to_owned()),
        args.id,
        args.queues as u32,
        args.depth as u32,
        1024 * 1024,
        0,
        0,
        UblkFlags::UBLK_DEV_F_ADD_DEV,
    )
    .expect("create ublk control");
    let size = geometry.payload_bytes as u64 * sectors;
    let factory_for_queues = factory.clone();
    ctrl.run_target(
        move |dev| {
            dev.set_default_params(size);
            Ok(())
        },
        move |qid, dev| queue_handler(qid, dev, factory_for_queues.clone()),
        |_ctrl| {},
    )
    .expect("run ublk target");
}
