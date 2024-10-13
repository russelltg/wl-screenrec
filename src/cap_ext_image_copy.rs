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
                if let ExtImageCopyState::Probing(_, size, _) = &mut state.enc.unwrap_cap().state {
                    *size = Some((width, height))
                }
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
                if let ExtImageCopyState::Probing(_, _, dev) = &mut state.enc.unwrap_cap().state {
                    *dev = Some(path);
                }
            }
            ext_image_copy_capture_session_v1::Event::DmabufFormat { format, modifiers } => {
                assert!(modifiers.len() % 8 == 0);
                let modifiers = modifiers
                    .windows(8)
                    .map(|b| DrmModifier(u64::from_ne_bytes(b.try_into().unwrap())))
                    .collect();

                if let Ok(fourcc) = DrmFourcc::try_from(format) {
                    if let ExtImageCopyState::Probing(formats, _, _) =
                        &mut state.enc.unwrap_cap().state
                    {
                        formats.push(DmabufPotentialFormat { fourcc, modifiers })
                    }
                } else {
                    warn!("Unknown DRM Fourcc: 0x{:08x}", format)
                }
            }
            ext_image_copy_capture_session_v1::Event::Done => {
                // decide on format
                let probed = if let ExtImageCopyState::Probing(formats, size, dev) =
                    &state.enc.unwrap_cap().state
                {
                    Some((formats.clone(), size, dev.clone()))
                } else {
                    None
                };

                if let Some((formats, size, dev)) = probed {
                    let size = size.expect("Done received before BufferSize...");
                    let fmt = state.negotiate_format(&formats, size, dev.as_deref());
                    if let Some(fmt) = fmt {
                        state.enc.unwrap_cap().state = ExtImageCopyState::Ready(fmt, size);
                    } else {
                        return; // error, it's already reported so we just have to cleanup & exit
                    }
                }

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

enum ExtImageCopyState {
    Probing(
        Vec<DmabufPotentialFormat>,
        Option<(u32, u32)>,
        Option<PathBuf>,
    ),
    Ready(DmabufFormat, (u32, u32)),
}

pub struct CapExtImageCopy {
    output_capture_session: ExtImageCopyCaptureSessionV1,
    time: Option<(u32, u32, u32)>,
    state: ExtImageCopyState,
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
            state: ExtImageCopyState::Probing(Vec::new(), None, None),
        })
    }

    fn queue_capture_frame(
        &self,
        eq: &QueueHandle<crate::State<Self>>,
    ) -> Option<(u32, u32, DrmFourcc, Self::Frame)> {
        if let ExtImageCopyState::Ready(fmt, (w, h)) = &self.state {
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
