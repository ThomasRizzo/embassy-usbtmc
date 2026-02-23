#![no_std]
#![no_main]

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicU8, Ordering};

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {}
}

use embassy_executor::{Spawner, main};
use embassy_rp::bind_interrupts;
use embassy_rp::peripherals::USB;
use embassy_rp::usb::{Driver, Endpoint, InterruptHandler};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::{Channel, Receiver, Sender};
use embassy_usb::control::{InResponse, OutResponse, Recipient, RequestType};
use embassy_usb::driver::{EndpointIn, EndpointOut};
use embassy_usb::{Builder, Config, Handler};
use static_cell::StaticCell;

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => InterruptHandler<USB>;
});

static CMD_CHANNEL: Channel<CriticalSectionRawMutex, Command, 4> = Channel::new();
static RESP_CHANNEL: Channel<CriticalSectionRawMutex, Response, 4> = Channel::new();

const MAX_SCPI_LEN: usize = 512;

#[derive(Clone)]
pub struct Command {
    pub len: usize,
    pub data: [u8; MAX_SCPI_LEN],
}

#[derive(Clone)]
pub struct Response {
    pub len: usize,
    pub data: [u8; MAX_SCPI_LEN],
}

pub fn cmd_receiver() -> Receiver<'static, CriticalSectionRawMutex, Command, 4> {
    CMD_CHANNEL.receiver()
}

pub fn resp_sender() -> Sender<'static, CriticalSectionRawMutex, Response, 4> {
    RESP_CHANNEL.sender()
}

const USBTMC_CLASS: u8 = 0xFE;
const USBTMC_SUBCLASS: u8 = 0x03;
const USBTMC_PROTOCOL: u8 = 0x00;

const DEV_DEP_MSG_OUT: u8 = 1;
const REQUEST_DEV_DEP_MSG_IN: u8 = 2;
const DEV_DEP_MSG_IN: u8 = 2;

const GET_CAPABILITIES: u8 = 0x07;
const INITIATE_ABORT_BULK_OUT: u8 = 0x01;
const CHECK_ABORT_BULK_OUT_STATUS: u8 = 0x02;

const MPS: usize = 64;

static ABORT_BTAG: AtomicU8 = AtomicU8::new(0);
static HANDLER: StaticCell<TmcControlHandler> = StaticCell::new();

struct TmcControlHandler;

impl Handler for TmcControlHandler {
    fn control_out(
        &mut self,
        req: embassy_usb::control::Request,
        _buf: &[u8],
    ) -> Option<OutResponse> {
        if req.request_type != RequestType::Class
            || req.recipient != Recipient::Interface
        {
            return None;
        }

        match req.request {
            INITIATE_ABORT_BULK_OUT => {
                let btag = (req.value as u8) & 0x7F;
                ABORT_BTAG.store(btag, Ordering::Relaxed);
                Some(OutResponse::Accepted)
            }
            0x05 => Some(OutResponse::Accepted),
            _ => None,
        }
    }

    fn control_in<'a>(
        &mut self,
        req: embassy_usb::control::Request,
        buf: &'a mut [u8],
    ) -> Option<InResponse<'a>> {
        if req.request_type != RequestType::Class
            || req.recipient != Recipient::Interface
        {
            return None;
        }

        match req.request {
            GET_CAPABILITIES => {
                if buf.len() < 6 {
                    return Some(InResponse::Rejected);
                }
                buf[0..6].copy_from_slice(&[0x00, 0x01, 0x07, 0x00, 0x00, 0x00]);
                Some(InResponse::Accepted(&buf[..6]))
            }
            CHECK_ABORT_BULK_OUT_STATUS => {
                if buf.len() < 8 {
                    return Some(InResponse::Rejected);
                }
                let btag = ABORT_BTAG.load(Ordering::Relaxed);
                let status = if btag != 0 { 0x00 } else { 0x01 };

                buf[0] = status;
                buf[1] = btag;
                buf[2..8].fill(0);

                if status == 0x00 && btag != 0 {
                    ABORT_BTAG.store(0, Ordering::Relaxed);
                }

                Some(InResponse::Accepted(&buf[..8]))
            }
            _ => None,
        }
    }
}

type MyDriver = Driver<'static, USB>;

pub struct UsbTmc {
    out: Endpoint<'static, USB, embassy_rp::usb::Out>,
    inp: Endpoint<'static, USB, embassy_rp::usb::In>,
}

impl UsbTmc {
    pub fn new(builder: &mut Builder<'static, MyDriver>) -> Self {
        builder.handler(HANDLER.init(TmcControlHandler));

        let mut func = builder.function(USBTMC_CLASS, USBTMC_SUBCLASS, USBTMC_PROTOCOL);
        let mut iface = func.interface();
        let mut alt = iface.alt_setting(USBTMC_CLASS, USBTMC_SUBCLASS, USBTMC_PROTOCOL, None);

        let out = alt.endpoint_bulk_out(None, MPS as u16);
        let inp = alt.endpoint_bulk_in(None, MPS as u16);

        Self { out, inp }
    }

