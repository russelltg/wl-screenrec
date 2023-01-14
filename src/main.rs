extern crate ffmpeg_next as ffmpeg;

use std::{
    collections::VecDeque,
    ffi::{c_int, CString},
    io,
    num::ParseIntError,
    ops::RangeInclusive,
    ptr::null_mut,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use clap::{command, error::ErrorKind, ArgAction, Parser};
use crossbeam::{
    channel::{bounded, Sender},
    thread,
};
use ffmpeg::{
    codec::{self, context, Parameters},
    color::Range,
    dict, encoder,
    ffi::{
        av_buffer_ref, av_buffer_unref, av_buffersink_params_alloc,
        av_buffersrc_parameters_alloc, av_buffersrc_parameters_set, av_free,
        av_hwdevice_ctx_create, av_hwframe_ctx_alloc, av_hwframe_ctx_init, av_hwframe_get_buffer,
        av_hwframe_map, av_opt_set, av_rescale, av_rescale_q, avcodec_alloc_context3,
        AVDRMFrameDescriptor, AVHWFramesContext, AVPixelFormat, AV_HWFRAME_MAP_READ,
        AV_HWFRAME_MAP_WRITE,
    },
    filter,
    format::{self, Output, Pixel},
    frame::{self, video},
    Error, Packet, Rational,
};
use thiserror::Error;
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
struct Args {
    #[clap(long="no-hw", default_value = "true", action=ArgAction::SetFalse)]
    hw: bool,

    #[clap(long, short, default_value = "screenrecord.avi")]
    output: String,

    #[clap(long, short, value_parser=parse_geometry, help="geometry to capture, format x,y WxH. Compatiable with the output of `slurp`")]
    geometry: Option<(u32, u32, u32, u32)>,
}

#[derive(Error, Debug)]
enum ParseGeometryError {
    #[error("invalid integer")]
    BadInt(#[from] ParseIntError),
    #[error("invalid geometry string")]
    BadStructure,
    #[error("invalid location string")]
    BadLocation,
    #[error("invalid size string")]
    BadSize,
}

fn parse_geometry(s: &str) -> Result<(u32, u32, u32, u32), ParseGeometryError> {
    use ParseGeometryError::*;
    let mut it = s.split(' ');
    let loc = it.next().ok_or(BadStructure)?;
    let size = it.next().ok_or(BadStructure)?;
    if it.next().is_some() {
        return Err(BadStructure);
    }

    let mut it = loc.split(",");
    let startx = it.next().ok_or(BadLocation)?.parse()?;
    let starty = it.next().ok_or(BadLocation)?.parse()?;
    if it.next().is_some() {
        return Err(BadLocation);
    }

    let mut it = size.split("x");
    let sizex = it.next().ok_or(BadSize)?.parse()?;
    let sizey = it.next().ok_or(BadSize)?.parse()?;
    if it.next().is_some() {
        return Err(BadSize);
    }

    Ok((startx, starty, sizex, sizey))
}

struct FpsCounter {
    last_print_time: Instant,
    ct: u64,
}

impl FpsCounter {
    fn new() -> Self {
        Self {
            last_print_time: Instant::now(),
            ct: 0,
        }
    }
    fn on_frame(&mut self) {
        self.ct += 1;

        if self.last_print_time.elapsed() > Duration::from_secs(1) {
            println!("{} fps", self.ct);
            self.last_print_time = Instant::now();
            self.ct = 0;
        }
    }
}

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
    // dims: Option<(i32, i32)>,
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
    // frame_send: crossbeam::channel::Sender<VaSurface>,
    enc: Option<EncState>,
    starting_timestamp: Option<i64>,
    fps_counter: FpsCounter,
    geometry: Option<(u32, u32, u32, u32)>,
    output: String,
    hw: bool,
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
                state.fps_counter.on_frame();

                let (mut surf, drop_mapping, destroy_buffer_params, destroy_frame, destroy_buffer) =
                    state.surfaces_owned_by_compositor.pop_front().unwrap();

                drop(drop_mapping);
                destroy_buffer_params.destroy();
                destroy_frame.destroy();
                destroy_buffer.destroy();

                let secs = (i64::from(tv_sec_hi) << 32) + i64::from(tv_sec_lo);
                let pts = secs * 1_000_000_000 + i64::from(tv_nsec);

                if state.starting_timestamp.is_none() {
                    state.starting_timestamp = Some(pts);
                }

                surf.f
                    .set_pts(Some(pts - state.starting_timestamp.unwrap()));
                unsafe {
                    (*surf.f.as_mut_ptr()).time_base.num = 1;
                    (*surf.f.as_mut_ptr()).time_base.den = 1_000_000_000;
                }

                state.enc.as_mut().unwrap().push(surf);

                state.queue_copy(qhandle);
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => {}
            zwlr_screencopy_frame_v1::Event::LinuxDmabuf { .. } => {}
            zwlr_screencopy_frame_v1::Event::Damage { .. } => {}
            zwlr_screencopy_frame_v1::Event::Buffer { .. } => {}
            zwlr_screencopy_frame_v1::Event::Flags { .. } => {}
            zwlr_screencopy_frame_v1::Event::Failed => {
                println!("Failed to screencopy!");
                RUNNING.store(false, Ordering::SeqCst)
            }

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
        match event {
            wayland_client::protocol::wl_buffer::Event::Release => {}
            _ => {
                dbg!(event);
            }
        }
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
        dbg!(event);
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
                if state.enc.is_none() {
                    let (enc_x, enc_y, enc_w, enc_h) = if let Some((x, y, w, h)) = state.geometry {
                        (x as i32, y as i32, w as i32, h as i32)
                    } else {
                        (0, 0, width, height)
                    };
                    state.enc = Some(EncState::new(
                        &state.output,
                        state.hw,
                        width,
                        height,
                        enc_x,
                        enc_y,
                        enc_w,
                        enc_h,
                    ));
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
        // frames_rgb: AvHwFrameCtx,
        // frame_send: Sender<VaSurface>,

        // x y w h
        geometry: Option<(u32, u32, u32, u32)>,
        output: String,
        hw: bool,
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
            // dims: None,
            surfaces_owned_by_compositor: VecDeque::new(),
            dma,
            screencopy_manager: man,
            wl_output,
            // running,
            // frames_rgb,
            enc: None,
            // frame_send,
            starting_timestamp: None,
            fps_counter: FpsCounter::new(),
            geometry,
            output,
            hw,
        }
    }

    fn queue_copy(&mut self, eq: &QueueHandle<State>) {
        // let mut surf = self.free_surfaces.pop().unwrap();
        let enc = self.enc.as_mut().unwrap();
        let mut surf = enc.frames_rgb.alloc().unwrap();

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

        let (w, h) = enc.capture_size;
        let buf = dma_params.create_immed(
            w,
            h,
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
    frames_rgb: AvHwFrameCtx,
    // frame_recv: crossbeam::channel::Receiver<VaSurface>,
    filter_output_timebase: Rational,
    octx_time_base: Rational,
    last_pts: i64,
    vid_stream_idx: usize,
    capture_size: (i32, i32),
    encode_rect: (i32, i32, i32, i32),
}

impl EncState {
    fn new(
        output: &str,
        hw: bool,
        capture_w: i32,
        capture_h: i32,
        encode_x: i32,
        encode_y: i32,
        encode_w: i32,
        encode_h: i32,
    ) -> Self {
        let mut octx = ffmpeg_next::format::output(&output).unwrap();

        let mut ost = octx
            .add_stream(ffmpeg_next::encoder::find(codec::Id::H264))
            .unwrap();

        ost.set_time_base((1, 120));

        let vid_stream_idx = ost.index();

        let codec =
            ffmpeg_next::encoder::find_by_name(if hw { "h264_vaapi" } else { "libx264" }).unwrap();

        let mut enc =
            unsafe { codec::context::Context::wrap(avcodec_alloc_context3(codec.as_ptr()), None) }
                .encoder()
                .video()
                .unwrap();

        let mut hw_device_ctx = AvHwDevCtx::new_libva();
        let mut frames_rgb = hw_device_ctx
            .create_frame_ctx(AVPixelFormat::AV_PIX_FMT_BGR0, capture_w, capture_h)
            .unwrap();

        let (mut filter, filter_timebase) =
            filter(&frames_rgb, hw, capture_w, capture_h, encode_w, encode_h);

        let mut frames_yuv = hw_device_ctx
            .create_frame_ctx(AVPixelFormat::AV_PIX_FMT_NV12, encode_w, encode_h)
            .unwrap();

        enc.set_bit_rate(40_000_000);
        enc.set_width(encode_w as u32);
        enc.set_height(encode_h as u32);
        enc.set_time_base((1, 120));
        enc.set_frame_rate(Some((120, 1)));
        enc.set_flags(codec::Flags::GLOBAL_HEADER);

        if hw {
            enc.set_format(Pixel::VAAPI);

            unsafe {
                (*enc.as_mut_ptr()).hw_device_ctx = hw_device_ctx.ptr as *mut _;
                (*enc.as_mut_ptr()).hw_frames_ctx = frames_yuv.ptr as *mut _;
                (*enc.as_mut_ptr()).sw_pix_fmt = AVPixelFormat::AV_PIX_FMT_NV12;
            }
        } else {
            enc.set_format(Pixel::NV12);
        }

        println!("{}", filter.dump());

        ost.set_parameters(&enc);

        let octx_time_base = ost.time_base();

        unsafe {
            dbg!((*enc.as_mut_ptr()).time_base.num);
            dbg!((*enc.as_mut_ptr()).time_base.den);
        }

        let opts = if hw {
            dict! {
                // "profile" => "high",
                "low_power" => "1"
            }
        } else {
            dict! {
                "preset" => "ultrafast"
            }
        };

        let mut enc = enc.open_with(opts).unwrap();

        ffmpeg_next::format::context::output::dump(&octx, 0, Some(&output));
        octx.write_header().unwrap();
        EncState {
            filter,
            enc,
            filter_output_timebase: filter_timebase,
            octx_time_base,
            octx,
            last_pts: 0,
            vid_stream_idx,
            frames_rgb,
            capture_size: (capture_w, capture_h),
            encode_rect: (encode_x, encode_y, encode_w, encode_h),
        }
    }
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
            unsafe {
                let new_pts = av_rescale_q(
                    yuv_frame.pts().unwrap(),
                    self.filter_output_timebase.into(),
                    self.octx_time_base.into(),
                );
                yuv_frame.set_pts(Some(new_pts));
            }

            self.enc.send_frame(&yuv_frame).unwrap();
        }

        let mut encoded = Packet::empty();
        while self.enc.receive_packet(&mut encoded).is_ok() {
            encoded.set_stream(self.vid_stream_idx);
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

    // fn thread(&mut self) {
    //     while let Ok(frame) = self.frame_recv.recv() {
    //         self.push(frame);
    //     }

    //     self.flush();
    // }

    fn push(&mut self, mut surf: VaSurface) {
        let (x, y, w, h) = self.encode_rect;

        unsafe {
            let f = &mut (*surf.f.as_mut_ptr());
            f.crop_left = x as usize;
            f.crop_right = (self.capture_size.0 - w - x) as usize;

            f.crop_top = y as usize;
            f.crop_bottom = (self.capture_size.1 - h - y) as usize;

            println!(
                "in={}x{} -> {}x{} ({},{},{},{})",
                self.capture_size.0,
                self.capture_size.1,
                w,
                h,
                f.crop_left,
                f.crop_right,
                f.crop_top,
                f.crop_bottom
            );
        }

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

    fn create_frame_ctx(
        &mut self,
        pixfmt: AVPixelFormat,
        width: i32,
        height: i32,
    ) -> Result<AvHwFrameCtx, ffmpeg::Error> {
        unsafe {
            let mut hwframe = av_hwframe_ctx_alloc(self.ptr as *mut _);
            let hwframe_casted = (*hwframe).data as *mut AVHWFramesContext;

            // ffmpeg does not expose RGB vaapi
            (*hwframe_casted).format = Pixel::VAAPI.into();
            // (*hwframe_casted).sw_format = AVPixelFormat::AV_PIX_FMT_YUV420P;
            (*hwframe_casted).sw_format = pixfmt;
            (*hwframe_casted).width = width;
            (*hwframe_casted).height = height;
            (*hwframe_casted).initial_pool_size = 5;

            let sts = av_hwframe_ctx_init(hwframe);
            if sts != 0 {
                return Err(Error::from(sts));
            }

            let ret = Ok(AvHwFrameCtx {
                ptr: av_buffer_ref(hwframe),
                // _devctx: self.clone(),
            });

            av_buffer_unref(&mut hwframe);

            ret
        }
    }
}

impl Drop for AvHwDevCtx {
    fn drop(&mut self) {
        unsafe {
            // TODO: debug: segfault when I add this
            // av_buffer_unref(&mut self.ptr);
        }
    }
}

struct AvHwFrameCtx {
    ptr: *mut ffmpeg::sys::AVBufferRef,
}

impl Drop for AvHwFrameCtx {
    fn drop(&mut self) {
        unsafe {
            // TODO: debug: segfault when I uncomment
            // av_buffer_unref(&mut self.ptr);
        }
    }
}

static CTR: AtomicU32 = AtomicU32::new(0);

impl AvHwFrameCtx {
    fn alloc(&mut self) -> Result<VaSurface, Error> {
        let id = CTR.fetch_add(1, Ordering::SeqCst);

        let mut frame = ffmpeg_next::frame::video::Video::empty();
        match unsafe { av_hwframe_get_buffer(self.ptr, frame.as_mut_ptr(), 0) } {
            0 => Ok(VaSurface { f: frame }),
            e => Err(Error::from(e)),
        }
    }
}

fn filter(
    inctx: &AvHwFrameCtx,
    hw: bool,
    capture_width: i32,
    capture_height: i32,
    enc_width: i32,
    enc_height: i32,
) -> (filter::Graph, Rational) {
    let mut g = ffmpeg::filter::graph::Graph::new();
    g.add(
        &filter::find("buffer").unwrap(),
        "in",
        &format!(
            "video_size=2840x2160:pix_fmt={}:time_base=1/1000000000",
            AVPixelFormat::AV_PIX_FMT_VAAPI as c_int
        ),
    )
    .unwrap();

    unsafe {
        let p = &mut *av_buffersrc_parameters_alloc();

        p.width = capture_width;
        p.height = capture_height;
        p.format = AVPixelFormat::AV_PIX_FMT_VAAPI as c_int;
        p.time_base.num = 1;
        p.time_base.den = 1_000_000_000;
        p.hw_frames_ctx = inctx.ptr;

        let sts = av_buffersrc_parameters_set(g.get("in").unwrap().as_mut_ptr(), p as *mut _);
        assert_eq!(sts, 0);

        av_free(p as *mut _ as *mut _);
    }

    g.add(&filter::find("buffersink").unwrap(), "out", "")
        .unwrap();

    let mut out = g.get("out").unwrap();
    if hw {
        out.set_pixel_format(Pixel::VAAPI);
    } else {
        out.set_pixel_format(Pixel::NV12);
    }

    g.output("in", 0)
        .unwrap()
        .input("out", 0)
        .unwrap()
        .parse(&format!(
            "scale_vaapi=format=nv12{}:w={}:h={}",
            if hw { "" } else { ", hwdownload" },
            enc_width,
            enc_height
        ))
        .unwrap();

    g.validate().unwrap();

    (g, Rational::new(1, 1_000_000_000))
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

    let (globals, mut queue) = registry_queue_init(&conn).unwrap();

    let mut disp_name = None;
    for g in globals.contents().clone_list() {
        if g.interface == WlOutput::interface().name {
            assert_eq!(disp_name, None);
            disp_name = Some(g.name);
        }
    }

    let mut state = State::new(
        &display,
        &queue.handle(),
        &globals,
        disp_name.unwrap(),
        // running,
        // frame_send,
        // enc_state,
        args.geometry,
        args.output,
        args.hw,
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

    if let Some(enc) = &mut state.enc {
        enc.flush();
    }
}
