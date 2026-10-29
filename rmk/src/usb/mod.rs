use core::cell::Cell;
use core::sync::atomic::{AtomicBool, Ordering};

use embassy_futures::join::join5;
use embassy_futures::select::{Either, select};
use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::signal::Signal;
#[cfg(feature = "usb_log")]
use embassy_usb::class::cdc_acm::CdcAcmClass;
use embassy_usb::class::hid::{HidProtocolMode, HidReader, HidReaderWriter, HidWriter, ReportId, RequestHandler};
use embassy_usb::control::OutResponse;
use embassy_usb::driver::{Driver, EndpointError};
use embassy_usb::{Builder, Handler, UsbDevice};
use rmk_types::DEVICE_INFO_STRING_MAX_LEN;
use rmk_types::connection::{ConnectionType, UsbState};
use rmk_types::led_indicator::LedIndicator;
use static_cell::StaticCell;
use usbd_hid::descriptor::AsInputReport;

use crate::RawMutex;
use crate::channel::USB_REPORT_CHANNEL;
use crate::config::DeviceConfig;
use crate::core_traits::Runnable;
#[cfg(feature = "steno")]
use crate::hid::StenoReport;
use crate::hid::{
    BATTERY, CompositeReport, CompositeReportType, HidError, HidWriterTrait, KeyboardReport, MOUSE_REPORT_SIZE, Report,
    run_led_reader,
};
use crate::light::UsbLedReader;
use crate::state::{current_usb_state, set_usb_state};

#[cfg(feature = "_dfu")]
pub(crate) mod dfu;
#[cfg(feature = "dongle")]
pub(crate) mod dfu_detach;
// The Rynk vendor interface serves the keyboard's Rynk session and the dongle's router.
#[cfg(any(feature = "rynk", all(feature = "dongle", not(feature = "vial"))))]
pub(crate) mod rynk;
#[cfg(feature = "vial")]
pub(crate) mod vial;

// A build has at most one host interface — Vial's HID report pair or the Rynk
// vendor bulk pair, and the protocols are mutually exclusive. A dongle relays
// its keyboard's protocol: Rynk unless `vial` says otherwise. The two modules
// expose the same names, so the rest of the file only talks to `host_usb`.
#[cfg(any(feature = "rynk", all(feature = "dongle", not(feature = "vial"))))]
use rynk as host_usb;
#[cfg(feature = "vial")]
use vial as host_usb;

pub(crate) static USB_REMOTE_WAKEUP: Signal<RawMutex, ()> = Signal::new();

// Set while the host has selected the boot protocol (BIOS, UEFI, some KVMs).
// Such a host reads only the boot keyboard interface, never the composite one.
static BOOT_MODE: AtomicBool = AtomicBool::new(false);

/// Serves one framed session over the USB byte stream: the keyboard's Vial or
/// Rynk service, the dongle's router, or `()` in a build that serves none.
/// Which one is a type parameter of [`UsbTransport`], so only the attached one
/// reaches the image.
pub(crate) trait HostSession {
    async fn serve<R: embedded_io_async::Read, W: embedded_io_async::Write>(&self, rx: &mut R, tx: &mut W);
}

impl HostSession for () {
    async fn serve<R: embedded_io_async::Read, W: embedded_io_async::Write>(&self, _rx: &mut R, _tx: &mut W) {
        core::future::pending().await
    }
}

/// Borrowed view over the USB HID IN endpoints used by the report writer task.
///
/// `UsbTransport` owns the USB device, readers, writers, host interface, and
/// optional logger; `run` borrows those fields separately so they can run
/// concurrently without moving the whole transport into one task.
pub(crate) struct UsbKeyboardWriter<'a, 'd, D: Driver<'d>> {
    pub(crate) keyboard_writer: &'a mut HidWriter<'d, D, 8>,
    pub(crate) other_writer: &'a mut HidWriter<'d, D, COMPOSITE_WRITE_SIZE>,
    #[cfg(feature = "steno")]
    pub(crate) steno_writer: &'a mut HidWriter<'d, D, 9>,
}

impl<'d, D: Driver<'d>> UsbKeyboardWriter<'_, 'd, D> {
    pub(crate) async fn run_writer(&mut self) -> ! {
        loop {
            // The battery report only while configured (it mustn't wake the
            // host), and dropped after 20 ms if the host isn't reading the
            // endpoint: it mustn't hold up the keys.
            let report = match select(USB_REPORT_CHANNEL.receive(), BATTERY.wait()).await {
                Either::First(report) => report,
                Either::Second(battery) => {
                    if current_usb_state() == UsbState::Configured {
                        let write = self.write_composite(CompositeReportType::Battery, &battery);
                        if let Ok(Err(e)) =
                            embassy_time::with_timeout(embassy_time::Duration::from_millis(20), write).await
                        {
                            error!("Failed to send battery report: {:?}", e);
                        }
                    }
                    continue;
                }
            };

            // EndpointError::Disabled never fires on non-OTG STM32/GD32
            // peripherals during suspend, so signal wakeup proactively when a
            // USB report is pending and the bus is suspended.
            if current_usb_state() == UsbState::Suspended {
                USB_REMOTE_WAKEUP.signal(());
                continue;
            }

            if let Err(e) = self.write_report(&report).await {
                error!("Failed to send report: {:?}", e);

                // OTG peripherals disable the endpoints in suspend, so there
                // Disabled asks for a wakeup. Otherwise a bus reset disabled
                // them until the host configures the device again. Either way
                // the report goes once the endpoint is back.
                if let HidError::UsbEndpointError(EndpointError::Disabled) = e {
                    if current_usb_state() == UsbState::Suspended {
                        USB_REMOTE_WAKEUP.signal(());
                    }
                    self.wait_enabled(&report).await;
                    if let Err(e) = self.write_report(&report).await {
                        error!("Failed to send report after wakeup: {:?}", e);
                    }
                }
            }
        }
    }

    /// Wait for the endpoint `report` goes out on to be enabled.
    async fn wait_enabled(&mut self, report: &Report) {
        match report {
            Report::KeyboardReport(_) => self.keyboard_writer.ready().await,
            Report::MouseReport(_) | Report::MediaKeyboardReport(_) | Report::SystemControlReport(_) => {
                self.other_writer.ready().await
            }
            #[cfg(feature = "steno")]
            Report::StenoReport(_) => self.steno_writer.ready().await,
        }
    }

