mod drop_privs;
mod handle_message;
mod kex;
mod metacommands;
mod restrict_message;
mod socks;

use arg;
use message::OxyMessage::{self, *};
#[cfg(unix)]
use pty::Pty;
use shlex;
use std::{
    cell::RefCell,
    collections::HashMap,
    fs::File,
    io::Read,
    rc::Rc,
    time::{Duration, Instant},
};
use transportation::{self, mio::net::TcpListener, set_timeout, BufferedTransport, Notifiable, Notifies};
#[cfg(unix)]
use tuntap::TunTap;
use ui::Ui;

#[derive(Clone)]
pub struct Oxy {
    internal: Rc<OxyInternal>,
}

pub(crate) struct TransferOut {
    reference:        u64,
    file:             File,
    current_position: u64,
    cutoff_position:  u64,
}

pub(crate) struct PipeChild {
    child: ::std::process::Child,
    inp:   BufferedTransport,
    out:   BufferedTransport,
    err:   BufferedTransport,
}

#[derive(Default)]
pub(crate) struct OxyInternal {
    naked_transport: RefCell<Option<BufferedTransport>>,
    noise_session: RefCell<Option<::snow::Session>>,
    peer_name: RefCell<Option<String>>,
    piped_children: RefCell<HashMap<u64, PipeChild>>,
    ui: RefCell<Option<Ui>>,
    outgoing_ticker: RefCell<u64>,
    incoming_ticker: RefCell<u64>,
    transfers_out: RefCell<Vec<TransferOut>>,
    port_binds: RefCell<HashMap<u64, PortBind>>,
    local_streams: RefCell<HashMap<u64, PortStream>>,
    remote_streams: RefCell<HashMap<u64, PortStream>>,
    remote_bind_destinations: RefCell<HashMap<u64, String>>,
    socks_binds: RefCell<HashMap<u64, socks::SocksBind>>,
    last_message_seen: RefCell<Option<Instant>>,
    launched: RefCell<bool>,
    is_server: RefCell<bool>,
    response_watchers: RefCell<Vec<Rc<dyn Fn(&OxyMessage, u64) -> bool>>>,
    metacommand_queue: RefCell<Vec<Vec<String>>>,
    is_daemon: RefCell<bool>,
    post_auth_hooks: RefCell<Vec<Rc<dyn Fn() -> ()>>>,
    send_hooks: RefCell<Vec<Rc<dyn Fn() -> bool>>>,
    pipecmd_reference: RefCell<Option<u64>>,
    stdin_bt: RefCell<Option<BufferedTransport>>,
    remote_bind_cleaners: RefCell<HashMap<u64, Rc<dyn Fn() -> ()>>>,
    socks_bind_cleaners: RefCell<HashMap<String, Rc<dyn Fn() -> ()>>>,
    local_bind_cleaners: RefCell<HashMap<String, Rc<dyn Fn() -> ()>>>,
    kr_references: RefCell<HashMap<String, u64>>,
    peer_user: RefCell<Option<String>>,
    message_claim: RefCell<bool>,
    privs_dropped: RefCell<bool>,
    outbound_compression: RefCell<bool>,
    inbound_compression: RefCell<bool>,
    inbound_cleartext_buffer: RefCell<Vec<u8>>,
    active_pty: RefCell<Option<u64>>,
    #[cfg(unix)]
    ptys: RefCell<HashMap<u64, Pty>>,
    #[cfg(unix)]
    tuntaps: RefCell<HashMap<u64, TunTap>>,
}

impl Oxy {
    fn client_only(&self) {
        if !self.is_client() {
            error!("The peer sent a message that is only acceptable for a server to send to a client, but I am not a client");
            ::std::process::exit(1);
        }
    }

    fn server_only(&self) {
        if !self.is_server() {
            error!("The peer sent a message that is only acceptable for a client to send to a server, but I am not a server");
            ::std::process::exit(1);
        }
    }

    fn is_server(&self) -> bool {
        *self.internal.is_server.borrow()
    }

    fn is_client(&self) -> bool {
        !self.is_server()
    }

