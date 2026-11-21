//! Owners for the proof-only software codec contexts built with MinGW.

use std::{ffi::c_void, ptr::NonNull};

use skyber_core::Track;
use thiserror::Error;

pub const MAX_CONFIG: usize = 65_536;
pub const MAX_PACKET: usize = 1_048_576;

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("native codec error {0}")]
    Native(i32),
    #[error("native codec returned an unexpected pointer or size")]
    InvalidResult,
    #[error("codec data exceeds the A1 size limit")]
    Oversize,
}

#[repr(C)]
#[derive(Default)]
pub struct Decoded {
    pub frames: u32,
    pub samples: u32,
    pub crossings: u32,
    pub fingerprint: u32,
}

unsafe extern "C" {
    fn a1_encoder_open(track: u32, source: u32, out: *mut *mut c_void) -> i32;
    fn a1_decoder_open(track: u32, config: *const u8, size: u32, out: *mut *mut c_void) -> i32;
    fn a1_encoder_config(codec: *const c_void, out: *mut u8, capacity: u32) -> i32;
    fn a1_encode(codec: *mut c_void, out: *mut u8, capacity: u32, key: *mut u32) -> i32;
    fn a1_decode(codec: *mut c_void, data: *const u8, size: u32, out: *mut Decoded) -> i32;
    fn a1_codec_close(codec: *mut c_void);
}

type Result<T> = std::result::Result<T, CodecError>;

fn check(code: i32) -> Result<usize> {
    usize::try_from(code).map_err(|_| CodecError::Native(code))
}

struct Context(NonNull<c_void>);

impl Context {
    fn from_open(code: i32, pointer: *mut c_void) -> Result<Self> {
        check(code)?;
        NonNull::new(pointer)
            .map(Self)
            .ok_or(CodecError::InvalidResult)
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        // SAFETY: The successful open call transferred one native context to this owner.
        unsafe { a1_codec_close(self.0.as_ptr()) }
    }
}

pub struct Encoder {
    context: Context,
    scratch: Vec<u8>,
}

impl Encoder {
    pub fn new(track: Track, source: u32) -> Result<Self> {
        let mut pointer = std::ptr::null_mut();
        // SAFETY: The out pointer is initialized and writable for this call.
        let code = unsafe { a1_encoder_open(track as u32, source, &mut pointer) };
        Ok(Self {
            context: Context::from_open(code, pointer)?,
            scratch: vec![0; MAX_PACKET],
        })
    }

    pub fn config(&self) -> Result<Vec<u8>> {
        let mut config = vec![0; MAX_CONFIG];
        // SAFETY: The live encoder and writable config allocation remain valid for this call.
        let code = unsafe {
            a1_encoder_config(
                self.context.0.as_ptr(),
                config.as_mut_ptr(),
                MAX_CONFIG as u32,
            )
        };
        let len = check(code)?;
        if len > MAX_CONFIG {
            return Err(CodecError::InvalidResult);
        }
        config.truncate(len);
        Ok(config)
    }

    pub fn next_packet(&mut self) -> Result<(&[u8], bool)> {
        let mut key = 0;
        // SAFETY: This thread exclusively owns the encoder and the output buffers.
        let code = unsafe {
            a1_encode(
                self.context.0.as_ptr(),
                self.scratch.as_mut_ptr(),
                MAX_PACKET as u32,
                &mut key,
            )
        };
        let len = check(code)?;
        let data = self.scratch.get(..len).ok_or(CodecError::InvalidResult)?;
        Ok((data, key != 0))
    }
}

pub struct Decoder(Context);

impl Decoder {
    pub fn new(track: Track, config: &[u8]) -> Result<Self> {
        if config.len() > MAX_CONFIG {
            return Err(CodecError::Oversize);
        }
        let mut pointer = std::ptr::null_mut();
        // SAFETY: The configuration bytes are readable during this call and the DLL copies them.
        let code = unsafe {
            a1_decoder_open(
                track as u32,
                config.as_ptr(),
                config.len() as u32,
                &mut pointer,
            )
        };
        Ok(Self(Context::from_open(code, pointer)?))
    }

    pub fn decode(&mut self, data: &[u8]) -> Result<Decoded> {
        if data.len() > MAX_PACKET {
            return Err(CodecError::Oversize);
        }
        let mut output = Decoded::default();
        // SAFETY: The decoder is exclusively owned and both buffers live through the call.
        check(unsafe {
            a1_decode(
                self.0.0.as_ptr(),
                data.as_ptr(),
                data.len() as u32,
                &mut output,
            )
        })?;
        Ok(output)
    }
}
