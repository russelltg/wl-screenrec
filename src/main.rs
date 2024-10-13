extern crate ffmpeg_next as ffmpeg;

use std::{
    collections::{HashMap, VecDeque},
    ffi::{c_int, CStr, CString},
    fmt,
    hash::Hash,
    io,
    marker::PhantomData,
    mem::{self, swap},
    num::ParseIntError,
    os::fd::BorrowedFd,
    path::Path,
    process::exit,
    ptr::null_mut,
    str::from_utf8_unchecked,
    sync::{
        atomic::{
            AtomicBool, AtomicU64, AtomicUsize,
            Ordering::{self, SeqCst},
        },
        Arc,
    },
    thread::{self, sleep},
    time::Duration,
};

use anyhow::{bail, format_err, Context};
use audio::AudioHandle;
use cap_ext_image_copy::CapExtImageCopy;
use cap_wlr_screencopy::CapWlrScreencopy;
use clap::{command, ArgAction, CommandFactory, Parser};
use drm::buffer::DrmFourcc;
use ffmpeg::{
    codec, dict, dictionary, encoder,
    ffi::{
        av_buffer_ref, av_buffersrc_parameters_alloc, av_buffersrc_parameters_set,
        av_dict_parse_string, av_free, av_get_pix_fmt_name, av_hwframe_map, avcodec_alloc_context3,
        avformat_query_codec, AVDRMFrameDescriptor, AVPixelFormat, AV_HWFRAME_MAP_WRITE,
        FF_COMPLIANCE_STRICT,
    },
    filter,
    format::{self, Pixel},
    frame::{self, video},
    media, Packet, Rational,
};
use human_size::{Byte, Megabyte, Size, SpecificSize};
use log::{debug, error, info, trace, warn};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGUSR1};
use simplelog::{ColorChoice, CombinedLogger, LevelFilter, TermLogger, TerminalMode};
use thiserror::Error;
use transform::{transpose_if_transform_transposed, Rect};
use wayland_client::{
    backend::ObjectId,
    event_created_child,
    globals::{registry_queue_init, GlobalList, GlobalListContents},
    protocol::{
        wl_buffer::WlBuffer,
        wl_output::{self, Mode, Transform, WlOutput},
        wl_registry::WlRegistry,
    },
    ConnectError, Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
};
use wayland_protocols::{
    wp::linux_dmabuf::zv1::client::{
        zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
        zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
    },
    xdg::xdg_output::zv1::client::{
        zxdg_output_manager_v1::ZxdgOutputManagerV1,
        zxdg_output_v1::{self, ZxdgOutputV1},
    },
};

mod avhw;
use avhw::{AvHwDevCtx, AvHwFrameCtx};

mod audio;
mod cap_ext_image_copy;
mod cap_wlr_screencopy;
mod fifo;
mod transform;

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
use wayland_protocols_wlr::output_management::v1::client::{
    zwlr_output_head_v1::{self, ZwlrOutputHeadV1},
    zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
    zwlr_output_mode_v1::ZwlrOutputModeV1,
};

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

    #[clap(long, short, value_parser=parse_geometry, help="geometry to capture, format x,y WxH. Compatible with the output of `slurp`. Mutually exclusive with --output", allow_hyphen_values=true)]
    geometry: Option<(i32, i32, u32, u32)>,

    #[clap(
        long,
        short,
        help = "Which output (display) to record. Mutually exclusive with --geometry. Defaults to your only display if you only have one",
        default_value = ""
    )]
    output: String,

    #[clap(long, short, default_value = "0", action=ArgAction::Count, help = "add very loud logging. can be specified multiple times")]
    verbose: u8,

    #[clap(
        long,
        help = "which dri device to use for vaapi. by default, this is obtained from the drm-lease-v1 protocol when using wlr-screencopy, and from ext-image-copy-capture-session if using ext-image-copy-capture, if present. if not present, /dev/dri/renderD128 is guessed"
    )]
    dri_device: Option<String>,

    #[clap(long, value_enum, default_value_t)]
    low_power: LowPowerMode,

    #[clap(
        long,
        value_enum,
        default_value_t,
        help = "which video codec to use. Ignored if `--ffmpeg-encoder` is supplied"
    )]
    codec: Codec,

    #[clap(
        long,
        help = "Which ffmpeg muxer to use. Guessed from output filename by default"
    )]
    ffmpeg_muxer: Option<String>,

    #[clap(
        long,
        help = "Options to pass to the muxer. Format looks like key=val,key2=val2"
    )]
    ffmpeg_muxer_options: Option<String>,

    #[clap(
        long,
        value_enum,
        help = "Use this to force a particular ffmpeg encoder. Generally, this is not necessary and the combo of --codec and --hw can get you to where you need to be"
    )]
    ffmpeg_encoder: Option<String>,

    #[clap(
        long,
        value_enum,
        help = "Options to pass to the encoder. Format looks like key=val,key2=val2"
    )]
    ffmpeg_encoder_options: Option<String>,

    #[clap(
        long,
        value_enum,
        default_value_t,
        help = "Which audio codec to use. Ignored if `--ffmpeg-audio-encoder` is supplied"
    )]
    audio_codec: AudioCodec,

    #[clap(
        long,
        value_enum,
        help = "Use this to force a particular audio ffmpeg encoder. By default, this is guessed from the muxer (which is guess by the file extension if --ffmpeg-muxer isn't passed)"
    )]
    ffmpeg_audio_encoder: Option<String>,

    #[clap(
        long,
        help = "which pixel format to encode with. not all codecs will support all pixel formats. This should be a ffmpeg pixel format string, like nv12 or x2rgb10. If the encoder supports vaapi memory, it will use this pixel format type but in vaapi memory"
    )]
    encode_pixfmt: Option<Pixel>,

    #[clap(long, value_parser=parse_size, help="what resolution to encode at. example: 1920x1080. Default is the resolution of the captured region. If your goal is reducing filesize, it's suggested to try --bitrate/-b first")]
    encode_resolution: Option<(u32, u32)>,

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

    #[clap(long = "gop-size", help = "GOP (group of pictures) size")]
    gop_size: Option<u32>,

    #[clap(
        long = "generate-completions",
        help = "print completions for the specified shell to stdout"
    )]
    completions_generator: Option<clap_complete::Shell>,

    #[clap(
        long = "experimental-ext-image-copy-capture",
        help = "use the new ext-image-copy-capture protocol",
        default_value = "false"
    )]
    ext_image_copy_capture: bool,
}

