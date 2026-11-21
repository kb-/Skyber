//! Owners for the native software codec contexts used by the synthetic peers.

use std::{ffi::c_void, ptr::NonNull};

use skyber_core::Track;
use thiserror::Error;

pub const MAX_CONFIG: usize = 65_536;
pub const MAX_PACKET: usize = 1_048_576;
const PCM_SAMPLES: usize = 960 * 2;
const I420_BYTES: usize = 320 * 180 * 3 / 2;

/// One frame in the synthetic proof's software codec profile.
pub enum CaptureFrame<'a> {
    /// Interleaved stereo PCM at 48 kHz, one 20 ms block.
    PcmF32(&'a [f32]),
    /// A 320 by 180 frame with tightly packed Y, U and V planes.
    I420(&'a [u8]),
}

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("native codec error {0}")]
    Native(i32),
    #[error("native codec returned an unexpected pointer or size")]
    InvalidResult,
    #[error("codec data exceeds the A1 size limit")]
    Oversize,
    #[error("capture frame does not match the encoder track or format")]
    InvalidCaptureFrame,
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
    fn skyber_encode_pcm_f32(
        codec: *mut c_void,
        samples: *const f32,
        count: u32,
        out: *mut u8,
        capacity: u32,
        key: *mut u32,
    ) -> i32;
    fn skyber_encode_i420(
        codec: *mut c_void,
        pixels: *const u8,
        size: u32,
        out: *mut u8,
        capacity: u32,
        key: *mut u32,
    ) -> i32;
    fn a1_decode(codec: *mut c_void, data: *const u8, size: u32, out: *mut Decoded) -> i32;
    fn skyber_decode_output(
        codec: *mut c_void,
        data: *const u8,
        size: u32,
        out: *mut Decoded,
        frames: *mut u8,
        capacity: u32,
        written: *mut u32,
    ) -> i32;
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
    track: Track,
    context: Context,
    scratch: Vec<u8>,
}