    pub fn create<T: Into<BufferedTransport>>(transport: T) -> Oxy {
        let bt: BufferedTransport = transport.into();
        let internal = OxyInternal::default();
        *internal.naked_transport.borrow_mut() = Some(bt);
        *internal.last_message_seen.borrow_mut() = Some(Instant::now());
        *internal.is_server.borrow_mut() = ["server", "serve-one", "reexec", "reverse-server"].contains(&::arg::mode().as_str());
        let x = Oxy { internal: Rc::new(internal) };
        let y = x.clone();
        set_timeout(Rc::new(move || y.notify_keepalive()), Duration::from_secs(60));
        let y = x.clone();
        transportation::set_timeout(Rc::new(move || y.launch()), Duration::from_secs(0));
        x
    }

    pub fn peer(&self) -> Option<String> {
        self.internal.peer_name.borrow().clone()
    }

    pub fn set_peer_name(&self, name: &str) {
        trace!("Setting peer name to {:?}", name);
        *self.internal.peer_name.borrow_mut() = Some(name.to_string());
    }

    pub fn set_daemon(&self) {
        *self.internal.is_daemon.borrow_mut() = true;
    }

    pub fn push_post_auth_hook(&self, callback: Rc<dyn Fn() -> ()>) {
        if self.is_encrypted() {
            (callback)();
        } else {
            self.internal.post_auth_hooks.borrow_mut().push(callback);
        }
    }

    pub fn push_send_hook(&self, callback: Rc<dyn Fn() -> bool>) {
        self.internal.send_hooks.borrow_mut().push(callback);
        self.notify_main_transport();
    }

    pub(crate) fn queue_metacommand(&self, command: Vec<String>) {
        self.internal.metacommand_queue.borrow_mut().push(command);
    }

    fn pop_metacommand(&self) {
        if !self.internal.metacommand_queue.borrow().is_empty() {
            self.handle_metacommand(self.internal.metacommand_queue.borrow_mut().remove(0));
        }
    }

    fn create_ui(&self) {
        if self.is_server() {
            return;
        }
        #[cfg(unix)]
        {
            if !self.interactive() {
                return;
            }
        }

        *self.internal.ui.borrow_mut() = Some(Ui::create());
        let proxy = self.clone();
        let proxy = Rc::new(move || proxy.notify_ui());
        self.internal.ui.borrow().as_ref().unwrap().set_notify(proxy);
    }

    fn is_encrypted(&self) -> bool {
        self.internal.noise_session.borrow().is_some() && self.internal.noise_session.borrow().as_ref().unwrap().is_handshake_finished()
    }

    fn launch(&self) {
        trace!("Launching");
        #[cfg(unix)]
        {
            let proxy = self.clone();
            ::exit::push_hook(move || {
                if let Some(x) = proxy.internal.ui.borrow_mut().as_ref() {
                    x.cooked()
                };
                if proxy.internal.ptys.borrow().values().next().is_some() {
                    use nix::sys::signal::{kill, Signal::*};
                    kill(proxy.internal.ptys.borrow().values().next().unwrap().child_pid, SIGTERM).ok();
                }
                if proxy.is_client() && !*proxy.internal.is_daemon.borrow() {
                    eprint!("\r");
                    info!("Goodbye!");
                }
            });
        }
        if *self.internal.launched.borrow() {
            panic!("Attempted to launch an Oxy instance twice.");
        }
        *self.internal.launched.borrow_mut() = true;
        if self.is_client() {
            self.advertise_client_key();
        } else if self.is_server() {
            let proxy = self.clone();
            ::transportation::Notifies::set_notify(
                &*self.internal.naked_transport.borrow_mut().as_mut().unwrap(),
                Rc::new(move || proxy.server_finish_handshake()),
            );
            self.server_finish_handshake();
        } else {
            unreachable!();
        }
    }

    pub(crate) fn run<T: Into<BufferedTransport>>(transport: T) -> ! {
        Oxy::create(transport);
        transportation::run();
    }

