extern crate ffmpeg_next as ffmpeg;

use std::{
    collections::VecDeque,
    ffi::c_int,
    io,
    ops::RangeInclusive,
    ptr::null_mut,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc,
    },
};

use clap::{command, error::ErrorKind, Parser};
use crossbeam::{
    channel::{bounded, Sender},
    thread,
};
use ffmpeg::{
    codec::{self, context, Parameters},
    dict, encoder,
    ffi::{
        av_buffer_ref, av_buffer_unref, av_buffersrc_parameters_alloc, av_buffersrc_parameters_set,
        av_free, av_hwdevice_ctx_create, av_hwframe_ctx_alloc, av_hwframe_ctx_init,
        av_hwframe_get_buffer, av_hwframe_map, AVDRMFrameDescriptor, AVHWFramesContext,
        AVPixelFormat, AV_HWFRAME_MAP_READ, AV_HWFRAME_MAP_WRITE, av_buffer_get_ref_count,
    },
    filter,
    format::{self, Output, Pixel},
    frame::{self, video},
    Packet, Error, color::Range, 
};
use wayland_client::{
    globals::{registry_queue_init, GlobalList, GlobalListContents},
    protocol::{
        wl_buffer::WlBuffer,
        wl_display::WlDisplay,
        wl_output::{self, WlOutput},
        wl_registry::{self, WlRegistry},
    },
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {}

struct VaSurface {
    f: video::Video,
}

impl VaSurface {
    fn map(&mut self) -> (AVDRMFrameDescriptor, video::Video) {
        let mut dst = video::Video::empty();
        dst.set_format(Pixel::DRM_PRIME);

        unsafe {
            let sts = av_hwframe_map(
                dst.as_mut_ptr(),
                self.f.as_ptr(),
                AV_HWFRAME_MAP_WRITE as c_int | AV_HWFRAME_MAP_READ as c_int,
            );
            assert_eq!(sts, 0);

            (
                *((*dst.as_ptr()).data[0] as *const AVDRMFrameDescriptor),
                dst,
            )
        }
    }
}

struct State {
    dims: Option<(i32, i32)>,

    surfaces_owned_by_compositor: VecDeque<(
        VaSurface,
        video::Video,
        ZwpLinuxBufferParamsV1,
        ZwlrScreencopyFrameV1,
        WlBuffer,
    )>,
    // free_surfaces: Vec<VaSurface>,

    // va_dpy: *mut c_void,
    // va_context: u32,
    dma: ZwpLinuxDmabufV1,
    screencopy_manager: ZwlrScreencopyManagerV1,
    // capture: ZwlrScreencopyFrameV1,
    wl_output: WlOutput,
    // running: Arc<AtomicBool>,
    frames_rgb: AvHwFrameCtx,
    // frame_send: crossbeam::channel::Sender<VaSurface>,
    enc: EncState,
    starting_timestamp: Option<i64>,
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ZwlrScreencopyManagerV1,
        event: <ZwlrScreencopyManagerV1 as Proxy>::Event,
        data: &(),
        conn: &Connection,
        qhandle: &wayland_client::QueueHandle<Self>,
    ) {
        todo!()
    }
}

impl Dispatch<ZwpLinuxDmabufV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ZwpLinuxDmabufV1,
        event: <ZwpLinuxDmabufV1 as Proxy>::Event,
        data: &(),
        conn: &Connection,
        qhandle: &wayland_client::QueueHandle<Self>,
    ) {
        todo!()
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ZwlrScreencopyFrameV1,
        event: <ZwlrScreencopyFrameV1 as Proxy>::Event,
        data: &(),
        conn: &Connection,
        qhandle: &wayland_client::QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Ready {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
            } => {
                let (mut surf, drop_mapping, destroy_buffer_params, destroy_frame, destroy_buffer) =
                    state.surfaces_owned_by_compositor.pop_front().unwrap();

                drop(drop_mapping);
                destroy_buffer_params.destroy();
                destroy_frame.destroy();
                destroy_buffer.destroy();

                let secs = (i64::from(tv_sec_hi) << 32) + i64::from(tv_sec_lo);
                let mut pts = secs * 1_000_000 + i64::from(tv_nsec) / 1_000;
                println!("pts={pts}");

                if state.starting_timestamp.is_none() {
                    state.starting_timestamp = Some(pts);
                }

                surf.f
                    .set_pts(Some(pts - state.starting_timestamp.unwrap()));

                // if state.frame_send.try_send(surf).is_err() {
                //     println!("dropping frame!");
                // }
                state.enc.push(surf);

                state.queue_copy(qhandle);
            }
            // zwlr_screencopy_frame_v1::Event::BufferDone => {
            //     state.queue_copy(qhandle);
            // }
            // zwlr_screencopy_frame_v1::Event::LinuxDmabuf {
            //     format,
            //     width,
            //     height,
            // } => {
            //     state.dims = Some((width, height));
            // }
            _ => {
                dbg!(event);
            }
        }
    }
}

