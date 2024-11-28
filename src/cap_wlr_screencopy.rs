use std::path::PathBuf;

use anyhow::Context;
use drm::{buffer::DrmFourcc, node::DrmNode};
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
            zwlr_screencopy_frame_v1::Event::BufferDone => {}
            zwlr_screencopy_frame_v1::Event::LinuxDmabuf {
                format,
                width: dmabuf_width,
                height: dmabuf_height,
            } => {
                let fourcc = DrmFourcc::try_from(format).unwrap();
                let cap = state.enc.unwrap_cap();
                if !cap.sent_format {
                    cap.sent_format = true;
                    let device = cap.drm_device.clone();
                    if state
                        .negotiate_format(
                            &[DmabufPotentialFormat {
                                fourcc,
                                modifiers: vec![DrmModifier::LINEAR],
                            }],
                            (dmabuf_width, dmabuf_height),
                            device.as_deref(),
                        )
                        .is_none()
                    {
                        return; // error, which has already been reported
                    }
                }
                state.on_copy_src_ready(dmabuf_width, dmabuf_height, fourcc, qhandle, capture);
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
            let node = node
                .node_with_type(drm::node::NodeType::Render)
                .unwrap()
                .unwrap();

            let path = node.dev_path().unwrap();
            state.enc.unwrap_cap().drm_device = Some(path);
        }
    }
}

pub struct CapWlrScreencopy {
    screencopy_manager: ZwlrScreencopyManagerV1,
    output: WlOutput,
    sent_format: bool,
    drm_device: Option<PathBuf>,
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
            sent_format: false,
            drm_device: None,
        })
    }

    fn queue_copy_frame(&self, damage: bool, buf: &WlBuffer, capture: &Self::Frame) {
        if damage {
            capture.copy_with_damage(buf);
        } else {
            capture.copy(buf);
        }
    }

    fn queue_capture_frame(
        &self,
        eq: &QueueHandle<State<Self>>,
    ) -> Option<(u32, u32, DrmFourcc, Self::Frame)> {
        // creating this triggers the linux_dmabuf event, which is where we allocate etc

        let _capture = self
            .screencopy_manager
            .capture_output(1, &self.output, eq, ());

        None
    }

    fn on_done_with_frame(&self, f: Self::Frame) {
        f.destroy();
    }

    type Frame = ZwlrScreencopyFrameV1;
}
