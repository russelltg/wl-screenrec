extern crate ffmpeg_next as ffmpeg;

use std::{
    collections::{BTreeMap, VecDeque},
    ffi::{c_int, CStr},
    mem::swap,
    num::ParseIntError,
    os::fd::{AsRawFd, BorrowedFd},
    process::exit,
    str::from_utf8_unchecked,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    thread::{sleep, spawn},
    time::Duration,
};

use anyhow::{bail, format_err};
use audio::AudioHandle;
use clap::{command, ArgAction, Parser};
use ffmpeg::{
    codec, dict, encoder,
    ffi::{
        av_buffer_ref, av_buffersrc_parameters_alloc, av_buffersrc_parameters_set, av_free,
        av_get_pix_fmt_name, av_hwframe_map, avcodec_alloc_context3, avformat_query_codec,
        AVDRMFrameDescriptor, AVPixelFormat, AV_HWFRAME_MAP_WRITE, FF_COMPLIANCE_STRICT,
    },
    filter,
    format::{self, Pixel},
    frame::{self, video},
    media, Packet, Rational,
};
use human_size::{Byte, Megabyte, Size, SpecificSize};
use signal_hook::consts::{SIGINT, SIGUSR1};
use thiserror::Error;
use wayland_client::{
    event_created_child,
    globals::{registry_queue_init, GlobalListContents},
    protocol::{
        wl_buffer::WlBuffer,
        wl_output::{self, Mode, WlOutput},
        wl_registry::WlRegistry,
    },
    ConnectError, Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
};
use wayland_protocols::{
    wp::{
        drm_lease::v1::client::{
            wp_drm_lease_connector_v1::WpDrmLeaseConnectorV1,
            wp_drm_lease_device_v1::{self, WpDrmLeaseDeviceV1},
        },
        linux_dmabuf::zv1::client::{
            zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
            zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        },
    },
    xdg::xdg_output::zv1::client::{
        zxdg_output_manager_v1::ZxdgOutputManagerV1,
        zxdg_output_v1::{self, ZxdgOutputV1},
    },
};
use wayland_protocols_wlr::{
    output_management::v1::client::{
        zwlr_output_head_v1::{self, ZwlrOutputHeadV1},
        zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
        zwlr_output_mode_v1::ZwlrOutputModeV1,
    },
    screencopy::v1::client::{
        zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
        zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
    },
};

mod avhw;
use avhw::{AvHwDevCtx, AvHwFrameCtx};

mod audio;
mod fifo;

#[cfg(target_os = "linux")]
mod platform {
    pub const DEFAULT_AUDIO_CAPTURE_DEVICE: &str = "default";
    pub const AUDIO_DEVICE_HELP: &str =
        "which audio device to record from. list devices with `pactl list short sources`";
    pub const DEFAULT_AUDIO_BACKEND: &str = "pulse";
}
#[cfg(any(target_os = "dragonfly", target_os = "freebsd"))]
mod platform {
    pub const DEFAULT_AUDIO_CAPTURE_DEVICE: &str = "/dev/dsp";
    pub const AUDIO_DEVICE_HELP: &str =
        "which audio device to record from. list devices with `cat /dev/sndstat` (pcmN -> dspN)";
    pub const DEFAULT_AUDIO_BACKEND: &str = "oss";
}
use platform::*;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    #[clap(long="no-hw", default_value = "true", action=ArgAction::SetFalse, help="don't use the GPU encoder, download the frames onto the CPU and use a software encoder. Ignored if `encoder` is supplied")]
    hw: bool,

    #[clap(
        long,
        short,
        default_value = "screenrecord.mp4",
        help = "filename to write to. container type is detected from extension"
    )]
    filename: String,

    #[clap(long, short, value_parser=parse_geometry, help="geometry to capture, format x,y WxH. Compatiable with the output of `slurp`. Mutually exclusive with --output")]
    geometry: Option<(u32, u32, u32, u32)>,

    #[clap(
        long,
        short,
        help = "Which output to record to. Mutually exclusive with --geometry. Defaults to your only display if you only have one",
        default_value = ""
    )]
    output: String,

    #[clap(long, short, default_value = "0", action=ArgAction::Count, help = "add very loud logging. can be specified multiple times")]
    verbose: u8,

    #[clap(
        long,
        help = "which dri device to use for vaapi. by default, this is obtained from the drm-lease-v1 protocol, if present. if not present, /dev/dri/renderD128 is guessed"
    )]
    dri_device: Option<String>,

    #[clap(long, value_enum, default_value_t)]
    low_power: LowPowerMode,

    #[clap(
        long,
        value_enum,
        default_value_t,
        help = "which codec to use. Used in conjunction with --no-hw to determinte which enocder to use. Ignored if `encoder` is supplied"
    )]
    codec: Codec,

    #[clap(
        long,
        value_enum,
        help = "Use this to force a particular ffmpeg encoder. Generally, this is not necessary and the combo of --codec and --hw can get you to where you need to be"
    )]
    ffmpeg_encoder: Option<String>,

    #[clap(
        long,
        help = "which pixel format to encode with. not all codecs will support all pixel formats. This should be a ffmpeg pixel format string, like nv12 or x2rgb10"
    )]
    encode_pixfmt: Option<Pixel>,

    #[clap(long, short, default_value_t=SpecificSize::new(5, Megabyte).unwrap().into(), help="bitrate to encode at. Unit is bytes per second, so 5 MB is 40 Mbps")]
    bitrate: Size,

    #[clap(long,
        help="run in a mode where the screen is recorded, but nothing is written to the output file until SIGUSR1 is sent to the process. Then, it writes the most recent N seconds to a file and continues recording", 
        value_parser=parse_duration
    )]
    history: Option<Duration>,

    #[clap(long, default_value = "false", action=ArgAction::SetTrue, help="record audio with the stream. Defaults to the default audio capture device")]
    audio: bool,

    #[clap(long, default_value_t = DEFAULT_AUDIO_CAPTURE_DEVICE.to_string(), help = AUDIO_DEVICE_HELP)]
    audio_device: String,

    #[clap(long, default_value_t = DEFAULT_AUDIO_BACKEND.to_string(), help = "which ffmpeg audio capture backend (see https://ffmpeg.org/ffmpeg-devices.html`) to use. you almost certainally want to specify --audio-device if you use this, as the values depend on the backend used")]
    audio_backend: String,

    #[clap(long="no-damage", default_value = "true", action=ArgAction::SetFalse, help="copy every frame, not just unique frames. This can be helpful to get a non-variable framerate video, but is generally discouraged as it uses much more resources. Useful for testing")]
    damage: bool,
}

