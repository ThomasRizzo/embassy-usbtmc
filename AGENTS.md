# AGENTS.md - Embassy USBTMC Development Guide

This is an embedded Rust project using Embassy for RP2350. Build targets are thumbv8m.main-none-eabihf.

## Build Commands

```bash
# Build for debug
cargo build

# Build for release (optimized for size)
cargo build --release

# Run on hardware (requires probe-rs with RP2350 connected)
cargo run

# Flash using picotool (after building UF2)
# Build UF2 first: cargo build --release
# Then copy to RP2350 USB drive (boot mode)
elf2uf2 target/thumbv8m.main-none-eabihf/release/embassy-usbtmc firmware.uf2
```

## Code Style Guidelines

### General Conventions
- **Edition**: Rust 2024 (specified in cargo.toml)
- **no_std**: This is a bare-metal embedded project - do not use std
- **Formatting**: Run `cargo fmt` before committing
- **Clippy**: Run `cargo clippy -- -D warnings` to catch issues

### Imports
- Use absolute paths for embassy crates: `embassy_rp::`, `embassy_usb::`, etc.
- Group imports by crate, sorted alphabetically within groups
- Use `use` statements rather than full paths in functions
- Order: std → external crates → local modules

### Naming Conventions
- **Types**: PascalCase (`UsbTmc`, `Command`, `Response`)
- **Constants**: SCREAMING_SNAKE_CASE (`MAX_SCPI_LEN`, `MPS`)
- **Functions**: snake_case (`cmd_receiver`, `resp_sender`)
- **Fields**: snake_case (`len`, `data`)
- **Private fields**: prefix with underscore if truly private: `self.out`, `self.inp`

### Error Handling
- Embedded: prefer `unwrap()` for truly unrecoverable errors (e.g., task spawn failures)
- Use `match` with meaningful error handling for I/O operations
- Use `?` operator in async contexts where errors can propagate
- Use `_ =` to explicitly ignore Result returns when failure is non-critical

### Types & Memory
- Use `heapless::Vec` for fixed-size dynamic collections
- Use fixed-size arrays with const generics for buffers: `[u8; MAX_SCPI_LEN]`
- Prefer `usize` for sizes and indices
- Use `u32::to_le_bytes` / `u32::from_le_bytes` for wire format

### Async & Embassy
- Use `#[embassy_executor::task]` for async tasks
- Tasks must be `'static` due to no heap (or spawn with sufficient stack)
- Use `embassy_sync::channel` for inter-task communication
- Use `Channel<CriticalSectionRawMutex, T, N>` for thread-safe channels

### USBTMC Specific
- Constants: `USBTMC_CLASS = 0xFE`, `USBTMC_SUBCLASS = 0x03`
- Message types: `DEV_DEP_MSG_OUT = 1`, `REQUEST_DEV_DEP_MSG_IN = 2`
- Bulk endpoints: 64-byte max packet size for full-speed
- Multi-packet transfers must handle 4-byte alignment padding

### Documentation
- Document public APIs with doc comments: `/// Description here`
- Explain async behavior in docs
- Note any requirements (e.g., "must be called before spawn")

### Testing
- This project targets bare-metal hardware - no unit tests in traditional sense
- Integration testing via hardware: observe USB enumeration, SCPI responses
- Use `defmt` for logging: `defmt::info!("message {}", value)`
- Use `panic-probe` for panic debugging

### Common Issues
- **Linker errors**: Ensure `.cargo/config.toml` sets correct target and rustflags
- **USB not enumerating**: Check VID/PID, ensure `usb_config.max_packet_size_0 = 64`
- **Stack overflow**: Increase task stack size: `.task(stack: [u8; 4096])`
- **Build errors**: Ensure correct Embassy versions in cargo.toml

## Project Structure

```
embassy-usbtmc/
├── src/
│   └── main.rs          # All code (MVP - single file is intentional)
├── .cargo/
│   └── config.toml      # Build target and runner config
├── cargo.toml           # Dependencies and profiles
└── memory.x             # Linker script for RP2350
```

## Dependencies

Key crates (see cargo.toml for versions):
- `embassy-rp` - RP2350 HAL
- `embassy-usb` - USB device stack
- `embassy-executor` - Async executor
- `embassy-sync` - Synchronization primitives
- `embassy-time` - Time utilities
- `heapless` - No-heap collections
- `defmt` - Efficient logging