trait CaptureSource: Sized {
    type Frame: Clone;

    fn new(
        gm: &GlobalList,
        eq: &QueueHandle<State<Self>>,
        output: WlOutput,
    ) -> anyhow::Result<Self>;
    fn queue_capture_frame(
        &self,
        eq: &QueueHandle<State<Self>>,
    ) -> Option<(u32, u32, DrmFourcc, Self::Frame)>;
    fn queue_copy_frame(&self, damage: bool, buf: &WlBuffer, cap: &Self::Frame);
    fn on_done_with_frame(&self, f: Self::Frame);
}

#[derive(clap::ValueEnum, Debug, Clone, Default, PartialEq, Eq)]
enum Codec {
    #[default]
    Auto,
    Avc,
    Hevc,
    VP8,
    VP9,
    AV1,
}

#[derive(clap::ValueEnum, Debug, Default, Clone, PartialEq, Eq)]
enum AudioCodec {
    #[default]
    Auto,
    Aac,
    Mp3,
    Flac,
    Opus,
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

fn parse_geometry(s: &str) -> Result<(i32, i32, u32, u32), ParseGeometryError> {
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

    let (sizex, sizey) = parse_size(size)?;

    Ok((startx, starty, sizex, sizey))
}

fn parse_size(size: &str) -> Result<(u32, u32), ParseGeometryError> {
    use ParseGeometryError::*;
    let mut it = size.split('x');
    let sizex = it.next().ok_or(Size)?.parse()?;
    let sizey = it.next().ok_or(Size)?.parse()?;
    if it.next().is_some() {
        return Err(Size);
    }

    Ok((sizex, sizey))
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

        thread::Builder::new()
            .name("FpsCounter".to_owned())
            .spawn(move || {
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
            })
            .unwrap();

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
    has_recvd_done: bool,
    transform: Option<Transform>,
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
                transform: self.transform.unwrap_or(Transform::Normal),
            })
        } else {
            None
        }
    }
}

#[derive(Clone, Debug)]
struct OutputInfo {
    name: String,
    loc: (i32, i32),
    logical_size: (i32, i32),
    size_pixels: (i32, i32),
    refresh: Rational,
    fractional_scale: f64,
    output: WlOutput,
    transform: Transform,
}

impl OutputInfo {
    fn logical_to_pixel(&self, logical: i32) -> i32 {
        (f64::from(logical) * self.fractional_scale).round() as i32
    }

    fn size_screen_space(&self) -> (i32, i32) {
        transpose_if_transform_transposed(self.size_pixels, self.transform)
    }
}

#[derive(Default)]
struct PartialOutputInfoWlr {
    name: Option<String>,
    scale: Option<f64>,
    enabled: Option<bool>,
}

#[derive(Hash, PartialEq, Eq, Clone)]
struct TypedObjectId<T>(ObjectId, PhantomData<T>);

impl<T> TypedObjectId<T> {
    fn new(from: &impl Proxy) -> Self {
        TypedObjectId(from.id(), Default::default())
    }
}

impl<T> fmt::Debug for TypedObjectId<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
struct DrmModifier(u64);

impl DrmModifier {
    const LINEAR: DrmModifier = DrmModifier(0);
}

impl fmt::Debug for DrmModifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        unsafe {
            let vendor_p = drmGetFormatModifierVendor(self.0);
            let name_p = drmGetFormatModifierName(self.0);

            let vendor = if vendor_p.is_null() {
                None
            } else {
                CStr::from_ptr(vendor_p).to_str().ok()
            };
            let name = if name_p.is_null() {
                None
            } else {
                CStr::from_ptr(name_p).to_str().ok()
            };

            match (vendor, name) {
                (None, None) => write!(f, "0x{:08x}", self.0)?,
                (None, Some(name)) => write!(f, "0x{:08x} = UNKNOWN_{}", self.0, name)?,
                (Some(vendor), None) => write!(f, "0x{:08x} = {}_UNKNOWN", self.0, vendor)?,
                (Some(vendor), Some(name)) => write!(f, "0x{:08x} = {}_{}", self.0, vendor, name)?,
            }

            if !vendor_p.is_null() {
                libc::free(vendor_p as _);
            }
            if !name_p.is_null() {
                libc::free(name_p as _);
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone)]
struct DmabufPotentialFormat {
    fourcc: DrmFourcc,
    modifiers: Vec<DrmModifier>,
}

struct DmabufFormat {
    fourcc: DrmFourcc,
    _modifier: DrmModifier,
}

#[link(name = "drm")]
extern "C" {
    pub fn drmGetRenderDeviceNameFromFd(fd: libc::c_int) -> *mut libc::c_char;
    pub fn drmGetFormatModifierVendor(modifier: u64) -> *mut libc::c_char;
    pub fn drmGetFormatModifierName(modifier: u64) -> *mut libc::c_char;
}

struct State<S: CaptureSource> {
    pub(crate) surfaces_owned_by_compositor: VecDeque<(
        frame::Video,
        video::Video,
        ZwpLinuxBufferParamsV1,
        S::Frame,
        WlBuffer,
    )>,
    dma: ZwpLinuxDmabufV1,
    wl_output: Option<WlOutput>,
    enc: EncConstructionStage<S>,
    starting_timestamp: Option<i64>,
    fps_counter: FpsCounter,
    args: Args,
    quit_flag: Arc<AtomicUsize>,
    sigusr1_flag: Arc<AtomicBool>,
    gm: GlobalList,
}

struct OutputProbeState {
    partial_outputs: HashMap<TypedObjectId<WlOutput>, PartialOutputInfo>, // key is xdg-output name (wayland object ID)
    partial_outputs_wlr: HashMap<TypedObjectId<ZwlrOutputHeadV1>, PartialOutputInfoWlr>,
    outputs: HashMap<TypedObjectId<WlOutput>, Option<OutputInfo>>, // none for disabled
}

