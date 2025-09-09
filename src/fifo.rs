use std::ptr::NonNull;

use ffmpeg::{
    ffi::{
        AVAudioFifo, av_audio_fifo_alloc, av_audio_fifo_read, av_audio_fifo_size,
        av_audio_fifo_write,
    },
    format,
    frame::Audio,
};

pub struct AudioFifo(NonNull<AVAudioFifo>);

unsafe impl Send for AudioFifo {}

impl AudioFifo {
    pub fn new(
        sample_format: format::Sample,
        channels: i32,
        nb_samples: u32,
    ) -> Result<Self, ffmpeg::Error> {
        unsafe {
            let fifo = av_audio_fifo_alloc(
                sample_format.into(),
                channels,
                nb_samples.try_into().unwrap(),
            );
            if let Some(fifo) = NonNull::new(fifo) {
                Ok(Self(fifo))
            } else {
                Err(ffmpeg::Error::Unknown)
            }
        }
    }

    pub fn push(&mut self, frame: &Audio) -> usize {
        unsafe {
            av_audio_fifo_write(
                self.0.as_ptr(),
                frame.as_ptr().read().data.as_ptr() as _,
                frame.samples() as i32,
            ) as usize
        }
    }

    pub fn size(&self) -> usize {
        unsafe { av_audio_fifo_size(self.0.as_ptr()) as usize }
    }

    pub fn pop(&mut self, frame: &mut Audio) {
        unsafe {
            av_audio_fifo_read(
                self.0.as_ptr(),
                frame.as_ptr().read().data.as_mut_ptr() as _,
                frame.samples() as i32,
            );
        }
    }
}