#[derive(clap::ValueEnum, Debug, Clone, Default)]
enum Codec {
    #[default]
    Auto,
    Avc,
    Hevc,
    VP8,
    VP9,
}

#[derive(clap::ValueEnum, Debug, Clone, Default)]
enum LowPowerMode {
    #[default]
    Auto,
    On,
    Off,
}

#[derive(Error, Debug)]
enum ParseGeometryError {
    #[error("invalid integer")]
    Int(#[from] ParseIntError),
    #[error("invalid geometry string")]
    Structure,
    #[error("invalid location string")]
    Location,
    #[error("invalid size string")]
    Size,
}

fn parse_geometry(s: &str) -> Result<(u32, u32, u32, u32), ParseGeometryError> {
    use ParseGeometryError::*;
    let mut it = s.split(' ');
    let loc = it.next().ok_or(Structure)?;
    let size = it.next().ok_or(Structure)?;
    if it.next().is_some() {
        return Err(Structure);
    }

    let mut it = loc.split(',');
    let startx = it.next().ok_or(Location)?.parse()?;
    let starty = it.next().ok_or(Location)?.parse()?;
    if it.next().is_some() {
        return Err(Location);
    }

    let mut it = size.split('x');
    let sizex = it.next().ok_or(Size)?.parse()?;
    let sizey = it.next().ok_or(Size)?.parse()?;
    if it.next().is_some() {
        return Err(Size);
    }

    Ok((startx, starty, sizex, sizey))
}

fn parse_duration(arg: &str) -> Result<std::time::Duration, std::num::ParseIntError> {
    let seconds = arg.parse()?;
    Ok(std::time::Duration::from_secs(seconds))
}

struct FpsCounter {
    ct: Arc<AtomicU64>,
}

impl FpsCounter {
    fn new() -> Self {
        let ct = Arc::new(AtomicU64::new(0));
        let ct_weak = Arc::<AtomicU64>::downgrade(&ct);

        spawn(move || {
            let mut last_ct = 0;
            loop {
                sleep(Duration::from_millis(1000));

                if let Some(ct_ptr) = ct_weak.upgrade() {
                    let ct = ct_ptr.load(Ordering::SeqCst);
                    println!("{} fps", ct - last_ct);
                    last_ct = ct;
                } else {
                    return;
                }
            }
        });

        Self { ct }
    }
    fn on_frame(&mut self) {
        self.ct.fetch_add(1, Ordering::SeqCst);
    }
}

fn map_drm(frame: &frame::Video) -> (AVDRMFrameDescriptor, video::Video) {
    let mut dst = video::Video::empty();
    dst.set_format(Pixel::DRM_PRIME);

    unsafe {
        let sts = av_hwframe_map(
            dst.as_mut_ptr(),
            frame.as_ptr(),
            AV_HWFRAME_MAP_WRITE as c_int,
        );
        assert_eq!(sts, 0);

        (
            *((*dst.as_ptr()).data[0] as *const AVDRMFrameDescriptor),
            dst,
        )
    }
}

#[derive(Debug)]
struct PartialOutputInfo {
    name: Option<String>,
    loc: Option<(i32, i32)>,
    logical_size: Option<(i32, i32)>,
    size_pixels: Option<(i32, i32)>,
    refresh: Option<Rational>,
    output: WlOutput,
}
impl PartialOutputInfo {
    fn complete(&self, fractional_scale: f64) -> Option<OutputInfo> {
        if let (Some(name), Some(loc), Some(logical_size), Some(size_pixels), Some(refresh)) = (
            &self.name,
            &self.loc,
            &self.logical_size,
            &self.size_pixels,
            &self.refresh,
        ) {
            Some(OutputInfo {
                loc: *loc,
                name: name.clone(),
                logical_size: *logical_size,
                refresh: *refresh,
                fractional_scale,
                size_pixels: *size_pixels,
                output: self.output.clone(),
            })
        } else {
            None
        }
    }
}

#[derive(Clone)]
struct OutputInfo {
    name: String,
    loc: (i32, i32),
    logical_size: (i32, i32),
    size_pixels: (i32, i32),
    refresh: Rational,
    fractional_scale: f64,
    output: WlOutput,
}

impl OutputInfo {
    fn logical_to_pixel(&self, logical: i32) -> i32 {
        (f64::from(logical) * self.fractional_scale).round() as i32
    }
}

struct State {
    surfaces_owned_by_compositor: VecDeque<(
        frame::Video,
        video::Video,
        ZwpLinuxBufferParamsV1,
        ZwlrScreencopyFrameV1,
        WlBuffer,
    )>,
    dma: ZwpLinuxDmabufV1,
    screencopy_manager: ZwlrScreencopyManagerV1,
    wl_output: Option<WlOutput>,
    enc: EncConstructionStage,
    starting_timestamp: Option<i64>,
    fps_counter: FpsCounter,
    args: Args,
    output_fractional_scales: BTreeMap<u32, (Option<String>, Option<f64>)>, // key is zwlr_output_head name (object ID) -> (name property, fractional scale)
    partial_outputs: BTreeMap<u32, PartialOutputInfo>, // key is xdg-output name (wayland object ID)
    outputs: BTreeMap<u32, OutputInfo>,
    quit_flag: Arc<AtomicBool>,
    sigusr1_flag: Arc<AtomicBool>,
    dri_device: Option<String>,
}

enum EncConstructionStage {
    None,
    EverythingButFormat {
        output: OutputInfo,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    },
    Complete(EncState),
}
impl EncConstructionStage {
    #[track_caller]
    fn unwrap(&mut self) -> &mut EncState {
        if let EncConstructionStage::Complete(enc) = self {
            enc
        } else {
            panic!("unwrap on non-complete EncConstructionStage")
        }
    }