enum EncConstructionStage<S> {
    ProbingOutputs(OutputProbeState),
    EverythingButFormat {
        output: OutputInfo,
        roi: Rect,
        cap: S,
    },
    Complete(EncState, S),
    Intermediate,
}
impl<S> EncConstructionStage<S> {
    #[track_caller]
    fn unwrap(&mut self) -> (&mut EncState, &mut S) {
        if let EncConstructionStage::Complete(enc, s) = self {
            (enc, s)
        } else {
            panic!("unwrap on non-complete EncConstructionStage")
        }
    }

    #[track_caller]
    fn unwrap_cap(&mut self) -> &mut S {
        match self {
            EncConstructionStage::EverythingButFormat { cap, .. } => cap,
            EncConstructionStage::Complete(_, cap) => cap,
            _ => panic!("no capture source yet"),
        }
    }
}

enum HistoryState {
    RecordingHistory(Duration, VecDeque<Packet>), // --history specified, but SIGUSR1 not received yet. State is (duration of history, history)
    Recording(i64), // --history not specified OR (--history specified and SIGUSR1 has been sent). Data is the PTS offset (in nanoseconds), which is required when using history. If a stream is not present, then assume 0 offset
}

impl<S: CaptureSource> Dispatch<WlBuffer, ()> for State<S> {
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

impl<S: CaptureSource> Dispatch<WlRegistry, GlobalListContents> for State<S> {
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

impl<S: CaptureSource> Dispatch<ZwpLinuxDmabufV1, ()> for State<S> {
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

impl<S: CaptureSource> Dispatch<ZxdgOutputManagerV1, ()> for State<S> {
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

impl<S: CaptureSource + 'static> Dispatch<ZxdgOutputV1, TypedObjectId<WlOutput>> for State<S> {
    fn event(
        state: &mut Self,
        proxy: &ZxdgOutputV1,
        event: <ZxdgOutputV1 as Proxy>::Event,
        out_id: &TypedObjectId<WlOutput>,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        debug!("zxdg-output event: {:?} {event:?}", proxy.id());
        match event {
            zxdg_output_v1::Event::Name { name } => {
                state.update_output_info_wl_output(out_id, |info| info.name = Some(name));
            }
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                state.update_output_info_wl_output(out_id, |info| info.loc = Some((x, y)));
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                state.update_output_info_wl_output(out_id, |info| {
                    info.logical_size = Some((width, height))
                });
            }
            _ => {}
        }
    }
}

impl<S> Dispatch<ZwlrOutputManagerV1, ()> for State<S>
where
    S: CaptureSource + 'static,
{
    fn event(
        state: &mut Self,
        proxy: &ZwlrOutputManagerV1,
        event: <ZwlrOutputManagerV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        debug!("zwlr-output-manager event: {:?} {event:?}", proxy.id());
        if let zwlr_output_manager_v1::Event::Done { .. } = event {
            state.zwlr_ouptut_info_done(qhandle);
        }
    }

    event_created_child!(State<S>, ZwlrOutputManagerV1, [
        zwlr_output_manager_v1::EVT_HEAD_OPCODE => (ZwlrOutputHeadV1, ()),
    ]);
}

impl<S> Dispatch<ZwlrOutputHeadV1, ()> for State<S>
where
    S: CaptureSource + 'static,
{
    fn event(
        state: &mut Self,
        proxy: &ZwlrOutputHeadV1,
        event: <ZwlrOutputHeadV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        debug!("zwlr-output-head event: {:?} {event:?}", proxy.id());
        let id = TypedObjectId::new(proxy);
        match event {
            zwlr_output_head_v1::Event::Name { name } => {
                state.update_output_info_zwlr_head(id, |data| data.name = Some(name));
            }
            zwlr_output_head_v1::Event::Scale { scale } => {
                state.update_output_info_zwlr_head(id, |data| data.scale = Some(scale));
            }
            zwlr_output_head_v1::Event::Enabled { enabled } => {
                state.update_output_info_zwlr_head(id, |data| data.enabled = Some(enabled != 0));
            }
            _ => {}
        }
    }

    event_created_child!(State<S>, ZwlrOutputHeadV1, [
        zwlr_output_head_v1::EVT_MODE_OPCODE => (ZwlrOutputModeV1, ()),
    ]);
}

impl<S: CaptureSource> Dispatch<ZwlrOutputModeV1, ()> for State<S> {
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

impl<S: CaptureSource + 'static> Dispatch<WlOutput, ()> for State<S> {
    fn event(
        state: &mut Self,
        proxy: &WlOutput,
        event: <WlOutput as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        debug!("wl-output event: {:?} {event:?}", proxy.id());
        let id = TypedObjectId::new(proxy);
        match event {
            wl_output::Event::Mode {
                refresh,
                flags: WEnum::Value(flags),
                width,
                height,
            } => {
                if flags.contains(Mode::Current) {
                    state.update_output_info_wl_output(&id, |info| {
                        info.refresh = Some(Rational(refresh, 1000));
                        info.size_pixels = Some((width, height));
                    });
                }
            }
            wl_output::Event::Geometry { transform, .. } => match transform {
                WEnum::Value(v) => {
                    state.update_output_info_wl_output(&id, |info| info.transform = Some(v))
                }
                WEnum::Unknown(u) => {
                    eprintln!("Unknown output transform value: {u}")
                }
            },
            wl_output::Event::Done => {
                state.done_output_info_wl_output(id, qhandle);
            }
            _ => (),
        }
    }
}

impl<S: CaptureSource> Dispatch<ZwpLinuxBufferParamsV1, ()> for State<S> {
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

impl<S: CaptureSource> Dispatch<WlRegistry, ()> for State<S> {
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

fn dmabuf_to_av(dmabuf: DrmFourcc) -> Pixel {
    match dmabuf {
        DrmFourcc::Xrgb8888 => Pixel::BGRZ,
        DrmFourcc::Xrgb2101010 => Pixel::X2RGB10LE,
        f => unimplemented!("fourcc {f:?}"),
    }
}

impl<S: CaptureSource + 'static> State<S> {
    fn new(
        conn: &Connection,
        args: Args,
        quit_flag: Arc<AtomicUsize>,
        sigusr1_flag: Arc<AtomicBool>,
    ) -> anyhow::Result<(Self, EventQueue<Self>)> {
        let display = conn.display();

        let (gm, queue) = registry_queue_init(conn).unwrap();
        let eq: QueueHandle<State<S>> = queue.handle();

        let dma: ZwpLinuxDmabufV1 = gm
            .bind(&eq, 4..=ZwpLinuxDmabufV1::interface().version, ())
            .context("your compositor does not support zwp-linux-dmabuf and therefore is not support by wl-screenrec. See the README for supported compositors")?;

        let registry = display.get_registry(&eq, ());

        let xdg_output_man: ZxdgOutputManagerV1 = gm
            .bind(&eq, 3..=ZxdgOutputManagerV1::interface().version, ())
            .context("your compositor does not support zxdg-output-manager and therefore is not support by wl-screenrec. See the README for supported compositors")?;

        // bind to get events so we can get the fractional scale
        let _wlr_output_man: ZwlrOutputManagerV1 = gm
            .bind(
                &eq,
                1..=ZwlrOutputManagerV1::interface().version,
                (),
            )
            .context("your compositor does not support zwlr-output-manager and therefore is not support by wl-screenrec. See the README for supported compositors")?;

        let mut partial_outputs = HashMap::new();
        for g in gm.contents().clone_list() {
            if g.interface == WlOutput::interface().name {
                let output: WlOutput =
                    registry.bind(g.name, WlOutput::interface().version, &eq, ());

                // query so we get the dispatch callbacks
                let _xdg = xdg_output_man.get_xdg_output(&output, &eq, TypedObjectId::new(&output));

                partial_outputs.insert(
                    TypedObjectId::new(&output),
                    PartialOutputInfo {
                        name: None,
                        loc: None,
                        logical_size: None,
                        size_pixels: None,
                        refresh: None,
                        output,
                        has_recvd_done: false,
                        transform: None,
                    },
                );
            }
        }

        Ok((
            State {
                surfaces_owned_by_compositor: VecDeque::new(),
                dma,
                enc: EncConstructionStage::ProbingOutputs(OutputProbeState {
                    partial_outputs,
                    partial_outputs_wlr: HashMap::new(),
                    outputs: HashMap::new(),
                }),
                starting_timestamp: None,
                fps_counter: FpsCounter::new(),
                args,
                wl_output: None,
                quit_flag,
                sigusr1_flag,
                gm,
            },
            queue,
        ))
    }