    async fn write_composite<R: AsInputReport>(
        &mut self,
        kind: CompositeReportType,
        report: &R,
    ) -> Result<usize, HidError> {
        let mut buf = [0u8; COMPOSITE_WRITE_SIZE];
        buf[0] = kind as u8;
        let n = report
            .serialize(&mut buf[1..])
            .map_err(|_| HidError::ReportSerializeError)?;
        // A boot protocol host never drains this endpoint, and a blocked write here
        // would hold up the keyboard reports queued behind it.
        if BOOT_MODE.load(Ordering::Relaxed) {
            return Ok(n);
        }
        match embassy_time::with_timeout(COMPOSITE_WRITE_TIMEOUT, self.other_writer.write(&buf[0..n + 1])).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(HidError::UsbEndpointError(e)),
            Err(_) => warn!("The host isn't reading the composite interface; report dropped"),
        }
        Ok(n)
    }
}

impl<'d, D: Driver<'d>> HidWriterTrait for UsbKeyboardWriter<'_, 'd, D> {
    type ReportType = Report;

    async fn write_report(&mut self, report: &Self::ReportType) -> Result<usize, HidError> {
        match report {
            Report::KeyboardReport(keyboard_report) => {
                let mut buf: [u8; 8] = [0; 8];
                let n: usize = keyboard_report
                    .serialize(&mut buf)
                    .map_err(|_| HidError::ReportSerializeError)?;
                self.keyboard_writer
                    .write(&buf[0..n])
                    .await
                    .map_err(HidError::UsbEndpointError)?;
                LAST_KEYBOARD_REPORT.lock(|last| last.set(buf));
                Ok(n)
            }
            Report::MouseReport(r) => self.write_composite(CompositeReportType::Mouse, r).await,
            Report::MediaKeyboardReport(r) => self.write_composite(CompositeReportType::Media, r).await,
            Report::SystemControlReport(r) => self.write_composite(CompositeReportType::System, r).await,
            #[cfg(feature = "steno")]
            Report::StenoReport(steno_report) => {
                let mut buf: [u8; 9] = [0; 9];
                let n = steno_report
                    .serialize(&mut buf)
                    .map_err(|_| HidError::ReportSerializeError)?;

                // Cap on how long a steno report write is allowed to block. The host only
                // drains the steno IN endpoint while Plover is running; without this cap the
                // writer task stalls indefinitely (and starves keyboard reports) whenever
                // Plover is absent.
                match embassy_time::with_timeout(
                    embassy_time::Duration::from_millis(5),
                    self.steno_writer.write(&buf[0..n]),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => return Err(HidError::UsbEndpointError(e)),
                    Err(_) => {} // Plover not reading; drop this report and continue
                }
                Ok(n)
            }
        }
    }
}

/// bRequest value Windows sends to fetch the MS OS 2.0 descriptor set.
pub(crate) const MSOS_VENDOR_CODE: u8 = 0x52;

/// How long a mouse, media or system report waits for the host to take the one
/// before it. A host that reads only the boot keyboard interface (a BIOS, UEFI,
/// a KVM) never does, and the keyboard reports queued behind it must not wait
/// with it. The interface is polled every 1 ms, so a reading host is never close.
const COMPOSITE_WRITE_TIMEOUT: embassy_time::Duration = embassy_time::Duration::from_millis(50);

/// Report id byte plus the largest composite payload, which is the mouse report.
const COMPOSITE_WRITE_SIZE: usize = 1 + MOUSE_REPORT_SIZE;

/// Extra interfaces (usb_log, steno, dfu, rynk) overflow the 128-byte buffer.
const DEFAULT_CONFIG_DESC_SIZE: usize = if cfg!(any(
    feature = "usb_log",
    feature = "steno",
    feature = "_dfu",
    feature = "rynk",
    feature = "dongle"
)) {
    256
} else {
    128
};

fn default_config_descriptor() -> &'static mut [u8] {
    static CONFIG_DESC: StaticCell<[u8; DEFAULT_CONFIG_DESC_SIZE]> = StaticCell::new();
    &mut CONFIG_DESC.init([0; DEFAULT_CONFIG_DESC_SIZE])[..]
}

pub(crate) fn new_usb_builder<'d, D: Driver<'d>>(
    driver: D,
    keyboard_config: DeviceConfig<'d>,
    config_descriptor: &'d mut [u8],
) -> Builder<'d, D> {
    let mut usb_config = embassy_usb::Config::new(keyboard_config.vid, keyboard_config.pid);
    usb_config.manufacturer = Some(keyboard_config.manufacturer);
    usb_config.product = Some(keyboard_config.product_name);
    static SERIAL: StaticCell<heapless::String<DEVICE_INFO_STRING_MAX_LEN>> = StaticCell::new();
    usb_config.serial_number = Some(SERIAL.init(keyboard_config.serial_number).as_str());
    usb_config.max_power = 450;
    usb_config.supports_remote_wakeup = true;

    // Required for windows compatibility.
    usb_config.max_packet_size_0 = 64;
    usb_config.device_class = 0xEF;
    usb_config.device_sub_class = 0x02;
    usb_config.device_protocol = 0x01;
    usb_config.composite_with_iads = true;

    // Control buffer must be large enough for the largest DFU transfer block.
    #[cfg(feature = "_dfu")]
    const CONTROL_BUF_SIZE: usize = crate::dfu::BLOCK_SIZE_DFU;
    #[cfg(not(feature = "_dfu"))]
    const CONTROL_BUF_SIZE: usize = DEFAULT_CONFIG_DESC_SIZE;

    // The rynk MS OS 2.0 descriptor set (WinUSB binding) takes ~178 bytes, the
    // dongle's DFU runtime interface another 28, and the BOS platform
    // capability 28 on top of the 5-byte BOS header.
    const WINUSB: bool = cfg!(any(feature = "rynk", feature = "dongle"));
    const BOS_BUF_SIZE: usize = if WINUSB { 64 } else { 16 };
    const MSOS_BUF_SIZE: usize = if WINUSB { 256 } else { 16 };

    static BOS_DESC: StaticCell<[u8; BOS_BUF_SIZE]> = StaticCell::new();
    static MSOS_DESC: StaticCell<[u8; MSOS_BUF_SIZE]> = StaticCell::new();
    static CONTROL_BUF: StaticCell<[u8; CONTROL_BUF_SIZE]> = StaticCell::new();

    let mut builder = Builder::new(
        driver,
        usb_config,
        config_descriptor,
        &mut BOS_DESC.init([0; BOS_BUF_SIZE])[..],
        &mut MSOS_DESC.init([0; MSOS_BUF_SIZE])[..],
        &mut CONTROL_BUF.init([0; CONTROL_BUF_SIZE])[..],
    );

    static device_handler: StaticCell<UsbDeviceHandler> = StaticCell::new();
    builder.handler(device_handler.init(UsbDeviceHandler::new()));

    builder
}