    fn is_complete(&self) -> bool {
        matches!(self, EncConstructionStage::Complete(_))
    }
}

enum HistoryState {
    RecordingHistory(Duration, VecDeque<Packet>), // --history specified, but SIGUSR1 not received yet. State is (duration of history, history)
    Recording(i64), // --history not specified OR (--history specified and SIGUSR1 has been sent). Data is the PTS offset (in nanoseconds), which is required when using history. If a stream is not present, then assume 0 offset
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for State {
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

impl Dispatch<ZwpLinuxDmabufV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpLinuxDmabufV1,
        _event: <ZwpLinuxDmabufV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &wayland_client::QueueHandle<Self>,
    ) {
    }
}

fn dmabuf_to_av(dmabuf: drm_fourcc::DrmFourcc) -> Pixel {
    match dmabuf {
        drm_fourcc::DrmFourcc::Xrgb8888 => Pixel::BGRZ,
        drm_fourcc::DrmFourcc::Xrgb2101010 => Pixel::X2RGB10LE,
        f => unimplemented!("fourcc {f:?}"),
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        capture: &ZwlrScreencopyFrameV1,
        event: <ZwlrScreencopyFrameV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
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
                let pts_abs = secs * 1_000_000_000 + i64::from(tv_nsec);

                let enc = state.enc.unwrap();

                if state.starting_timestamp.is_none() {
                    state.starting_timestamp = Some(pts_abs);

                    // start audio when we get the first timestamp so it's properly sync'd
                    if let Some(audio) = &mut enc.audio {
                        audio.start();
                    }
                }
                let pts = pts_abs - state.starting_timestamp.unwrap();
                surf.set_pts(Some(pts));

                unsafe {
                    (*surf.as_mut_ptr()).time_base.num = 1;
                    (*surf.as_mut_ptr()).time_base.den = 1_000_000_000;
                }

                enc.push(surf);

                state.queue_copy(qhandle);
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => {}
            zwlr_screencopy_frame_v1::Event::LinuxDmabuf {
                format,
                width,
                height,
            } => {
                match &state.enc {
                    EncConstructionStage::None => unreachable!(
                        "Oops, somehow created a screencopy frame without initial enc state stuff?"
                    ),
                    EncConstructionStage::EverythingButFormat { output, x, y, w, h } => {
                        state.enc = EncConstructionStage::Complete(
                            match EncState::new(
                                &state.args,
                                dmabuf_to_av(
                                    drm_fourcc::DrmFourcc::try_from(format)
                                        .expect("Unknown fourcc"),
                                ),
                                output.refresh,
                                output.size_pixels,
                                (*x, *y),
                                (*w, *h),
                                Arc::clone(&state.sigusr1_flag),
                                state
                                    .dri_device
                                    .as_ref()
                                    .expect("somehow got screenrec before getting DRI device?"),
                            ) {
                                Ok(enc) => enc,
                                Err(e) => {
                                    eprintln!("failed to create encoder: {}", e);
                                    state.quit_flag.store(true, Ordering::SeqCst);
                                    return;
                                }
                            },
                        );
                    }
                    EncConstructionStage::Complete(_) => {}
                }

                let enc = state.enc.unwrap();

                let surf = enc.frames_rgb.alloc().unwrap();

                let (desc, mapping) = map_drm(&surf);

                let modifier = desc.objects[0].format_modifier.to_be_bytes();
                let stride = desc.layers[0].planes[0].pitch as u32;
                let fd = unsafe { BorrowedFd::borrow_raw(desc.objects[0].fd) };

                let dma_params = state.dma.create_params(qhandle, ());
                dma_params.add(
                    fd,
                    0,
                    0,
                    stride,
                    u32::from_be_bytes(modifier[..4].try_into().unwrap()),
                    u32::from_be_bytes(modifier[4..].try_into().unwrap()),
                );

                let buf = dma_params.create_immed(
                    width as i32,
                    height as i32,
                    format,
                    zwp_linux_buffer_params_v1::Flags::empty(),
                    qhandle,
                    (),
                );

                if state.args.damage {
                    capture.copy_with_damage(&buf);
                } else {
                    capture.copy(&buf);
                }

                state.surfaces_owned_by_compositor.push_back((
                    surf,
                    mapping,
                    dma_params,
                    capture.clone(),
                    buf,
                ));
            }
            zwlr_screencopy_frame_v1::Event::Damage { .. } => {}
            zwlr_screencopy_frame_v1::Event::Buffer { .. } => {}
            zwlr_screencopy_frame_v1::Event::Flags { .. } => {}
            zwlr_screencopy_frame_v1::Event::Failed => {
                eprintln!("Failed to screencopy!");
                state.quit_flag.store(true, Ordering::SeqCst)
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpLinuxBufferParamsV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpLinuxBufferParamsV1,
        _event: <ZwpLinuxBufferParamsV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlBuffer, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlBuffer,
        _event: <WlBuffer as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlRegistry,
        _event: <WlRegistry as Proxy>::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlRegistry, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlRegistry,
        _event: <WlRegistry as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _proxy: &WlOutput,
        event: <WlOutput as Proxy>::Event,
        data: &u32,
        _conn: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Mode {
            refresh,
            flags: WEnum::Value(flags),
            width,
            height,
        } = event
        {
            if flags.contains(Mode::Current) {
                state.update_output_info_wl_output(*data, qhandle, |info| {
                    info.refresh = Some(Rational(refresh, 1000));
                    info.size_pixels = Some((width, height));
                });
            }
        }
    }
}

impl Dispatch<ZxdgOutputManagerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZxdgOutputManagerV1,
        _event: <ZxdgOutputManagerV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZxdgOutputV1, u32> for State {
    fn event(
        state: &mut Self,
        _proxy: &ZxdgOutputV1,
        event: <ZxdgOutputV1 as Proxy>::Event,
        data: &u32,
        _conn: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        match event {
            zxdg_output_v1::Event::Name { name } => {
                state.update_output_info_wl_output(*data, qhandle, |info| info.name = Some(name));
            }
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                state.update_output_info_wl_output(*data, qhandle, |info| info.loc = Some((x, y)));
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                state.update_output_info_wl_output(*data, qhandle, |info| {
                    info.logical_size = Some((width, height))
                });
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrOutputManagerV1,
        event: <ZwlrOutputManagerV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        if let zwlr_output_manager_v1::Event::Done { .. } = event {
            state.zwlr_ouptut_info_done(qhandle);
        }
    }

    event_created_child!(State, ZwlrOutputManagerV1, [
        zwlr_output_manager_v1::EVT_HEAD_OPCODE => (ZwlrOutputHeadV1, ()),
    ]);
}

impl Dispatch<ZwlrOutputHeadV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ZwlrOutputHeadV1,
        event: <ZwlrOutputHeadV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        let id = proxy.id().protocol_id();
        match event {
            zwlr_output_head_v1::Event::Name { name } => {
                state.update_output_info_zwlr_head(id, qhandle, |data| data.0 = Some(name));
            }
            zwlr_output_head_v1::Event::Scale { scale } => {
                state.update_output_info_zwlr_head(id, qhandle, |data| data.1 = Some(scale));
            }
            _ => {}
        }
    }

    event_created_child!(State, ZwlrOutputHeadV1, [
        zwlr_output_head_v1::EVT_MODE_OPCODE => (ZwlrOutputModeV1, ()),
    ]);
}

impl Dispatch<ZwlrOutputModeV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrOutputModeV1,
        _event: <ZwlrOutputModeV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

#[link(name = "drm")]
extern "C" {
    pub fn drmGetRenderDeviceNameFromFd(fd: libc::c_int) -> *mut libc::c_char;
}

impl Dispatch<WpDrmLeaseDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &WpDrmLeaseDeviceV1,
        event: <WpDrmLeaseDeviceV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        if let wp_drm_lease_device_v1::Event::DrmFd { fd } = event {
            unsafe {
                let ptr = drmGetRenderDeviceNameFromFd(fd.as_raw_fd());
                state.dri_device = Some(if ptr.is_null() {
                    eprintln!(
                        "drmGetRenderDeviceNameFromFd returned null, guessing /dev/dri/renderD128. pass --dri-device if this is not correct or to suppress this warning"
                    );
                    "/dev/dri/renderD128".to_owned()
                } else {
                    let ret = CStr::from_ptr(ptr).to_string_lossy().to_string();
                    libc::free(ptr as *mut _);
                    ret
                });
            };
        }
    }

    event_created_child!(State, WpDrmLeaseDeviceV1, [
        wp_drm_lease_device_v1::EVT_CONNECTOR_OPCODE => (WpDrmLeaseConnectorV1, ()),
    ]);
}

