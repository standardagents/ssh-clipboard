//! X11 selection ownership, including ICCCM incremental transfers.
//!
//! Keep this in-process: a closed or slow paste consumer must not terminate the
//! owner, and large selections must never exceed the server's request limit.
use std::collections::HashMap;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt, CreateWindowAux, EventMask, PropMode, Property,
    SELECTION_NOTIFY_EVENT, SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{COPY_DEPTH_FROM_PARENT, CURRENT_TIME, NONE};

use crate::model::Representation;

type Publish = (Vec<Representation>, mpsc::SyncSender<Result<()>>);

pub(super) struct Owner(mpsc::Sender<Publish>);

impl Owner {
    pub(super) fn new() -> Result<Self> {
        let mut server = Server::new()?;
        let (sender, receiver) = mpsc::channel();
        std::thread::Builder::new()
            .name("clipboard-x11-owner".into())
            .spawn(move || server.run(&receiver))?;
        Ok(Self(sender))
    }

    pub(super) fn publish(&self, representations: Vec<Representation>) -> Result<()> {
        let (sender, receiver) = mpsc::sync_channel(1);
        self.0.send((representations, sender))?;
        receiver.recv_timeout(Duration::from_secs(5))??;
        Ok(())
    }
}

struct Transfer {
    target: Atom,
    data: Arc<[u8]>,
    offset: usize,
    touched: Instant,
}

struct Server {
    conn: RustConnection,
    window: Window,
    clipboard: Atom,
    targets: Atom,
    incr: Atom,
    chunk_size: usize,
    data: HashMap<Atom, Arc<[u8]>>,
    transfers: HashMap<(Window, Atom), Transfer>,
}

impl Server {
    fn new() -> Result<Self> {
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
            &CreateWindowAux::new(),
        )?
        .check()?;
        let atom = |name: &[u8]| -> Result<Atom> { Ok(conn.intern_atom(false, name)?.reply()?.atom) };
        let clipboard = atom(b"CLIPBOARD")?;
        let targets = atom(b"TARGETS")?;
        let incr = atom(b"INCR")?;
        // The core limit is in four-byte units; leave room for request headers.
        let chunk_size = (usize::from(conn.setup().maximum_request_length) * 4)
            .saturating_sub(64)
            .clamp(1, 256 * 1024);
        Ok(Self {
            conn,
            window,
            clipboard,
            targets,
            incr,
            chunk_size,
            data: HashMap::new(),
            transfers: HashMap::new(),
        })
    }

    fn run(&mut self, receiver: &mpsc::Receiver<Publish>) {
        loop {
            match receiver.try_recv() {
                Ok((representations, reply)) => {
                    let _ = reply.send(self.publish(representations));
                }
                Err(mpsc::TryRecvError::Disconnected) => break,
                Err(mpsc::TryRecvError::Empty) => {}
            }
            match self.conn.poll_for_event() {
                Ok(Some(event)) => {
                    // A requestor can disappear at any time. Report that request's
                    // failure without sacrificing clipboard ownership or others.
                    if let Err(error) = self.event(&event) {
                        tracing::debug!(%error, "X11 clipboard request failed");
                    }
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(2)),
                Err(error) => {
                    tracing::error!(%error, "X11 clipboard connection lost");
                    break;
                }
            }
            self.transfers
                .retain(|_, transfer| transfer.touched.elapsed() < Duration::from_secs(30));
        }
    }

    fn publish(&mut self, representations: Vec<Representation>) -> Result<()> {
        let mut data = HashMap::new();
        for representation in representations {
            let atom = self
                .conn
                .intern_atom(false, representation.format.as_bytes())?
                .reply()?
                .atom;
            data.entry(atom).or_insert_with(|| Arc::from(representation.data));
        }
        self.conn
            .set_selection_owner(self.window, self.clipboard, CURRENT_TIME)?
            .check()?;
        self.data = data;
        self.conn.flush()?;
        // Active incremental transfers retain their original bytes even if the
        // clipboard changes before a consumer has finished reading them.
        Ok(())
    }

    fn event(&mut self, event: &Event) -> Result<()> {
        match event {
            Event::SelectionRequest(request) => self.request(*request)?,
            Event::PropertyNotify(event) if event.state == Property::DELETE => {
                let key = (event.window, event.atom);
                if let Some(transfer) = self.transfers.get_mut(&key) {
                    let end = (transfer.offset + self.chunk_size).min(transfer.data.len());
                    let result = self
                        .conn
                        .change_property8(
                            PropMode::REPLACE,
                            event.window,
                            event.atom,
                            transfer.target,
                            &transfer.data[transfer.offset..end],
                        )?
                        .check();
                    let finished = transfer.offset == transfer.data.len();
                    transfer.offset = end;
                    transfer.touched = Instant::now();
                    if finished || result.is_err() {
                        self.transfers.remove(&key);
                    }
                    result?;
                    self.conn.flush()?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn request(&mut self, request: SelectionRequestEvent) -> Result<()> {
        let property = if request.property == NONE {
            request.target
        } else {
            request.property
        };
        let result = self.convert(&request, property);
        self.conn
            .send_event(
                false,
                request.requestor,
                EventMask::NO_EVENT,
                SelectionNotifyEvent {
                    response_type: SELECTION_NOTIFY_EVENT,
                    sequence: 0,
                    time: request.time,
                    requestor: request.requestor,
                    selection: request.selection,
                    target: request.target,
                    property: if result.is_ok() { property } else { NONE },
                },
            )?
            .check()?;
        self.conn.flush()?;
        result
    }

    fn convert(&mut self, request: &SelectionRequestEvent, property: Atom) -> Result<()> {
        anyhow::ensure!(request.selection == self.clipboard, "unsupported selection");
        if request.target == self.targets {
            let mut targets = vec![self.targets];
            targets.extend(self.data.keys().copied());
            self.conn
                .change_property32(
                    PropMode::REPLACE,
                    request.requestor,
                    property,
                    AtomEnum::ATOM,
                    &targets,
                )?
                .check()?;
            return Ok(());
        }
        let data = self.data.get(&request.target).context("unsupported target")?;
        if data.len() <= self.chunk_size {
            self.conn
                .change_property8(
                    PropMode::REPLACE,
                    request.requestor,
                    property,
                    request.target,
                    data,
                )?
                .check()?;
        } else {
            anyhow::ensure!(
                self.transfers.len() < 128,
                "too many incremental clipboard consumers"
            );
            self.conn
                .change_window_attributes(
                    request.requestor,
                    &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
                )?
                .check()?;
            self.conn
                .change_property32(
                    PropMode::REPLACE,
                    request.requestor,
                    property,
                    self.incr,
                    &[u32::try_from(data.len())?],
                )?
                .check()?;
            self.transfers.insert(
                (request.requestor, property),
                Transfer {
                    target: request.target,
                    data: Arc::clone(data),
                    offset: 0,
                    touched: Instant::now(),
                },
            );
        }
        Ok(())
    }
}
