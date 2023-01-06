extern crate ffmpeg_next as ffmpeg;

use std::{
    collections::VecDeque,
    ffi::c_int,
    io,
    ptr::null_mut,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use clap::{command, error::ErrorKind, Parser};
use crossbeam::{channel::bounded, thread};
use ffmpeg::{
    codec::{self, Parameters, context},
    dict, encoder,
    ffi::{
        av_buffer_ref, av_buffer_unref, av_buffersrc_parameters_alloc, av_buffersrc_parameters_set,
        av_free, av_hwdevice_ctx_create, av_hwframe_ctx_alloc, av_hwframe_ctx_init,
        av_hwframe_get_buffer, av_hwframe_map, AVDRMFrameDescriptor, AVHWFramesContext,
        AVPixelFormat, AV_HWFRAME_MAP_READ, AV_HWFRAME_MAP_WRITE,
    },
    filter,
    format::{self, Pixel, Output},
    frame::{self, video},
    Packet,
};
use wayland_client::{
    protocol::{
        wl_buffer::WlBuffer,
        wl_output::{self, WlOutput},
    },
    Display, Filter, GlobalManager, Interface, Main,
};
use wayland_protocols::{
    unstable::linux_dmabuf::v1::client::{
        zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
        zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
    },
    wlr::unstable::screencopy::v1::client::{
        zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
        zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
    },
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
        Main<ZwpLinuxBufferParamsV1>,
        Main<ZwlrScreencopyFrameV1>,
        Main<WlBuffer>,
    )>,
    // free_surfaces: Vec<VaSurface>,

    // va_dpy: *mut c_void,
    // va_context: u32,
    dma: Main<ZwpLinuxDmabufV1>,
    copy_manager: Main<ZwlrScreencopyManagerV1>,
    wl_output: Main<WlOutput>,
    running: Arc<AtomicBool>,
    frames_rgb: AvHwFrameCtx,
    frame_send: crossbeam::channel::Sender<VaSurface>,
}

impl State {
    fn process_ready(&mut self) {}
    fn queue_copy(&mut self) {
        // let mut surf = self.free_surfaces.pop().unwrap();
        let mut surf = self.frames_rgb.alloc();

        // let modifier = surf.export.objects[0].drm_format_modifier.to_be_bytes();
        // let stride = surf.export.layers[0].pitch[0];
        // let fd =                 surf.export.objects[0].fd;

        let (desc, mapping) = surf.map();

        let modifier = desc.objects[0].format_modifier.to_be_bytes();
        let stride = desc.layers[0].planes[0].pitch as u32;
        let fd = desc.objects[0].fd;

        let dma_params = self.dma.create_params();
        dma_params.add(
            fd,
            0,
            0,
            stride,
            u32::from_be_bytes(modifier[..4].try_into().unwrap()),
            u32::from_be_bytes(modifier[4..].try_into().unwrap()),
        );

        let out = self.copy_manager.capture_output(1, &*self.wl_output);

        out.assign(Filter::new(
            move |(interface, event), _, mut d| match event {
                zwlr_screencopy_frame_v1::Event::Ready {
                    tv_sec_hi,
                    tv_sec_lo,
                    tv_nsec,
                } => {
                    let state = d.get::<State>().unwrap();

                    let (
                        mut surf,
                        drop_mapping,
                        destroy_buffer_params,
                        destroy_frame,
                        destroy_buffer,
                    ) = state.surfaces_owned_by_compositor.pop_front().unwrap();

                    drop(drop_mapping);
                    destroy_buffer_params.destroy();
                    destroy_frame.destroy();
                    destroy_buffer.destroy();

                    let secs = (i64::from(tv_sec_hi) << 32) + i64::from(tv_sec_lo);
                    let pts = secs * 1_000_000 + i64::from(tv_nsec) / 1_000;

                    surf.f.set_pts(Some(pts));

                    state.frame_send.try_send(surf);
                }
                _ => {}
            },
        ));

        let buf = dma_params.create_immed(
            self.dims.unwrap().0,
            self.dims.unwrap().1,
            gbm::Format::Xrgb8888 as u32,
            zwp_linux_buffer_params_v1::Flags::empty(),
        );

        // dma_params.destroy();

        out.copy_with_damage(&*buf);

        self.surfaces_owned_by_compositor
            .push_back((surf, mapping, dma_params, out, buf));
    }
}

struct EncState {
    filter: filter::Graph,
    enc: encoder::Video,
    octx: format::context::Output,
    frame_recv: crossbeam::channel::Receiver<VaSurface>
}