impl Dispatch<ZwpLinuxBufferParamsV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ZwpLinuxBufferParamsV1,
        event: <ZwpLinuxBufferParamsV1 as Proxy>::Event,
        data: &(),
        conn: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        todo!()
    }
}

impl Dispatch<WlBuffer, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &WlBuffer,
        event: <WlBuffer as Proxy>::Event,
        data: &(),
        conn: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        dbg!(event);
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        state: &mut Self,
        proxy: &WlRegistry,
        event: <WlRegistry as Proxy>::Event,
        data: &GlobalListContents,
        conn: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        todo!()
    }
}

impl Dispatch<WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &WlRegistry,
        event: <WlRegistry as Proxy>::Event,
        data: &(),
        conn: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        dbg!(event);
    }
}

impl Dispatch<WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &WlOutput,
        event: <WlOutput as Proxy>::Event,
        data: &(),
        conn: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        match event {
            wl_output::Event::Mode {
                flags,
                width,
                height,
                refresh,
            } => {
                if state.dims.is_none() {
                    state.dims = Some((width, height));
                    state.queue_copy(qhandle);
                }
            }
            _ => {
                dbg!(event);
            }
        }
    }
}

impl State {
    fn new(
        display: &WlDisplay,
        eq: &QueueHandle<State>,
        gm: &GlobalList,
        wl_output_name: u32,
        // running: Arc<AtomicBool>,
        frames_rgb: AvHwFrameCtx,
        // frame_send: Sender<VaSurface>,
        enc: EncState,
    ) -> Self {
        let man: ZwlrScreencopyManagerV1 = gm
            .bind(
                &eq,
                ZwlrScreencopyManagerV1::interface().version
                    ..=ZwlrScreencopyManagerV1::interface().version,
                (),
            )
            .unwrap();

        let dma: ZwpLinuxDmabufV1 = gm
            .bind(
                &eq,
                ZwpLinuxDmabufV1::interface().version..=ZwpLinuxDmabufV1::interface().version,
                (),
            )
            .unwrap();

        let registry = display.get_registry(eq, ());

        let wl_output: WlOutput =
            registry.bind(wl_output_name, WlOutput::interface().version, eq, ());

        State {
            dims: None,
            surfaces_owned_by_compositor: VecDeque::new(),
            dma,
            screencopy_manager: man,
            wl_output,
            // running,
            frames_rgb,
            // frame_send,
            enc,
            starting_timestamp: None,
        }
    }

    fn queue_copy(&mut self, eq: &QueueHandle<State>) {
        // let mut surf = self.free_surfaces.pop().unwrap();
        let mut surf = self.frames_rgb.alloc().unwrap();
        
        // let modifier = surf.export.objects[0].drm_format_modifier.to_be_bytes();
        // let stride = surf.export.layers[0].pitch[0];
        // let fd =                 surf.export.objects[0].fd;

        let (desc, mapping) = surf.map();
        

        let modifier = desc.objects[0].format_modifier.to_be_bytes();
        let stride = desc.layers[0].planes[0].pitch as u32;
        let fd = desc.objects[0].fd;

        let dma_params = self.dma.create_params(eq, ());
        dma_params.add(
            fd,
            0,
            0,
            stride,
            u32::from_be_bytes(modifier[..4].try_into().unwrap()),
            u32::from_be_bytes(modifier[4..].try_into().unwrap()),
        );

        let buf = dma_params.create_immed(
            self.dims.unwrap().0 as i32,
            self.dims.unwrap().1 as i32,
            gbm::Format::Xrgb8888 as u32,
            zwp_linux_buffer_params_v1::Flags::empty(),
            eq,
            (),
        );

        // dma_params.destroy();

        let capture = self
            .screencopy_manager
            .capture_output(1, &self.wl_output, &eq, ());
        capture.copy_with_damage(&buf);

        self.surfaces_owned_by_compositor
            .push_back((surf, mapping, dma_params, capture, buf));
    }
}

struct EncState {
    filter: filter::Graph,
    enc: encoder::Video,
    octx: format::context::Output,
    frame_recv: crossbeam::channel::Receiver<VaSurface>,
}

impl EncState {
    fn process_ready(&mut self) {
        let mut yuv_frame = frame::Video::empty();
        while self
            .filter
            .get("out")
            .unwrap()
            .sink()
            .frame(&mut yuv_frame)
            .is_ok()
        {
            self.enc.send_frame(&yuv_frame).unwrap();
        }

        let mut encoded = Packet::empty();
        while self.enc.receive_packet(&mut encoded).is_ok() {
            encoded.set_stream(0);
            // rescale?
            encoded.write_interleaved(&mut self.octx).unwrap();
        }
    }

