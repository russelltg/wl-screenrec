use std::{
    ffi::{CStr, c_int},
    ptr::null_mut,
    str::from_utf8,
};

use ffmpeg::{Rational, format::Pixel};
use ffmpeg_sys_next::{
    AVPixelFormat, av_buffersrc_parameters_alloc, av_buffersrc_parameters_set, av_free,
    av_get_pix_fmt_name, avfilter_graph_alloc_filter, avfilter_init_dict,
};
use log::info;
use wayland_client::protocol::wl_output::Transform;

use crate::{
    EncodePixelFormat,
    avhw::AvHwFrameCtx,
    transform::{Rect, transpose_if_transform_transposed},
};

pub enum CropScale {
    Full,
    RectScale(Rect, (i32, i32)), // rect is roi in screen coordinates, (i32, i32) is encode resolution. Will be scaled if different size than the rect0.
}

pub fn video_filter(
    inctx: &mut AvHwFrameCtx,
    pix_fmt: EncodePixelFormat,
    (capture_width, capture_height): (i32, i32),
    crop_scale: CropScale,
    transform: Transform,
    vulkan: bool,
) -> (ffmpeg::filter::Graph, Rational) {
    let graph = ffmpeg::filter::graph::Graph::new();
    let mut g = graph;

    let pixfmt_int = if vulkan {
        AVPixelFormat::AV_PIX_FMT_VULKAN as c_int
    } else {
        AVPixelFormat::AV_PIX_FMT_VAAPI as c_int
    };

    // src
    unsafe {
        let buffersrc_ctx = avfilter_graph_alloc_filter(
            g.as_mut_ptr(),
            ffmpeg::filter::find("buffer").unwrap().as_mut_ptr(),
            c"in".as_ptr() as _,
        );
        if buffersrc_ctx.is_null() {
            panic!("faield to alloc buffersrc filter");
        }

        let p = &mut *av_buffersrc_parameters_alloc();

        p.width = capture_width;
        p.height = capture_height;
        p.format = pixfmt_int;
        p.time_base.num = 1;
        p.time_base.den = 1_000_000_000;
        p.hw_frames_ctx = inctx.as_mut_ptr();

        let sts = av_buffersrc_parameters_set(buffersrc_ctx, p as *mut _);
        assert_eq!(sts, 0);
        av_free(p as *mut _ as *mut _);

        let sts = avfilter_init_dict(buffersrc_ctx, null_mut());
        assert_eq!(sts, 0);
    }

    // sink
    {
        let enc_pix_fmt = match pix_fmt {
            EncodePixelFormat::Sw(sw) => sw,
            EncodePixelFormat::Vaapi(_) => Pixel::VAAPI,
            EncodePixelFormat::Vulkan(_) => Pixel::VULKAN,
        };

        #[cfg(ffmpeg_8_0)]
        let buffersink_args = format!("pixel_formats={}", pixfmt_name(enc_pix_fmt));
        #[cfg(not(ffmpeg_8_0))]
        let buffersink_args = format!(
            "pix_fmts={:08x}",
            u32::from_be_bytes((AVPixelFormat::from(enc_pix_fmt) as u32).to_ne_bytes()) // flip endian on little-endian
        );

        g.add(
            &ffmpeg::filter::find("buffersink").unwrap(),
            "out",
            &buffersink_args,
        )
        .unwrap();
    }

    // it seems intel's vaapi driver doesn't support transpose in RGB space, so we have to transpose
    // after the format conversion
    // which means we have to transform the crop to be in the *pre* transpose space
    let (crop, (enc_w, enc_h)) = match crop_scale {
        CropScale::Full => ("".to_string(), (capture_width, capture_height)),
        CropScale::RectScale(roi_screen_coord, (enc_w_screen_coord, enc_h_screen_coord)) => {
            let Rect {
                x: roi_x,
                y: roi_y,
                w: roi_w,
                h: roi_h,
            } = roi_screen_coord.screen_to_frame(capture_width, capture_height, transform);

            // sanity check
            assert!(roi_x >= 0, "{roi_x} < 0");
            assert!(roi_y >= 0, "{roi_y} < 0");

            // exact=1 should not be necessary, as the input is not chroma-subsampled
            // however, there is a bug in ffmpeg that makes it required: https://trac.ffmpeg.org/ticket/10669
            // it is harmless to add though, so keep it as a workaround
            (
                format!("crop={roi_w}:{roi_h}:{roi_x}:{roi_y}:exact=1"),
                transpose_if_transform_transposed(
                    (enc_w_screen_coord, enc_h_screen_coord),
                    transform,
                ),
            )
        }
    };

    let scale_filter = scale_filterelem(enc_w, enc_h, transform, pix_fmt, vulkan);
    let transpose_filter = transform_filterelem(transform, vulkan);

    let hwdownload_filter = match pix_fmt {
        EncodePixelFormat::Sw(_) => "hwdownload",
        _ => "",
    };

    let filtergraph = {
        let mut filtergraph = String::new();
        for elem in [&crop, &scale_filter, &transpose_filter, hwdownload_filter] {
            if elem.is_empty() {
                continue;
            }
            if !filtergraph.is_empty() {
                filtergraph.push(',');
            }
            filtergraph.push_str(elem);
        }
        filtergraph
    };

    g.output("in", 0)
        .unwrap()
        .input("out", 0)
        .unwrap()
        .parse(&filtergraph)
        .map_err(|e| {
            panic!("failed to parse filter graph `{filtergraph}`: {}", e);
        })
        .unwrap();

    info!("{}", g.dump());

    g.validate().unwrap();

    (g, Rational::new(1, 1_000_000_000))
}

fn scale_filterelem(
    enc_w_screen_coord: i32,
    enc_h_screen_coord: i32,
    transform: Transform,
    pix_fmt: EncodePixelFormat,
    vulkan: bool,
) -> String {
    let (enc_w, enc_h) =
        transpose_if_transform_transposed((enc_w_screen_coord, enc_h_screen_coord), transform);

    let underlying_output_pixfmt_name = pixfmt_name(match pix_fmt {
        EncodePixelFormat::Vaapi(fmt) => fmt,
        EncodePixelFormat::Sw(fmt) => fmt,
        EncodePixelFormat::Vulkan(fmt) => fmt,
    });

    if vulkan {
        format!("scale_vulkan=format={underlying_output_pixfmt_name}:w={enc_w}:h={enc_h}")
    } else {
        format!("scale_vaapi=format={underlying_output_pixfmt_name}:w={enc_w}:h={enc_h}")
    }
}

fn transform_filterelem(transform: Transform, vulkan: bool) -> String {
    let transpose_dir = match transform {
        Transform::_90 => Some("clock"),
        Transform::_180 => Some("reversal"),
        Transform::_270 => Some("cclock"),
        Transform::Flipped => Some("hflip"),
        Transform::Flipped90 => Some("cclock_flip"),
        Transform::Flipped180 => Some("vflip"),
        Transform::Flipped270 => Some("clock_flip"),
        _ => None,
    };
    transpose_dir
        .map(|transpose_dir| {
            if vulkan {
                format!("transpose_vulkan=dir={transpose_dir}")
            } else {
                format!("transpose_vaapi=dir={transpose_dir}")
            }
        })
        .unwrap_or_default()
}

fn pixfmt_name(p: Pixel) -> String {
    unsafe {
        let c_name = av_get_pix_fmt_name(p.into());
        assert!(!c_name.is_null());
        from_utf8(CStr::from_ptr(c_name).to_bytes())
            .unwrap()
            .to_string()
    }
}