    pub(crate) fn send(&self, message: OxyMessage) -> u64 {
        let message_number = self.tick_outgoing();
        debug!("Sending message {}", message_number);
        trace!("Sending message {}: {:?}", message_number, message);
        if !self.is_encrypted() {
            error!("Attempted to send protocol message before key-exchange completed.");
            ::exit::exit(1);
        }
        let serialized: Vec<u8> = serialize(message);
        let compressed: Vec<u8> = if *self.internal.outbound_compression.borrow() {
            compress(&serialized)
        } else {
            serialized
        };
        let framed: Vec<Vec<u8>> = frame(compressed);
        for frame in framed {
            let encrypted_frame: Vec<u8> = self.encrypt(frame);
            self.internal.naked_transport.borrow().as_ref().unwrap().put(&encrypted_frame);
        }
        message_number
    }

    pub(crate) fn encrypt(&self, message: impl AsRef<[u8]>) -> Vec<u8> {
        let mut buf = [0u8; 65535].to_vec();
        let result = self
            .internal
            .noise_session
            .borrow_mut()
            .as_mut()
            .unwrap()
            .write_message(message.as_ref(), &mut buf);
        if result.is_err() {
            error!("Failed to encrypt outbound message {:?}", result);
            ::std::process::exit(1);
        }
        buf.resize(result.unwrap(), 0);
        buf
    }

    pub(crate) fn decrypt(&self, message: impl AsRef<[u8]>) -> Vec<u8> {
        let mut buf = [0u8; 65535].to_vec();
        let result = self
            .internal
            .noise_session
            .borrow_mut()
            .as_mut()
            .unwrap()
            .read_message(message.as_ref(), &mut buf);
        if result.is_err() {
            error!("Failed to decrypt incoming frame: {:?}", result);
            ::std::process::exit(1);
        }
        buf.resize(result.unwrap(), 0);
        buf
    }

    pub(crate) fn recv(&self) -> Option<(OxyMessage, u64)> {
        loop {
            let frame = self.internal.naked_transport.borrow().as_ref().unwrap().take_chunk(272);
            if frame.is_none() {
                return None;
            }
            let frame = frame.unwrap();
            let plaintext = self.decrypt(frame);
            if plaintext.len() != 256 {
                error!("Incorrect frame length.");
                ::std::process::exit(1);
            }
            let relevant_bytes = plaintext[0] as usize;
            self.internal
                .inbound_cleartext_buffer
                .borrow_mut()
                .extend(&plaintext[1..(1 + relevant_bytes)]);
            if relevant_bytes != 255 {
                let mut message = Vec::new();
                ::std::mem::swap(&mut message, &mut *self.internal.inbound_cleartext_buffer.borrow_mut());
                let message = if *self.internal.inbound_compression.borrow() {
                    decompress(&message)
                } else {
                    message
                };
                let message_number = self.tick_incoming();
                let message: Result<OxyMessage, _> = ::serde_cbor::from_slice(&message);
                if message.is_err() {
                    self.send(Reject {
                        reference: message_number,
                        note:      "Invalid message".to_string(),
                    });
                    continue;
                }
                return Some((message.unwrap(), message_number));
            }
        }
    }

    #[cfg(unix)]
    pub fn notify_tuntap(&self, reference_number: u64) {
        let borrow = self.internal.tuntaps.borrow_mut();
        let tuntap = borrow.get(&reference_number).unwrap();
        for packet in tuntap.get_packets() {
            self.send(TunnelData {
                reference: reference_number,
                data:      packet,
            });
        }
    }

    fn notify_bind(&self, token: u64) {
        let stream = self.internal.port_binds.borrow_mut().get_mut(&token).unwrap().listener.accept().unwrap();
        let remote_addr = self.internal.port_binds.borrow_mut().get_mut(&token).unwrap().remote_spec.clone();
        let local_addr = self.internal.port_binds.borrow_mut().get_mut(&token).unwrap().local_spec.clone();
        debug!("Accepting a connection for local bind {}", local_addr);
        let stream_token = match self.is_client() {
            true => self.send(RemoteOpen { addr: remote_addr }),
            false => self.send(BindConnectionAccepted { reference: token }),
        };
        let bt = BufferedTransport::from(stream.0);
        let stream = PortStream {
            stream: bt,
            token:  stream_token,
            oxy:    self.clone(),
            local:  true,
        };
        let stream2 = Rc::new(stream.clone());
        stream.stream.set_notify(stream2);
        self.internal.local_streams.borrow_mut().insert(stream_token, stream);
    }