impl Encoder {
    pub fn new(track: Track, source: u32) -> Result<Self> {
        let mut pointer = std::ptr::null_mut();
        // SAFETY: The out pointer is initialized and writable for this call.
        let code = unsafe { a1_encoder_open(track as u32, source, &mut pointer) };
        Ok(Self {
            track,
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
        self.packet(code, key)
    }

    pub fn encode_capture(&mut self, frame: CaptureFrame<'_>) -> Result<(&[u8], bool)> {
        let mut key = 0;
        let codec = self.context.0.as_ptr();
        let output = self.scratch.as_mut_ptr();
        let capacity = MAX_PACKET as u32;
        let code = match (self.track, frame) {
            (Track::Microphone, CaptureFrame::PcmF32(samples))
                if samples.len() == PCM_SAMPLES
                    && samples.iter().all(|sample| sample.is_finite()) =>
            {
                // SAFETY: The input and output buffers have the declared lengths. The encoder
                // copies the frame synchronously and this thread owns the codec and scratch buffer.
                unsafe {
                    skyber_encode_pcm_f32(
                        codec,
                        samples.as_ptr(),
                        PCM_SAMPLES as u32,
                        output,
                        capacity,
                        &mut key,
                    )
                }
            }
            (Track::Camera | Track::Screen, CaptureFrame::I420(pixels))
                if pixels.len() == I420_BYTES =>
            {
                // SAFETY: The tightly packed frame has the validated byte size. The input is
                // borrowed for this call and the codec and scratch buffer are exclusively owned.
                unsafe {
                    skyber_encode_i420(
                        codec,
                        pixels.as_ptr(),
                        I420_BYTES as u32,
                        output,
                        capacity,
                        &mut key,
                    )
                }
            }
            _ => return Err(CodecError::InvalidCaptureFrame),
        };
        self.packet(code, key)
    }

    fn packet(&self, code: i32, key: u32) -> Result<(&[u8], bool)> {
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
        // SAFETY: The configuration bytes are readable during this call and the native codec copies them.
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

    /// Decode PCM or tightly packed I420 into a reusable, bounded destination.
    pub fn decode_output<'a>(
        &mut self,
        data: &[u8],
        frames: &'a mut [u8],
    ) -> Result<(Decoded, &'a [u8])> {
        if data.len() > MAX_PACKET || frames.len() > MAX_PACKET {
            return Err(CodecError::Oversize);
        }
        let mut output = Decoded::default();
        let mut written = 0;
        // SAFETY: The decoder is exclusively owned. The input and output slices hold
        // the declared lengths and the native shim does not retain their pointers.
        check(unsafe {
            skyber_decode_output(
                self.0.0.as_ptr(),
                data.as_ptr(),
                data.len() as u32,
                &mut output,
                frames.as_mut_ptr(),
                frames.len() as u32,
                &mut written,
            )
        })?;
        let bytes = frames
            .get(..written as usize)
            .ok_or(CodecError::InvalidResult)?;
        Ok((output, bytes))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camera_encodes_caller_owned_i420() -> Result<()> {
        let mut encoder = Encoder::new(Track::Camera, 0)?;
        let mut decoder = Decoder::new(Track::Camera, &encoder.config()?)?;
        assert!(matches!(
            encoder.encode_capture(CaptureFrame::I420(&[0])),
            Err(CodecError::InvalidCaptureFrame)
        ));
        let mut pixels = vec![128; I420_BYTES];
        let mut fingerprints = Vec::new();
        for level in [32, 224] {
            pixels[..320 * 180].fill(level);
            let (packet, _) = encoder.encode_capture(CaptureFrame::I420(&pixels))?;
            let mut decoded_pixels = [0; I420_BYTES];
            let (decoded, bytes) = decoder.decode_output(packet, &mut decoded_pixels)?;
            assert_eq!(bytes.len(), I420_BYTES);
            assert!(bytes[0].abs_diff(level) <= 2);
            assert_eq!(decoded.frames, 1);
            fingerprints.push(decoded.fingerprint);
        }
        assert_ne!(fingerprints[0], fingerprints[1]);
        Ok(())
    }

    #[test]
    fn microphone_encodes_caller_owned_pcm() -> Result<()> {
        let mut encoder = Encoder::new(Track::Microphone, 0)?;
        let mut decoder = Decoder::new(Track::Microphone, &encoder.config()?)?;
        assert!(matches!(
            encoder.encode_capture(CaptureFrame::PcmF32(&[f32::NAN; PCM_SAMPLES])),
            Err(CodecError::InvalidCaptureFrame)
        ));
        assert!(matches!(
            encoder.encode_capture(CaptureFrame::I420(&vec![128; I420_BYTES])),
            Err(CodecError::InvalidCaptureFrame)
        ));
        let mut samples = [0.0; PCM_SAMPLES];
        let mut decoded_samples = 0;
        let mut crossings = 0;
        for block in 0..6 {
            for (index, pair) in samples.chunks_exact_mut(2).enumerate() {
                let phase = (block * 960 + index) as f32 * 440.0 * std::f32::consts::TAU / 48_000.0;
                pair.fill(0.25 * phase.sin());
            }
            let (packet, _) = encoder.encode_capture(CaptureFrame::PcmF32(&samples))?;
            if !packet.is_empty() {
                let mut pcm = [0; 960 * 4];
                let (decoded, bytes) = decoder.decode_output(packet, &mut pcm)?;
                assert_eq!(bytes.len(), decoded.samples as usize * 4);
                assert!(bytes.iter().any(|byte| *byte != 0));
                decoded_samples += decoded.samples;
                crossings += decoded.crossings;
            }
        }
        assert!(decoded_samples > 0);
        let tone = f64::from(crossings) * 48_000.0 / f64::from(decoded_samples);
        assert!((tone - 440.0).abs() < 15.0);
        Ok(())
    }
}
