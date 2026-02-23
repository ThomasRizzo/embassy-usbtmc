# Embassy USBTMC

A USB Test & Measurement Class (USBTMC) driver for Raspberry Pi RP2350 using Embassy.

## Overview

This is an embedded Rust implementation of USBTMC using Embassy for the RP2350 microcontroller. It implements the USBTMC protocol to expose a SCPI-compatible command interface over USB.

### Features

- USBTMC class driver (bulk IN/OUT endpoints)
- SCPI command handling via channel-based async communication
- 64-byte max packet size (full-speed USB)
- Respond to `*IDN?` with device identification

## Hardware

- **Target**: Raspberry Pi RP2350
- **USB**: Full-speed (12 Mbps)
- **VID/PID**: 0x2E8A / 0x000A

## Building

```bash
# Build debug
cargo build

# Build release
cargo build --release

# Run on hardware (requires probe-rs)
cargo run

# Create UF2 for flashing via boot mode
elf2uf2 target/thumbv8m.main-none-eabihf/release/embassy-usbtmc firmware.uf2
```

## Usage

1. Flash the firmware to RP2350
2. Connect USB to host
3. Device enumerates as USBTMC device
4. Send SCPI commands via USBTMC (e.g., `*IDN?`)

## SCPI Parsing

For more complex SCPI command parsing, consider using [nom](https://docs.rs/nom/latest/nom/). Nom is a parser combinator library that works well in `no_std` environments.

### Adding Nom

Add to `Cargo.toml`:
```toml
nom = { version = "=7.1.0", default-features = false, features = ["alloc"] }
```

Note: Pin to version 7.x for `no_std` compatibility with `alloc` feature.

### Integration Example

Here's how to integrate nom with the USBTMC driver's channel-based architecture:

```rust
// In src/main.rs - add these imports
use nom::bytes::complete::{tag, take_until};
use nom::sequence::terminated;
use nom::branch::alt;

// Define parsed command types
#[derive(Clone, Debug)]
pub enum ScpiCommand {
    Idn,
    Meas,
    Out(u16),        // e.g., OUTP 5000 (voltage in mV)
    MeasCurrent,     // MEAS:CURR?
    Unknown,
}

// Parse a SCPI command from raw bytes
fn parse_scpi_command(data: &[u8]) -> ScpiCommand {
    // Convert to &str (safe because ASCII)
    let input = core::str::from_utf8(data).unwrap_or("");

    // Trim whitespace and handle optional query suffix
    let trimmed = input.trim();

    // Try each parser - order matters (specific to general)
    if trimmed.starts_with("*IDN") {
        return ScpiCommand::Idn;
    }
    if trimmed.starts_with("MEAS:CURR") {
        return ScpiCommand::MeasCurrent;
    }
    if trimmed.starts_with("MEAS") {
        return ScpiCommand::Meas;
    }

    // Parse "OUTP <value>" command
    if let Ok((_, value)) = terminated(take_until(" "), nom::character::complete::u16)(trimmed) {
        return ScpiCommand::Out(value);
    }

    ScpiCommand::Unknown
}

// Update scpi_task to use parser
#[embassy_executor::task]
async fn scpi_task() {
    let cmd_rx = cmd_receiver();
    let resp_tx = resp_sender();

    loop {
        let cmd = cmd_rx.receive().await;

        // Parse the incoming SCPI command
        let response = match parse_scpi_command(&cmd.data[..cmd.len]) {
            ScpiCommand::Idn => {
                b"RP2350-USBTMC,1,0,FW1.0\n"
            }
            ScpiCommand::Meas => {
                b"+1.234E+00\n"  // Example voltage reading
            }
            ScpiCommand::MeasCurrent => {
                b"+5.678E-03\n"  // Example current reading
            }
            ScpiCommand::Out(val) => {
                // Handle output voltage command
                defmt::info!("Setting output to {} mV", val);
                b"OK\n"
            }
            ScpiCommand::Unknown => {
                b"ERROR: Unknown command\n"
            }
        };

        // Send response
        let mut resp = Response { len: 0, data: [0; MAX_SCPI_LEN] };
        let len = response.len().min(MAX_SCPI_LEN);
        resp.data[0..len].copy_from_slice(&response[0..len]);
        resp.len = len;

        let _ = resp_tx.try_send(resp);
    }
}
```

### Key Points

1. **Command enum**: Define an enum to represent parsed SCPI commands
2. **Parse function**: Convert raw USBTMC bytes to command enum
3. **Match in task**: Use `match` to generate appropriate responses
4. **Error handling**: Return error messages for unknown commands

### Considerations

- Use `default-features = false` to avoid std dependencies
- Enable `alloc` feature for `no_std` with heap allocation
- Consider `heapless` for fixed-size buffers instead of nominal location
- Keep parsers simple to minimize stack usage in embedded context
- Parse functions should be `#[inline]` to reduce call overhead

## Project Structure

```
embassy-usbtmc/
├── src/main.rs       # USBTMC driver and SCPI handler
├── Cargo.toml        # Dependencies
├── .cargo/config.toml
├── memory.x          # Linker script
└── AGENTS.md         # Development guide
```