/// USB transport. Owns the embassy-usb device + every HID reader/writer
/// pair and runs them concurrently for the lifetime of the program.
///
/// `S` is whatever serves the host interface in this binary — a keyboard's
/// Vial or Rynk service, a dongle's `DongleRouter`, or `()` for a build that
/// serves none. Picking it at compile time keeps the others out of the image.
pub struct UsbTransport<'a, D: Driver<'static>, S = ()> {
    device: UsbDevice<'static, D>,
    keyboard_reader: HidReader<'static, D, 1>,
    keyboard_writer: HidWriter<'static, D, 8>,
    other_writer: HidWriter<'static, D, COMPOSITE_WRITE_SIZE>,
    #[cfg(feature = "steno")]
    steno_writer: HidWriter<'static, D, 9>,
    /// Taken by `run`: the logger future consumes the CDC class.
    #[cfg(feature = "usb_log")]
    logger: Option<embassy_usb::class::cdc_acm::CdcAcmClass<'static, D>>,
    #[cfg(any(feature = "host", feature = "dongle"))]
    host_reader: host_usb::HostUsbReader<D>,
    #[cfg(any(feature = "host", feature = "dongle"))]
    host_writer: host_usb::HostUsbWriter<D>,
    /// Serves the host interface; `&()` until a binary attaches its own.
    session: &'a S,
}

impl<'a, D: Driver<'static>> UsbTransport<'a, D> {
    pub fn new(
        driver: D,
        device_config: DeviceConfig<'static>,
        #[cfg(feature = "dfu_split")] num_peripherals: usize,
    ) -> Self {
        UsbTransportBuilder::new(
            driver,
            device_config,
            default_config_descriptor(),
            #[cfg(feature = "dfu_split")]
            num_peripherals,
        )
        .build()
    }

    /// Start a USB stack the caller finishes, for binaries serving USB classes of
    /// their own alongside the keyboard.
    ///
    /// ```rust,ignore
    /// let mut builder = UsbTransport::builder(driver, device_config);
    /// let mut cdc = CdcAcmClass::new(builder.usb_builder(), CDC_STATE.init(State::new()), 64);
    /// let mut usb_transport = builder.build().with_host_service(&host_service);
    /// ```
    pub fn builder(driver: D, device_config: DeviceConfig<'static>) -> UsbTransportBuilder<D> {
        // A CDC ACM function costs ~66 descriptor bytes, an extra HID interface ~40.
        const SIZE: usize = DEFAULT_CONFIG_DESC_SIZE + 256;
        static CONFIG_DESC: StaticCell<[u8; SIZE]> = StaticCell::new();
        UsbTransportBuilder::new(
            driver,
            device_config,
            &mut CONFIG_DESC.init([0; SIZE])[..],
            #[cfg(feature = "dfu_split")]
            crate::SPLIT_PERIPHERALS_NUM,
        )
    }
}

/// A [`UsbTransport`] mid-construction. See [`UsbTransport::builder`].
pub struct UsbTransportBuilder<D: Driver<'static>> {
    builder: Builder<'static, D>,
    keyboard_rw: HidReaderWriter<'static, D, 1, 8>,
    other_writer: HidWriter<'static, D, COMPOSITE_WRITE_SIZE>,
    #[cfg(feature = "steno")]
    steno_writer: HidWriter<'static, D, 9>,
    #[cfg(feature = "usb_log")]
    logger: CdcAcmClass<'static, D>,
    #[cfg(any(feature = "host", feature = "dongle"))]
    host_reader: host_usb::HostUsbReader<D>,
    #[cfg(any(feature = "host", feature = "dongle"))]
    host_writer: host_usb::HostUsbWriter<D>,
}

impl<D: Driver<'static>> UsbTransportBuilder<D> {
    // Without `always`, opt-level="z" moves the whole struct between the two: +300 bytes.
    #[inline(always)]
    fn new(
        driver: D,
        device_config: DeviceConfig<'static>,
        config_descriptor: &'static mut [u8],
        #[cfg(feature = "dfu_split")] num_peripherals: usize,
    ) -> Self {
        #[cfg(feature = "_dfu")]
        let product_name = device_config.product_name;
        let mut builder: Builder<'static, D> = new_usb_builder(driver, device_config, config_descriptor);
        // Linux's usbhid driver auto-enables power/wakeup when it probes a
        // boot-protocol keyboard, so advertise Boot/Keyboard on the primary
        // HID interface.
        let keyboard_rw = add_usb_reader_writer!(
            &mut builder,
            KeyboardReport,
            1,
            8,
            8,
            ::embassy_usb::class::hid::HidSubclass::Boot,
            ::embassy_usb::class::hid::HidBootProtocol::Keyboard
        );
        let other_writer = add_usb_writer!(&mut builder, CompositeReport, COMPOSITE_WRITE_SIZE, 16);
        #[cfg(feature = "steno")]
        let steno_writer = add_usb_writer!(&mut builder, StenoReport, 9, 16);
        #[cfg(feature = "usb_log")]
        let logger = add_usb_logger!(&mut builder);

        #[cfg(feature = "_dfu")]
        dfu::register_dfu_iface(
            &mut builder,
            product_name,
            #[cfg(feature = "dfu_split")]
            num_peripherals,
        );

        #[cfg(any(feature = "host", feature = "dongle"))]
        let (host_reader, host_writer) = host_usb::build_host_usb(&mut builder);
        // After the vendor interface: whichever comes first writes the MS OS header.
        #[cfg(feature = "dongle")]
        dfu_detach::register(&mut builder);

        Self {
            builder,
            keyboard_rw,
            other_writer,
            #[cfg(feature = "steno")]
            steno_writer,
            #[cfg(feature = "usb_log")]
            logger,
            #[cfg(any(feature = "host", feature = "dongle"))]
            host_reader,
            #[cfg(any(feature = "host", feature = "dongle"))]
            host_writer,
        }
    }

    /// RMK's interfaces are already registered, so the keyboard keeps interface 0.
    pub fn usb_builder(&mut self) -> &mut Builder<'static, D> {
        &mut self.builder
    }

    #[inline(always)]
    pub fn build<'a>(self) -> UsbTransport<'a, D> {
        let (keyboard_reader, keyboard_writer) = self.keyboard_rw.split();

        UsbTransport {
            device: self.builder.build(),
            keyboard_reader,
            keyboard_writer,
            other_writer: self.other_writer,
            #[cfg(feature = "steno")]
            steno_writer: self.steno_writer,
            #[cfg(feature = "usb_log")]
            logger: Some(self.logger),
            #[cfg(any(feature = "host", feature = "dongle"))]
            host_reader: self.host_reader,
            #[cfg(any(feature = "host", feature = "dongle"))]
            host_writer: self.host_writer,
            session: &(),
        }
    }
}

