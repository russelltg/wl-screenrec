use std::path::PathBuf;

use anyhow::Context;
use drm::{buffer::DrmFourcc, node::DrmNode};
use libc::dev_t;
use log::warn;
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

use crate::{CaptureSource, DmabufFormat, DmabufPotentialFormat, DrmModifier, State};

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
                state.enc.unwrap_cap().in_progress_constraints.buffer_size = Some((width, height));
            }
            ext_image_copy_capture_session_v1::Event::ShmFormat { .. } => {}
            ext_image_copy_capture_session_v1::Event::DmabufDevice { device } => {
                let dev = dev_t::from_ne_bytes(device.try_into().unwrap());
                let node = DrmNode::from_dev_id(dev).unwrap();
                let node = node
                    .node_with_type(drm::node::NodeType::Render)
                    .unwrap()
                    .unwrap();
                let path = node.dev_path().unwrap();
                state.enc.unwrap_cap().in_progress_constraints.dmabuf_device = Some(path);
            }
            ext_image_copy_capture_session_v1::Event::DmabufFormat { format, modifiers } => {
                assert!(modifiers.len() % 8 == 0);
                let modifiers = modifiers
                    .windows(8)
                    .map(|b| DrmModifier(u64::from_ne_bytes(b.try_into().unwrap())))
                    .collect();

                if let Ok(fourcc) = DrmFourcc::try_from(format) {
                    state
                        .enc
                        .unwrap_cap()
                        .in_progress_constraints
                        .dmabuf_formats
                        .push(DmabufPotentialFormat { fourcc, modifiers });
                } else {
                    warn!("Unknown DRM Fourcc: 0x{:08x}", format)
                }
            }
            ext_image_copy_capture_session_v1::Event::Done => {
                let mut constraints = BufferConstraints {
                    dmabuf_formats: Vec::new(),
                    buffer_size: None,
                    dmabuf_device: None,
                };
                // All buffer constraint events will be resent on every change, so reset
                // accumulated state
                std::mem::swap(
                    &mut state.enc.unwrap_cap().in_progress_constraints,
                    &mut constraints,
                );

                let size = constraints
                    .buffer_size
                    .expect("Done received before BufferSize...");
                let fmt = state.negotiate_format(
                    &constraints.dmabuf_formats,
                    size,
                    constraints.dmabuf_device.as_deref(),
                );
                let Some(fmt) = fmt else {
                    // error, it's already reported so we just have to cleanup & exit
                    return;
                };

                let cap = state.enc.unwrap_cap();
                cap.current_config = Some((fmt, size));

                let (width, height, format, frame) = cap
                    .queue_capture_frame(qhandle)
                    .expect("Done without size/format!");
                state.on_copy_src_ready(width, height, format, qhandle, &frame);
            }
            ext_image_copy_capture_session_v1::Event::Stopped => {
                state.on_copy_fail(qhandle); // untested if this actually works
            }
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
            } => state.enc.unwrap().cap.time = Some((tv_sec_hi, tv_sec_lo, tv_nsec)),
            Ready => {
                let (hi, lo, n) = state.enc.unwrap().cap.time.take().unwrap();
                state.on_copy_complete(qhandle, hi, lo, n);
            }
            Failed { .. } => todo!(),
            _ => {}
        }
    }
}

/** Struct to collect buffer constraint information as the events arrive */
struct BufferConstraints {
    dmabuf_formats: Vec<DmabufPotentialFormat>,
    buffer_size: Option<(u32, u32)>,
    dmabuf_device: Option<PathBuf>,
}

pub struct CapExtImageCopy {
    output_capture_session: ExtImageCopyCaptureSessionV1,
    time: Option<(u32, u32, u32)>,
    in_progress_constraints: BufferConstraints,
    current_config: Option<(DmabufFormat, (u32, u32))>,
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
                1..=ExtOutputImageCaptureSourceManagerV1::interface().version,
                (),
            )
            .context(
                "Your compositor does not support expt-output-image-capture-source-manager-v1",
            )?;

        let capture_src = capture_man.create_source(&output, eq, ());

        let copy_man: ExtImageCopyCaptureManagerV1 = gm
            .bind(
                eq,
                1..=ExtImageCopyCaptureManagerV1::interface().version,
                (),
            )
            .context("Your compositor does not support ext-image-copy-capture-manager-v1")?;

        let output_capture_session =
            copy_man.create_session(&capture_src, Options::PaintCursors, eq, ());

        Ok(Self {
            output_capture_session,
            time: None,
            in_progress_constraints: BufferConstraints {
                dmabuf_formats: Vec::new(),
                buffer_size: None,
                dmabuf_device: None,
            },
            current_config: None,
        })
    }

    fn queue_capture_frame(
        &self,
        eq: &QueueHandle<crate::State<Self>>,
    ) -> Option<(u32, u32, DrmFourcc, Self::Frame)> {
        if let Some((fmt, (w, h))) = &self.current_config {
            let frame = self.output_capture_session.create_frame(eq, ());
            Some((*w, *h, fmt.fourcc, frame))
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
