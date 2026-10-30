use embassy_usb::class::hid::HidReader;
use embassy_usb::driver::Driver;
use rmk_types::led_indicator::LedIndicator;

use crate::hid::{HidError, HidReaderTrait};

pub(crate) struct UsbLedReader<'a, 'd, D: Driver<'d>> {
    hid_reader: &'a mut HidReader<'d, D, 1>,
}

impl<'a, 'd, D: Driver<'d>> UsbLedReader<'a, 'd, D> {
    pub(crate) fn new(hid_reader: &'a mut HidReader<'d, D, 1>) -> Self {
        Self { hid_reader }
    }
}

impl<'d, D: Driver<'d>> HidReaderTrait for UsbLedReader<'_, 'd, D> {
    type ReportType = LedIndicator;

    async fn read_report(&mut self) -> Result<Self::ReportType, HidError> {
        let mut buf = [0u8; 1];
        loop {
            match self.hid_reader.read(&mut buf).await {
                Ok(_) => return Ok(LedIndicator::from_bits(buf[0])),
                // Disabled until the host configures the device again, after a
                // bus reset or before the first enumeration: read once it has.
                Err(embassy_usb::class::hid::ReadError::Disabled) => self.hid_reader.ready().await,
                Err(e) => return Err(HidError::UsbReadError(e)),
            }
        }
    }
}
