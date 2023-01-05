extern crate ffmpeg_next as ffmpeg;

use std::{
    collections::VecDeque,
    fs::File,
    mem::MaybeUninit,
    os::{
        fd::{AsFd, AsRawFd},
        raw::c_void,
    },
    ptr::{null, null_mut},
    sync::{Arc, Mutex},
};

use clap::{command, Parser};
use ffmpeg::{
    codec::{self, Parameters},
    dict, encoder,
    ffi::{
        av_hwdevice_ctx_create, av_hwframe_ctx_alloc, av_hwframe_ctx_create_derived,
        av_hwframe_get_buffer, AVFrame, AVHWFramesContext, AVPixelFormat,
    },
    format::Pixel,
    frame::video,
    Codec,
};
// use gbm::{BufferObject, BufferObjectFlags, Device};
use image::{EncodableLayout, ImageBuffer, ImageOutputFormat, Rgba};
use vaapi_sys::{
    vaCreateSurfaces, vaExportSurfaceHandle, vaGetDisplayDRM, vaInitialize,
    VADRMPRIMESurfaceDescriptor, _VADRMPRIMESurfaceDescriptor__bindgen_ty_1, vaBeginPicture,
    vaCreateConfig, vaCreateContext, vaDeriveImage, vaDestroyImage, vaEndPicture, vaMapBuffer,
    vaRenderPicture, vaUnmapBuffer, VAEntrypoint_VAEntrypointEncSlice,
    VAEntrypoint_VAEntrypointEncSliceLP, VAImage, VAProfile_VAProfileAV1Profile0,
    VAProfile_VAProfileH264High, VA_EXPORT_SURFACE_READ_WRITE, VA_FOURCC_XRGB, VA_PROGRESSIVE,
    VA_RT_FORMAT_RGB32, VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
};
use wayland_client::{
    event_enum,
    protocol::wl_output::{self, WlOutput},
    Display, Filter, GlobalManager, Interface, Main,
};
use wayland_protocols::{
    unstable::linux_dmabuf::{
        self,
        v1::client::{
            zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
            zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        },
    },
    wlr::unstable::screencopy::v1::client::{
        zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
    },
};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {}

struct VaSurface {
    // va_surface: u32,
    // export: VADRMPRIMESurfaceDescriptor,
    // img: VAImage,
    f: video::Video,
}

struct State {
    dims: Option<(i32, i32)>,
    pending_surfaces: VecDeque<VaSurface>,
    free_surfaces: Vec<VaSurface>,
    va_dpy: *mut c_void,
    // va_context: u32,
    enc: ffmpeg_next::encoder::Video,
}

struct AvHwDevCtx {
    ptr: *mut ffmpeg::sys::AVHWDeviceContext,
}

impl AvHwDevCtx {
    fn new() -> Self {
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

            Self {
                ptr: hw_device_ctx as *mut _,
            }
        }
    }

    fn create_frame_ctx(&mut self, pixfmt: AVPixelFormat) -> Result<AvHwFrameCtx, ffmpeg::Error> {
        unsafe {
            let hwframe = av_hwframe_ctx_alloc(self.ptr as *mut _);
            let hwframe_casted = (*hwframe).data as *mut AVHWFramesContext;

            // ffmpeg does not expose RGB vaapi
            (*hwframe_casted).format = Pixel::VAAPI.into();
            // (*hwframe_casted).sw_format = AVPixelFormat::AV_PIX_FMT_YUV420P;
            (*hwframe_casted).sw_format = pixfmt;
            (*hwframe_casted).width = 1920;
            (*hwframe_casted).height = 1080;
            (*hwframe_casted).initial_pool_size = 20;

            // (*enc.as_mut_ptr()).hw_device_ctx = hw_device_ctx;
            // (*enc.as_mut_ptr()).hw_frames_ctx = hwframe;
            // (*enc.as_mut_ptr()).sw_pix_fmt = AVPixelFormat::AV_PIX_FMT_YUV420P;
            Ok(AvHwFrameCtx {
                ptr: hwframe_casted,
            })
        }
    }
}

