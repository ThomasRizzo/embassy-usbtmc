// Cargo.toml dependencies (add to your project):
// embassy-rp = { version = "0.3", features = ["rp235xa"] }  // or rp235xb depending on exact chip variant
// embassy-usb = "0.5"
// embassy-executor = "0.6"
// embassy-sync = "0.6"
// embassy-time = "0.4"
// heapless = "0.8"
// defmt-rtt = "0.4"
// panic-probe = "0.3"
// (optional: log, etc.)

// src/main.rs  (full working MVP example on RP2350)
#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_rp::{bind_interrupts, peripherals::USB, usb::Driver};
use embassy_rp::usb::InterruptHandler;
use embassy_usb::{Builder, Config, Handler};
use embassy_usb::driver::{EndpointIn, EndpointOut};
use embassy_sync::channel::{Channel, Receiver, Sender};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use heapless::Vec;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => InterruptHandler<USB>;
});

// Static channels for EASY HOOKUP with your separate nom parser task.
// Commands (SCPI from DEV_DEP_MSG_OUT) flow to parser.
// Parser sends response back when ready.
// Driver auto-handles REQUEST_DEV_DEP_MSG_IN by pulling from response channel.
static CMD_CHANNEL: Channel<CriticalSectionRawMutex, Command, 4> = Channel::new();
static RESP_CHANNEL: Channel<CriticalSectionRawMutex, Response, 4> = Channel::new();

const MAX_SCPI_LEN: usize = 512;  // Adjust for your longest expected SCPI command/response

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

pub fn cmd_receiver() -> Receiver<'static, Command, 4> {
    CMD_CHANNEL.receiver()
}

pub fn resp_sender() -> Sender<'static, Response, 4> {
    RESP_CHANNEL.sender()
}

// ==================== USBTMC DRIVER (MVP) ====================

const USBTMC_CLASS: u8 = 0xFE;
const USBTMC_SUBCLASS: u8 = 0x03;
const USBTMC_PROTOCOL: u8 = 0x00;  // 0x01 if you want USB488 subclass

const DEV_DEP_MSG_OUT: u8 = 1;
const REQUEST_DEV_DEP_MSG_IN: u8 = 2;
const DEV_DEP_MSG_IN: u8 = 2;

const MPS: usize = 64;  // Full-speed bulk max packet size

struct TmcControlHandler;

impl Handler for TmcControlHandler {
    fn control_in(&mut self, req: embassy_usb::control::Request, buf: &mut [u8]) -> Option<usize> {
        // Minimal GET_CAPABILITIES (required by most hosts)
        if req.request_type == embassy_usb::types::RequestType::Class &&
           req.recipient == embassy_usb::types::Recipient::Interface &&
           req.request == 0x01 &&  // GET_CAPABILITIES
           buf.len() >= 6 {
            // bcdUSBTMC = 0x0100, basic capabilities
            buf[0..6].copy_from_slice(&[0x00, 0x01, 0x00, 0x00, 0x00, 0x00]);
            Some(6)
        } else {
            None
        }
    }
}

pub struct UsbTmc<'d, D: embassy_usb::driver::Driver<'d>> {
    out: EndpointOut<'d, D>,
    inp: EndpointIn<'d, D>,
}

impl<'d, D: embassy_usb::driver::Driver<'d>> UsbTmc<'d, D> {
    /// Easy hookup: call once in main after creating Builder.
    pub fn new(builder: &mut Builder<'d, D>) -> Self {
        // Register minimal control handler for GET_CAPABILITIES etc.
        builder.handler(&mut TmcControlHandler);

        let mut iface = builder.interface();
        let mut alt = iface.alt_setting(USBTMC_CLASS, USBTMC_SUBCLASS, USBTMC_PROTOCOL, None);

        let out = alt.endpoint_bulk_out(MPS as u16);
        let inp = alt.endpoint_bulk_in(MPS as u16);

        Self { out, inp }
    }

    /// Spawn the background runner task (handles all multi-packet logic).
    pub fn spawn(self, spawner: Spawner) {
        spawner.spawn(usbtmc_runner(self)).unwrap();
    }
}

