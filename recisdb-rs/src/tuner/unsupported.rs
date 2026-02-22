use std::io;
use std::io::ErrorKind;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::io::{AsyncBufRead, AsyncRead};

use crate::channels::Channel;
use crate::tuner::{Tunable, Voltage};

const UNSUPPORTED_MSG: &str = "Tuner device access is not supported on this platform (supported: Linux/Windows)";

pub struct UnTunedTuner;

impl UnTunedTuner {
    pub fn new(_path: String, _buf_sz: usize) -> Result<Self, io::Error> {
        Err(io::Error::new(ErrorKind::Unsupported, UNSUPPORTED_MSG))
    }
}

impl Tunable for UnTunedTuner {
    fn tune(self, _ch: Channel, _lnb: Option<Voltage>) -> Result<Tuner, io::Error> {
        Err(io::Error::new(ErrorKind::Unsupported, UNSUPPORTED_MSG))
    }
}

pub struct Tuner {
    _private: (),
}

impl Tuner {
    pub fn signal_quality(&self) -> f64 {
        0.0
    }
}

impl Tunable for Tuner {
    fn tune(self, _ch: Channel, _lnb: Option<Voltage>) -> Result<Tuner, io::Error> {
        Err(io::Error::new(ErrorKind::Unsupported, UNSUPPORTED_MSG))
    }
}

impl AsyncRead for Tuner {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(0))
    }
}

impl AsyncBufRead for Tuner {
    fn poll_fill_buf(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        Poll::Ready(Ok(&[]))
    }

    fn consume(self: Pin<&mut Self>, _amt: usize) {
    }
}