    fn notify_ui(&self) {
        use ui::UiMessage::*;
        while let Some(msg) = self.internal.ui.borrow().as_ref().unwrap().recv() {
            match msg {
                MetaCommand { parts } => {
                    if parts.is_empty() {
                        continue;
                    }
                    self.handle_metacommand(parts);
                }
                RawInput { input } => {
                    if let Some(&reference) = self.internal.active_pty.borrow().as_ref() {
                        self.send(PtyInput { reference, data: input });
                    }
                }
            }
        }
    }

    #[cfg(unix)]
    fn notify_pty(&self, reference: u64) {
        if let Some(pty) = self.internal.ptys.borrow().get(&reference) {
            let data = pty.underlying.take();
            debug!("PTY Data: {:?}", data);
            if !data.is_empty() {
                self.send(PtyOutput { reference, data });
            }
        } else {
            warn!("Invalid PTY Notification recieved");
        }
    }

    fn tick_outgoing(&self) -> u64 {
        let message_number = *self.internal.outgoing_ticker.borrow_mut();
        let next = message_number.checked_add(1).unwrap();
        *self.internal.outgoing_ticker.borrow_mut() = next;
        message_number
    }

    fn tick_incoming(&self) -> u64 {
        let message_number = *self.internal.incoming_ticker.borrow_mut();
        let next = message_number.checked_add(1).unwrap();
        *self.internal.incoming_ticker.borrow_mut() = next;
        message_number
    }

    pub fn has_write_space(&self) -> bool {
        self.internal.naked_transport.borrow().as_ref().unwrap().has_write_space()
    }

    fn service_transfers(&self) {
        if !self.has_write_space() {
            debug!("Write buffer full! Holding off on servicing transfers.");
            return;
        }
        let mut to_remove = Vec::new();
        for TransferOut {
            reference,
            file,
            current_position,
            cutoff_position,
        } in self.internal.transfers_out.borrow_mut().iter_mut()
        {
            debug!("Servicing transfer {}", reference);
            let mut data = [0; 16384];
            let amt = file.read(&mut data[..]).unwrap();
            if *current_position + amt as u64 > *cutoff_position {
                let to_take = (*cutoff_position - *current_position) as usize;
                self.send(FileData {
                    reference: *reference,
                    data:      data[..to_take].to_vec(),
                });
                self.send(FileData {
                    reference: *reference,
                    data:      Vec::new(),
                });
                self.paint_progress_bar(1000, 0);
                self.log_info("File transfer completed");
                debug!("Transfer finished with cutoff: {}", reference);
                to_remove.push(*reference);
                continue;
            }
            if amt == 0 {
                self.paint_progress_bar(1000, 0);
                self.log_info("File transfer completed.");
                debug!("Transfer finished: {}", reference);
                to_remove.push(*reference);
            }
            self.send(FileData {
                reference: *reference,
                data:      data[..amt].to_vec(),
            });
            *current_position += amt as u64;
            if *cutoff_position != 0 {
                self.paint_progress_bar((*current_position * 1000) / *cutoff_position, amt as u64);
            } else {
                self.paint_progress_bar(1000, 0);
            }
        }
        self.internal.transfers_out.borrow_mut().retain(|x| !to_remove.contains(&x.reference));
        if !to_remove.is_empty() {
            self.pop_metacommand();
        }
    }

    fn paint_progress_bar(&self, progress: u64, bytes: u64) {
        self.internal.ui.borrow().as_ref().map(|x| x.paint_progress_bar(progress, bytes));
    }

    fn log_info(&self, message: &str) {
        if let Some(x) = self.internal.ui.borrow().as_ref() {
            x.log_info(message);
        } else {
            info!("{}", message);
        }
    }

    fn log_debug(&self, message: &str) {
        if let Some(x) = self.internal.ui.borrow().as_ref() {
            x.log_debug(message);
        } else {
            debug!("{}", message);
        }
    }

