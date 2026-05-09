//! USB tasks: composite device runner, HID writer, CDC TX, CDC RX loop. The
//! USB descriptor + builder lives in `main` because it pulls in peripheral
//! init; everything that just needs the already-built handles lives here.

use defmt::{info, panic};
use embassy_rp::peripherals::USB;
use embassy_rp::usb::{Driver as UsbDriver, Instance as UsbInstance};
use embassy_time::{Duration, Timer};
use embassy_usb::class::cdc_acm::{Receiver as CdcReceiver, Sender as CdcSender};
use embassy_usb::class::hid::HidWriter;
use embassy_usb::driver::EndpointError;
use embassy_usb::UsbDevice;

use proto::HostToDevice;

use crate::state::{apply_host_message, CONSUMER_EVENTS, DEVICE_TX_EVENTS};

pub type UsbDrv = UsbDriver<'static, USB>;

/// Hand-rolled HID report descriptor: a single application collection on
/// the Consumer page. Each report is `[report_id=1, usage_lsb, usage_msb]`.
/// Send `[1, 0, 0]` to mark "release". 26 bytes total.
#[rustfmt::skip]
pub const CONSUMER_REPORT_DESCRIPTOR: &[u8] = &[
    0x05, 0x0C,        // Usage Page (Consumer Devices)
    0x09, 0x01,        // Usage      (Consumer Control)
    0xA1, 0x01,        // Collection (Application)
    0x85, 0x01,        //   Report ID (1)
    0x15, 0x00,        //   Logical Min (0)
    0x26, 0xFF, 0xFF,  //   Logical Max (0xFFFF)
    0x1A, 0x00, 0x00,  //   Usage Min (0)
    0x2A, 0xFF, 0xFF,  //   Usage Max (0xFFFF)
    0x75, 0x10,        //   Report Size (16 bits)
    0x95, 0x01,        //   Report Count (1 usage per report)
    0x81, 0x00,        //   Input (Data, Array, Absolute)
    0xC0,              // End Collection
];

#[embassy_executor::task]
pub async fn usb_task(mut usb: UsbDevice<'static, UsbDrv>) -> ! {
    usb.run().await
}

/// CDC ACM TX side. Drains `DEVICE_TX_EVENTS`, postcard+COBS encodes each
/// message, writes one packet to the host. Lives in its own task so the main
/// loop can keep the RX side responsive.
#[embassy_executor::task]
pub async fn cdc_tx_task(mut tx: CdcSender<'static, UsbDrv>) {
    let mut buf = [0u8; proto::MAX_FRAME_LEN];
    loop {
        tx.wait_connection().await;
        loop {
            let msg = DEVICE_TX_EVENTS.receive().await;
            let frame = match postcard::to_slice_cobs(&msg, &mut buf) {
                Ok(f) => f,
                Err(_) => continue,
            };
            if tx.write_packet(frame).await.is_err() {
                break; // host gone — wait for reconnect
            }
        }
    }
}

/// Drain the shared consumer-event channel and emit press+release HID reports.
#[embassy_executor::task]
pub async fn hid_writer_task(mut writer: HidWriter<'static, UsbDrv, 8>) {
    loop {
        let key = CONSUMER_EVENTS.receive().await;
        let usage = key as u16;
        let press = [0x01, (usage & 0xFF) as u8, ((usage >> 8) & 0xFF) as u8];
        let release = [0x01, 0x00, 0x00];

        if let Err(e) = writer.write(&press).await {
            defmt::warn!("hid press write failed: {:?}", e);
        }
        Timer::after(Duration::from_millis(5)).await;
        if let Err(e) = writer.write(&release).await {
            defmt::warn!("hid release write failed: {:?}", e);
        }
    }
}

pub async fn cdc_rx_loop<'d, T: UsbInstance + 'd>(
    class: &mut CdcReceiver<'d, UsbDriver<'d, T>>,
) -> Result<(), Disconnected> {
    let mut packet_buf = [0u8; 64];
    let mut frame_buf = [0u8; proto::MAX_FRAME_LEN];
    let mut frame_pos: usize = 0;

    loop {
        let n = class.read_packet(&mut packet_buf).await?;
        for &b in &packet_buf[..n] {
            if b == 0 {
                if frame_pos > 0 {
                    match postcard::from_bytes_cobs::<HostToDevice>(&mut frame_buf[..frame_pos]) {
                        Ok(msg) => {
                            info!("rx host msg, {} bytes", frame_pos);
                            apply_host_message(msg);
                        }
                        Err(_) => info!("dropped malformed frame ({} bytes)", frame_pos),
                    }
                    frame_pos = 0;
                }
            } else if frame_pos < frame_buf.len() {
                frame_buf[frame_pos] = b;
                frame_pos += 1;
            } else {
                // Buffer overflow — sender is sending more than MAX_FRAME_LEN
                // before a delimiter. Discard and resync.
                frame_pos = 0;
            }
        }
    }
}

pub struct Disconnected;

impl From<EndpointError> for Disconnected {
    fn from(val: EndpointError) -> Self {
        match val {
            EndpointError::BufferOverflow => panic!("CDC buffer overflow"),
            EndpointError::Disabled => Disconnected,
        }
    }
}