impl Dispatch<WpDrmLeaseConnectorV1, ()> for State {
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

impl State {
    fn new(
        conn: &Connection,
        args: Args,
        quit_flag: Arc<AtomicBool>,
        sigusr1_flag: Arc<AtomicBool>,
    ) -> (Self, EventQueue<Self>) {
        let display = conn.display();

        let (gm, queue) = registry_queue_init(conn).unwrap();
        let eq: QueueHandle<State> = queue.handle();

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

        let registry = display.get_registry(&eq, ());

        let xdg_output_man: ZxdgOutputManagerV1 = gm
            .bind(
                &eq,
                ZxdgOutputManagerV1::interface().version..=ZxdgOutputManagerV1::interface().version,
                (),
            )
            .unwrap();

        // bind to get events so we can get the fractional scale
        let _wlr_output_man: ZwlrOutputManagerV1 = gm
            .bind(
                &eq,
                ZwlrOutputManagerV1::interface().version..=ZwlrOutputManagerV1::interface().version,
                (),
            )
            .expect("Your compositor does not seem to support the wlr-output-manager protocol. wl-screenrec requires a wlroots based compositor like sway or Hyprland");

        let dri_device = if let Some(dev) = &args.dri_device {
            Some(dev.clone())
        } else if gm
            .bind::<WpDrmLeaseDeviceV1, _, _>(
                &eq,
                WpDrmLeaseDeviceV1::interface().version..=WpDrmLeaseDeviceV1::interface().version,
                (),
            )
            .is_err()
        {
            if args.verbose >= 1 {
                eprintln!("Your compositor does not support wp_drm_lease_device_v1, so guessing that dri device is /dev/dri/renderD128. pass --dri-device if this is incorrect or to suppress this warning");
            }

            Some("/dev/dri/renderD128".to_owned())
        } else {
            None // will be filled by the callback
        };

        let mut partial_outputs = BTreeMap::new();
        for g in gm.contents().clone_list() {
            if g.interface == WlOutput::interface().name {
                let output: WlOutput =
                    registry.bind(g.name, WlOutput::interface().version, &eq, g.name);

                // query so we get the dispatch callbacks
                let _xdg = xdg_output_man.get_xdg_output(&output, &eq, g.name);

                partial_outputs.insert(
                    g.name,
                    PartialOutputInfo {
                        name: None,
                        loc: None,
                        logical_size: None,
                        size_pixels: None,
                        refresh: None,
                        output,
                    },
                );
            }
        }

        (
            State {
                surfaces_owned_by_compositor: VecDeque::new(),
                dma,
                screencopy_manager: man,
                enc: EncConstructionStage::None,
                starting_timestamp: None,
                fps_counter: FpsCounter::new(),
                args,
                wl_output: None,
                partial_outputs,
                outputs: BTreeMap::new(),
                output_fractional_scales: BTreeMap::new(),
                quit_flag,
                sigusr1_flag,
                dri_device,
            },
            queue,
        )
    }