impl<'a, D: Driver<'static>, S> UsbTransport<'a, D, S> {
    /// Attach the host-protocol service (Vial or Rynk, picked by feature).
    #[cfg(feature = "host")]
    pub fn with_host_service(
        self,
        service: &'a crate::host::HostService<'a>,
    ) -> UsbTransport<'a, D, crate::host::HostService<'a>> {
        self.serving(service)
    }

    /// Attach the dongle's router — this is what makes a binary a dongle. The
    /// same router goes to [`crate::dongle::Dongle`], which relays through it.
    #[cfg(feature = "dongle")]
    pub fn with_dongle_router(
        self,
        router: &'a crate::dongle::DongleRouter,
    ) -> UsbTransport<'a, D, crate::dongle::DongleRouter> {
        self.serving(router)
    }

    /// Rebuild around the session that answers the host interface.
    fn serving<T>(self, session: &'a T) -> UsbTransport<'a, D, T> {
        UsbTransport {
            device: self.device,
            keyboard_reader: self.keyboard_reader,
            keyboard_writer: self.keyboard_writer,
            other_writer: self.other_writer,
            #[cfg(feature = "steno")]
            steno_writer: self.steno_writer,
            #[cfg(feature = "usb_log")]
            logger: self.logger,
            #[cfg(any(feature = "host", feature = "dongle"))]
            host_reader: self.host_reader,
            #[cfg(any(feature = "host", feature = "dongle"))]
            host_writer: self.host_writer,
            session,
        }
    }
}

impl<D: Driver<'static>, S: HostSession> Runnable for UsbTransport<'_, D, S> {
    async fn run(&mut self) -> ! {
        let Self {
            device,
            keyboard_reader,
            keyboard_writer,
            other_writer,
            #[cfg(feature = "steno")]
            steno_writer,
            #[cfg(feature = "usb_log")]
            logger,
            #[cfg(any(feature = "host", feature = "dongle"))]
            host_reader,
            #[cfg(any(feature = "host", feature = "dongle"))]
            host_writer,
            session,
        } = self;

        let usb_device_task = async {
            loop {
                device.run_until_suspend().await;
                match select(device.wait_resume(), USB_REMOTE_WAKEUP.wait()).await {
                    // A wakeup asked for as the host resumed on its own is served:
                    // left set, it would wake the host the moment it next sleeps.
                    Either::First(_) => USB_REMOTE_WAKEUP.reset(),
                    Either::Second(_) => {
                        info!("USB remote wakeup requested");
                        if let Err(e) = device.remote_wakeup().await {
                            warn!("Remote wakeup failed: {:?}", e);
                        }
                    }
                }
            }
        };

        let mut writer = UsbKeyboardWriter {
            keyboard_writer,
            other_writer,
            #[cfg(feature = "steno")]
            steno_writer,
        };
        let writer_task = writer.run_writer();

        let mut led_reader = UsbLedReader::new(keyboard_reader);
        let led_task = run_led_reader(&mut led_reader, ConnectionType::Usb);

        #[cfg(any(feature = "host", feature = "dongle"))]
        let host_task = host_usb::run_host_usb(host_reader, host_writer, *session);
        #[cfg(not(any(feature = "host", feature = "dongle")))]
        let host_task = {
            // No host interface was built, so the session is always `()`.
            let _ = session;
            core::future::pending::<()>()
        };

        #[cfg(feature = "usb_log")]
        let logger_task = run_usb_logger(logger.take().expect("UsbTransport::run called twice"));
        #[cfg(not(feature = "usb_log"))]
        let logger_task = core::future::pending::<()>();

        join5(usb_device_task, writer_task, led_task, host_task, logger_task).await;
        unreachable!("UsbTransport sub-tasks must run forever");
    }
}

#[cfg(feature = "usb_log")]
async fn run_usb_logger<D: Driver<'static>>(logger_class: CdcAcmClass<'static, D>) {
    // Add a usb logger with log filter set to `Trace` to catch all logs.
    // The log level itself is set via the `max_level_*` feature of the log crate.
    let logger_fut =
        ::embassy_usb_logger::with_custom_style!(1024, log::LevelFilter::Trace, logger_class, |record, writer| {
            use core::fmt::Write;
            let ms = embassy_time::Instant::now().as_millis();
            let _ = write!(writer, "[{:>8}ms {:5}] {}\r\n", ms, record.level(), record.args());
        });
    logger_fut.await;
}

#[cfg(any(feature = "usb_log", feature = "_dfu"))]
pub async fn run_peripheral_usb<D: Driver<'static>>(driver: D, config: DeviceConfig<'static>) {
    #[cfg(feature = "_dfu")]
    let product_name = config.product_name;
    let mut builder = new_usb_builder(driver, config, default_config_descriptor());

    #[cfg(feature = "usb_log")]
    let logger_fut = run_usb_logger(add_usb_logger!(&mut builder));
    #[cfg(not(feature = "usb_log"))]
    let logger_fut = ::core::future::pending::<()>();

    #[cfg(feature = "_dfu")]
    dfu::register_dfu_iface(
        &mut builder,
        product_name,
        #[cfg(feature = "dfu_split")]
        0,
    );

    let mut usb_device = builder.build();

    ::embassy_futures::join::join(usb_device.run(), logger_fut).await;
}

