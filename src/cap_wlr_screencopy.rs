use std::{ffi::CStr, os::fd::AsRawFd, path::PathBuf, sync::atomic::Ordering::SeqCst};

use anyhow::Context;
use drm::buffer::DrmFourcc;
use log::debug;
use wayland_client::{
    event_created_child,
    globals::GlobalList,
    protocol::{wl_buffer::WlBuffer, wl_output::WlOutput},
    Connection, Dispatch, Proxy, QueueHandle,
};
use wayland_protocols::wp::drm_lease::v1::client::{
    wp_drm_lease_connector_v1::WpDrmLeaseConnectorV1,
    wp_drm_lease_device_v1::{self, WpDrmLeaseDeviceV1},
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

use crate::{
    drmGetRenderDeviceNameFromFd, CaptureSource, DmabufPotentialFormat, DrmModifier, State,
};

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
                    state.negotiate_format(
                        &[DmabufPotentialFormat {
                            fourcc,
                            modifiers: vec![DrmModifier::LINEAR],
                        }],
                        (dmabuf_width, dmabuf_height),
                        device.as_deref(),
                    );
                }
                state.on_copy_src_ready(dmabuf_width, dmabuf_height, fourcc, qhandle, capture);
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

impl Dispatch<WpDrmLeaseDeviceV1, ()> for State<CapWlrScreencopy> {
    fn event(
        state: &mut Self,
        proxy: &WpDrmLeaseDeviceV1,
        event: <WpDrmLeaseDeviceV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        debug!("zwp-drm-lease-device event: {:?} {event:?}", proxy.id());
        if let wp_drm_lease_device_v1::Event::DrmFd { fd } = event {
            unsafe {
                let ptr = drmGetRenderDeviceNameFromFd(fd.as_raw_fd());

                if !ptr.is_null() {
                    let ret = CStr::from_ptr(ptr).to_string_lossy().to_string();
                    libc::free(ptr as *mut _);

                    state.enc.unwrap_cap().drm_device = Some(PathBuf::from(ret));
                }
            };
        }
    }

    event_created_child!(State<CapWlrScreencopy>, WpDrmLeaseDeviceV1, [
        wp_drm_lease_device_v1::EVT_CONNECTOR_OPCODE => (WpDrmLeaseConnectorV1, ()),
    ]);
}

impl Dispatch<WpDrmLeaseConnectorV1, ()> for State<CapWlrScreencopy> {
    fn event(
        _state: &mut Self,
        _proxy: &WpDrmLeaseConnectorV1,
        _event: <WpDrmLeaseConnectorV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
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

        // if this doesn't exist, it will get logged later anyways
        let _ = gm.bind::<WpDrmLeaseDeviceV1, _, _>(
            eq,
            WpDrmLeaseDeviceV1::interface().version..=WpDrmLeaseDeviceV1::interface().version,
            (),
        );

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
