use std::{error, fmt, io};
use zvariant::Error as VariantError;

use crate::{Message, MessageError, MessageType};

/// The error type for `zbus`.
///
/// The various errors that can be reported by this crate.
#[derive(Debug)]
pub enum Error {
    /// Invalid D-Bus address.
    Address(String),
    /// An I/O error.
    IO(io::Error),
    /// Message parsing error.
    Message(MessageError),
    /// A [zvariant](../zvariant/index.html) error.
    Variant(VariantError),
    /// Initial handshake error.
    Handshake(String),
    /// Unexpected or incorrect reply.
    InvalidReply,
    /// A D-Bus method error reply.
    // According to the spec, there can be all kinds of details in D-Bus errors but nobody adds anything more than a
    // string description.
    MethodError(String, Option<String>, Message),
    /// Invalid D-Bus GUID.
    InvalidGUID,
    /// Unsupported function, or support currently lacking.
    Unsupported,
    /// Thread-local connection is not set.
    NoTLSConnection,
    /// Thread-local node is not set.
    NoTLSNode,
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Error::Address(_) => None,
            Error::IO(e) => Some(e),
            Error::Handshake(_) => None,
            Error::Message(e) => Some(e),
            Error::Variant(e) => Some(e),
            Error::InvalidReply => None,
            Error::MethodError(_, _, _) => None,
            Error::InvalidGUID => None,
            Error::Unsupported => None,
            Error::NoTLSConnection => None,
            Error::NoTLSNode => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Address(e) => write!(f, "address error: {}", e),
            Error::IO(e) => write!(f, "I/O error: {}", e),
            Error::Handshake(e) => write!(f, "D-Bus handshake failed: {}", e),
            Error::Message(e) => write!(f, "Message creation error: {}", e),
            Error::Variant(e) => write!(f, "{}", e),
            Error::InvalidReply => write!(f, "Invalid D-Bus method reply"),
            Error::MethodError(name, detail, _reply) => write!(
                f,
                "{}: {}",
                name,
                detail.as_ref().map(|s| s.as_str()).unwrap_or("no details")
            ),
            Error::InvalidGUID => write!(f, "Invalid GUID"),
            Error::Unsupported => write!(f, "Connection support is lacking"),
            Error::NoTLSConnection => write!(f, "No TLS connection"),
            Error::NoTLSNode => write!(f, "No TLS node"),
        }
    }
}

impl From<io::Error> for Error {
    fn from(val: io::Error) -> Self {
        Error::IO(val)
    }
}

impl From<MessageError> for Error {
    fn from(val: MessageError) -> Self {
        Error::Message(val)
    }
}

impl From<VariantError> for Error {
    fn from(val: VariantError) -> Self {
        Error::Variant(val)
    }
}

// For messages that are D-Bus error returns
impl From<Message> for Error {
    fn from(message: Message) -> Error {
        // FIXME: Instead of checking this, we should have Method as trait and specific types for
        // each message type.
        let header = match message.header() {
            Ok(header) => header,
            Err(e) => {
                return Error::Message(e);
            }
        };
        if header.primary().msg_type() != MessageType::Error {
            return Error::InvalidReply;
        }

        if let Ok(Some(name)) = header.error_name() {
            let name = String::from(name);
            match message.body::<&str>() {
                Ok(detail) => Error::MethodError(name, Some(String::from(detail)), message),
                Err(_) => Error::MethodError(name, None, message),
            }
        } else {
            Error::InvalidReply
        }
    }
}

/// Alias for a `Result` with the error type `zbus::Error`.
pub type Result<T> = std::result::Result<T, Error>;