use std::{convert::TryInto, fmt, io::BufRead, str::FromStr};

use nix::{poll::PollFlags, unistd::Uid};

use crate::{
    guid::Guid,
    raw::{Connection, Socket},
    utils::wait_on,
    Error, Result,
};

/*
 * Client-side handshake logic
 */

#[derive(Debug, PartialEq)]
enum ClientHandshakeStep {
    Init,
    WaitOauth,
    WaitNegociateFd,
    Done,
}

pub enum IoOperation {
    None,
    Read,
    Write,
}

// See <https://dbus.freedesktop.org/doc/dbus-specification.html#auth-mechanisms>
#[derive(Clone, Copy, Debug)]
enum Mechanism {
    External,
    Cookie,
    Anonymous,
}

/// A representation of an in-progress handshake, client-side
///
/// This struct is an async-compatible representation of the initial handshake that must be performed before
/// a D-Bus connection can be used. To use it, you should call the [`advance_handshake`] method whenever the
/// underlying socket becomes ready (tracking the readiness itself is not managed by this abstraction) until
/// it returns `Ok(())`, at which point you can invoke the [`try_finish`] method to get an [`Authenticated`],
/// which can be given to [`Connection::new_authenticated`].
///
/// If handling the handshake asynchronously is not necessary, the [`blocking_finish`] method is provided
/// which blocks until the handshake is completed or an error occurs.
///
/// [`advance_handshake`]: struct.ClientHandshake.html#method.advance_handshake
/// [`try_finish`]: struct.ClientHandshake.html#method.try_finish
/// [`Authenticated`]: struct.AUthenticated.html
/// [`Connection::new_authenticated`]: ../struct.Connection.html#method.new_authenticated
/// [`blocking_finish`]: struct.ClientHandshake.html#method.blocking_finish
#[derive(Debug)]
pub struct ClientHandshake<S> {
    socket: S,
    recv_buffer: Vec<u8>,
    send_buffer: Vec<u8>,
    step: ClientHandshakeStep,
    server_guid: Option<Guid>,
    cap_unix_fd: bool,
}

/// The result of a finalized handshake
///
/// The result of a finalized [`ClientHandshake`] or [`ServerHandshake`]. It can be passed to
/// [`Connection::new_authenticated`] to initialize a connection.
///
/// [`ClientHandshake`]: struct.ClientHandshake.html
/// [`ServerHandshake`]: struct.ServerHandshake.html
/// [`Connection::new_authenticated`]: ../struct.Connection.html#method.new_authenticated
#[derive(Debug)]
pub struct Authenticated<S> {
    pub(crate) conn: Connection<S>,
    /// The server Guid
    pub(crate) server_guid: Guid,
    /// Whether file descriptor passing has been accepted by both sides
    pub(crate) cap_unix_fd: bool,
}

pub trait Handshake<S> {
    /// Block and automatically drive the handshake for this server
    ///
    /// This method will block until the handshake is finalized, even if the
    /// socket is in non-blocking mode.
    fn blocking_finish(self) -> Result<Authenticated<S>>;

    /// The next I/O operation needed for advancing the handshake.
    ///
    /// If [`Handshake::advance_handshake`] returns a `std::io::ErrorKind::WouldBlock` error, you
    /// can use this to figure out which operation to poll for, before calling `advance_handshake`
    /// again.
    fn next_io_operation(&self) -> IoOperation;

    /// Attempt to advance the handshake
    ///
    /// In non-blocking mode, you need to invoke this method repeatedly
    /// until it returns `Ok(())`. Once it does, the handshake is finished
    /// and you can invoke the [`Handshake::try_finish`] method.
    ///
    /// Note that only the intial handshake is done. If you need to send a
    /// Bus Hello, this remains to be done.
    fn advance_handshake(&mut self) -> Result<()>;

