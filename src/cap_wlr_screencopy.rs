use std::path::PathBuf;

use anyhow::Context;
use drm::{
    buffer::DrmFourcc,
    node::{node_path, DrmNode},
};
use libc::dev_t;
use log::debug;
use wayland_client::{
    globals::GlobalList,
    protocol::{wl_buffer::WlBuffer, wl_output::WlOutput},
    Connection, Dispatch, Proxy, QueueHandle,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1, zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

use crate::{CaptureSource, DmabufPotentialFormat, DrmModifier, State};

impl Dispatch<ZwlrScreencopyManagerV1, ()> for State<CapWlrScreencopy> {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrScreencopyManagerV1,
        _event: <ZwlrScreencopyManagerV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &wayland_client::QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State<CapWlrScreencopy> {
    fn event(
        state: &mut Self,
        capture: &ZwlrScreencopyFrameV1,
        event: <ZwlrScreencopyFrameV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        qhandle: &wayland_client::QueueHandle<Self>,
    ) {
        debug!("zwlr-screencopy-frame event: {:?} {event:?}", capture.id());
        match event {
            zwlr_screencopy_frame_v1::Event::Ready {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
            } => {
                state.on_copy_complete(qhandle, tv_sec_hi, tv_sec_lo, tv_nsec);
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => {
                let cap = state.enc.unwrap_cap();
                let device = cap.drm_device.clone();
                let formats = std::mem::replace(&mut cap.formats, Vec::new());
                let size = cap.size.unwrap();
                state.negotiate_format(&formats, size, device.as_deref(), qhandle);
                state.on_frame_allocd(qhandle, capture);
            }
            zwlr_screencopy_frame_v1::Event::LinuxDmabuf {
                format,
                width: dmabuf_width,
                height: dmabuf_height,
            } => {
                let fourcc = DrmFourcc::try_from(format).unwrap();
                let cap = state.enc.unwrap_cap();

                cap.formats.push(DmabufPotentialFormat {
                    fourcc,
                    modifiers: vec![DrmModifier::LINEAR],
                });
                cap.size = Some((dmabuf_width, dmabuf_height));
            }
            zwlr_screencopy_frame_v1::Event::Damage { .. } => {}
            zwlr_screencopy_frame_v1::Event::Buffer { .. } => {}
            zwlr_screencopy_frame_v1::Event::Flags { .. } => {}
            zwlr_screencopy_frame_v1::Event::Failed => {
                state.on_copy_fail(qhandle);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpLinuxDmabufFeedbackV1, ()> for State<CapWlrScreencopy> {
    fn event(
        state: &mut Self,
        _proxy: &ZwpLinuxDmabufFeedbackV1,
        event: <ZwpLinuxDmabufFeedbackV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_feedback_v1::Event;
        if let Event::MainDevice { device } = event {
            let dev = dev_t::from_ne_bytes(device.try_into().unwrap());
            let node = DrmNode::from_dev_id(dev).unwrap();
            let render_node_path = node_path(&node, drm::node::NodeType::Render).unwrap();

            state.enc.unwrap_cap().cap_cursor = state.args.cap_cursor;
            state.enc.unwrap_cap().drm_device = Some(render_node_path);
        }
    }
}

pub struct CapWlrScreencopy {
    formats: Vec<DmabufPotentialFormat>,
    size: Option<(u32, u32)>,
    screencopy_manager: ZwlrScreencopyManagerV1,
    output: WlOutput,
    drm_device: Option<PathBuf>,
    cap_cursor: bool,
}
impl CaptureSource for CapWlrScreencopy {
    fn new(
        gm: &GlobalList,
        eq: &QueueHandle<State<Self>>,
        output: WlOutput,
    ) -> anyhow::Result<Self> {
        let man: ZwlrScreencopyManagerV1 = gm
            .bind(eq, 3..=ZwlrScreencopyManagerV1::interface().version, ()).context("your compositor does not support zwlr-screencopy-manager and therefore is not support by wl-screenrec. See the README for supported compositors")?;

        let dma: ZwpLinuxDmabufV1 = gm
            .bind(eq, 4..=ZwpLinuxDmabufV1::interface().version, ())
            .context("your compositor does not support zwp-linux-dmabuf and therefore is not support by wl-screenrec. See the README for supported compositors")?;
        dma.get_default_feedback(eq, ());

        Ok(Self {
            screencopy_manager: man,
            output,
            drm_device: None,
            cap_cursor: false,
            formats: Vec::new(),
            size: None,
        })
    }

    fn queue_copy(&self, damage: bool, buf: &WlBuffer, _dims: (i32, i32), capture: &Self::Frame) {
        if damage {
            capture.copy_with_damage(buf);
        } else {
            capture.copy(buf);
        }
    }

    fn alloc_frame(&self, eq: &QueueHandle<State<Self>>) -> Option<Self::Frame> {
        // creating this triggers the linux_dmabuf event, which is where we allocate etc

        let _capture =
            self.screencopy_manager
                .capture_output(self.cap_cursor.into(), &self.output, eq, ());

        None
    }

    fn on_done_with_frame(&self, f: Self::Frame) {
        f.destroy();
    }

    type Frame = ZwlrScreencopyFrameV1;
}
