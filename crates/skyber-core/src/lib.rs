//! Application-owned media track identities.

#![forbid(unsafe_code)]

/// A separately controllable media stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Track {
    Microphone,
    Camera,
    Screen,
}

impl Track {
    pub const ALL: [Self; 3] = [Self::Microphone, Self::Camera, Self::Screen];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Microphone => "microphone",
            Self::Camera => "camera",
            Self::Screen => "screen",
        }
    }
}