    /// Attempt to finalize this handshake into an initialized client.
    ///
    /// This method should only be called once `advance_handshake()` has
    /// returned `Ok(())`. Otherwise it'll error and return you the object.
    fn try_finish(self) -> std::result::Result<Authenticated<S>, Self>
    where
        Self: Sized;

    /// Access the socket backing this handshake
    ///
    /// Would typically be used to register it for readiness.
    fn socket(&self) -> &S;
}

impl<S: Socket> ClientHandshake<S> {
    /// Start a handsake on this client socket
    pub fn new(socket: S) -> ClientHandshake<S> {
        ClientHandshake {
            socket,
            recv_buffer: Vec::new(),
            send_buffer: Vec::new(),
            step: ClientHandshakeStep::Init,
            server_guid: None,
            cap_unix_fd: false,
        }
    }

    fn flush_buffer(&mut self) -> Result<()> {
        while !self.send_buffer.is_empty() {
            let written = self.socket.sendmsg(&self.send_buffer, &[])?;
            self.send_buffer.drain(..written);
        }
        Ok(())
    }

    fn read_command(&mut self) -> Result<()> {
        self.recv_buffer.clear(); // maybe until \r\n instead?
        while !self.recv_buffer.ends_with(b"\r\n") {
            let mut buf = [0; 40];
            let (read, _) = self.socket.recvmsg(&mut buf)?;
            self.recv_buffer.extend(&buf[..read]);
        }
        Ok(())
    }
}