    fn on_copy_src_ready(
        &mut self,
        dmabuf_width: u32,
        dmabuf_height: u32,
        format: DrmFourcc,
        qhandle: &QueueHandle<State<S>>,
        frame: &S::Frame,
    ) {
        match mem::replace(&mut self.enc, EncConstructionStage::Intermediate) {
            EncConstructionStage::ProbingOutputs { .. } => unreachable!(
                "Oops, somehow created a screencopy frame without initial enc state stuff?"
            ),
            EncConstructionStage::EverythingButFormat { .. } => {
                panic!("you need to call negotiate_format before on_copy_src_ready")
            }
            EncConstructionStage::Complete(a, b) => {
                self.enc = EncConstructionStage::Complete(a, b) // put it back
            }
            EncConstructionStage::Intermediate => panic!("enc left in intermediate state"),
        }

        let (enc, cap) = self.enc.unwrap();

        let surf = enc.frames_rgb.alloc().unwrap();

        let (desc, mapping) = map_drm(&surf);

        let modifier = desc.objects[0].format_modifier.to_be_bytes();
        let stride = desc.layers[0].planes[0].pitch as u32;
        let fd = unsafe { BorrowedFd::borrow_raw(desc.objects[0].fd) };

        let dma_params = self.dma.create_params(qhandle, ());
        dma_params.add(
            fd,
            0,
            0,
            stride,
            u32::from_be_bytes(modifier[..4].try_into().unwrap()),
            u32::from_be_bytes(modifier[4..].try_into().unwrap()),
        );

        let buf = dma_params.create_immed(
            dmabuf_width as i32,
            dmabuf_height as i32,
            format as u32,
            zwp_linux_buffer_params_v1::Flags::empty(),
            qhandle,
            (),
        );

        cap.queue_copy_frame(self.args.damage, &buf, frame);

        self.surfaces_owned_by_compositor.push_back((
            surf,
            mapping,
            dma_params,
            frame.clone(),
            buf,
        ));
    }

    fn update_output_info_wl_output(
        &mut self,
        id: &TypedObjectId<WlOutput>,
        f: impl FnOnce(&mut PartialOutputInfo),
    ) {
        if let EncConstructionStage::ProbingOutputs(p) = &mut self.enc {
            let output = p.partial_outputs.get_mut(id).unwrap();
            f(output);
        }
    }

    fn done_output_info_wl_output(
        &mut self,
        id: TypedObjectId<WlOutput>,
        qhandle: &QueueHandle<Self>,
    ) {
        let p = if let EncConstructionStage::ProbingOutputs(p) = &mut self.enc {
            p
        } else {
            // got this event because of some dispaly changes, ignore...
            return;
        };

        let output = p.partial_outputs.get_mut(&id).unwrap();

        // for each output, we will get 2 done events
        // * when we create the WlOutput the first time
        // * then again when we probe the xdg output
        // we only care about the second one, as we want the xdg output info
        if !output.has_recvd_done {
            output.has_recvd_done = true;
            return;
        }

        let name = match &output.name {
            Some(name) => name,
            None => {
                warn!(
                    "compositor did not provide name for wl_output {}, strange",
                    id.0.protocol_id()
                );
                "<unknown>"
            }
        };

        // see if the associated zwlr_head has been probed yet
        if let Some((
            _head_name,
            PartialOutputInfoWlr {
                scale: Some(scale),
                enabled: Some(enabled),
                ..
            },
        )) = p
            .partial_outputs_wlr
            .iter()
            .find(|elem| elem.1.name.as_deref() == Some(name))
        {
            if let Some(info) = output.complete(*scale) {
                if *enabled {
                    p.outputs.insert(id, Some(info));
                } else {
                    p.outputs.insert(id, None);
                }
            }
        }

        self.start_if_output_probe_complete(qhandle);
    }

