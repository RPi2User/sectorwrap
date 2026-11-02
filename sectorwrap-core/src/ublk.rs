use crate::{Backend, SectorError, SectorMapper};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestOp {
    Read,
    Write,
    Flush,
    Discard,
}

pub struct UblkRequest<'a> {
    pub op: RequestOp,
    pub offset: u64,
    pub data: &'a mut [u8],
}

pub struct UblkTarget<B> {
    pub mapper: SectorMapper<B>,
}

impl<B: Backend> UblkTarget<B> {
    pub fn handle(&mut self, request: UblkRequest<'_>) -> Result<(), SectorError> {
        match request.op {
            RequestOp::Read => self.mapper.read(request.offset, request.data),
            RequestOp::Write => self.mapper.write(request.offset, request.data),
            RequestOp::Flush => self.mapper.flush(),
            RequestOp::Discard => Err(SectorError::UnsupportedOperation),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Geometry, MockBackend};

    #[test]
    fn dispatches_aligned_read_and_write() {
        let geometry = Geometry::for_sector_size(528).unwrap();
        let mut target = UblkTarget {
            mapper: SectorMapper {
                geometry,
                backend: MockBackend::new(geometry, 2),
            },
        };
        let mut write = vec![7; geometry.payload_bytes];
        target
            .handle(UblkRequest {
                op: RequestOp::Write,
                offset: 0,
                data: &mut write,
            })
            .unwrap();
        let mut read = vec![0; geometry.payload_bytes];
        target
            .handle(UblkRequest {
                op: RequestOp::Read,
                offset: 0,
                data: &mut read,
            })
            .unwrap();
        assert_eq!(read, write);
    }

    #[test]
    fn rejects_unaligned_request() {
        let geometry = Geometry::for_sector_size(520).unwrap();
        let mut target = UblkTarget {
            mapper: SectorMapper {
                geometry,
                backend: MockBackend::new(geometry, 1),
            },
        };
        let mut data = [0; 512];
        let error = target
            .handle(UblkRequest {
                op: RequestOp::Read,
                offset: 1,
                data: &mut data,
            })
            .unwrap_err();
        assert!(matches!(error, SectorError::Unaligned));
    }
}