    fn queue_copy(&mut self, eq: &QueueHandle<State>) {
        // creating this triggers the linux_dmabuf event, which is where we allocate etc
        let _capture =
            self.screencopy_manager
                .capture_output(1, self.wl_output.as_ref().unwrap(), eq, ());
    }

    fn update_output_info_wl_output(
        &mut self,
        wl_output_name: u32,
        qhandle: &QueueHandle<State>,
        f: impl FnOnce(&mut PartialOutputInfo),
    ) {
        let output = self.partial_outputs.get_mut(&wl_output_name).unwrap();
        f(output);

        // see if the associated zwlr_head has been probed yet
        if let Some(name) = &output.name {
            if let Some((_head_name, (_name, Some(scale)))) = self
                .output_fractional_scales
                .iter()
                .find(|elem| elem.1 .0.as_ref() == Some(name))
            {
                if let Some(info) = output.complete(*scale) {
                    self.outputs.insert(wl_output_name, info);
                    self.start_if_output_probe_complete(qhandle);
                }
            }
        }
    }

    fn update_output_info_zwlr_head(
        &mut self,
        zwlr_head_name: u32,
        qhandle: &QueueHandle<State>,
        f: impl FnOnce(&mut (Option<String>, Option<f64>)),
    ) {
        let output = self
            .output_fractional_scales
            .entry(zwlr_head_name)
            .or_default();
        f(output);

        if let (Some(name), Some(fractional_scale)) = output {
            if let Some((wl_output_name, partial_output)) = self
                .partial_outputs
                .iter()
                .find(|po| po.1.name.as_ref() == Some(name))
            {
                if let Some(info) = partial_output.complete(*fractional_scale) {
                    self.outputs.insert(*wl_output_name, info);
                    self.start_if_output_probe_complete(qhandle);
                }
            }
        }
    }

    fn zwlr_ouptut_info_done(&mut self, qhandle: &QueueHandle<State>) {
        let keys = self
            .output_fractional_scales
            .keys()
            .copied()
            .collect::<Vec<_>>();
        for k in keys {
            self.update_output_info_zwlr_head(k, qhandle, |(name, scale)| {
                if name.is_none() {
                    eprintln!("compositor did not report output name, strange");
                    *name = Some("<unknown>".to_owned());
                }
                if scale.is_none() {
                    eprintln!(
                        "compositor did not report scale for output {}, assuming one",
                        name.as_deref().unwrap()
                    );
                    *scale = Some(1.);
                }
            });
        }
    }

    fn start_if_output_probe_complete(&mut self, qhandle: &QueueHandle<State>) {
        assert!(!self.enc.is_complete());

        if self.outputs.len() != self.partial_outputs.len() {
            // probe not complete
            return;
        }

        let (output, (x, y), (w, h)) = match (self.args.geometry, self.args.output.as_str()) {
            (None, "") => {
                // default case, capture whole monitor
                if self.outputs.len() != 1 {
                    eprintln!("multiple displays and no --geometry or --output supplied, bailing");
                    self.quit_flag.store(true, Ordering::SeqCst);
                    return;
                }

                let output = self.outputs.iter().next().unwrap().1;
                (output, (0, 0), output.size_pixels)
            }
            (None, disp) => {
                // --output but no --geoemetry
                if let Some((_, output)) = self.outputs.iter().find(|(_, i)| i.name == disp) {
                    (output, (0, 0), output.size_pixels)
                } else {
                    eprintln!("display {} not found, bailing", disp);
                    self.quit_flag.store(true, Ordering::SeqCst);
                    return;
                }
            }
            (Some((x, y, w, h)), "") => {
                let x = x as i32;
                let y = y as i32;
                let w = w as i32;
                let h = h as i32;
                // --geometry but no --output
                if let Some((_, output)) = self.outputs.iter().find(|(_, i)| {
                    x >= i.loc.0 && x + w <= i.loc.0 + i.logical_size.0 && // x within
                        y >= i.loc.1 && y + h <= i.loc.1 + i.logical_size.1 // y within
                }) {
                    (
                        output,
                        (
                            output.logical_to_pixel(x - output.loc.0),
                            output.logical_to_pixel(y - output.loc.1),
                        ),
                        (output.logical_to_pixel(w), output.logical_to_pixel(h)),
                    )
                } else {
                    eprintln!(
                        "region {},{} {}x{} is not entirely within one output, bailing",
                        x, y, w, h
                    );
                    self.quit_flag.store(true, Ordering::SeqCst);
                    return;
                }
            }
            (Some(_), _) => {
                eprintln!(
                    "both --geometry and --output were passed, which is not allowed, bailing"
                );
                self.quit_flag.store(true, Ordering::SeqCst);
                return;
            }
        };

        eprintln!("Using output {}", output.name);

        self.wl_output = Some(output.output.clone());
        self.enc = EncConstructionStage::EverythingButFormat {
            output: output.clone(),
            x,
            y,
            w,
            h,
        };
        self.queue_copy(qhandle);
    }
}

struct EncState {
    video_filter: filter::Graph,
    enc_video: encoder::Video,
    octx: format::context::Output,
    frames_rgb: AvHwFrameCtx,
    filter_output_timebase: Rational,
    vid_stream_idx: usize,
    verbose: u8,
    history_state: HistoryState,
    sigusr1_flag: Arc<AtomicBool>,
    audio: Option<AudioHandle>,
}

#[derive(Copy, Clone)]
enum EncodePixelFormat {
    Vaapi(Pixel),
    Sw(Pixel),
}

fn vaapi_codec_id(codec: codec::Id) -> Option<&'static str> {
    match codec {
        codec::Id::H264 => Some("h264_vaapi"),
        codec::Id::H265 | codec::Id::HEVC => Some("hevc_vaapi"),
        codec::Id::VP8 => Some("vp8_vaapi"),
        codec::Id::VP9 => Some("vp9_vaapi"),
        _ => None,
    }
}