#[cfg(feature = "usb_log")]
macro_rules! add_usb_logger {
    ($usb_builder:expr) => {{
        use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
        use static_cell::StaticCell;

        // The usb logger can be only initialized once, so just use a fixed name for the state
        static LOGGER_STATE: StaticCell<State> = StaticCell::new();
        let state = LOGGER_STATE.init(State::new());
        CdcAcmClass::new($usb_builder, state, embassy_usb_logger::MAX_PACKET_SIZE as u16)
    }};
}

/// Per-descriptor HID `(State, Config)` pair. `paste` generates the `static`s
/// from the descriptor name so each interface keeps its own State/Handler.
/// Size `$max_packet` to the actual report to conserve Packet Memory Area on tight parts.
macro_rules! usb_hid_state_and_config {
    ($descriptor:ty, $max_packet:expr, $subclass:expr, $protocol:expr) => {{
        use usbd_hid::descriptor::SerializedDescriptor;
        paste::paste! {
            static [<$descriptor:snake:upper _STATE>]: ::static_cell::StaticCell<::embassy_usb::class::hid::State> = ::static_cell::StaticCell::new();
            static [<$descriptor:snake:upper _HANDLER>]: ::static_cell::StaticCell<$crate::usb::UsbRequestHandler> = ::static_cell::StaticCell::new();
        }

        let state = paste::paste! { [<$descriptor:snake:upper _STATE>].init(::embassy_usb::class::hid::State::new()) };
        let request_handler = paste::paste! {
            [<$descriptor:snake:upper _HANDLER>].init($crate::usb::UsbRequestHandler {
                keyboard: matches!($protocol, ::embassy_usb::class::hid::HidBootProtocol::Keyboard),
            })
        };

        let hid_config = ::embassy_usb::class::hid::Config {
            report_descriptor: <$descriptor>::desc(),
            request_handler: Some(request_handler),
            poll_ms: 1,
            max_packet_size: $max_packet,
            hid_subclass: $subclass,
            hid_boot_protocol: $protocol,
        };
        (state, hid_config)
    }};
}

macro_rules! add_usb_writer {
    ($usb_builder:expr, $descriptor:ty, $n:expr, $max_packet:expr) => {{
        let (state, hid_config) = $crate::usb::usb_hid_state_and_config!(
            $descriptor,
            $max_packet,
            ::embassy_usb::class::hid::HidSubclass::No,
            ::embassy_usb::class::hid::HidBootProtocol::None
        );
        ::embassy_usb::class::hid::HidWriter::<_, $n>::new($usb_builder, state, hid_config)
    }};
}

macro_rules! add_usb_reader_writer {
    ($usb_builder:expr, $descriptor:ty, $read_n:expr, $write_n:expr, $max_packet:expr) => {
        $crate::usb::add_usb_reader_writer!(
            $usb_builder,
            $descriptor,
            $read_n,
            $write_n,
            $max_packet,
            ::embassy_usb::class::hid::HidSubclass::No,
            ::embassy_usb::class::hid::HidBootProtocol::None
        )
    };
    ($usb_builder:expr, $descriptor:ty, $read_n:expr, $write_n:expr, $max_packet:expr, $subclass:expr, $protocol:expr) => {{
        let (state, hid_config) =
            $crate::usb::usb_hid_state_and_config!($descriptor, $max_packet, $subclass, $protocol);
        ::embassy_usb::class::hid::HidReaderWriter::<_, $read_n, $write_n>::new($usb_builder, state, hid_config)
    }};
}

#[cfg(feature = "usb_log")]
pub(crate) use add_usb_logger;
pub(crate) use add_usb_reader_writer;
pub(crate) use add_usb_writer;
pub(crate) use usb_hid_state_and_config;

pub(crate) struct UsbRequestHandler {
    /// The boot keyboard interface: it answers for the keyboard report and
    /// takes the lock LEDs.
    pub(crate) keyboard: bool,
}

/// The last keyboard report the host was sent, for its GET_REPORT.
static LAST_KEYBOARD_REPORT: BlockingMutex<RawMutex, Cell<[u8; 8]>> = BlockingMutex::new(Cell::new([0; 8]));

impl RequestHandler for UsbRequestHandler {
    /// The keyboard interface answers report 0 with the last keyboard report;
    /// the composite one answers the battery report (id 0x05). Anything else
    /// stalls. Before the level is known, 100% and not charging: a stall fails
    /// Linux's first read and upower then ignores the battery until it
    /// restarts, while a stand-in is replaced by the first real report.
    fn get_report(&mut self, id: ReportId, buf: &mut [u8]) -> Option<usize> {
        if self.keyboard {
            if id != ReportId::In(0) {
                return None;
            }
            let report = LAST_KEYBOARD_REPORT.lock(Cell::get);
            let n = report.len().min(buf.len());
            buf[..n].copy_from_slice(&report[..n]);
            return Some(n);
        }
        let ReportId::In(id) = id else { return None };
        if id != CompositeReportType::Battery as u8 {
            return None;
        }
        let report = crate::hid::battery_now().unwrap_or(crate::hid::BatteryReport {
            level: 100,
            charging: false,
        });
        let (first, rest) = buf.split_first_mut()?;
        *first = id;
        Some(1 + report.serialize(rest).ok()?)
    }

    fn set_report(&mut self, id: ReportId, data: &[u8]) -> OutResponse {
        info!("Set report for {:?}: {:?}", id, data);
        // UEFI and some KVMs set the lock LEDs over the control pipe rather
        // than the interrupt OUT endpoint.
        if self.keyboard
            && id == ReportId::Out(0)
            && let Some(&leds) = data.first()
        {
            crate::hid::apply_led_report(LedIndicator::from_bits(leds), ConnectionType::Usb);
        }
        OutResponse::Accepted
    }

    fn get_protocol(&self) -> HidProtocolMode {
        if self.keyboard && BOOT_MODE.load(Ordering::Relaxed) {
            HidProtocolMode::Boot
        } else {
            HidProtocolMode::Report
        }
    }

    fn set_protocol(&mut self, protocol: HidProtocolMode) -> OutResponse {
        // KeyboardReport is already the 8-byte boot keyboard layout, so the
        // mode only changes what GET_PROTOCOL answers and whether composite
        // reports are sent.
        if self.keyboard {
            BOOT_MODE.store(protocol == HidProtocolMode::Boot, Ordering::Relaxed);
        }
        OutResponse::Accepted
    }
}