impl<S: Socket> Handshake<S> for ClientHandshake<S> {
    fn blocking_finish(mut self) -> Result<Authenticated<S>> {
        loop {
            match self.advance_handshake() {
                Ok(()) => return Ok(self.try_finish().unwrap_or_else(|_| unreachable!())),
                Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // we raised a WouldBlock error, this means this is a non-blocking socket
                    // we use poll to wait until the action we need is available
                    let flags = match self.next_io_operation() {
                        IoOperation::Write => PollFlags::POLLOUT,
                        IoOperation::Read => PollFlags::POLLIN,
                        _ => unreachable!(),
                    };
                    wait_on(self.socket.as_raw_fd(), flags)?;
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn next_io_operation(&self) -> IoOperation {
        use ClientHandshakeStep::*;
        if self.send_buffer.is_empty() {
            match self.step {
                WaitOauth | WaitNegociateFd => IoOperation::Read,
                Init | Done => IoOperation::None,
            }
        } else {
            IoOperation::Write
        }
    }

    fn advance_handshake(&mut self) -> Result<()> {
        use ClientHandshakeStep::*;
        loop {
            self.flush_buffer()?;
            match self.step {
                Init => {
                    // send the SASL handshake
                    let uid_str = Uid::current()
                        .to_string()
                        .chars()
                        .map(|c| format!("{:x}", c as u32))
                        .collect::<String>();
                    self.send_buffer = format!("\0AUTH EXTERNAL {}\r\n", uid_str).into();
                    self.step = ClientHandshakeStep::WaitOauth;
                }
                WaitOauth => {
                    self.read_command()?;
                    let mut reply = String::new();
                    (&self.recv_buffer[..]).read_line(&mut reply)?;
                    let mut words = reply.split_whitespace();
                    // We expect a 2 words answer "OK" and the server Guid
                    let guid = match (words.next(), words.next(), words.next()) {
                        (Some("OK"), Some(guid), None) => guid.try_into()?,
                        _ => {
                            return Err(Error::Handshake(
                                "Unexpected server AUTH reply".to_string(),
                            ))
                        }
                    };
                    self.server_guid = Some(guid);
                    self.send_buffer = Vec::from(&b"NEGOTIATE_UNIX_FD\r\n"[..]);
                    self.step = ClientHandshakeStep::WaitNegociateFd;
                }
                WaitNegociateFd => {
                    self.read_command()?;
                    if self.recv_buffer.starts_with(b"AGREE_UNIX_FD") {
                        self.cap_unix_fd = true;
                    } else if self.recv_buffer.starts_with(b"ERROR") {
                        self.cap_unix_fd = false;
                    } else {
                        return Err(Error::Handshake(
                            "Unexpected server UNIX_FD reply".to_string(),
                        ));
                    }
                    self.send_buffer = Vec::from(&b"BEGIN\r\n"[..]);
                    self.step = ClientHandshakeStep::Done;
                }
                Done => return Ok(()),
            }
        }
    }

    fn try_finish(self) -> std::result::Result<Authenticated<S>, Self> {
        if let ClientHandshakeStep::Done = self.step {
            Ok(Authenticated {
                conn: Connection::wrap(self.socket),
                server_guid: self.server_guid.unwrap(),
                cap_unix_fd: self.cap_unix_fd,
            })
        } else {
            Err(self)
        }
    }

    fn socket(&self) -> &S {
        &self.socket
    }
}

/*
 * Server-side handshake logic
 */

#[derive(Debug)]
enum ServerHandshakeStep {
    WaitingForNull,
    WaitingForAuth,
    SendingAuthOK,
    SendingAuthError,
    WaitingForBegin,
    SendingBeginMessage,
    Done,
}

/// A representation of an in-progress handshake, server-side
///
/// This would typically be used to implement a D-Bus broker, or in the context of a P2P connection.
///
/// This struct is an async-compatible representation of the initial handshake that must be performed before
/// a D-Bus connection can be used. To use it, you should call the [`advance_handshake`] method whenever the
/// underlying socket becomes ready (tracking the readiness itself is not managed by this abstraction) until
/// it returns `Ok(())`, at which point you can invoke the [`try_finish`] method to get an [`Authenticated`],
/// which can be given to [`Connection::new_authenticated`].
///
/// If handling the handshake asynchronously is not necessary, the [`blocking_finish`] method is provided
/// which blocks until the handshake is completed or an error occurs.
///
/// [`advance_handshake`]: struct.ServerHandshake.html#method.advance_handshake
/// [`try_finish`]: struct.ServerHandshake.html#method.try_finish
/// [`Authenticated`]: struct.Authenticated.html
/// [`Connection::new_authenticated`]: ../struct.Connection.html#method.new_authenticated
/// [`blocking_finish`]: struct.ServerHandshake.html#method.blocking_finish
#[derive(Debug)]
pub struct ServerHandshake<S> {
    socket: S,
    buffer: Vec<u8>,
    step: ServerHandshakeStep,
    server_guid: Guid,
    cap_unix_fd: bool,
    client_uid: u32,
}

impl<S: Socket> ServerHandshake<S> {
    pub fn new(socket: S, guid: Guid, client_uid: u32) -> ServerHandshake<S> {
        ServerHandshake {
            socket,
            buffer: Vec::new(),
            step: ServerHandshakeStep::WaitingForNull,
            server_guid: guid,
            cap_unix_fd: false,
            client_uid,
        }
    }

    fn flush_buffer(&mut self) -> Result<()> {
        while !self.buffer.is_empty() {
            let written = self.socket.sendmsg(&self.buffer, &[])?;
            self.buffer.drain(..written);
        }
        Ok(())
    }

    fn read_command(&mut self) -> Result<()> {
        while !self.buffer.ends_with(b"\r\n") {
            let mut buf = [0; 40];
            let (read, _) = self.socket.recvmsg(&mut buf)?;
            self.buffer.extend(&buf[..read]);
        }
        Ok(())
    }
}

impl<S: Socket> Handshake<S> for ServerHandshake<S> {
    fn blocking_finish(mut self) -> Result<Authenticated<S>> {
        loop {
            match self.advance_handshake() {
                Ok(()) => return Ok(self.try_finish().unwrap_or_else(|_| unreachable!())),
                Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // we raised a WouldBlock error, this means this is a non-blocking socket
                    // we use poll to wait until the action we need is available
                    let flags = match self.step {
                        ServerHandshakeStep::SendingAuthError
                        | ServerHandshakeStep::SendingAuthOK
                        | ServerHandshakeStep::SendingBeginMessage => PollFlags::POLLOUT,
                        ServerHandshakeStep::WaitingForNull
                        | ServerHandshakeStep::WaitingForBegin
                        | ServerHandshakeStep::WaitingForAuth => PollFlags::POLLIN,
                        ServerHandshakeStep::Done => unreachable!(),
                    };
                    wait_on(self.socket.as_raw_fd(), flags)?;
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn next_io_operation(&self) -> IoOperation {
        match self.step {
            ServerHandshakeStep::Done => IoOperation::None,
            ServerHandshakeStep::WaitingForNull
            | ServerHandshakeStep::WaitingForAuth
            | ServerHandshakeStep::WaitingForBegin => IoOperation::Read,
            ServerHandshakeStep::SendingAuthOK
            | ServerHandshakeStep::SendingAuthError
            | ServerHandshakeStep::SendingBeginMessage => IoOperation::Write,
        }
    }

    fn advance_handshake(&mut self) -> Result<()> {
        loop {
            match self.step {
                ServerHandshakeStep::WaitingForNull => {
                    let mut buffer = [0; 1];
                    let (read, _) = self.socket.recvmsg(&mut buffer)?;
                    // recvmsg cannot return anything else than Ok(1) or Err
                    debug_assert!(read == 1);
                    if buffer[0] != 0 {
                        return Err(Error::Handshake(
                            "First client byte is not NUL!".to_string(),
                        ));
                    }
                    self.step = ServerHandshakeStep::WaitingForAuth;
                }
                ServerHandshakeStep::WaitingForAuth => {
                    self.read_command()?;
                    let mut reply = String::new();
                    (&self.buffer[..]).read_line(&mut reply)?;
                    let mut words = reply.split_whitespace();
                    match (words.next(), words.next(), words.next(), words.next()) {
                        (Some("AUTH"), Some("EXTERNAL"), Some(uid), None) => {
                            let uid = id_from_str(uid)
                                .map_err(|e| Error::Handshake(format!("Invalid UID: {}", e)))?;
                            if uid == self.client_uid {
                                self.buffer = format!("OK {}\r\n", self.server_guid).into();
                                self.step = ServerHandshakeStep::SendingAuthOK;
                            } else {
                                self.buffer = Vec::from(&b"REJECTED EXTERNAL\r\n"[..]);
                                self.step = ServerHandshakeStep::SendingAuthError;
                            }
                        }
                        (Some("AUTH"), _, _, _) | (Some("ERROR"), _, _, _) => {
                            self.buffer = Vec::from(&b"REJECTED EXTERNAL\r\n"[..]);
                            self.step = ServerHandshakeStep::SendingAuthError;
                        }
                        (Some("BEGIN"), None, None, None) => {
                            return Err(Error::Handshake(
                                "Received BEGIN while not authenticated".to_string(),
                            ));
                        }
                        _ => {
                            self.buffer = Vec::from(&b"ERROR Unsupported command\r\n"[..]);
                            self.step = ServerHandshakeStep::SendingAuthError;
                        }
                    }
                }
                ServerHandshakeStep::SendingAuthError => {
                    self.flush_buffer()?;
                    self.step = ServerHandshakeStep::WaitingForAuth;
                }
                ServerHandshakeStep::SendingAuthOK => {
                    self.flush_buffer()?;
                    self.step = ServerHandshakeStep::WaitingForBegin;
                }
                ServerHandshakeStep::WaitingForBegin => {
                    self.read_command()?;
                    let mut reply = String::new();
                    (&self.buffer[..]).read_line(&mut reply)?;
                    let mut words = reply.split_whitespace();
                    match (words.next(), words.next()) {
                        (Some("BEGIN"), None) => {
                            self.step = ServerHandshakeStep::Done;
                        }
                        (Some("CANCEL"), None) => {
                            self.buffer = Vec::from(&b"REJECTED EXTERNAL\r\n"[..]);
                            self.step = ServerHandshakeStep::SendingAuthError;
                        }
                        (Some("ERROR"), _) => {
                            self.buffer = Vec::from(&b"REJECTED EXTERNAL\r\n"[..]);
                            self.step = ServerHandshakeStep::SendingAuthError;
                        }
                        (Some("NEGOTIATE_UNIX_FD"), None) => {
                            self.cap_unix_fd = true;
                            self.buffer = Vec::from(&b"AGREE_UNIX_FD\r\n"[..]);
                            self.step = ServerHandshakeStep::SendingBeginMessage;
                        }
                        _ => {
                            self.buffer = Vec::from(&b"ERROR Unsupported command\r\n"[..]);
                            self.step = ServerHandshakeStep::SendingBeginMessage;
                        }
                    }
                }
                ServerHandshakeStep::SendingBeginMessage => {
                    self.flush_buffer()?;
                    self.step = ServerHandshakeStep::WaitingForBegin;
                }
                ServerHandshakeStep::Done => return Ok(()),
            }
        }
    }

    fn try_finish(self) -> std::result::Result<Authenticated<S>, Self> {
        if let ServerHandshakeStep::Done = self.step {
            Ok(Authenticated {
                conn: Connection::wrap(self.socket),
                server_guid: self.server_guid,
                cap_unix_fd: self.cap_unix_fd,
            })
        } else {
            Err(self)
        }
    }

    fn socket(&self) -> &S {
        &self.socket
    }
}

fn id_from_str(s: &str) -> std::result::Result<u32, Box<dyn std::error::Error>> {
    let mut id = String::new();
    for s in s.as_bytes().chunks(2) {
        let c = char::from(u8::from_str_radix(std::str::from_utf8(s)?, 16)?);
        id.push(c);
    }
    Ok(id.parse::<u32>()?)
}

impl fmt::Display for Mechanism {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mech = match self {
            Mechanism::External => "EXTERNAL",
            Mechanism::Cookie => "DBUS_COOKIE_SHA1",
            Mechanism::Anonymous => "ANONYMOUS",
        };
        write!(f, "{}", mech)
    }
}

impl FromStr for Mechanism {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "EXTERNAL" => Ok(Mechanism::External),
            "DBUS_COOKIE_SHA1" => Ok(Mechanism::Cookie),
            "ANONYMOUS" => Ok(Mechanism::Anonymous),
            _ => Err(Error::Handshake(format!("Unknown mechanism: {}", s))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixStream;

    use super::*;

    use crate::Guid;

    #[test]
    fn handshake() {
        // a pair of non-blocking connection UnixStream
        let (p0, p1) = UnixStream::pair().unwrap();
        p0.set_nonblocking(true).unwrap();
        p1.set_nonblocking(true).unwrap();

        // initialize both handshakes
        let mut client = ClientHandshake::new(p0);
        let mut server = ServerHandshake::new(p1, Guid::generate(), Uid::current().into());

        // proceed to the handshakes
        let mut client_done = false;
        let mut server_done = false;
        while !(client_done && server_done) {
            match client.advance_handshake() {
                Ok(()) => client_done = true,
                Err(Error::Io(e)) => assert!(e.kind() == std::io::ErrorKind::WouldBlock),
                Err(e) => panic!("Unexpected error: {:?}", e),
            }

            match server.advance_handshake() {
                Ok(()) => server_done = true,
                Err(Error::Io(e)) => assert!(e.kind() == std::io::ErrorKind::WouldBlock),
                Err(e) => panic!("Unexpected error: {:?}", e),
            }
        }

        let client = client.try_finish().unwrap();
        let server = server.try_finish().unwrap();

        assert_eq!(client.server_guid, server.server_guid);
        assert_eq!(client.cap_unix_fd, server.cap_unix_fd);
    }
}