struct AvHwFrameCtx {
    ptr: *mut ffmpeg::sys::AVHWFramesContext,
}

impl AvHwFrameCtx {
    fn alloc(&mut self) -> video::Video {
        let mut frame = ffmpeg_next::frame::video::Video::empty();
        let sts = unsafe { av_hwframe_get_buffer(self.ptr as *mut _, frame.as_mut_ptr(), 0) };
        assert_eq!(sts, 0);

        frame
    }
}

// unsafe fn alloc_va_surface(va_dpy: *mut c_void, w: i32, h: i32) -> VaSurface {
//     let mut surface = 0;
//     let sts = vaCreateSurfaces(
//         va_dpy,
//         VA_RT_FORMAT_RGB32,
//         w as u32,
//         h as u32,
//         &mut surface,
//         1,
//         null_mut(),
//         0,
//     );
//     if sts != 0 {
//         panic!();
//     }

//     let mut p: MaybeUninit<VADRMPRIMESurfaceDescriptor> = MaybeUninit::uninit();
//     let sts = vaExportSurfaceHandle(
//         va_dpy,
//         surface,
//         VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
//         VA_EXPORT_SURFACE_READ_WRITE,
//         p.as_mut_ptr() as *mut _,
//     );
//     if sts != 0 {
//         panic!();
//     }

//     let mut image = MaybeUninit::uninit();
//     let sts = vaDeriveImage(va_dpy, surface, image.as_mut_ptr());
//     if sts != 0 {
//         panic!();
//     }

//     VaSurface {
//         va_surface: surface,
//         export: p.assume_init(),
//         img: image.assume_init(),
//     }
// }