    fn flush(&mut self) {
        self.filter.get("in").unwrap().source().flush().unwrap();
        self.process_ready();
        self.enc.send_eof().unwrap();
        self.process_ready();
        self.octx.write_trailer().unwrap();
    }

    fn thread(&mut self) {
        while let Ok(frame) = self.frame_recv.recv() {
            self.push(frame);
        }

        self.flush();
    }

    fn push(&mut self, surf: VaSurface) {
        self.filter
            .get("in")
            .unwrap()
            .source()
            .add(&surf.f)
            .unwrap();

        self.process_ready();
    }
}

struct AvHwDevCtx {
    ptr: *mut ffmpeg::sys::AVBufferRef,
}

impl AvHwDevCtx {
    fn new_libva() -> Self {
        unsafe {
            let mut hw_device_ctx = null_mut();

            let opts = dict! {
                "connection_type" => "drm"
            };

            let sts = av_hwdevice_ctx_create(
                &mut hw_device_ctx,
                ffmpeg_next::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                &b"/dev/dri/card0\0"[0] as *const _ as *const _,
                opts.as_mut_ptr(),
                0,
            );
            assert_eq!(sts, 0);

            Self { ptr: hw_device_ctx }
        }
    }

    fn create_frame_ctx(&mut self, pixfmt: AVPixelFormat) -> Result<AvHwFrameCtx, ffmpeg::Error> {
        unsafe {
            let mut hwframe = av_hwframe_ctx_alloc(self.ptr as *mut _);
            let hwframe_casted = (*hwframe).data as *mut AVHWFramesContext;

            // ffmpeg does not expose RGB vaapi
            (*hwframe_casted).format = Pixel::VAAPI.into();
            // (*hwframe_casted).sw_format = AVPixelFormat::AV_PIX_FMT_YUV420P;
            (*hwframe_casted).sw_format = pixfmt;
            (*hwframe_casted).width = 3840;
            (*hwframe_casted).height = 2160;
            (*hwframe_casted).initial_pool_size = 5;

            let sts= av_hwframe_ctx_init(hwframe);
            if sts != 0 {
                return Err(Error::from(sts));
            }

            let ret = Ok(AvHwFrameCtx {
                ptr: av_buffer_ref(hwframe),
            });

            av_buffer_unref(&mut hwframe);

            ret
        }
    }
}

impl Drop for AvHwDevCtx {
    fn drop(&mut self) {
        unsafe {
            av_buffer_unref(&mut self.ptr);
        }
    }
}

struct AvHwFrameCtx {
    ptr: *mut ffmpeg::sys::AVBufferRef,
}

impl Drop for AvHwFrameCtx {
    fn drop(&mut self) {
        unsafe {
            av_buffer_unref(&mut self.ptr);
        }
    }
}

static CTR: AtomicU32 = AtomicU32::new(0);

impl AvHwFrameCtx {
    fn alloc(&mut self) -> Result<VaSurface, Error> {
        let id = CTR.fetch_add(1, Ordering::SeqCst);

        let mut frame = ffmpeg_next::frame::video::Video::empty();
        match unsafe { av_hwframe_get_buffer(self.ptr, frame.as_mut_ptr(), 0) } {
            0 => Ok(VaSurface{ f: frame} ),
            e => Err(Error::from(e)),
        }
    }
}

fn filter(inctx: &AvHwFrameCtx) -> filter::Graph {
    let mut g = ffmpeg::filter::graph::Graph::new();
    g.add(
        &filter::find("buffer").unwrap(),
        "in",
        &format!(
            "video_size=2840x2160:pix_fmt={}:time_base=1/1000000",
            AVPixelFormat::AV_PIX_FMT_VAAPI as c_int
        ),
    )
    .unwrap();

    unsafe {
        let p = &mut *av_buffersrc_parameters_alloc();

        p.width = 3840;
        p.height = 2161;
        p.format = AVPixelFormat::AV_PIX_FMT_VAAPI as c_int;
        p.time_base.num = 1;
        p.time_base.den = 1_000_000;
        p.hw_frames_ctx = inctx.ptr;

        let sts = av_buffersrc_parameters_set(g.get("in").unwrap().as_mut_ptr(), p as *mut _);
        assert_eq!(sts, 0);

        av_free(p as *mut _ as *mut _);
    }

    g.add(&filter::find("buffersink").unwrap(), "out", "")
        .unwrap();

    let mut out = g.get("out").unwrap();
    out.set_pixel_format(Pixel::VAAPI);

    g.output("in", 0)
        .unwrap()
        .input("out", 0)
        .unwrap()
        .parse("scale_vaapi=format=nv12")
        .unwrap();

    g.validate().unwrap();

    g
}