#[embassy_executor::task]
async fn usbtmc_runner(mut tmc: UsbTmc<'static, Driver<'static, USB>>) {
    let cmd_tx = CMD_CHANNEL.sender();
    let resp_rx = RESP_CHANNEL.receiver();

    loop {
        let mut header = [0u8; 12];
        let n = match tmc.out.read(&mut header).await {
            Ok(n) => n,
            Err(_) => continue,
        };

        if n < 12 {
            continue;
        }

        let msg_id = header[0];
        let b_tag = header[1];
        let b_tag_inv = header[2];
        if b_tag_inv != (!b_tag) {
            continue;  // invalid tag
        }

        let transfer_len = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;

        match msg_id {
            DEV_DEP_MSG_OUT => {
                // === MULTI-PACKET COMMAND HANDLING (SCPI payload) ===
                let mut payload = [0u8; MAX_SCPI_LEN];
                let mut copied = 0usize;

                // Payload from first packet
                let first_payload = n.saturating_sub(12);
                let take = first_payload.min(transfer_len);
                if take > 0 {
                    payload[0..take].copy_from_slice(&header[12..12 + take]);
                    copied = take;
                }

                // Continue reading remaining packets (multi-packet support)
                let mut remaining = transfer_len.saturating_sub(copied);
                while remaining > 0 && copied < MAX_SCPI_LEN {
                    let mut tmp = [0u8; MPS];
                    let read = match tmc.out.read(&mut tmp).await {
                        Ok(r) => r,
                        Err(_) => break,
                    };
                    let take = read.min(remaining);
                    payload[copied..copied + take].copy_from_slice(&tmp[0..take]);
                    copied += take;
                    remaining -= take;
                    if read < MPS {
                        break;  // end of bulk transfer
                    }
                }

                // Drain any padding (USBTMC requires total transfer multiple of 4)
                // pad <= 3 bytes, so at most one extra read
                if (12 + transfer_len) % 4 != 0 {
                    let _ = tmc.out.read(&mut [0; MPS]).await;  // ignore padding/ZLP
                }

                let cmd = Command { len: copied.min(MAX_SCPI_LEN), data: payload };
                let _ = cmd_tx.try_send(cmd);  // non-blocking for robustness
            }

            REQUEST_DEV_DEP_MSG_IN => {
                // === RESPONSE (host requested via REQUEST) ===
                let max_resp = transfer_len;  // host tells us max bytes it accepts
                let resp = resp_rx.receive().await;  // wait for your nom parser to produce one
                let send_len = resp.len.min(max_resp).min(MAX_SCPI_LEN);

                let mut header = [0u8; 12];
                header[0] = DEV_DEP_MSG_IN;
                header[1] = b_tag;
                header[2] = !b_tag;
                header[4..8].copy_from_slice(&(send_len as u32).to_le_bytes());
                header[8] = 1;  // EOM = 1

                // Build full transfer (header + data + pad to 4-byte boundary)
                let mut buf = [0u8; 1024];  // safe for MVP (SCPI responses rarely > 512)
                buf[0..12].copy_from_slice(&header);
                buf[12..12 + send_len].copy_from_slice(&resp.data[0..send_len]);

                let total = 12 + send_len;
                let pad = ((4 - (total % 4)) % 4) as usize;
                for i in 0..pad {
                    buf[total + i] = 0;
                }

                let _ = tmc.inp.write(&buf[0..total + pad]).await;
            }
            _ => {}  // ignore other TMC messages for MVP (add ABORT etc. later)
        }
    }
}

// ==================== USAGE EXAMPLE (your main) ====================
#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    let driver = Driver::new(p.USB, Irqs);

    let mut usb_config = Config::new(0x2E8A, 0x000A);  // Raspberry Pi test VID/PID (change if needed)
    usb_config.manufacturer = Some("YourCompany");
    usb_config.product = Some("RP2350 USBTMC");
    usb_config.serial_number = Some("123456");
    usb_config.max_power = 100;
    usb_config.max_packet_size_0 = 64;

    let mut usb_builder = Builder::new(
        driver,
        usb_config,
        &mut [0; 256],      // device descriptor
        &mut [0; 256],      // config descriptor
        &mut [0; 64],       // bos (none needed)
        &mut [0; 64],       // control buffer
    );

    let tmc = UsbTmc::new(&mut usb_builder);

    let usb = usb_builder.build();

    // Run USB stack in background
    spawner.spawn(usb_task(usb)).unwrap();

    // Run USBTMC driver (handles multi-packet, channels, etc.)
    tmc.spawn(spawner);

    // ==================== YOUR NOM PARSER TASK ====================
    // This is completely separate - easy hookup!
    let mut cmd_rx = cmd_receiver();
    let mut resp_tx = resp_sender();

    loop {
        let cmd = cmd_rx.receive().await;

        // === YOUR NOM PARSER GOES HERE ===
        // let scpi = &cmd.data[0..cmd.len];
        // let parsed = your_nom_parser(scpi);  // e.g. parse SCPI command
        // ... execute command ...

        // Prepare response (example)
        let resp_str = b"RP2350-USBTMC,1,0,FW1.0\n";  // or from your instrument logic
        let mut resp = Response { len: 0, data: [0; MAX_SCPI_LEN] };
        let len = resp_str.len().min(MAX_SCPI_LEN);
        resp.data[0..len].copy_from_slice(&resp_str[0..len]);
        resp.len = len;

        let _ = resp_tx.try_send(resp);  // send back; driver will deliver on next REQUEST
    }
}

#[embassy_executor::task]
async fn usb_task(mut usb: embassy_usb::UsbDevice<'static, Driver<'static, USB>>) {
    usb.run().await;
}