fn main() {
    ffmpeg_next::init().unwrap();

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

    let out = man.capture_output(1, &*wl_output);

    out.assign(Filter::new(
        move |(interface, event), _, mut d| match event {
            zwlr_screencopy_frame_v1::Event::Ready {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
            } => {
                let state = d.get::<State>().unwrap();

                let surf = state.pending_surfaces.pop_front().unwrap();

                // unsafe {
                //     let sts = vaBeginPicture(state.va_dpy, state.va_context, surf.va_surface);
                //     assert_eq!(sts, 0);

                //     let sts = vaRenderPicture(state.va_dpy, state.va_context, null_mut(), 0);
                //     assert_eq!(sts, 0);

                //     let sts = vaEndPicture(state.va_dpy, state.va_context);
                //     assert_eq!(sts, 0);
                // };

                // let mut frame = ffmpeg_next::frame::video::Video::empty();

                // frame.set_format(Pixel::VAAPI);
                // unsafe { (*frame.as_mut_ptr()).data[3] = surf.va_surface as usize as *mut _ };

                state.enc.send_frame(&surf.f).unwrap();
            }
            _ => {}
        },
    ));

    let dma: Main<ZwpLinuxDmabufV1> = gm.instantiate_exact(ZwpLinuxDmabufV1::VERSION).unwrap();
    let dma_params = dma.create_params();

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

    let mut output = ffmpeg_next::format::output(&"out.mp4").unwrap();
    let mut ost = output
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
    enc.set_width(1920);
    enc.set_height(1080);
    enc.set_time_base((1, 60));

    unsafe {

        // let mut hw_device_ctx = null_mut();

        // let opts = dict!{
        //     "connection_type" => "drm"
        // };

        // let sts = av_hwdevice_ctx_create(&mut hw_device_ctx, ffmpeg_next::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
        //                             &b"/dev/dri/card0\0"[0] as *const _ as *const _, opts.as_mut_ptr(), 0);
        // assert_eq!(sts, 0);

        // let hwframe = av_hwframe_ctx_alloc(hw_device_ctx);
        // let hwframe_casted = (*hwframe).data as *mut AVHWFramesContext;

        // // ffmpeg does not expose RGB vaapi
        // (*hwframe_casted).format = Pixel::VAAPI.into();
        // (*hwframe_casted).sw_format = AVPixelFormat::AV_PIX_FMT_YUV420P;
        // (*hwframe_casted).width = 1920;
        // (*hwframe_casted).height = 1080;
        // (*hwframe_casted).initial_pool_size = 20;

        // (*enc.as_mut_ptr()).hw_device_ctx = hw_device_ctx;
        // (*enc.as_mut_ptr()).hw_frames_ctx = hwframe;
        // (*enc.as_mut_ptr()).sw_pix_fmt = AVPixelFormat::AV_PIX_FMT_YUV420P;
    }

    let mut hw_device_ctx = AvHwDevCtx::new();
    let mut frames_rgb = hw_device_ctx
        .create_frame_ctx(AVPixelFormat::AV_PIX_FMT_RGB0)
        .unwrap();

    let mut frames_yuv= hw_device_ctx
        .create_frame_ctx(AVPixelFormat::AV_PIX_FMT_YUV420P)
        .unwrap();

    unsafe {
        (*enc.as_mut_ptr()).hw_device_ctx = hw_device_ctx.ptr as *mut _;
        (*enc.as_mut_ptr()).hw_frames_ctx = frames_yuv.ptr as *mut _;
        (*enc.as_mut_ptr()).sw_pix_fmt = AVPixelFormat::AV_PIX_FMT_YUV420P;
    }

    let mut g = ffmpeg::filter::graph::Graph::new();
    g.add(
        &ffmpeg_next::filter::find("format").unwrap(),
        "format",
        "pix_fmts=yuv420p",
    )
    .unwrap();

    ost.set_parameters(&enc);

    let enc = enc
        .open_as_with(
            encoder::find_by_name("h264_vaapi"),
            dict! {
                // "profile" => "high"
            },
        )
        .unwrap();

    let mut state = State {
        dims: None,
        free_surfaces: Vec::new(),
        va_dpy: null_mut(),
        // va_context: 0,
        pending_surfaces: VecDeque::new(),
        enc,
    };

    ffmpeg_next::format::context::output::dump(&output, 0, Some(&"out.mp4"));

    output.write_header().unwrap();

    eq.sync_roundtrip(&mut state, |a, b, c| println!("{:?} {:?} {:?}", a, b, c))
        .unwrap();

    let (w, h) = state.dims.unwrap();

    // TODO: detect formats
    let mut drm_device_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/dri/card0")
        .unwrap();

    // let gbm = unsafe { Device::new_from_fd(card_file.as_raw_fd()) }.unwrap();

    // let gbm_buf: BufferObject<u8> = gbm
    //     .create_buffer_object(
    //         w as u32,
    //         h as u32,
    //         gbm::Format::Xrgb8888,
    //         BufferObjectFlags::LINEAR | BufferObjectFlags::RENDERING,
    //     )
    //     .unwrap();

    // let va_dpy = unsafe {
    //     let display = vaGetDisplayDRM(drm_device_file.as_raw_fd());
    //     if display.is_null() {
    //         panic!();
    //     }
    //     let mut major = 0;
    //     let mut minor = 0;
    //     let sts = vaInitialize(display, &mut major, &mut minor);
    //     if sts != 0 {
    //         panic!();
    //     }
    //     display
    // };
    // state.va_dpy = va_dpy;

    // let config_id = unsafe {
    //     let mut config_id = MaybeUninit::uninit();
    //     let sts = vaCreateConfig(
    //         va_dpy,
    //         VAProfile_VAProfileH264High,
    //         VAEntrypoint_VAEntrypointEncSliceLP,
    //         null_mut(),
    //         0,
    //         config_id.as_mut_ptr(),
    //     );
    //     assert_eq!(sts, 0);
    //     config_id.assume_init()
    // };

    // let mut surfaces = Vec::new();
    for _ in 0..5 {
        // state
        //     .free_surfaces
        //     .push(unsafe { alloc_va_surface(va_dpy, w, h) });


        state.free_surfaces.push(VaSurface { f: frames_rgb.alloc() });
        // surfaces.push(state.free_surfaces.last().unwrap().f.clone());
    }

    // unsafe {
    //     let sts = vaCreateContext(
    //         va_dpy,
    //         config_id,
    //         w as i32,
    //         h as i32,
    //         VA_PROGRESSIVE as i32,
    //         surfaces.as_mut_ptr(),
    //         surfaces.len() as i32,
    //         &mut state.va_context,
    //     );
    //     assert_eq!(sts, 0);
    // }

    // let modifier = u64::from(gbm_buf.modifier().unwrap()).to_be_bytes();
    // dma_params.add(
    //     gbm_buf.as_raw_fd(),
    //     0,
    //     0,
    //     gbm_buf.stride().unwrap(),
    //     u32::from_be_bytes(modifier[..4].try_into().unwrap()),
    //     u32::from_be_bytes(modifier[4..].try_into().unwrap()),
    // );

    {
        let surf = state.free_surfaces.pop().unwrap();

        let fd = unsafe { (*surf.f.as_ptr()).data[3] as usize as i32 };

        // let modifier = surf.export.objects[0].drm_format_modifier.to_be_bytes();
        // let stride = surf.export.layers[0].pitch[0];
        // dma_params.add(
        //     surf.export.objects[0].fd,
        //     0,
        //     0,
        //     stride,
        //     u32::from_be_bytes(modifier[..4].try_into().unwrap()),
        //     u32::from_be_bytes(modifier[4..].try_into().unwrap()),
        // );

        let buf = dma_params.create_immed(
            w,
            h,
            gbm::Format::Xrgb8888 as u32,
            zwp_linux_buffer_params_v1::Flags::empty(),
        );

        // dma_params.destroy();

        out.copy_with_damage(&*buf);

        state.pending_surfaces.push_back(surf);
    }

    loop {
        eq.dispatch(&mut state, |_, _, _| ()).unwrap();
    }

    // gbm_buf
    //     .map(&gbm, 0, 0, w as u32, h as u32, |map| {
    //         map.buffer();

    //         let img = ImageBuffer::from_fn(map.width(), map.height(), |x, y| {
    //             let start_idx = usize::try_from(map.stride() * y + x * 4).unwrap();
    //             let buf = &map.buffer()[start_idx..];
    //             Rgba([buf[2], buf[1], buf[0], buf[3]])
    //         });
    //         img.write_to(
    //             &mut File::create("out.png").unwrap(),
    //             ImageOutputFormat::Png,
    //         )
    //         .unwrap();
    //     })
    //     .unwrap()
    //     .unwrap();

    // unsafe {
    //     let mut ptr = null_mut();
    //     let sts = vaMapBuffer(va_dpy, image.buf, &mut ptr);
    //     if sts != 0 {
    //         panic!("sts={sts}");
    //     }

    //     let ptr = ptr as *const u8;

    //     let img = ImageBuffer::from_fn(w as u32, h as u32, |x, y| {
    //         let start_idx = usize::try_from(stride * y + x * 4).unwrap();
    //         let buf = ptr.add(start_idx);
    //         Rgba([
    //             buf.add(2).read(),
    //             buf.add(1).read(),
    //             buf.add(0).read(),
    //             buf.add(3).read(),
    //         ])
    //     });

    //     let sts = vaUnmapBuffer(va_dpy, image.buf);
    //     assert_eq!(sts, 0);

    //     img.write_to(
    //         &mut File::create("out.png").unwrap(),
    //         ImageOutputFormat::Png,
    //     )
    //     .unwrap();
    // }

    output.write_trailer().unwrap();
}