impl EncState {

    fn process_ready(&mut self) {

            let mut yuv_frame = frame::Video::empty();
            while self.filter
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

    fn thread(&mut self) {

        while let Ok(frame) = self.frame_recv.recv() {
            self.filter.get("in").unwrap().source().add(&frame.f).unwrap();

            self.process_ready();
        }

        self.filter.get("in").unwrap().source().flush().unwrap();
        self.process_ready();
        self.enc.send_eof().unwrap();
        self.process_ready();
        self.octx.write_trailer().unwrap();
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
            (*hwframe_casted).initial_pool_size = 20;

            let sts = av_hwframe_ctx_init(hwframe);
            assert_eq!(sts, 0);

            let ret = Ok(AvHwFrameCtx {
                ptr: av_buffer_ref(hwframe),
            });

            av_buffer_unref(&mut hwframe);

            ret
        }
    }
}

struct AvHwFrameCtx {
    ptr: *mut ffmpeg::sys::AVBufferRef,
}

impl AvHwFrameCtx {
    fn alloc(&mut self) -> VaSurface {
        let mut frame = ffmpeg_next::frame::video::Video::empty();
        let sts = unsafe { av_hwframe_get_buffer(self.ptr, frame.as_mut_ptr(), 0) };
        assert_eq!(sts, 0);

        VaSurface { f: frame }
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

fn main() {
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || r.store(false, Ordering::SeqCst)).unwrap();

    ffmpeg_next::init().unwrap();

    // ffmpeg_next::log::set_level(ffmpeg::log::Level::Trace);

    let args = Args::parse();

    let conn = Display::connect_to_env().unwrap();
    let mut eq = conn.create_event_queue();
    let attachment = conn.attach(eq.token());

    let gm = GlobalManager::new(&attachment);

    eq.sync_roundtrip(&mut (), |_, _, _| unreachable!())
        .unwrap();

    let mut outputs = Vec::new();
    for (name, interface_name, _version) in gm.list() {
        if interface_name == wl_output::WlOutput::NAME {
            outputs.push(name);
        }
    }

    if outputs.len() != 1 {
        panic!("oops for now!");
    }
    let output = outputs[0];

    let wl_output: Main<WlOutput> = attachment.get_registry().bind(WlOutput::VERSION, output);

    // let out: Main<ZxdgOutputManagerV1> = gm.instantiate_exact(ZxdgOutputManagerV1::VERSION).unwrap();

    // out.get

    let man: Main<ZwlrScreencopyManagerV1> = gm
        .instantiate_exact(ZwlrScreencopyManagerV1::VERSION)
        .unwrap();

    let dma: Main<ZwpLinuxDmabufV1> = gm.instantiate_exact(ZwpLinuxDmabufV1::VERSION).unwrap();

    // dma.assign(Filter::new(move |ev, _, _| match ev {
    //     Events::Dma { event: ev, object: o } => {

    //     }

    // }));

    wl_output.assign(Filter::new(
        move |(interface, event), _, mut d| match event {
            wl_output::Event::Mode {
                flags,
                width,
                height,
                refresh,
            } => {
                if flags.contains(wl_output::Mode::Current) {
                    d.get::<State>().unwrap().dims = Some((width, height));
                }
            }
            _ => {}
        },
    ));

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

    let mut state = State {
        dims: None,
        surfaces_owned_by_compositor: VecDeque::new(),
        dma,
        copy_manager: man,
        wl_output,
        running,
        frames_rgb,
        frame_send,
    };

    let mut enc_state = EncState {
        frame_recv,
        filter,
        enc,
        octx,
    };

    let enc_thread = std::thread::spawn(move || {
        enc_state.thread();
    });

    eq.sync_roundtrip(&mut state, |a, b, c| println!("{:?} {:?} {:?}", a, b, c))
        .unwrap();

    let (w, h) = state.dims.unwrap();

    // TODO: detect formats
    let mut drm_device_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/dri/card0")
        .unwrap();

    // for _ in 0..1 {
    //     state.free_surfaces.push(frames_rgb.alloc());
    // }

    while state.running.load(Ordering::SeqCst) {
        while state.surfaces_owned_by_compositor.len() < 5 {
            state.queue_copy();
        }
        match eq.dispatch(&mut state, |_, _, _| ()) {
            Ok(_) => continue,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => panic!("{}", e),
        }
    }

    enc_thread.join();
}
