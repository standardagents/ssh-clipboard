//! Bounded X11 selection reads, with incremental transfer support.
use std::time::{Duration, Instant};

use anyhow::{Result, bail, ensure};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt, CreateWindowAux, EventMask, Property, WindowClass};
use x11rb::{COPY_DEPTH_FROM_PARENT, CURRENT_TIME, NONE};

pub(super) fn read(format: &str, max_bytes: u64) -> Result<Vec<u8>> {
    let (conn, screen) = x11rb::connect(None)?;
    let window = conn.generate_id()?;
    conn.create_window(
        COPY_DEPTH_FROM_PARENT,
        window,
        conn.setup().roots[screen].root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )?
    .check()?;
    let target = conn.intern_atom(false, format.as_bytes())?.reply()?.atom;
    let selection = conn.intern_atom(false, b"CLIPBOARD")?.reply()?.atom;
    let property = conn.intern_atom(false, b"SSH_CLIPBOARD_READ")?.reply()?.atom;
    let incr = conn.intern_atom(false, b"INCR")?.reply()?.atom;
    if conn.get_selection_owner(selection)?.reply()?.owner == NONE {
        return Ok(Vec::new());
    }
    let expected_type = if format == "TARGETS" {
        AtomEnum::ATOM.into()
    } else {
        target
    };
    let expected_bits = if format == "TARGETS" { 32 } else { 8 };
    conn.convert_selection(window, selection, target, property, CURRENT_TIME)?
        .check()?;
    conn.flush()?;
    let start = Instant::now();
    let mut progress = start;
    let mut incremental = false;
    let mut data = Vec::new();
    loop {
        ensure!(
            start.elapsed() < Duration::from_secs(30) && progress.elapsed() < Duration::from_secs(5),
            "X11 clipboard read timed out"
        );
        let Some(event) = conn.poll_for_event()? else {
            std::thread::sleep(Duration::from_millis(1));
            continue;
        };
        match event {
            Event::SelectionNotify(event) if event.requestor == window => {
                ensure!(event.property != NONE, "X11 clipboard target unavailable");
            }
            Event::PropertyNotify(event)
                if incremental
                    && event.window == window
                    && event.atom == property
                    && event.state == Property::NEW_VALUE => {}
            _ => continue,
        }
        let remaining = max_bytes.saturating_sub(data.len() as u64);
        let reply = conn
            .get_property(
                true,
                window,
                property,
                AtomEnum::ANY,
                0,
                u32::try_from(remaining.div_ceil(4).max(1)).unwrap_or(u32::MAX),
            )?
            .reply()?;
        if reply.type_ == NONE {
            continue;
        }
        if reply.type_ == incr && !incremental {
            let announced = reply.value32().and_then(|mut values| values.next()).unwrap_or(0);
            ensure!(
                u64::from(announced) <= max_bytes,
                "X11 clipboard exceeds configured limit"
            );
            incremental = true;
            // GetProperty(delete=true) deleted the INCR header; flush its ack.
            conn.flush()?;
            progress = Instant::now();
            continue;
        }
        ensure!(
            reply.type_ == expected_type && reply.format == expected_bits,
            "X11 clipboard data type mismatch"
        );
        if reply.bytes_after != 0 || reply.value.len() as u64 > remaining {
            bail!("X11 clipboard exceeds configured limit");
        }
        if !incremental || reply.value.is_empty() {
            data.extend_from_slice(&reply.value);
            return Ok(data);
        }
        data.extend_from_slice(&reply.value);
        progress = Instant::now();
        conn.flush()?;
    }
}

pub(super) fn formats() -> Result<Vec<String>> {
    // Bound TARGETS separately; format metadata should never consume the user's
    // entire clipboard-size allowance. Use the same progress-aware reader.
    let bytes = read("TARGETS", 1024 * 1024)?;
    let (conn, _) = x11rb::connect(None)?;
    let mut formats = Vec::new();
    for atom in bytes.chunks_exact(4) {
        let atom = u32::from_ne_bytes(atom.try_into()?);
        let name = String::from_utf8(conn.get_atom_name(atom)?.reply()?.name)?;
        if !matches!(
            name.as_str(),
            "TARGETS" | "MULTIPLE" | "TIMESTAMP" | "SAVE_TARGETS"
        ) {
            formats.push(name);
        }
    }
    Ok(formats)
}