    fn log_warn(&self, message: &str) {
        if let Some(x) = self.internal.ui.borrow().as_ref() {
            x.log_warn(message);
        } else {
            warn!("{}", message);
        }
    }

    fn notify_pipe_child(&self, token: u64) {
        if let Some(child) = self.internal.piped_children.borrow_mut().get_mut(&token) {
            let out = child.out.take();
            let err = child.err.take();
            self.send(PipeCommandOutput {
                reference: token,
                stdout:    out,
                stderr:    err,
            });
        }
    }

    fn notify_local_stream(&self, token: u64) {
        debug!("Local stream notify for stream {}", token);
        let data = self.internal.local_streams.borrow_mut().get_mut(&token).unwrap().stream.take();
        self.send(RemoteStreamData { reference: token, data });
        if self.internal.local_streams.borrow_mut().get_mut(&token).unwrap().stream.is_closed() {
            self.internal.local_streams.borrow_mut().get_mut(&token).unwrap().stream.close();
            self.send(RemoteStreamClosed { reference: token });
            debug!("Stream closed");
        }
    }

    fn notify_remote_stream(&self, token: u64) {
        debug!("Remote stream notify for stream {}", token);
        let data = self.internal.remote_streams.borrow_mut().get_mut(&token).unwrap().stream.take();
        self.send(LocalStreamData { reference: token, data });
        if self.internal.remote_streams.borrow_mut().get_mut(&token).unwrap().stream.is_closed() {
            self.internal.remote_streams.borrow_mut().get_mut(&token).unwrap().stream.close();
            debug!("Stream closed.");
            self.send(LocalStreamClosed { reference: token });
        }
    }

    fn upgrade_to_encrypted(&self) {
        debug!("Activating encryption.");
        let proxy = self.clone();
        self.internal
            .naked_transport
            .borrow_mut()
            .as_mut()
            .unwrap()
            .set_notify(Rc::new(move || proxy.notify_main_transport()));
        self.notify_main_transport();
        self.do_post_auth();
    }

    #[cfg(unix)]
    fn register_signal_handler(&self) {
        let proxy = self.clone();
        transportation::set_signal_handler(Rc::new(move || proxy.notify_signal()));
    }

    #[cfg(unix)]
    fn notify_signal(&self) {
        match transportation::get_signal_name().as_str() {
            "SIGWINCH" => {
                if self.is_client() {
                    if let Some(ui) = self.internal.ui.borrow().as_ref() {
                        if let Some(&reference) = self.internal.active_pty.borrow().as_ref() {
                            let (w, h) = ui.pty_size();
                            self.send(PtySizeAdvertisement { reference, w, h });
                        }
                    }
                }
            }
            "SIGCHLD" => {
                info!("Received SIGCHLD");
                if !self.internal.ptys.borrow().is_empty() {
                    for (reference, ptypid) in self.internal.ptys.borrow().iter().map(|(&k, v)| (k, v.child_pid)) {
                        let flags = ::nix::sys::wait::WaitPidFlag::WNOHANG;
                        let waitresult = ::nix::sys::wait::waitpid(ptypid, Some(flags));
                        use nix::sys::wait::WaitStatus::Exited;
                        match waitresult {
                            Ok(Exited(_pid, status)) => {
                                self.send(PtyExited { reference, status });
                            }
                            _ => (),
                        };
                    }
                }
                let mut to_remove = Vec::new();
                for (k, pipe_child) in self.internal.piped_children.borrow_mut().iter_mut() {
                    if let Ok(result) = pipe_child.child.try_wait() {
                        debug!("Pipe child exited. {:?}", result);
                        to_remove.push(*k);
                        self.send(PipeCommandExited { reference: *k });
                    }
                }
                for k in to_remove {
                    self.internal.piped_children.borrow_mut().remove(&k);
                }
            }
            _ => (),
        };
    }

    fn interactive(&self) -> bool {
        #[cfg(not(unix))]
        return false;

        #[cfg(unix)]
        return ::termion::is_tty(&::std::io::stdout()) && ::termion::is_tty(&::std::io::stdin());
    }

