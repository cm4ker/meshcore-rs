//! The internal flash for the store and the updates: the flash controller itself, or the
//! SoftDevice while it runs (`sd`), which then has the controller.

use embassy_nrf::nvmc::{self, Nvmc};
use embedded_storage::nor_flash::{ErrorType, NorFlash, NorFlashError, NorFlashErrorKind, ReadNorFlash};

use crate::sd;

const PAGE: u32 = 4096;

pub struct Flash {
    nvmc: Nvmc<'static>,
}

impl Flash {
    pub fn new(nvmc: Nvmc<'static>) -> Self {
        Self { nvmc }
    }
}

#[derive(Debug)]
pub enum Error {
    Nvmc(nvmc::Error),
    /// The SoftDevice did not get the work done.
    SoftDevice,
}

impl NorFlashError for Error {
    fn kind(&self) -> NorFlashErrorKind {
        match self {
            Error::Nvmc(e) => e.kind(),
            Error::SoftDevice => NorFlashErrorKind::Other,
        }
    }
}

impl ErrorType for Flash {
    type Error = Error;
}

impl ReadNorFlash for Flash {
    const READ_SIZE: usize = 1;

    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Error> {
        self.nvmc.read(offset, bytes).map_err(Error::Nvmc)
    }

    fn capacity(&self) -> usize {
        self.nvmc.capacity()
    }
}

impl NorFlash for Flash {
    const WRITE_SIZE: usize = 4;
    const ERASE_SIZE: usize = PAGE as usize;

    fn erase(&mut self, from: u32, to: u32) -> Result<(), Error> {
        if !sd::enabled() {
            return self.nvmc.erase(from, to).map_err(Error::Nvmc);
        }
        if !from.is_multiple_of(PAGE) || !to.is_multiple_of(PAGE) {
            return Err(Error::Nvmc(nvmc::Error::Unaligned));
        }
        match (from..to).step_by(PAGE as usize).all(sd::flash_erase) {
            true => Ok(()),
            false => Err(Error::SoftDevice),
        }
    }

    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Error> {
        if !sd::enabled() {
            return self.nvmc.write(offset, bytes).map_err(Error::Nvmc);
        }
        if !offset.is_multiple_of(4) || !bytes.len().is_multiple_of(4) {
            return Err(Error::Nvmc(nvmc::Error::Unaligned));
        }
        // the SoftDevice takes whole words, from a word-aligned buffer
        let mut words = [0u32; 64];
        for (i, chunk) in bytes.chunks(256).enumerate() {
            for (w, b) in words.iter_mut().zip(chunk.as_chunks::<4>().0) {
                *w = u32::from_le_bytes(*b);
            }
            if !sd::flash_write(offset + 256 * i as u32, &words[..chunk.len() / 4]) {
                return Err(Error::SoftDevice);
            }
        }
        Ok(())
    }
}
