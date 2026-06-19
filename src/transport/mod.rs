use core::{
    error::Error,
    fmt::{self, Debug, Display},
};

use gdbstub::conn::{Connection, ConnectionExt};
use vex_sdk::vexTasksRun;

#[cfg(target_arch = "arm")]
pub mod mux;

#[derive(Debug)]
pub struct TransportError(pub &'static str);

impl Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Error for TransportError {}

impl From<&'static str> for TransportError {
    fn from(value: &'static str) -> Self {
        Self(value)
    }
}

/// Method of communication between debugger and host.
pub trait Transport: ConnectionExt + Connection<Error = TransportError> + Send + 'static {}
impl<T: ConnectionExt + Connection<Error = TransportError> + Send + 'static> Transport for T {}

/// Debug logging via stdio.
#[derive(Debug)]
pub struct StdioTransport;

impl Default for StdioTransport {
    fn default() -> Self {
        Self
    }
}

impl Clone for StdioTransport {
    fn clone(&self) -> Self {
        Self
    }
}

impl Connection for StdioTransport {
    type Error = TransportError;

    fn write(&mut self, byte: u8) -> Result<(), Self::Error> {
        #[cfg(target_arch = "arm")]
        mux::write_all(mux::ChannelId::Debug, &[byte]);
        Ok(())
    }

    fn write_all(&mut self, buf: &[u8]) -> Result<(), Self::Error> {
        #[cfg(target_arch = "arm")]
        mux::write_all(mux::ChannelId::Debug, buf);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        #[cfg(target_arch = "arm")]
        mux::flush_serial();
        Ok(())
    }

    fn on_session_start(&mut self) -> Result<(), Self::Error> {
        #[cfg(target_arch = "arm")]
        mux::enable_auto_muxing();

        Ok(())
    }
}

impl ConnectionExt for StdioTransport {
    fn peek(&mut self) -> Result<Option<u8>, Self::Error> {
        let char = unsafe { vex_sdk::vexSerialPeekChar(1) };

        if char == -1 {
            return Ok(None);
        }

        Ok(Some(char as u8))
    }

    fn read(&mut self) -> Result<u8, Self::Error> {
        loop {
            let c = unsafe { vex_sdk::vexSerialReadChar(1) };

            if c != -1 {
                return Ok(c as u8);
            }

            unsafe {
                vexTasksRun();
            }
        }
    }
}