    fn do_post_auth(&self) {
        if self.is_client() {
            self.pop_metacommand();
            self.activate_compression();
            if !*self.internal.is_daemon.borrow() {
                self.run_batched_metacommands();
                #[cfg(unix)]
                {
                    if self.interactive() {
                        if let Ok(term) = ::std::env::var("TERM") {
                            self.send(EnvironmentAdvertisement {
                                key:   "TERM".to_string(),
                                value: term,
                            });
                        }
                        let mut cmd = vec!["pty".to_string(), "--".to_string()];
                        if let Some(command) = ::arg::matches().values_of("command") {
                            cmd.extend(command.map(|x| x.to_string()));
                        }
                        self.handle_metacommand(cmd);
                    } else {
                        if let Some(cmd) = ::arg::matches().values_of("command") {
                            let stdin_bt = BufferedTransport::from(0);
                            let proxy = self.clone();
                            stdin_bt.set_notify(Rc::new(move || {
                                proxy.notify_pipe_stdin();
                            }));
                            *self.internal.stdin_bt.borrow_mut() = Some(stdin_bt);
                            let mut cmd2 = vec!["pipe".to_string(), "--".to_string()];
                            cmd2.extend(cmd.into_iter().map(|x| x.to_string()));
                            self.handle_metacommand(cmd2);
                        }
                    }
                }
                self.create_ui();
            }
        }
        #[cfg(unix)]
        self.register_signal_handler();
        let mut hooks = Vec::new();
        ::std::mem::swap(&mut hooks, &mut *self.internal.post_auth_hooks.borrow_mut());
        for hook in hooks {
            (hook)();
        }
    }

    fn activate_compression(&self) {
        if ::arg::matches().is_present("compression") {
            // This v is intended to block compression for via forwarders, because they'll
            // just be handling encrypted data, which isn't very compressible
            if !*self.internal.is_daemon.borrow() || ::arg::mode() == "copy" {
                self.send(CompressionRequest { compression_type: 0 });
            }
        }
    }

    fn notify_pipe_stdin(&self) {
        if !self.has_write_space() {
            return;
        }
        if self.internal.stdin_bt.borrow().is_none() {
            return;
        }
        let closed = self.internal.stdin_bt.borrow_mut().as_mut().unwrap().is_closed();
        let available2 = self.internal.stdin_bt.borrow_mut().as_mut().unwrap().available();
        let available = if available2 > 8192 { 8192 } else { available2 };
        if available == 0 && !closed {
            return;
        }
        debug!("Processing stdin data {}", available);
        let input = self.internal.stdin_bt.borrow_mut().as_mut().unwrap().take_chunk(available).unwrap();
        if closed && available == 0 {
            self.internal.stdin_bt.borrow_mut().take();
        }
        let reference = self.internal.pipecmd_reference.borrow_mut().unwrap();
        self.send(PipeCommandInput { reference, input });
    }

    fn run_batched_metacommands(&self) {
        if let Some(user) = arg::matches().value_of("user") {
            self.send(UsernameAdvertisement { username: user.to_string() });
        }
        for command in arg::batched_metacommands() {
            let parts = shlex::split(&command).unwrap();
            self.handle_metacommand(parts);
        }
        let ls = arg::matches().values_of("local port forward");
        if ls.is_some() {
            for l in ls.unwrap() {
                self.handle_metacommand(vec!["L".to_string(), l.to_string()]);
            }
        }
        let rs = arg::matches().values_of("remote port forward");
        if rs.is_some() {
            for r in rs.unwrap() {
                self.handle_metacommand(vec!["R".to_string(), r.to_string()]);
            }
        }
        let ds = arg::matches().values_of("socks");
        if ds.is_some() {
            for d in ds.unwrap() {
                self.handle_metacommand(vec!["D".to_string(), d.to_string()]);
            }
        }
        self.init_tuntap();
    }

    fn init_tuntap(&self) {
        for mode in &["tun", "tap"] {
            if let Some(tuns) = arg::matches().values_of(mode) {
                for tun in tuns {
                    let local;
                    let remote;
                    if tun.contains(':') {
                        let mut iter = tun.split(':');
                        local = iter.next().unwrap().to_string();
                        remote = iter.next().unwrap().to_string();
                    } else {
                        local = tun.to_string();
                        remote = tun.to_string();
                    }
                    self.handle_metacommand(vec![mode.to_string(), local, remote]);
                }
            }
        }
    }

