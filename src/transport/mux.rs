//! Automatic framing of user serial writes.
//!
//! This module contains alternative implementations of certain VEX SDK functions which capture
//! calls to functions like `vexSerial*` to automatically add framing via a multiplexing protocol.
//!
//! The current implementation is compatible with the following SDKs:
//!
//! - vex_sdk_jumptable
//! - v5rt
//! - v5rts

#![allow(non_snake_case)]

use cobs::CobsEncoder;

use crate::sdk::jumptable;

/// Capture calls to `vexSerial*` functions and automatically add multiplexing packet framing,
/// sending them over the User channel.
pub fn enable_auto_muxing() {
    unsafe {
        crate::sdk::redirect_function(
            vex_sdk::vexSerialWriteBuffer as *mut (),
            user_write_buffer as *const (),
        );

        crate::sdk::redirect_function(
            vex_sdk::vexSerialWriteChar as *mut (),
            user_write_char as *const (),
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ChannelId {
    /// Standard I/O.
    User = b'u',
    /// Debug channel. Messages should use the GDB Protocol.
    Debug = b'd',
}

pub const USER: u32 = 1;
pub const OUT_BUFFER_SIZE: usize = 2048;

/// Write one or more COBS-encoded packets to serial output, each prefixed with the given channel
/// id.
///
/// Returns the number of bytes that were written from `buf`.
pub fn write_all(channel: ChannelId, mut buf: &[u8]) {
    while !buf.is_empty() {
        let mut out_buf = [0u8; OUT_BUFFER_SIZE];

        // The actual out-buffer has 1 extra byte for the packet delimiter.
        let max_len = out_buf.len() - 1;
        let out_buf_without_delimiter = &mut out_buf[..max_len];

        let mut encoder = CobsEncoder::new(out_buf_without_delimiter);

        encoder.push(&[channel as u8]).unwrap();

        // Put as many bytes as possible into this packet.
        while let Some(&byte) = buf.first() {
            let Ok(_) = encoder.push(&[byte]) else {
                break;
            };
            buf = &buf[1..];
        }

        let length = encoder.finalize();

        // When the payload is exactly 254 bytes long, the COBS encoder will start a new 0-sized
        // block at index `length`, which means the pre-zeroed byte from when we first created the
        // buffer might not still be there. Thus we have to explicitly add it back in.
        out_buf[length] = 0;
        write_raw(&out_buf[..=length]); // Include `0` packet delimiter.
    }
}

/// Writes up to [`OUT_BUFFER_SIZE`] bytes to serial.
fn write_raw(buf: &[u8]) {
    let vexSerialWriteBuffer =
        unsafe { jumptable!(0x89c, unsafe extern "C" fn(u32, *const u8, u32) -> i32) };

    if unsafe { vex_sdk::vexSerialWriteFree(USER) as usize } < buf.len() {
        flush_serial();
    }

    unsafe {
        vexSerialWriteBuffer(USER, buf.as_ptr(), buf.len() as u32);
    }
}

pub fn flush_serial() {
    unsafe {
        while (vex_sdk::vexSerialWriteFree(USER) as usize) != OUT_BUFFER_SIZE {
            vex_sdk::vexTasksRun();
        }
    }
}

unsafe extern "C" fn user_write_buffer(channel: u32, data: *const u8, data_len: u32) -> i32 {
    if channel != 1 {
        return -1;
    }
    let user_data = unsafe { core::slice::from_raw_parts(data, data_len as usize) };
    write_all(ChannelId::User, user_data);
    data_len as i32
}

unsafe extern "C" fn user_write_char(channel: u32, c: u8) -> i32 {
    if channel != 1 {
        return -1;
    }
    write_all(ChannelId::User, &[c]);
    1
}