    fn update_output_info_zwlr_head(
        &mut self,
        id: TypedObjectId<ZwlrOutputHeadV1>,
        f: impl FnOnce(&mut PartialOutputInfoWlr),
    ) {
        if let EncConstructionStage::ProbingOutputs(p) = &mut self.enc {
            let output = p.partial_outputs_wlr.entry(id).or_default();
            f(output);
        }
    }

    fn zwlr_ouptut_info_done(&mut self, qhandle: &QueueHandle<Self>) {
        let p = if let EncConstructionStage::ProbingOutputs(p) = &mut self.enc {
            p
        } else {
            return;
        };

        for wlr_info in p.partial_outputs_wlr.values() {
            let enabled = match wlr_info.enabled {
                None => {
                    warn!(
                        "compositor did not report if output {} is enabled, strange",
                        wlr_info.name.as_deref().unwrap_or("<unknown>")
                    );
                    true
                }
                Some(enabled) => enabled,
            };

            let name = match &wlr_info.name {
                Some(name) => name,
                None => {
                    warn!("compositor did not report output name, strange");
                    "<unknown>"
                }
            };

            if let Some((wl_output_name, partial_output)) = p
                .partial_outputs
                .iter()
                .find(|po| po.1.name.as_deref() == Some(name))
            {
                if let Some(info) = partial_output.complete(wlr_info.scale.unwrap_or(1.)) {
                    info!("output probe for {name} is complete");
                    if enabled {
                        if wlr_info.scale.is_none() {
                            warn!("compositor did not report fractional scale for enabled output {name}");
                        }
                        p.outputs.insert(wl_output_name.clone(), Some(info));
                    } else {
                        p.outputs.insert(wl_output_name.clone(), None);
                    }
                } else {
                    debug!("output probe still incomplete for {name}: {partial_output:?}");
                }
            }
        }

        self.start_if_output_probe_complete(qhandle);
    }

