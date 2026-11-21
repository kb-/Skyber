#[cxx_qt::bridge(namespace = "skyber")]
pub mod ffi {
    extern "RustQt" {
        #[qobject]
        #[qproperty(bool, call_active, cxx_name = "callActive")]
        type SessionStatus = super::SessionStatusRust;
    }
}

#[derive(Default)]
pub struct SessionStatusRust {
    call_active: bool,
}