struct RegistryHandler {
    output_names: Vec<u32>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for RegistryHandler {
    fn event(
        state: &mut Self,
        proxy: &wl_registry::WlRegistry,
        event: <wl_registry::WlRegistry as wayland_client::Proxy>::Event,
        data: &(),
        conn: &Connection,
        qhandle: &wayland_client::QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            if interface == wl_output::WlOutput::interface().name {
                state.output_names.push(name);
            }
        }
    }
}

static RUNNING: AtomicBool = AtomicBool::new(true);

#[no_mangle]
extern "C" fn quit() {
    RUNNING.store(false, Ordering::SeqCst);
}

fn main() {
    ctrlc::set_handler(move || RUNNING.store(false, Ordering::SeqCst)).unwrap();

    ffmpeg_next::init().unwrap();

    // ffmpeg_next::log::set_level(ffmpeg::log::Level::Trace);

    let args = Args::parse();

    let conn = Connection::connect_to_env().unwrap();
    let display = conn.display();

    let mut octx = ffmpeg_next::format::output(&"out.mp4").unwrap();
    let mut ost = octx
        .add_stream(ffmpeg_next::encoder::find(codec::Id::H264))
        .unwrap();

    let mut param = Parameters::new();
    unsafe {
        (*param.as_mut_ptr()).codec_id = codec::id::Id::H264.into();
    }

    // let mut enc = codec::context::Context::from_parameters(ost.parameters()).unwrap()
    let mut enc = codec::context::Context::from_parameters(param)
        .unwrap()
        .encoder()
        .video()
        .unwrap();

    enc.set_format(Pixel::VAAPI);
    enc.set_flags(codec::Flags::GLOBAL_HEADER);
    enc.set_width(3840);
    enc.set_height(2160);
    enc.set_time_base((1, 60));

    let mut hw_device_ctx = AvHwDevCtx::new_libva();
    let mut frames_rgb = hw_device_ctx
        .create_frame_ctx(AVPixelFormat::AV_PIX_FMT_BGR0)
        .unwrap();

    let mut frames_yuv = hw_device_ctx
        .create_frame_ctx(AVPixelFormat::AV_PIX_FMT_NV12)
        .unwrap();

    unsafe {
        (*enc.as_mut_ptr()).hw_device_ctx = hw_device_ctx.ptr as *mut _;
        (*enc.as_mut_ptr()).hw_frames_ctx = frames_yuv.ptr as *mut _;
        (*enc.as_mut_ptr()).sw_pix_fmt = AVPixelFormat::AV_PIX_FMT_NV12;
    }

    let mut filter = filter(&frames_rgb);
    println!("{}", filter.dump());

    ost.set_parameters(&enc);
    let mut enc = enc
        .open_as_with(
            encoder::find_by_name("h264_vaapi"),
            dict! {
                // "profile" => "high",
                "low_power" => "1"
            },
        )
        .unwrap();

    ffmpeg_next::format::context::output::dump(&octx, 0, Some(&"out.mp4"));
    octx.write_header().unwrap();

    let (frame_send, frame_recv) = bounded(10);

    let (globals, mut queue) = registry_queue_init(&conn).unwrap();

    let mut disp_name = None;
    for g in globals.contents().clone_list() {
        if g.interface == WlOutput::interface().name {
            assert_eq!(disp_name, None);
            disp_name = Some(g.name);
        }
    }

    let mut enc_state = EncState {
        frame_recv,
        filter,
        enc,
        octx,
    };

    let mut state = State::new(
        &display,
        &queue.handle(),
        &globals,
        disp_name.unwrap(),
        // running,
        frames_rgb,
        // frame_send,
        enc_state,
    );

    // let enc_thread = std::thread::spawn(move || {
    //     enc_state.thread();
    // });

    // TODO: detect formats
    while RUNNING.load(Ordering::SeqCst) {
        // while state.surfaces_owned_by_compositor.len() < 5 {
        //     state.queue_copy();
        // }
        queue.blocking_dispatch(&mut state).unwrap();
    }

    state.enc.flush();
    // drop(state); // causes thread to quit
    // enc_thread.join().unwrap();
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn free_vaapi() {
        let mut hw = AvHwDevCtx::new_libva();
        let mut framectx = hw.create_frame_ctx(AVPixelFormat::AV_PIX_FMT_BGR0).unwrap();

        for _ in 0..100 {
            let f = framectx.alloc().unwrap();
            drop(f);
        }
    }
}