fn make_video_params(
    args: &Args,
    enc_pix_fmt: EncodePixelFormat,
    codec: &ffmpeg::Codec,
    (encode_w, encode_h): (i32, i32),
    framerate: Rational,
    global_header: bool,
    hw_device_ctx: &mut AvHwDevCtx,
    frames_yuv: &mut AvHwFrameCtx,
) -> anyhow::Result<encoder::video::Video> {
    let mut enc =
        unsafe { codec::context::Context::wrap(avcodec_alloc_context3(codec.as_ptr()), None) }
            .encoder()
            .video()
            .unwrap();

    enc.set_bit_rate(args.bitrate.into::<Byte>().value() as usize * 8);
    enc.set_width(encode_w as u32);
    enc.set_height(encode_h as u32);
    enc.set_time_base(Rational(1, 1_000_000_000));
    enc.set_frame_rate(Some(framerate));

    if global_header {
        enc.set_flags(codec::Flags::GLOBAL_HEADER);
    }

    enc.set_format(match enc_pix_fmt {
        EncodePixelFormat::Vaapi(_) => Pixel::VAAPI,
        EncodePixelFormat::Sw(sw) => sw,
    });

    if let EncodePixelFormat::Vaapi(sw_pix_fmt) = enc_pix_fmt {
        unsafe {
            (*enc.as_mut_ptr()).hw_device_ctx = av_buffer_ref(hw_device_ctx.as_mut_ptr());
            (*enc.as_mut_ptr()).hw_frames_ctx = av_buffer_ref(frames_yuv.as_mut_ptr());
            (*enc.as_mut_ptr()).sw_pix_fmt = sw_pix_fmt.into();
        }
    }

    Ok(enc)
}

impl EncState {
    // assumed that capture_{w,h}
    fn new(
        args: &Args,
        capture_pixfmt: Pixel,
        refresh: Rational,
        (capture_w, capture_h): (i32, i32), // pixels
        (encode_x, encode_y): (i32, i32),
        (encode_w, encode_h): (i32, i32),
        sigusr1_flag: Arc<AtomicBool>,
        dri_device: &str,
    ) -> anyhow::Result<Self> {
        let mut octx = ffmpeg_next::format::output(&args.filename).unwrap();

        let codec = if let Some(encoder) = &args.ffmpeg_encoder {
            ffmpeg_next::encoder::find_by_name(encoder).ok_or_else(|| {
                format_err!(
                    "Encoder {encoder} specified with --ffmpeg-encoder could not be instntiated"
                )
            })?
        } else {
            let codec_id = match args.codec {
                Codec::Auto => octx.format().codec(&args.filename, media::Type::Video),
                Codec::Avc => codec::Id::H264,
                Codec::Hevc => codec::Id::HEVC,
                Codec::VP8 => codec::Id::VP8,
                Codec::VP9 => codec::Id::VP9,
            };

            let maybe_hw_codec = if args.hw {
                if let Some(hw_codec_name) = vaapi_codec_id(codec_id) {
                    if let Some(codec) = ffmpeg_next::encoder::find_by_name(hw_codec_name) {
                        Some(codec)
                    } else {
                        eprintln!("there is a known vaapi codec ({hw_codec_name}) for codec {codec_id:?}, but it's not available. Using a generic encoder...");
                        None
                    }
                } else {
                    eprintln!("hw flag is specified, but there's no known vaapi codec for {codec_id:?}. Using a generic encoder...");
                    None
                }
            } else {
                None
            };

            match maybe_hw_codec {
                Some(codec) => codec,
                None => match ffmpeg_next::encoder::find(codec_id) {
                    Some(codec) => codec,
                    None => {
                        bail!("Failed to get any encoder for codec {codec_id:?}");
                    }
                },
            }
        };

        let codec_id = codec.id();

        let supported_formats = supported_formats(&codec);
        if supported_formats.is_empty() {
            bail!(
                "Encoder {} does not support any pixel formats?",
                codec.name()
            );
        }

        let enc_pixfmt = if supported_formats.contains(&Pixel::VAAPI) {
            EncodePixelFormat::Vaapi(args.encode_pixfmt.unwrap_or(Pixel::NV12))
        } else {
            match args.encode_pixfmt {
                None => EncodePixelFormat::Sw(supported_formats[0]),
                Some(fmt) if supported_formats.contains(&fmt) => EncodePixelFormat::Sw(fmt),
                Some(fmt) => bail!("Encoder does not support pixel format {fmt:?}"),
            }
        };

        if unsafe {
            avformat_query_codec(
                octx.format().as_ptr(),
                codec_id.into(),
                FF_COMPLIANCE_STRICT,
            )
        } != 1
        {
            bail!(
                "Format {} does not support {:?} codec",
                octx.format().name(),
                codec_id
            );
        }

        let global_header = octx.format().flags().contains(format::Flags::GLOBAL_HEADER);

        eprintln!("Opening libva device from DRM device {dri_device}");

        let mut hw_device_ctx = AvHwDevCtx::new_libva(dri_device);
        let mut frames_rgb = hw_device_ctx
            .create_frame_ctx(capture_pixfmt, capture_w, capture_h)
            .unwrap();

        let (video_filter, filter_timebase) = video_filter(
            &mut frames_rgb,
            enc_pixfmt,
            (capture_w, capture_h),
            (encode_x, encode_y),
            (encode_w, encode_h),
        );

        let mut frames_yuv = hw_device_ctx
            .create_frame_ctx(
                match enc_pixfmt {
                    EncodePixelFormat::Vaapi(fmt) => fmt,
                    EncodePixelFormat::Sw(fmt) => fmt,
                },
                encode_w,
                encode_h,
            )
            .unwrap();

        if args.verbose >= 1 {
            eprintln!("{}", video_filter.dump());
        }

        let enc = make_video_params(
            args,
            enc_pixfmt,
            &codec,
            (encode_w, encode_h),
            refresh,
            global_header,
            &mut hw_device_ctx,
            &mut frames_yuv,
        )?;

        let enc_video = if args.hw {
            let low_power_opts = dict! {
                "low_power" => "1"
            };

            let regular_opts = if codec_id == codec::Id::H264 {
                dict! {
                    "level" => "30"
                }
            } else {
                dict! {}
            };

            match args.low_power {
                LowPowerMode::Auto => match enc.open_with(low_power_opts) {
                    Ok(enc) => enc,
                    Err(e) => {
                        eprintln!("failed to open encoder in low_power mode ({}), trying non low_power mode. if you have an intel iGPU, set enable_guc=2 in the i915 module to use the fixed function encoder. pass --low-power=off to suppress this warning", e);
                        make_video_params(
                            args,
                            enc_pixfmt,
                            &codec,
                            (encode_w, encode_h),
                            refresh,
                            global_header,
                            &mut hw_device_ctx,
                            &mut frames_yuv,
                        )?
                        .open_with(regular_opts)?
                    }
                },
                LowPowerMode::On => enc.open_with(low_power_opts)?,
                LowPowerMode::Off => enc.open_with(regular_opts)?,
            }
        } else {
            enc.open_with(dict! {
                "preset" => "ultrafast"
            })
            .unwrap()
        };

        let mut ost_video = octx.add_stream(codec).unwrap();

        let vid_stream_idx = ost_video.index();
        ost_video.set_parameters(&enc_video);

        let incomplete_audio_state = if args.audio {
            Some(AudioHandle::create_stream(args, &mut octx)?)
        } else {
            None
        };

        octx.write_header().unwrap();
        let audio = incomplete_audio_state.map(|ias| ias.finish(args, &octx));

        if args.verbose >= 1 {
            ffmpeg_next::format::context::output::dump(&octx, 0, Some(&args.filename));
        }

        let history_state = match args.history {
            Some(history) => HistoryState::RecordingHistory(history, VecDeque::new()),
            None => HistoryState::Recording(0), // recording since the beginnging, no PTS offset
        };

        Ok(EncState {
            video_filter,
            enc_video,
            filter_output_timebase: filter_timebase,
            octx,
            vid_stream_idx,
            frames_rgb,
            verbose: args.verbose,
            history_state,
            sigusr1_flag,
            audio,
        })
    }