    fn notify_keepalive(&self) {
        trace!("Keepalive!");
        if self.internal.last_message_seen.borrow().as_ref().unwrap().elapsed() > Duration::from_secs(180) {
            trace!("Exiting due to lack of keepalives");
            self.exit(2);
        }
        self.send(Ping {});
        let proxy = self.clone();
        set_timeout(Rc::new(move || proxy.notify_keepalive()), Duration::from_secs(60));
    }

    fn exit(&self, status: i32) -> ! {
        ::exit::exit(status);
    }

    pub fn watch(&self, callback: Rc<dyn Fn(&OxyMessage, u64) -> bool>) {
        if self.internal.response_watchers.borrow().len() >= 10 {
            debug!("Potential response watcher accumulation detected.");
        }
        self.internal.response_watchers.borrow_mut().push(callback);
    }

    pub fn notify_main_transport(&self) {
        debug!("Core notified. Has write space: {}", self.has_write_space());
        if self.internal.naked_transport.borrow().as_ref().unwrap().is_closed() {
            eprint!("\n\r");
            self.log_info("Connection loss detected.");
            ::exit::exit(0);
        }
        loop {
            let message = self.recv();
            if message.is_none() {
                break;
            }
            let (message, message_number) = message.unwrap();
            let result = self.handle_message(message, message_number);
            if result.is_err() {
                self.send(Reject {
                    reference: message_number,
                    note:      result.unwrap_err(),
                });
            }
        }
        self.service_transfers();
        self.notify_pipe_stdin();
        let mut orig_send_hooks = self.internal.send_hooks.borrow().clone();
        let orig_send_hooks_len = orig_send_hooks.len();
        orig_send_hooks.retain(|x| !(x)());
        {
            let mut borrow = self.internal.send_hooks.borrow_mut();
            borrow.splice(..orig_send_hooks_len, orig_send_hooks.into_iter());
        }
    }
}

struct PortBind {
    listener:    TcpListener,
    remote_spec: String,
    local_spec:  String,
}

#[derive(Clone)]
struct PortStream {
    stream: BufferedTransport,
    oxy:    Oxy,
    token:  u64,
    local:  bool,
}

impl Notifiable for PortStream {
    fn notify(&self) {
        if self.local {
            self.oxy.notify_local_stream(self.token);
        } else {
            self.oxy.notify_remote_stream(self.token);
        }
    }
}

fn serialize(message: OxyMessage) -> Vec<u8> {
    let result = ::serde_cbor::ser::to_vec_packed(&message);
    if result.is_err() {
        error!("Failed to serialize outbound message: {:?}, {:?}", result, message);
        ::std::process::exit(1);
    }
    result.unwrap()
}

fn frame(data: Vec<u8>) -> Vec<Vec<u8>> {
    let mut result = Vec::new();
    if data.is_empty() {
        return result;
    }
    for i in data.chunks(255) {
        let mut frame = vec![i.len() as u8];
        frame.extend(i);
        frame.resize(256, 0);
        result.push(frame);
    }
    if result.iter().last().unwrap()[0] == 255 {
        result.push([0u8; 256][..].to_vec());
    }
    result
}

fn compress(data: &[u8]) -> Vec<u8> {
    let compressed_data = Vec::with_capacity(data.len());
    let mut encoder = ::libflate::zlib::Encoder::new(compressed_data).expect("Failed to create outbound compression encoder");
    ::std::io::Write::write_all(&mut encoder, &data[..]).expect("Failed to compress outbound message");
    encoder.finish().into_result().expect("Failed to compress outbound message.")
}

fn decompress(data: &[u8]) -> Vec<u8> {
    let mut decoder = ::libflate::zlib::Decoder::new(data).expect("Failed to create inbound compression decoder");
    let mut buf = Vec::new();
    decoder.read_to_end(&mut buf).expect("Failed to decompress inbound message");
    buf
}