    fn start_if_output_probe_complete(&mut self, qhandle: &QueueHandle<Self>) {
        let p = if let EncConstructionStage::ProbingOutputs(p) = &self.enc {
            p
        } else {
            panic!("bad precondition: is still constructing");
        };

        if p.outputs.len() != p.partial_outputs.len() {
            // probe not complete
            if self.args.verbose >= 2 {
                println!(
                    "output probe not yet complete, still waiting for {}",
                    p.partial_outputs
                        .iter()
                        .filter(|(id, _)| !p.outputs.contains_key(id))
                        .map(|(id, po)| format!("({id:?}, {:?})", po))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            return;
        }

        info!("output probe complete: {:?}", p.outputs);

        let enabled_outputs: Vec<_> = p.outputs.iter().flat_map(|(_, o)| o).collect();

        let (output, roi) = match (self.args.geometry, self.args.output.as_str()) {
            (None, "") => {
                // default case, capture whole monitor
                if enabled_outputs.len() != 1 {
                    eprintln!(
                        "multiple enabled displays and no --geometry or --output supplied, bailing"
                    );
                    self.quit_flag.store(1, Ordering::SeqCst);
                    return;
                }

                let output = enabled_outputs[0];
                (output, Rect::new((0, 0), output.size_screen_space()))
            }
            (None, disp) => {
                // --output but no --geometry
                if let Some(&output) = enabled_outputs.iter().find(|i| i.name == disp) {
                    (output, Rect::new((0, 0), output.size_screen_space()))
                } else {
                    eprintln!("display {} not found, bailing", disp);
                    self.quit_flag.store(1, Ordering::SeqCst);
                    return;
                }
            }
            (Some((x, y, w, h)), "") => {
                let w = w as i32;
                let h = h as i32;
                // --geometry but no --output
                if let Some(&output) = enabled_outputs.iter().find(|i| {
                    x >= i.loc.0 && x + w <= i.loc.0 + i.logical_size.0 && // x within
                        y >= i.loc.1 && y + h <= i.loc.1 + i.logical_size.1 // y within
                }) {
                    (
                        output,
                        Rect::new(
                            (
                                output.logical_to_pixel(x - output.loc.0),
                                output.logical_to_pixel(y - output.loc.1),
                            ),
                            (output.logical_to_pixel(w), output.logical_to_pixel(h)),
                        ),
                    )
                } else {
                    eprintln!(
                        "region {},{} {}x{} is not entirely within one output, bailing",
                        x, y, w, h
                    );
                    self.quit_flag.store(1, Ordering::SeqCst);
                    return;
                }
            }
            (Some(_), _) => {
                eprintln!(
                    "both --geometry and --output were passed, which is not allowed, bailing"
                );
                self.quit_flag.store(1, Ordering::SeqCst);
                return;
            }
        };

        info!("Using output {}", output.name);

        self.wl_output = Some(output.output.clone());

        let cap = match S::new(&self.gm, qhandle, output.output.clone()) {
            Ok(cap) => cap,
            Err(err) => {
                eprintln!("failed to create capture state: {}", err);
                self.quit_flag.store(1, SeqCst);
                return;
            }
        };

        let queue_ret = cap.queue_capture_frame(qhandle);
        self.enc = EncConstructionStage::EverythingButFormat {
            output: output.clone(),
            roi,
            cap,
        };

        if let Some((w, h, fmt, frame)) = queue_ret {
            self.on_copy_src_ready(w, h, fmt, qhandle, &frame);
        }
    }

    fn on_copy_complete(
        &mut self,
        qhandle: &QueueHandle<Self>,
        tv_sec_hi: u32,
        tv_sec_lo: u32,
        tv_nsec: u32,
    ) {
        let (enc, cap) = self.enc.unwrap();

        self.fps_counter.on_frame();

        let (mut surf, drop_mapping, destroy_buffer_params, destroy_frame, destroy_buffer) =
            self.surfaces_owned_by_compositor.pop_front().unwrap();

        drop(drop_mapping);
        destroy_buffer_params.destroy();
        cap.on_done_with_frame(destroy_frame);
        destroy_buffer.destroy();

        let secs = (i64::from(tv_sec_hi) << 32) + i64::from(tv_sec_lo);
        let pts_abs = secs * 1_000_000_000 + i64::from(tv_nsec);

        if self.starting_timestamp.is_none() {
            self.starting_timestamp = Some(pts_abs);

            // start audio when we get the first timestamp so it's properly sync'd
            if let Some(audio) = &mut enc.audio {
                audio.start();
            }
        }
        let pts = pts_abs - self.starting_timestamp.unwrap();
        surf.set_pts(Some(pts));

        unsafe {
            (*surf.as_mut_ptr()).time_base.num = 1;
            (*surf.as_mut_ptr()).time_base.den = 1_000_000_000;
        }

        enc.push(surf);

        if let Some((w, h, fmt, frame)) = cap.queue_capture_frame(qhandle) {
            self.on_copy_src_ready(w, h, fmt, qhandle, &frame);
        }
    }

    fn negotiate_format(
        &mut self,
        capture_formats: &[DmabufPotentialFormat],
        (w, h): (u32, u32),
        dri_device: Option<&Path>,
    ) -> Option<DmabufFormat> {
        debug!("Supported capture formats are {capture_formats:?}");
        let dri_device = if let Some(dev) = &self.args.dri_device {
            Path::new(dev)
        } else if let Some(dev) = dri_device {
            dev
        } else {
            warn!("dri device could not be auto-detected, using /dev/dri/renderD128. Pass --dri-device if this isn't correct or to suppress this warning");
            Path::new("/dev/dri/renderD128")
        };

        match mem::replace(&mut self.enc, EncConstructionStage::Intermediate) {
            EncConstructionStage::EverythingButFormat { output, roi, cap } => {
                let (enc, fmt) = match EncState::new(
                    &self.args,
                    capture_formats,
                    output.refresh,
                    output.transform,
                    (w as i32, h as i32),
                    roi,
                    Arc::clone(&self.sigusr1_flag),
                    dri_device,
                ) {
                    Ok(enc) => enc,
                    Err(e) => {
                        eprintln!("failed to create encoder(s): {}", e);
                        self.quit_flag.store(1, SeqCst);
                        return None;
                    }
                };

                self.enc = EncConstructionStage::Complete(enc, cap);

                Some(fmt)
            }
            _ => panic!("called negotiate_format in a strange state"),
        }
    }
}

struct EncState {
    video_filter: filter::Graph,
    enc_video: encoder::Video,
    octx: format::context::Output,
    frames_rgb: AvHwFrameCtx,
    filter_output_timebase: Rational,
    vid_stream_idx: usize,
    history_state: HistoryState,
    sigusr1_flag: Arc<AtomicBool>,
    audio: Option<AudioHandle>,
}

#[derive(Copy, Clone, Debug)]
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
        codec::Id::AV1 => Some("av1_vaapi"),
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
    if let Some(gop) = args.gop_size {
        enc.set_gop(gop);
    }

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

fn parse_dict(dict: &str) -> Result<dictionary::Owned, ffmpeg::Error> {
    let cstr = CString::new(dict).unwrap();

    let mut ptr = null_mut();
    unsafe {
        let res = av_dict_parse_string(
            &mut ptr,
            cstr.as_ptr(),
            b"=:\0".as_ptr().cast(),
            b",\0".as_ptr().cast(),
            0,
        );
        if res != 0 {
            return Err(ffmpeg::Error::from(res));
        }

        Ok(dictionary::Owned::own(ptr))
    }
}

impl EncState {
    // assumed that capture_{w,h}
    fn new(
        args: &Args,
        capture_formats: &[DmabufPotentialFormat],
        refresh: Rational,
        transform: Transform,
        (capture_w, capture_h): (i32, i32), // pixels
        roi_screen_coord: Rect, // roi in screen coordinates (0, 0 is screen upper left, which is not necessarily captured frame upper left)
        sigusr1_flag: Arc<AtomicBool>,
        dri_device: &Path,
    ) -> anyhow::Result<(Self, DmabufFormat)> {
        let muxer_options = if let Some(muxer_options) = &args.ffmpeg_muxer_options {
            parse_dict(muxer_options).unwrap()
        } else {
            dict!()
        };

        let mut octx = if let Some(muxer) = &args.ffmpeg_muxer {
            ffmpeg_next::format::output_as_with(&args.filename, muxer, muxer_options).unwrap()
        } else {
            ffmpeg_next::format::output_with(&args.filename, muxer_options).unwrap()
        };

        let encoder = if let Some(encoder_name) = &args.ffmpeg_encoder {
            ffmpeg_next::encoder::find_by_name(encoder_name).ok_or_else(|| {
                format_err!(
                    "Encoder {encoder_name} specified with --ffmpeg-encoder could not be instntiated"
                )
            })?
        } else {
            let codec_id = match args.codec {
                Codec::Auto => octx.format().codec(&args.filename, media::Type::Video),
                Codec::Avc => codec::Id::H264,
                Codec::Hevc => codec::Id::HEVC,
                Codec::VP8 => codec::Id::VP8,
                Codec::VP9 => codec::Id::VP9,
                Codec::AV1 => codec::Id::AV1,
            };

            let maybe_hw_codec = if args.hw {
                if let Some(hw_codec_name) = vaapi_codec_id(codec_id) {
                    if let Some(codec) = ffmpeg_next::encoder::find_by_name(hw_codec_name) {
                        Some(codec)
                    } else {
                        warn!("there is a known vaapi codec ({hw_codec_name}) for codec {codec_id:?}, but it's not available. Using a generic encoder...");
                        None
                    }
                } else {
                    warn!("hw flag is specified, but there's no known vaapi codec for {codec_id:?}. Using a generic encoder...");
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

        // format selection: naive version, should actually see what the ffmpeg filter supports...
        let mut selected_format = None;
        for preferred_format in [
            DrmFourcc::Xrgb8888,
            DrmFourcc::Xbgr8888,
            DrmFourcc::Xrgb2101010,
        ] {
            let is_fmt_supported = capture_formats
                .iter()
                .find(|p| {
                    p.fourcc == DrmFourcc::Xrgb8888
                        && p.modifiers
                            .iter()
                            .find(|m| **m == DrmModifier::LINEAR)
                            .is_some()
                })
                .is_some();

            if is_fmt_supported {
                selected_format = Some(DmabufFormat {
                    fourcc: preferred_format,
                    _modifier: DrmModifier::LINEAR,
                });
                break;
            }
        }
        let selected_format = match selected_format {
            Some(sf) => sf,
            None =>
                bail!("failed to select a viable capture format. This is probably a bug. Availabe capture formats are {:?}", capture_formats),
        };
        let capture_pixfmt = dmabuf_to_av(selected_format.fourcc);
        info!("capture pixel format is {}", selected_format.fourcc);

        let supported_formats = supported_formats(&encoder);
        let enc_pixfmt = if supported_formats.is_empty() {
            match args.encode_pixfmt {
                Some(fmt) => EncodePixelFormat::Sw(fmt),
                None => {
                    warn!(
                        "codec \"{}\" does not advertize supported pixel formats, just using NV12. Pass --encode-pixfmt to suppress this warning",
                        encoder.name()
                    );
                    EncodePixelFormat::Sw(Pixel::NV12)
                }
            }
        } else if supported_formats.contains(&Pixel::VAAPI) {
            EncodePixelFormat::Vaapi(args.encode_pixfmt.unwrap_or(Pixel::NV12))
        } else {
            match args.encode_pixfmt {
                None => EncodePixelFormat::Sw(supported_formats[0]),
                Some(fmt) if supported_formats.contains(&fmt) => EncodePixelFormat::Sw(fmt),
                Some(fmt) => bail!("Encoder does not support pixel format {fmt:?}"),
            }
        };
        info!("encode pixel format is {enc_pixfmt:?}");

        let codec_id = encoder.id();
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

        eprintln!(
            "Opening libva device from DRM device {}",
            dri_device.display()
        );

        let mut hw_device_ctx = match AvHwDevCtx::new_libva(dri_device) {
            Ok(hdc) => hdc,
            Err(e) => bail!("Failed to load vaapi device: {e}\nThis is likely *not* a bug in wl-screenrec, but an issue with your vaapi installation. Follow your distribution's instructions. If you're pretty sure you've done this correctly, create a new issue with the output of `vainfo` and if `wf-recorder -c h264_vaapi -d /dev/dri/card0` works."),
        };

        let mut frames_rgb = hw_device_ctx
            .create_frame_ctx(capture_pixfmt, capture_w, capture_h)
            .with_context(|| format!("Failed to create vaapi frame context for capture surfaces of format {capture_pixfmt:?} {capture_w}x{capture_h}"))?;

        let (enc_w_screen_coord, enc_h_screen_coord) = match args.encode_resolution {
            Some((x, y)) => (x as i32, y as i32),
            None => (roi_screen_coord.w, roi_screen_coord.h),
        };

        let (video_filter, filter_timebase) = video_filter(
            &mut frames_rgb,
            enc_pixfmt,
            (capture_w, capture_h),
            roi_screen_coord,
            (enc_w_screen_coord, enc_h_screen_coord),
            transform,
        );

        let enc_pixfmt_av = match enc_pixfmt {
            EncodePixelFormat::Vaapi(fmt) => fmt,
            EncodePixelFormat::Sw(fmt) => fmt,
        };
        let mut frames_yuv = hw_device_ctx
            .create_frame_ctx(enc_pixfmt_av, enc_w_screen_coord, enc_h_screen_coord)
            .with_context(|| {
                format!("Failed to create a vaapi frame context for encode surfaces of format {enc_pixfmt_av:?} {capture_w}x{capture_h}")
            })?;

        info!("{}", video_filter.dump());

        let enc = make_video_params(
            args,
            enc_pixfmt,
            &encoder,
            (enc_w_screen_coord, enc_h_screen_coord),
            refresh,
            global_header,
            &mut hw_device_ctx,
            &mut frames_yuv,
        )?;

        let passed_enc_options = match &args.ffmpeg_encoder_options {
            Some(enc_options) => parse_dict(enc_options).unwrap(),
            None => dict!(),
        };

        let enc_video = if args.hw {
            let low_power_opts = {
                let mut d = passed_enc_options.clone();
                d.set("low_power", "1");
                d
            };

            let regular_opts = if codec_id == codec::Id::H264 {
                let mut d = passed_enc_options.clone();
                d.set("level", "30");
                d
            } else {
                passed_enc_options.clone()
            };

            match args.low_power {
                LowPowerMode::Auto => match enc.open_with(low_power_opts) {
                    Ok(enc) => enc,
                    Err(e) => {
                        eprintln!("failed to open encoder in low_power mode ({}), trying non low_power mode. if you have an intel iGPU, set enable_guc=2 in the i915 module to use the fixed function encoder. pass --low-power=off to suppress this warning", e);
                        make_video_params(
                            args,
                            enc_pixfmt,
                            &encoder,
                            (enc_w_screen_coord, enc_h_screen_coord),
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
            let mut enc_options = passed_enc_options.clone();
            if enc_options.get("preset").is_none() {
                enc_options.set("preset", "ultrafast");
            }
            enc.open_with(enc_options).unwrap()
        };

        let mut ost_video = octx.add_stream(encoder).unwrap();

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

        Ok((
            EncState {
                video_filter,
                enc_video,
                filter_output_timebase: filter_timebase,
                octx,
                vid_stream_idx,
                frames_rgb,
                history_state,
                sigusr1_flag,
                audio,
            },
            selected_format,
        ))
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
            info!("pts offset is {:?}ns", pts_offset_ns);

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
                trace!(
                    "writing pts={} on {:?} is_key={}",
                    encoded.pts().unwrap(),
                    self.octx
                        .stream(encoded.stream())
                        .unwrap()
                        .parameters()
                        .medium(),
                    encoded.is_key()
                );
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

                            debug!(
                                "history is {:?} > {:?}, popping from history buffer {} bytes across {} packets on stream {:?}", 
                                current_history_size, history_dur,
                                removed_bytes,
                                removed_packets,
                                self.octx.stream(last_in_stream.stream()).unwrap().parameters().medium()
                            );
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
    roi_screen_coord: Rect,                               // size (pixels)
    (enc_w_screen_coord, enc_h_screen_coord): (i32, i32), // size (pixels) to encode. if not same as roi_{w,h}, the image will be scaled.
    transform: Transform,
) -> (filter::Graph, Rational) {
    let mut g = ffmpeg::filter::graph::Graph::new();
    g.add(
        &filter::find("buffer").unwrap(),
        "in",
        // format is bogus, will be replaced below, as we need to pass
        // hw_frames_ctx which isn't possible with args=
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

    let transpose_filter = match transform {
        Transform::_90 => ",transpose_vaapi=dir=clock",
        Transform::_180 => ",transpose_vaapi=dir=reversal",
        Transform::_270 => ",transpose_vaapi=dir=cclock",
        Transform::Flipped => ",transpose_vaapi=dir=hflip",
        Transform::Flipped90 => ",transpose_vaapi=dir=cclock_flip",
        Transform::Flipped180 => ",transpose_vaapi=dir=vflip",
        Transform::Flipped270 => ",transpose_vaapi=dir=clock_flip",
        _ => "",
    };

    // it seems intel's vaapi driver doesn't support transpose in RGB space, so we have to transpose
    // after the format conversion
    // which means we have to transform the crop to be in the *pre* transpose space
    let Rect {
        x: roi_x,
        y: roi_y,
        w: roi_w,
        h: roi_h,
    } = roi_screen_coord.screen_to_frame(capture_width, capture_height, transform);

    // sanity check
    assert!(roi_x >= 0, "{roi_x} < 0");
    assert!(roi_y >= 0, "{roi_y} < 0");

    let (enc_w, enc_h) =
        transpose_if_transform_transposed((enc_w_screen_coord, enc_h_screen_coord), transform);

    // exact=1 should not be necessary, as the input is not chroma-subsampled
    // however, there is a bug in ffmpeg that makes it required: https://trac.ffmpeg.org/ticket/10669
    // it is harmless to add though, so keep it as a workaround
    g.output("in", 0)
        .unwrap()
        .input("out", 0)
        .unwrap()
        .parse(&format!(
            "crop={roi_w}:{roi_h}:{roi_x}:{roi_y}:exact=1,scale_vaapi=format={output_real_pixfmt_name}:w={enc_w}:h={enc_h}{transpose_filter}{}",
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
    let args = Args::parse();
    if args.ext_image_copy_capture {
        execute::<CapExtImageCopy>(args);
    } else {
        execute::<CapWlrScreencopy>(args);
    }
}

fn execute<S: CaptureSource + 'static>(args: Args) {
    if let Some(generator) = args.completions_generator {
        let mut command = Args::command();
        let bin_name = command.get_name().to_string();
        clap_complete::generate(generator, &mut command, bin_name, &mut io::stdout());
        return;
    }

    let quit_flag = Arc::new(AtomicUsize::new(usize::MAX)); // ::MAX means still running, otherwise it's an exit value
    let sigusr1_flag = Arc::new(AtomicBool::new(false));

    signal_hook::flag::register_usize(SIGINT, Arc::clone(&quit_flag), 0).unwrap();
    signal_hook::flag::register_usize(SIGTERM, Arc::clone(&quit_flag), 1).unwrap();
    signal_hook::flag::register_usize(SIGHUP, Arc::clone(&quit_flag), 0).unwrap();
    signal_hook::flag::register(SIGUSR1, Arc::clone(&sigusr1_flag)).unwrap();

    CombinedLogger::init(vec![TermLogger::new(
        match args.verbose {
            0 => LevelFilter::Warn,
            1 => LevelFilter::Info,
            2 => LevelFilter::Debug,
            3.. => LevelFilter::Trace,
        },
        simplelog::Config::default(),
        TerminalMode::Stderr,
        ColorChoice::Auto,
    )])
    .unwrap();

    if !args.audio && args.audio_backend != DEFAULT_AUDIO_BACKEND {
        warn!("--audio-backend passed without --audio, will be ignored");
    }
    if !args.audio && args.audio_device != DEFAULT_AUDIO_CAPTURE_DEVICE {
        warn!("--audio-device passed without --audio, will be ignored");
    }
    if !args.audio && args.audio_codec != AudioCodec::Auto {
        warn!("--audio-codec passed without --audio, will be ignored");
    }
    if !args.audio && args.ffmpeg_audio_encoder.is_some() {
        warn!("--ffmpeg-audio-encoder without --audio, will be ignored");
    }
    if args.ffmpeg_audio_encoder.is_some() && args.audio_codec != AudioCodec::Auto {
        warn!("--ffmpeg-audio-encoder passed with --audio-codec, --audio-codec will be ignored");
    }
    if args.ffmpeg_encoder.is_some() && args.codec != Codec::Auto {
        warn!("--ffmpeg-encoder passed with --codec, --codec will be ignored");
    }
    if args.encode_pixfmt == Some(Pixel::VAAPI) {
        error!("`--encode-pixfmt vaapi` passed, this is nonsense. It will automatically be transformed into a vaapi pixel format if the selected encoder supports vaapi memory input");
        exit(1);
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

    let (mut state, mut queue) = match State::<S>::new(&conn, args, quit_flag.clone(), sigusr1_flag)
    {
        Ok(res) => res,
        Err(e) => {
            eprintln!("{e}");
            exit(1);
        }
    };

    while quit_flag.load(Ordering::SeqCst) == usize::MAX {
        queue.blocking_dispatch(&mut state).unwrap();
    }

    if let EncConstructionStage::Complete(enc, _) = &mut state.enc {
        enc.flush();
    }

    exit(quit_flag.load(Ordering::SeqCst) as i32)
}
