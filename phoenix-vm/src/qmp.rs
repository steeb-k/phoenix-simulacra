//! A tiny QMP (QEMU Machine Protocol) client — just enough to ask a running
//! guest to power down gracefully.
//!
//! Stopping a VM by killing QEMU is a power-cut from the guest's point of view.
//! With a QMP control socket we can instead send `system_powerdown`, which
//! raises an ACPI power button event the guest OS shuts down cleanly on. QMP is
//! newline-delimited JSON, so this needs no JSON dependency: read the greeting,
//! enter command mode, send one command.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

use anyhow::{Context, Result};

/// Ask the guest at `127.0.0.1:port` to power down via ACPI. Returns once QEMU
/// has accepted the command (the guest then shuts down on its own schedule).
pub fn system_powerdown(port: u16) -> Result<()> {
    let stream = TcpStream::connect(("127.0.0.1", port))
        .with_context(|| format!("connect to QMP on 127.0.0.1:{port}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();

    // 1. Server greeting.
    reader.read_line(&mut line).context("read QMP greeting")?;
    // 2. Leave negotiation, enter command mode.
    writeln!(writer, "{{\"execute\":\"qmp_capabilities\"}}")?;
    line.clear();
    reader.read_line(&mut line).context("read qmp_capabilities reply")?;
    // 3. The ACPI power button.
    writeln!(writer, "{{\"execute\":\"system_powerdown\"}}")?;
    line.clear();
    reader.read_line(&mut line).context("read system_powerdown reply")?;
    Ok(())
}

/// Bind an ephemeral loopback port, then release it, returning the number. The
/// caller passes it to QEMU's `-qmp` and to a later `system_powerdown`. There
/// is a small window where another process could take the port before QEMU
/// binds it; acceptable for a local control socket.
pub fn pick_free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).context("reserve a QMP port")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}
