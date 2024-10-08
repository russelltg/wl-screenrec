use anyhow::Context;
use log_once::warn_once;
use wayland_client::{
    globals::GlobalList, protocol::wl_output::WlOutput, Dispatch, Proxy, QueueHandle,
};
use wayland_protocols::ext::{
    image_capture_source::v1::client::{
        ext_image_capture_source_v1::ExtImageCaptureSourceV1,
        ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
    },
    image_copy_capture::v1::client::{
        ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1,
        ext_image_copy_capture_manager_v1::{ExtImageCopyCaptureManagerV1, Options},
        ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
    },
};

use crate::{CaptureSource, State};

impl Dispatch<ExtImageCopyCaptureManagerV1, ()> for State<CapExtImageCopy> {
    fn event(
        _state: &mut Self,
        _proxy: &ExtImageCopyCaptureManagerV1,
        _event: <ExtImageCopyCaptureManagerV1 as Proxy>::Event,
        _data: &(),
        _conn: &wayland_client::Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtOutputImageCaptureSourceManagerV1, ()> for State<CapExtImageCopy> {
    fn event(
        _state: &mut Self,
        _proxy: &ExtOutputImageCaptureSourceManagerV1,
        _event: <ExtOutputImageCaptureSourceManagerV1 as Proxy>::Event,
        _data: &(),
        _conn: &wayland_client::Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<ExtImageCaptureSourceV1, ()> for State<CapExtImageCopy> {
    fn event(
        _state: &mut Self,
        _proxy: &ExtImageCaptureSourceV1,
        _event: <ExtImageCaptureSourceV1 as Proxy>::Event,
        _data: &(),
        _conn: &wayland_client::Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for State<CapExtImageCopy> {
    fn event(
        state: &mut Self,
        _proxy: &ExtImageCopyCaptureSessionV1,
        event: <ExtImageCopyCaptureSessionV1 as Proxy>::Event,
        _data: &(),
        _conn: &wayland_client::Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_session_v1::Event::BufferSize { width, height } => {
                state.enc.unwrap_cap().size = Some((width, height))
            }
            ext_image_copy_capture_session_v1::Event::ShmFormat { .. } => {}
            ext_image_copy_capture_session_v1::Event::DmabufDevice { .. } => {}
            ext_image_copy_capture_session_v1::Event::DmabufFormat { format, modifiers } => {
                state.enc.unwrap_cap().dmabuf_format = Some((format, modifiers))
            }
            ext_image_copy_capture_session_v1::Event::Done => {
                let cap = state.enc.unwrap_cap();
                let (width, height, format, frame) = cap
                    .queue_capture_frame(qhandle)
                    .expect("Done without size/format!");
                state.on_copy_src_ready(width, height, format, qhandle, &frame);
            }
            ext_image_copy_capture_session_v1::Event::Stopped => {}
            _ => todo!(),
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for State<CapExtImageCopy> {
    fn event(
        state: &mut Self,
        _proxy: &ExtImageCopyCaptureFrameV1,
        event: <ExtImageCopyCaptureFrameV1 as Proxy>::Event,
        _data: &(),
        _conn: &wayland_client::Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_frame_v1::Event::*;
        match event {
            Transform { .. } => {}
            Damage { .. } => {} // TODO: maybe this is how you implement damage
            PresentationTime {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
            } => state.enc.unwrap().1.time = Some((tv_sec_hi, tv_sec_lo, tv_nsec)),
            Ready => {
                let (hi, lo, n) = state.enc.unwrap().1.time.take().unwrap();
                state.on_copy_complete(qhandle, hi, lo, n);
            }
            Failed { .. } => todo!(),
            _ => {}
        }
    }
}

pub struct CapExtImageCopy {
    output_capture_session: ExtImageCopyCaptureSessionV1,
    size: Option<(u32, u32)>,
    time: Option<(u32, u32, u32)>,
    // dmabuf_device: device,
    dmabuf_format: Option<(u32, Vec<u8>)>,
}

impl CaptureSource for CapExtImageCopy {
    type Frame = ExtImageCopyCaptureFrameV1;

    fn new(
        gm: &GlobalList,
        eq: &QueueHandle<crate::State<Self>>,
        output: WlOutput,
    ) -> anyhow::Result<Self> {
        let capture_man: ExtOutputImageCaptureSourceManagerV1 = gm
            .bind(
                eq,
                1..=ExtImageCopyCaptureManagerV1::interface().version,
                (),
            )
            .context("Your compositor does not support ext-image-capture-source-v1")?;

        let capture_src = capture_man.create_source(&output, eq, ());

        let copy_man: ExtImageCopyCaptureManagerV1 = gm
            .bind(
                eq,
                1..=ExtImageCopyCaptureManagerV1::interface().version,
                (),
            )
            .context("Your compositor does not support ext-image-capture-v1")?;

        let output_capture_session =
            copy_man.create_session(&capture_src, Options::PaintCursors, eq, ());

        Ok(Self {
            output_capture_session,
            size: None,
            dmabuf_format: None,
            time: None,
        })
    }

    fn queue_capture_frame(
        &self,
        eq: &QueueHandle<crate::State<Self>>,
    ) -> Option<(u32, u32, u32, Self::Frame)> {
        if let (Some((w, h)), Some((fmt, _mod))) = (self.size, self.dmabuf_format.as_ref()) {
            let frame = self.output_capture_session.create_frame(eq, ());
            Some((w, h, *fmt, frame))
        } else {
            None
        }
    }

    fn queue_copy_frame(
        &self,
        damage: bool,
        buf: &wayland_client::protocol::wl_buffer::WlBuffer,
        cap: &Self::Frame,
    ) {
        if !damage {
            warn_once!("--no-damage is not implemented in ext-image-capture");
        }
        cap.attach_buffer(buf);
        cap.capture();
    }

    fn on_done_with_frame(&self, f: Self::Frame) {
        f.destroy();
    }
}