pub(crate) struct UsbDeviceHandler {
    /// State to restore on resume. Only a Configured device is ever published as
    /// Suspended (see `suspended()`), so this always holds Configured while the
    /// device is suspended; kept as a snapshot rather than a hardcoded value so
    /// resume stays correct if another pre-suspend state becomes publishable.
    pre_suspend: UsbState,
}

impl UsbDeviceHandler {
    fn new() -> Self {
        UsbDeviceHandler {
            pre_suspend: UsbState::Disabled,
        }
    }
}

impl Handler for UsbDeviceHandler {
    fn enabled(&mut self, enabled: bool) {
        if enabled {
            info!("Device enabled");
            set_usb_state(UsbState::Enabled);
        } else {
            info!("Device disabled");
            set_usb_state(UsbState::Disabled);
        }
    }

    fn reset(&mut self) {
        BOOT_MODE.store(false, Ordering::Relaxed);
        self.pre_suspend = UsbState::Enabled;
        set_usb_state(UsbState::Enabled);
        info!("Bus reset, the Vbus current limit is 100mA");
    }

    fn addressed(&mut self, addr: u8) {
        info!("USB address set to: {}", addr);
    }

    fn configured(&mut self, configured: bool) {
        if configured {
            set_usb_state(UsbState::Configured);
            info!("Device configured, it may now draw up to the configured current from Vbus.")
        } else {
            set_usb_state(UsbState::Enabled);
            info!("Device is no longer configured, the Vbus current limit is 100mA.");
        }
    }

    fn suspended(&mut self, suspended: bool) {
        if suspended {
            // Only publish Suspended when the device was configured before the
            // suspend. `usb_ready()` deliberately treats Suspended as routable
            // (a suspended host must stay reachable for remote wakeup), but that
            // only holds for a device the host has actually enumerated. A
            // never-configured device also sees bus-idle suspends — a charge-only
            // cable or wall charger leaves D+/D- idle, which e.g. on nRF52840
            // raises SUSPEND ~3 ms after enable — and publishing Suspended there
            // would route reports to endpoints that were never configured,
            // silently dropping keystrokes that BLE could have delivered.
            let live = current_usb_state();
            if live == UsbState::Configured {
                self.pre_suspend = live;
                set_usb_state(UsbState::Suspended);
                info!(
                    "Device suspended, the Vbus current limit is 500µA (or 2.5mA for high-power devices with remote wakeup enabled)."
                );
            } else if live != UsbState::Suspended {
                info!("Bus suspended before enumeration (charger or charge-only cable?), USB stays inactive");
            }
        } else {
            // Only restore from Suspended; if we're somehow not in Suspended (out-of-order
            // callbacks), don't overwrite — `configured()`/`enabled()` will resync.
            if current_usb_state() == UsbState::Suspended {
                set_usb_state(self.pre_suspend);
            }
            info!(
                "Device resumed, the Vbus current limit is 500µA (or 2.5mA for high-power devices with remote wakeup enabled)."
            );
        }
    }

    fn remote_wakeup_enabled(&mut self, enabled: bool) {
        info!("Remote wakeup enabled state: {}", enabled);
    }
}

// These tests mutate the process-global CONNECTION_STATUS; cargo-nextest's
// per-test process isolation keeps them from racing each other (plain
// `cargo test` is rejected at startup by `test_support::require_nextest`).
#[cfg(test)]
mod tests {
    extern crate std;

    use std::rc::Rc;

    use embassy_usb::Handler;
    use embassy_usb::class::hid::RequestHandler;
    use rmk_types::connection::UsbState;

    use super::{HidProtocolMode, OutResponse, UsbDeviceHandler, UsbRequestHandler};
    use crate::state::{current_usb_state, set_usb_state};

    /// A BIOS or KVM switch selects the boot protocol before it will use the
    /// keyboard; rejecting the switch strands a host that trusts the boot
    /// subclass the interface advertises.
    #[test]
    fn the_keyboard_switches_protocol_and_reports_it() {
        let mut handler = UsbRequestHandler { keyboard: true };
        assert_eq!(handler.get_protocol(), HidProtocolMode::Report);

        assert_eq!(handler.set_protocol(HidProtocolMode::Boot), OutResponse::Accepted);
        assert_eq!(handler.get_protocol(), HidProtocolMode::Boot);

        assert_eq!(handler.set_protocol(HidProtocolMode::Report), OutResponse::Accepted);
        assert_eq!(handler.get_protocol(), HidProtocolMode::Report);

        // A bus reset returns the device to the default report protocol.
        handler.set_protocol(HidProtocolMode::Boot);
        UsbDeviceHandler::new().reset();
        assert_eq!(handler.get_protocol(), HidProtocolMode::Report);
    }

    /// UEFI and some KVMs send the lock LEDs as a SET_REPORT on the control
    /// pipe: the keyboard interface takes them, the others leave them alone.
    #[test]
    fn a_control_pipe_led_report_sets_the_lock_leds() {
        use embassy_usb::class::hid::ReportId;

        use crate::keyboard::LOCK_LED_STATES;

        set_usb_state(UsbState::Configured);
        let leds = || LOCK_LED_STATES.load(core::sync::atomic::Ordering::Relaxed);
        let mut other = UsbRequestHandler { keyboard: false };
        assert_eq!(other.set_report(ReportId::Out(0), &[0x02]), OutResponse::Accepted);
        assert_eq!(leds(), 0);
        let mut keyboard = UsbRequestHandler { keyboard: true };
        assert_eq!(keyboard.set_report(ReportId::Out(0), &[0x02]), OutResponse::Accepted);
        assert_eq!(leds(), 0x02);
    }

    /// GET_REPORT on the keyboard interface answers with the keys the host
    /// was last sent, as a boot keyboard must.
    #[test]
    fn get_report_answers_the_last_keyboard_report() {
        use embassy_usb::class::hid::ReportId;

        run_writer(&[key(4)], |_, _| {}, |_| core::future::pending(), 10);
        let mut handler = UsbRequestHandler { keyboard: true };
        let mut buf = [0xFF; 8];
        assert_eq!(handler.get_report(ReportId::In(0), &mut buf), Some(8));
        assert_eq!(buf, [0, 0, 4, 0, 0, 0, 0, 0]);
        assert_eq!(handler.get_report(ReportId::In(2), &mut buf), None);
    }

