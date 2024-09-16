use std::sync::atomic::Ordering::SeqCst;

use anyhow::Context;
use log::debug;
use wayland_client::{
    globals::GlobalList,
    protocol::{wl_buffer::WlBuffer, wl_output::WlOutput},
    Connection, Dispatch, Proxy, QueueHandle,
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

use crate::{CaptureSource, State};

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
                state.on_copy_src_ready(dmabuf_width, dmabuf_height, format, qhandle, capture);
            }
            zwlr_screencopy_frame_v1::Event::Damage { .. } => {}
            zwlr_screencopy_frame_v1::Event::Buffer { .. } => {}
            zwlr_screencopy_frame_v1::Event::Flags { .. } => {}
            zwlr_screencopy_frame_v1::Event::Failed => {
                eprintln!("Failed to screencopy!");
                state.quit_flag.store(1, SeqCst)
            }
            _ => {}
        }
    }
}

pub struct CapWlrScreencopy {
    screencopy_manager: ZwlrScreencopyManagerV1,
    output: WlOutput,
}
impl CaptureSource for CapWlrScreencopy {
    fn new(
        gm: &GlobalList,
        eq: &QueueHandle<State<Self>>,
        output: WlOutput,
    ) -> anyhow::Result<Self> {
        let man: ZwlrScreencopyManagerV1 = gm
            .bind(eq, 3..=ZwlrScreencopyManagerV1::interface().version, ()).context("your compositor does not support zwlr-screencopy-manager and therefore is not support by wl-screenrec. See the README for supported compositors")?;

        Ok(Self {
            screencopy_manager: man,
            output,
        })
    }

    fn queue_copy_frame(&self, damage: bool, buf: &WlBuffer, capture: &Self::Frame) {
        if damage {
            capture.copy_with_damage(buf);
        } else {
            capture.copy(buf);
        }
    }

    fn queue_capture_frame(&self, eq: &QueueHandle<State<Self>>) {
        // creating this triggers the linux_dmabuf event, which is where we allocate etc

        let _capture = self
            .screencopy_manager
            .capture_output(1, &self.output, eq, ());
    }

    fn on_done_with_frame(&self, f: Self::Frame) {
        f.destroy();
    }

    type Frame = ZwlrScreencopyFrameV1;
}