    fn process_ready(&mut self) {
        // if we were recording history and got the SIGUSR1 flag
        if let (HistoryState::RecordingHistory(_, hist), true) = (
            &mut self.history_state,
            self.sigusr1_flag.load(Ordering::SeqCst),
        ) {
            // write history to container

            // find minumum PTS offset of all streams to make sure
            // that there are no negative PTS values
            let pts_offset_ns = self
                .octx
                .streams()
                .filter_map(|st| hist.iter().find(|p| p.stream() == st.index()))
                .map(|packet| {
                    let tb = self.octx.stream(packet.stream()).unwrap().time_base();
                    packet.pts().unwrap() * 1_000_000_000 * tb.0 as i64 / tb.1 as i64
                })
                .min()
                .unwrap_or(0);

            eprintln!("SIGUSR1 received, flushing history");
            if self.verbose >= 1 {
                eprintln!("pts offset is {:?}ns", pts_offset_ns);
            }

            // grab this before we set history_state
            let mut hist_moved = VecDeque::new();
            swap(hist, &mut hist_moved);

            // transition history state
            self.history_state = HistoryState::Recording(pts_offset_ns);

            for packet in hist_moved.drain(..) {
                self.on_encoded_packet(packet);
            }
        }

        let mut yuv_frame = frame::Video::empty();
        while self
            .video_filter
            .get("out")
            .unwrap()
            .sink()
            .frame(&mut yuv_frame)
            .is_ok()
        {
            // encoder has same time base as the filter, so don't do any time scaling
            self.enc_video.send_frame(&yuv_frame).unwrap();
        }

        let mut encoded = Packet::empty();
        while self.enc_video.receive_packet(&mut encoded).is_ok() {
            encoded.set_stream(self.vid_stream_idx);
            encoded.rescale_ts(
                self.filter_output_timebase,
                self.octx.stream(self.vid_stream_idx).unwrap().time_base(),
            );

            self.on_encoded_packet(encoded);
            encoded = Packet::empty();
        }

        while let Some(pack) = self.audio.as_mut().and_then(|ar| ar.try_recv().ok()) {
            self.on_encoded_packet(pack);
        }
    }

    fn on_encoded_packet(&mut self, mut encoded: Packet) {
        let stream = self.octx.stream(encoded.stream()).unwrap();

        match &mut self.history_state {
            HistoryState::Recording(pts_offset) => {
                let tb = stream.time_base();
                let pts_offset = *pts_offset * i64::from(tb.1) / i64::from(tb.0) / 1_000_000_000;

                encoded.set_pts(Some(encoded.pts().unwrap() - pts_offset));
                if self.verbose >= 3 {
                    eprintln!(
                        "writing pts={} on {:?} is_key={}",
                        encoded.pts().unwrap(),
                        self.octx
                            .stream(encoded.stream())
                            .unwrap()
                            .parameters()
                            .medium(),
                        encoded.is_key()
                    );
                }
                encoded.set_dts(encoded.dts().map(|dts| dts - pts_offset));
                encoded.write_interleaved(&mut self.octx).unwrap();
            }
            HistoryState::RecordingHistory(history_dur, history) => {
                history.push_back(encoded);

                // discard old history if necessary
                while let Some(front) = history.front() {
                    let last_in_stream = history
                        .iter()
                        .rev()
                        .find(|p| p.stream() == front.stream())
                        .unwrap()
                        .clone();

                    if let Some((key_idx, _)) = history
                        .iter()
                        .enumerate()
                        .filter(|(_, a)| a.stream() == front.stream() && a.is_key())
                        .nth(1)
                    {
                        let key_pts = history[key_idx].pts().unwrap();

                        let current_history_size_pts =
                            u64::try_from(last_in_stream.pts().unwrap() - key_pts).unwrap();
                        let current_history_size = Duration::from_nanos(
                            current_history_size_pts * stream.time_base().0 as u64 * 1_000_000_000
                                / stream.time_base().1 as u64,
                        );

                        if current_history_size > *history_dur {
                            // erase everything in that stream <= key_idx
                            let mut removed_bytes = 0;
                            let mut removed_packets = 0;

                            let mut final_idx = key_idx;
                            let mut i = 0;
                            while i < final_idx {
                                if history[i].stream() == last_in_stream.stream() {
                                    removed_bytes += history[i].size();
                                    removed_packets += 1;

                                    history.remove(i);
                                    final_idx -= 1;
                                } else {
                                    i += 1;
                                }
                            }

                            if self.verbose >= 2 {
                                eprintln!(
                                        "history is {:?} > {:?}, popping from history buffer {} bytes across {} packets on stream {:?}", 
                                        current_history_size, history_dur,
                                        removed_bytes,
                                        removed_packets,
                                        self.octx.stream(last_in_stream.stream()).unwrap().parameters().medium()
                                    );
                            }
                        } else {
                            break; // there is a second keyframe in the stream, but it isn't old enough yet
                        }
                    } else {
                        break; // no second keyframe in the stream
                    }
                }
            }
        }
    }

