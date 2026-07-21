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
    reader
        .read_line(&mut line)
        .context("read qmp_capabilities reply")?;
    // 3. The ACPI power button.
    writeln!(writer, "{{\"execute\":\"system_powerdown\"}}")?;
    line.clear();
    reader
        .read_line(&mut line)
        .context("read system_powerdown reply")?;
    Ok(())
}

/// Cut the VM's power at `127.0.0.1:port` — QEMU exits at once, the guest
/// gets no say and no chance to flush.
///
/// The equivalent of pulling the plug, and used only where the alternative is
/// worse: the app hosts the WinFsp filesystem serving the VM's disk, so if
/// the app goes away the disk is yanked out from under a still-running guest.
/// An immediate, honest power-off beats a guest that limps on against storage
/// that has silently vanished. Prefer [`system_powerdown`] everywhere else.
pub fn quit(port: u16) -> Result<()> {
    let stream = TcpStream::connect(("127.0.0.1", port))
        .with_context(|| format!("connect to QMP on 127.0.0.1:{port}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();

    reader.read_line(&mut line).context("read QMP greeting")?;
    writeln!(writer, "{{\"execute\":\"qmp_capabilities\"}}")?;
    line.clear();
    reader
        .read_line(&mut line)
        .context("read qmp_capabilities reply")?;
    // No reply is read for `quit`: QEMU may close the socket before it lands.
    writeln!(writer, "{{\"execute\":\"quit\"}}")?;
    Ok(())
}

/// Plug or unplug the guest's virtual network cable at `127.0.0.1:port`.
///
/// The only part of a slirp NIC that can be changed while the VM runs:
/// `restrict` is fixed at creation, `netdev_del` silently no-ops while a
/// device references the backend, and there is no hot-swap for a live NIC's
/// backend. Link state is the one lever, and it is host-side — the guest
/// cannot raise its own cable.
///
/// `id` is the *device* id (`nic0`), which is why the NIC is created with an
/// explicit one rather than through `-nic`, whose generated id is not
/// something we can reliably name later.
pub fn set_link(port: u16, id: &str, up: bool) -> Result<()> {
    let stream = TcpStream::connect(("127.0.0.1", port))
        .with_context(|| format!("connect to QMP on 127.0.0.1:{port}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();

    reader.read_line(&mut line).context("read QMP greeting")?;
    writeln!(writer, "{{\"execute\":\"qmp_capabilities\"}}")?;
    line.clear();
    reader
        .read_line(&mut line)
        .context("read qmp_capabilities reply")?;
    writeln!(
        writer,
        "{{\"execute\":\"set_link\",\"arguments\":{{\"name\":\"{id}\",\"up\":{up}}}}}"
    )?;
    line.clear();
    reader.read_line(&mut line).context("read set_link reply")?;
    if line.contains("\"error\"") {
        anyhow::bail!("set_link failed: {}", line.trim());
    }
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