    /// A charge-only cable / wall charger enables the device (VBUS present) but
    /// never enumerates it; the bus-idle suspend that follows must not publish
    /// Suspended, otherwise `usb_ready()` would route reports to endpoints that
    /// were never configured while a BLE host could have received them.
    #[test]
    fn suspend_without_enumeration_stays_enabled() {
        let mut handler = UsbDeviceHandler::new();
        handler.enabled(true);
        assert_eq!(current_usb_state(), UsbState::Enabled);

        handler.suspended(true);
        assert_eq!(current_usb_state(), UsbState::Enabled);

        // Spurious resume (bus activity without enumeration) changes nothing.
        handler.suspended(false);
        assert_eq!(current_usb_state(), UsbState::Enabled);

        // A host showing up later still enumerates normally.
        handler.configured(true);
        assert_eq!(current_usb_state(), UsbState::Configured);
    }

    /// A genuinely suspended (previously enumerated) host keeps the Suspended
    /// state so it stays routable for remote wakeup, and resume restores
    /// Configured.
    #[test]
    fn suspend_after_configured_publishes_suspended_and_resume_restores() {
        let mut handler = UsbDeviceHandler::new();
        handler.enabled(true);
        handler.configured(true);

        handler.suspended(true);
        assert_eq!(current_usb_state(), UsbState::Suspended);

        handler.suspended(false);
        assert_eq!(current_usb_state(), UsbState::Configured);
    }

    /// A stray duplicate `suspended(true)` while already Suspended must not
    /// clobber the pre-suspend snapshot that resume restores.
    #[test]
    fn duplicate_suspend_preserves_pre_suspend_state() {
        let mut handler = UsbDeviceHandler::new();
        handler.enabled(true);
        handler.configured(true);

        handler.suspended(true);
        handler.suspended(true);
        assert_eq!(current_usb_state(), UsbState::Suspended);

        handler.suspended(false);
        assert_eq!(current_usb_state(), UsbState::Configured);
    }

    /// Out-of-order resume while not suspended must not overwrite the live
    /// state.
    #[test]
    fn resume_without_suspend_is_a_no_op() {
        let mut handler = UsbDeviceHandler::new();
        set_usb_state(UsbState::Configured);

        handler.suspended(false);
        assert_eq!(current_usb_state(), UsbState::Configured);
    }

    #[test]
    fn reset_clears_configuration_until_the_host_configures_again() {
        let mut handler = UsbDeviceHandler::new();
        handler.enabled(true);
        handler.configured(true);
        handler.reset();
        assert_eq!(current_usb_state(), UsbState::Enabled);
        handler.suspended(true);
        handler.suspended(false);
        assert_eq!(current_usb_state(), UsbState::Enabled);
        handler.configured(true);
        assert_eq!(current_usb_state(), UsbState::Configured);
    }

    #[test]
    fn reset_during_suspend_does_not_restore_the_old_configuration() {
        let mut handler = UsbDeviceHandler::new();
        handler.enabled(true);
        handler.configured(true);
        handler.suspended(true);
        handler.reset();
        assert_eq!(current_usb_state(), UsbState::Enabled);
        assert_eq!(handler.pre_suspend, UsbState::Enabled);
        handler.suspended(false);
        assert_eq!(current_usb_state(), UsbState::Enabled);
        handler.configured(true);
        handler.suspended(true);
        handler.suspended(false);
        assert_eq!(current_usb_state(), UsbState::Configured);
    }

    /// A stand-in USB part for the report writer: only IN endpoints, each with
    /// a one-packet FIFO the test empties or not.
    mod part {
        extern crate std;

        use core::cell::{Cell, RefCell};
        use core::task::Poll;
        use std::rc::Rc;
        use std::vec::Vec;

        use embassy_time::Instant;
        use embassy_usb::driver::{
            Bus, ControlPipe, Driver, Endpoint, EndpointAddress, EndpointAllocError, EndpointError, EndpointIn,
            EndpointInfo, EndpointOut, EndpointType, Event, Unsupported,
        };

        #[derive(Default)]
        pub(super) struct EpState {
            pub disabled: Cell<bool>,
            /// The host leaves the FIFO full: the next write waits for good.
            pub host_away: Cell<bool>,
            full: Cell<bool>,
            pub sent: RefCell<Vec<(Instant, Vec<u8>)>>,
        }

        /// Clones share the endpoints handed out.
        #[derive(Clone, Default)]
        pub(super) struct Part {
            pub eps: Rc<RefCell<Vec<Rc<EpState>>>>,
        }

        pub(super) struct In {
            info: EndpointInfo,
            state: Rc<EpState>,
        }

        impl Endpoint for In {
            fn info(&self) -> &EndpointInfo {
                &self.info
            }

            async fn wait_enabled(&mut self) {
                core::future::poll_fn(|_| match self.state.disabled.get() {
                    true => Poll::Pending,
                    false => Poll::Ready(()),
                })
                .await
            }
        }

        impl EndpointIn for In {
            async fn write(&mut self, buf: &[u8]) -> Result<(), EndpointError> {
                core::future::poll_fn(|_| {
                    if self.state.disabled.get() {
                        return Poll::Ready(Err(EndpointError::Disabled));
                    }
                    if self.state.full.get() && self.state.host_away.get() {
                        return Poll::Pending;
                    }
                    self.state.full.set(self.state.host_away.get());
                    self.state.sent.borrow_mut().push((Instant::now(), buf.to_vec()));
                    Poll::Ready(Ok(()))
                })
                .await
            }
        }

        pub(super) struct Unused;

        impl Endpoint for Unused {
            fn info(&self) -> &EndpointInfo {
                unimplemented!()
            }
            async fn wait_enabled(&mut self) {}
        }

        impl EndpointOut for Unused {
            async fn read(&mut self, _: &mut [u8]) -> Result<usize, EndpointError> {
                unimplemented!()
            }
        }

        impl ControlPipe for Unused {
            fn max_packet_size(&self) -> usize {
                64
            }
            async fn setup(&mut self) -> [u8; 8] {
                unimplemented!()
            }
            async fn data_out(&mut self, _: &mut [u8], _: bool, _: bool) -> Result<usize, EndpointError> {
                unimplemented!()
            }
            async fn data_in(&mut self, _: &[u8], _: bool, _: bool) -> Result<(), EndpointError> {
                unimplemented!()
            }
            async fn accept(&mut self) {}
            async fn reject(&mut self) {}
            async fn accept_set_address(&mut self, _: u8) {}
        }