    pub fn spawn(self, spawner: Spawner) {
        spawner.spawn(usbtmc_runner(self)).unwrap();
    }
}

#[embassy_executor::task]
async fn usbtmc_runner(mut tmc: UsbTmc) {
    let cmd_tx = CMD_CHANNEL.sender();
    let resp_rx = RESP_CHANNEL.receiver();

    loop {
        let mut buf = [0u8; 64];

        let n = match tmc.out.read(&mut buf).await {
            Ok(n) => n,
            Err(_) => continue,
        };
        if n < 12 {
            continue;
        }

        let msg_id = buf[0];
        let b_tag = buf[1];
        let b_tag_inv = buf[2];

        if b_tag_inv != !b_tag {
            continue;
        }

        let transfer_len = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;

        match msg_id {
            DEV_DEP_MSG_OUT => {
                let total_header_payload = 12 + transfer_len;
                let rem = total_header_payload % 4;
                let pad = if rem == 0 { 0 } else { 4 - rem };
                let bytes_to_consume = transfer_len + pad;

                let mut payload = [0u8; MAX_SCPI_LEN];
                let mut copied = 0usize;

                let first_payload = (n - 12).min(transfer_len);
                if first_payload > 0 {
                    payload[0..first_payload].copy_from_slice(&buf[12..12 + first_payload]);
                    copied = first_payload;
                }

                let mut remaining = bytes_to_consume.saturating_sub(first_payload);
                while remaining > 0 {
                    let read_n = match tmc.out.read(&mut buf).await {
                        Ok(r) => r,
                        Err(_) => break,
                    };
                    let take = read_n.min(remaining);

                    if copied < transfer_len {
                        let to_copy = take.min(transfer_len - copied);
                        payload[copied..copied + to_copy].copy_from_slice(&buf[0..to_copy]);
                        copied += to_copy;
                    }
                    remaining -= take;
                }

                let cmd = Command {
                    len: copied.min(MAX_SCPI_LEN),
                    data: payload,
                };
                let _ = cmd_tx.try_send(cmd);
            }

            REQUEST_DEV_DEP_MSG_IN => {
                let max_resp = transfer_len;
                let resp = resp_rx.receive().await;
                let send_len = resp.len.min(max_resp).min(MAX_SCPI_LEN);

                let mut header = [0u8; 12];
                header[0] = DEV_DEP_MSG_IN;
                header[1] = b_tag;
                header[2] = !b_tag;
                header[4..8].copy_from_slice(&(send_len as u32).to_le_bytes());
                header[8] = 1;

                let total = 12 + send_len;
                let rem = total % 4;
                let pad = if rem == 0 { 0 } else { 4 - rem };

                let mut out_buf = [0u8; 1024];
                out_buf[0..12].copy_from_slice(&header);
                out_buf[12..12 + send_len].copy_from_slice(&resp.data[0..send_len]);

                let _ = tmc.inp.write(&out_buf[0..total + pad]).await;
            }
            _ => {}
        }
    }
}

#[main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    let driver = Driver::new(p.USB, Irqs);

    let mut usb_config = Config::new(0x2E8A, 0x000A);
    usb_config.manufacturer = Some("YourCompany");
    usb_config.product = Some("RP2350 USBTMC");
    usb_config.serial_number = Some("123456");
    usb_config.max_power = 100;
    usb_config.max_packet_size_0 = 64;

    static CONFIG_DESCRIPTOR: StaticCell<[u8; 256]> = StaticCell::new();
    static BOS_DESCRIPTOR: StaticCell<[u8; 256]> = StaticCell::new();
    static CONTROL_BUF: StaticCell<[u8; 64]> = StaticCell::new();

    let mut usb_builder = Builder::new(
        driver,
        usb_config,
        CONFIG_DESCRIPTOR.init([0; 256]),
        BOS_DESCRIPTOR.init([0; 256]),
        &mut [],
        CONTROL_BUF.init([0; 64]),
    );

    let tmc = UsbTmc::new(&mut usb_builder);

    let usb = usb_builder.build();

    spawner.spawn(usb_task(usb)).unwrap();

    tmc.spawn(spawner);

    spawner.spawn(scpi_task()).unwrap();
}

#[embassy_executor::task]
async fn scpi_task() {
    let cmd_rx = cmd_receiver();
    let resp_tx = resp_sender();

    loop {
        let _cmd = cmd_rx.receive().await;

        let resp_str = b"RP2350-USBTMC,1,0,FW1.0\n";
        let mut resp = Response {
            len: 0,
            data: [0; MAX_SCPI_LEN],
        };
        let len = resp_str.len().min(MAX_SCPI_LEN);
        resp.data[0..len].copy_from_slice(&resp_str[0..len]);
        resp.len = len;

        let _ = resp_tx.try_send(resp);
    }
}

#[embassy_executor::task]
async fn usb_task(mut usb: embassy_usb::UsbDevice<'static, MyDriver>) {
    usb.run().await;
}