    fn flush_audio(&mut self) {
        if let Some(audio) = &mut self.audio {
            audio.start_flush();
        }
        while let Some(pack) = self.audio.as_mut().and_then(|a| a.recv().ok()) {
            self.on_encoded_packet(pack);
        }
    }

    fn flush(&mut self) {
        self.flush_audio();
        self.video_filter
            .get("in")
            .unwrap()
            .source()
            .flush()
            .unwrap();
        self.process_ready();
        self.enc_video.send_eof().unwrap();
        self.process_ready();
        self.octx.write_trailer().unwrap();
    }

    fn push(&mut self, surf: frame::Video) {
        self.video_filter
            .get("in")
            .unwrap()
            .source()
            .add(&surf)
            .unwrap();

        self.process_ready();
    }
}

fn video_filter(
    inctx: &mut AvHwFrameCtx,
    pix_fmt: EncodePixelFormat,
    (capture_width, capture_height): (i32, i32),
    (enc_x, enc_y): (i32, i32),
    (enc_width, enc_height): (i32, i32),
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
        p.hw_frames_ctx = inctx.as_mut_ptr();

        let sts = av_buffersrc_parameters_set(g.get("in").unwrap().as_mut_ptr(), p as *mut _);
        assert_eq!(sts, 0);

        av_free(p as *mut _ as *mut _);
    }

    g.add(&filter::find("buffersink").unwrap(), "out", "")
        .unwrap();

    let mut out = g.get("out").unwrap();

    out.set_pixel_format(match pix_fmt {
        EncodePixelFormat::Vaapi(_) => Pixel::VAAPI,
        EncodePixelFormat::Sw(sw) => sw,
    });

    let output_real_pixfmt_name = unsafe {
        from_utf8_unchecked(
            CStr::from_ptr(av_get_pix_fmt_name(
                match pix_fmt {
                    EncodePixelFormat::Vaapi(fmt) => fmt,
                    EncodePixelFormat::Sw(fmt) => fmt,
                }
                .into(),
            ))
            .to_bytes(),
        )
    };

    g.output("in", 0)
        .unwrap()
        .input("out", 0)
        .unwrap()
        .parse(&format!(
            "crop={}:{}:{}:{},scale_vaapi=format={}:w={}:h={}{}",
            enc_width,
            enc_height,
            enc_x,
            enc_y,
            output_real_pixfmt_name,
            enc_width,
            enc_height,
            if let EncodePixelFormat::Vaapi(_) = pix_fmt {
                ""
            } else {
                ", hwdownload"
            },
        ))
        .unwrap();

    g.validate().unwrap();

    (g, Rational::new(1, 1_000_000_000))
}

fn supported_formats(codec: &ffmpeg::Codec) -> Vec<Pixel> {
    unsafe {
        let mut frmts = Vec::new();
        let mut fmt_ptr = (*codec.as_ptr()).pix_fmts;
        while !fmt_ptr.is_null() && *fmt_ptr as c_int != -1
        /*AV_PIX_FMT_NONE */
        {
            frmts.push(Pixel::from(*fmt_ptr));
            fmt_ptr = fmt_ptr.add(1);
        }
        frmts
    }
}

fn main() {
    let quit_flag = Arc::new(AtomicBool::new(false));
    let sigusr1_flag = Arc::new(AtomicBool::new(false));

    signal_hook::flag::register(SIGINT, Arc::clone(&quit_flag)).unwrap();
    signal_hook::flag::register(SIGUSR1, Arc::clone(&sigusr1_flag)).unwrap();

    let args = Args::parse();

    if !args.audio && args.audio_backend != DEFAULT_AUDIO_BACKEND {
        eprintln!("Warning: --audio-backend passed without --audio, will be ignored");
    }
    if !args.audio && args.audio_device != DEFAULT_AUDIO_CAPTURE_DEVICE {
        eprintln!("Warning: --audio-device passed without --audio, will be ignored");
    }

    ffmpeg_next::init().unwrap();

    if args.verbose >= 3 {
        ffmpeg_next::log::set_level(ffmpeg::log::Level::Trace);
    }

    let conn = match Connection::connect_to_env() {
        Ok(conn) => conn,
        Err(e @ ConnectError::NoCompositor) => {
            eprintln!("WAYLAND_DISPLAY or XDG_RUNTIME_DIR environment variables are not set or are set to an invalid value: {e}");
            exit(1);
        }
        Err(e) => {
            eprintln!("{e}");
            exit(1)
        }
    };

    let (mut state, mut queue) = State::new(&conn, args, quit_flag.clone(), sigusr1_flag);

    while !quit_flag.load(Ordering::SeqCst) {
        queue.blocking_dispatch(&mut state).unwrap();
    }

    if let EncConstructionStage::Complete(enc) = &mut state.enc {
        enc.flush();
    }
}