        impl Bus for Unused {
            async fn enable(&mut self) {}
            async fn disable(&mut self) {}
            async fn poll(&mut self) -> Event {
                core::future::pending().await
            }
            fn endpoint_set_enabled(&mut self, _: EndpointAddress, _: bool) {}
            fn endpoint_set_stalled(&mut self, _: EndpointAddress, _: bool) {}
            fn endpoint_is_stalled(&mut self, _: EndpointAddress) -> bool {
                false
            }
            async fn remote_wakeup(&mut self) -> Result<(), Unsupported> {
                Err(Unsupported)
            }
        }

        impl<'a> Driver<'a> for Part {
            type EndpointOut = Unused;
            type EndpointIn = In;
            type ControlPipe = Unused;
            type Bus = Unused;

            fn alloc_endpoint_out(
                &mut self,
                _: EndpointType,
                _: Option<EndpointAddress>,
                _: u16,
                _: u8,
            ) -> Result<Unused, EndpointAllocError> {
                Err(EndpointAllocError)
            }

            fn alloc_endpoint_in(
                &mut self,
                ep_type: EndpointType,
                _: Option<EndpointAddress>,
                max_packet_size: u16,
                interval_ms: u8,
            ) -> Result<In, EndpointAllocError> {
                let state = Rc::new(EpState::default());
                self.eps.borrow_mut().push(state.clone());
                let info = EndpointInfo {
                    addr: EndpointAddress::from_parts(self.eps.borrow().len(), embassy_usb::driver::Direction::In),
                    ep_type,
                    max_packet_size,
                    interval_ms,
                };
                Ok(In { info, state })
            }

            fn start(self, _: u16) -> (Unused, Unused) {
                (Unused, Unused)
            }
        }
    }

    /// Run the report writer over a fresh part for `ms` of virtual time, with
    /// `reports` queued and `setup` applied to the keyboard and composite
    /// endpoints first; `during` runs alongside it with the keyboard endpoint.
    fn run_writer<F: core::future::Future<Output = ()>>(
        reports: &[crate::hid::Report],
        setup: impl FnOnce(&part::EpState, &part::EpState),
        during: impl FnOnce(Rc<part::EpState>) -> F,
        ms: u64,
    ) -> (Rc<part::EpState>, Rc<part::EpState>) {
        use embassy_usb::class::hid::{Config, HidBootProtocol, HidSubclass, HidWriter, State};

        use super::{COMPOSITE_WRITE_SIZE, UsbKeyboardWriter};
        use crate::channel::USB_REPORT_CHANNEL;

        let part = part::Part::default();
        let bufs = std::boxed::Box::leak(std::boxed::Box::new([[0u8; 256]; 4]));
        let [config, bos, msos, control] = bufs;
        let mut builder =
            embassy_usb::Builder::new(part.clone(), embassy_usb::Config::new(0, 0), config, bos, msos, control);
        let config = || Config {
            report_descriptor: &[],
            request_handler: None,
            poll_ms: 1,
            max_packet_size: 8,
            hid_subclass: HidSubclass::No,
            hid_boot_protocol: HidBootProtocol::None,
        };
        let states = std::boxed::Box::leak(std::boxed::Box::new([State::new(), State::new(), State::new()]));
        let [keyboard_state, other_state, _steno_state] = states;
        let mut keyboard_writer = HidWriter::<_, 8>::new(&mut builder, keyboard_state, config());
        let mut other_writer = HidWriter::<_, COMPOSITE_WRITE_SIZE>::new(&mut builder, other_state, config());
        #[cfg(feature = "steno")]
        let mut steno_writer = HidWriter::<_, 9>::new(&mut builder, _steno_state, config());
        drop(builder);
        let (keyboard, other) = (part.eps.borrow()[0].clone(), part.eps.borrow()[1].clone());
        setup(&keyboard, &other);

        USB_REPORT_CHANNEL.clear();
        for report in reports {
            USB_REPORT_CHANNEL.try_send(report.clone()).unwrap();
        }
        set_usb_state(UsbState::Configured);
        let mut writer = UsbKeyboardWriter {
            keyboard_writer: &mut keyboard_writer,
            other_writer: &mut other_writer,
            #[cfg(feature = "steno")]
            steno_writer: &mut steno_writer,
        };
        crate::test_support::test_block_on(async {
            embassy_futures::select::select3(
                writer.run_writer(),
                during(keyboard.clone()),
                embassy_time::Timer::after_millis(ms),
            )
            .await;
        });
        (keyboard, other)
    }

    fn key(code: u8) -> crate::hid::Report {
        crate::hid::Report::KeyboardReport(crate::hid::KeyboardReport {
            keycodes: [code, 0, 0, 0, 0, 0],
            ..Default::default()
        })
    }

    fn media(usage_id: u16) -> crate::hid::Report {
        crate::hid::Report::MediaKeyboardReport(usbd_hid::descriptor::MediaKeyboardReport { usage_id })
    }

    /// A host that reads only the boot keyboard (a BIOS, a KVM) never takes a
    /// media report: the second one must not hold up the keys behind it.
    #[test]
    fn an_unread_composite_interface_does_not_stall_the_keys() {
        let (keyboard, _) = run_writer(
            &[media(0xE9), media(0), key(4)],
            |_, other| other.host_away.set(true),
            |_| core::future::pending(),
            1_000,
        );
        assert_eq!(keyboard.sent.borrow().len(), 1, "the key report went out");
    }

    /// After a bus reset the endpoints are disabled until the host configures
    /// the device again: the report waits for that, not a fixed half second,
    /// and asks for no wakeup from a host that isn't asleep.
    #[test]
    fn a_report_after_a_bus_reset_goes_out_once_the_host_configures() {
        let start = embassy_time::Instant::now();
        let (keyboard, _) = run_writer(
            &[key(0), key(4)],
            |keyboard, _| keyboard.disabled.set(true),
            |keyboard| async move {
                embassy_time::Timer::after_millis(20).await;
                keyboard.disabled.set(false);
                core::future::pending().await
            },
            1_000,
        );
        let sent = keyboard.sent.borrow();
        assert_eq!(sent.len(), 2);
        assert!(
            sent[1].0 - start < embassy_time::Duration::from_millis(100),
            "{:?}",
            sent[1].0 - start
        );
        assert!(!super::USB_REMOTE_WAKEUP.signaled());
    }
}
