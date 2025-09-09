extern crate ffmpeg_next as ffmpeg;

use std::{
    collections::{HashMap, VecDeque},
    ffi::{CStr, CString, c_int},
    fmt,
    hash::Hash,
    io::{self, Write, stdout},
    marker::PhantomData,
    mem::{self, swap},
    num::ParseIntError,
    os::fd::{AsFd, AsRawFd, BorrowedFd},
    path::Path,
    process::exit,
    ptr::null_mut,
    str::from_utf8_unchecked,
    time::{Duration, Instant},
};

use anyhow::{Context, anyhow, bail, format_err};
use audio::AudioHandle;
use cap_ext_image_copy::CapExtImageCopy;
use cap_wlr_screencopy::CapWlrScreencopy;
use clap::{ArgAction, CommandFactory, Parser, command};
use drm::buffer::DrmFourcc;
use ffmpeg::{
    Packet, Rational, codec, dict, dictionary, encoder,
    ffi::{
        AV_HWFRAME_MAP_WRITE, AVDRMFrameDescriptor, AVPixelFormat, FF_COMPLIANCE_STRICT,
        av_buffer_ref, av_buffersrc_parameters_alloc, av_buffersrc_parameters_set,
        av_dict_parse_string, av_free, av_get_pix_fmt_name, av_hwframe_map, avcodec_alloc_context3,
        avfilter_graph_alloc_filter, avfilter_init_dict, avformat_query_codec,
    },
    filter,
    format::{self, Output, Pixel},
    frame::{self, video},
    media,
};
use fps_limit::FpsLimit;
use human_size::{Byte, Megabyte, Size, SpecificSize};
use libc::{EXIT_FAILURE, EXIT_SUCCESS};
use log::{debug, error, info, trace, warn};
use mio::{Events, Interest, Token, unix::SourceFd};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGUSR1};
use signal_hook_mio::v1_0::Signals;
use simplelog::{ColorChoice, CombinedLogger, LevelFilter, TermLogger, TerminalMode};
use thiserror::Error;
use transform::{Rect, transpose_if_transform_transposed};
use wayland_client::{
    ConnectError, Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
    backend::ObjectId,
    globals::{Global, GlobalList, GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer::WlBuffer,
        wl_output::{self, Mode, Transform, WlOutput},
        wl_registry::WlRegistry,
    },
};
use wayland_protocols::{
    ext::image_capture_source::v1::client::ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
    wp::linux_dmabuf::zv1::client::{
        zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
        zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
    },
    xdg::xdg_output::zv1::client::{
        zxdg_output_manager_v1::ZxdgOutputManagerV1,
        zxdg_output_v1::{self, ZxdgOutputV1},
    },
};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;

mod avhw;
use avhw::{AvHwDevCtx, AvHwFrameCtx};

mod audio;
mod cap_ext_image_copy;
mod cap_wlr_screencopy;
mod fifo;
mod fps_limit;
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

use crate::avhw::Usage;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    #[clap(long="no-hw", default_value = "true", action=ArgAction::SetFalse, help="don't use the GPU encoder, download the frames onto the CPU and use a software encoder. Ignored if `encoder` is supplied")]
    hw: bool,

    #[clap(long="no-cursor", default_value = "true", action=ArgAction::SetFalse, help="don't capture the cursor")]
    cap_cursor: bool,

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

    #[clap(
        long,
        short,
        help = "limit maximum framerate into the encoder. Note that by default, wl-screenrec only copies frames when the contents have changed, so it can drop below this"
    )]
    max_fps: Option<f64>,

    #[clap(long, short, default_value = "0", action=ArgAction::Count, help = "add very loud logging. can be specified multiple times")]
    verbose: u8,

    #[clap(
        long,
        help = "which dri device to use for vaapi. by default, this is obtained from the linux-dmabuf-v1 protocol when using wlr-screencopy, and from ext-image-copy-capture-session if using ext-image-copy-capture, if present. if not present, /dev/dri/renderD128 is guessed"
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
        help = "audio bitrate to encode at. Unit is bytes per second, 16 kB is 128 kbps"
    )]
    audio_bitrate: Option<Size>,

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
        long = "capture-backend",
        help = "which capture backend to use",
        value_enum,
        default_value_t
    )]
    capture_backend: CaptureBackend,

    #[cfg_attr(not(feature = "experimental-vulkan"), clap(hide = true))]
    #[clap(
        long = "experimental-vulkan",
        help = "use vulkan allocator & encode",
        default_value = "false"
    )]
    vulkan: bool,
}

trait CaptureSource: Sized {
    type Frame: Clone;

    fn new(
        gm: &GlobalList,
        eq: &QueueHandle<State<Self>>,
        output: WlOutput,
    ) -> anyhow::Result<Self>;

    // allocates a frame, either sync or async
    // if async, return None and call `on_frame_allocd` at a later moment
    // if sync, just return the allocated stuff
    fn alloc_frame(&self, eq: &QueueHandle<State<Self>>) -> Option<Self::Frame>;

    // queue a copy of the screen into `buf`
    // call `on_copy_complete` or `on_copy_fail` when the copy has completed
    fn queue_copy(&self, damage: bool, buf: &WlBuffer, dims: (i32, i32), cap: &Self::Frame);

    // destroy the `frame` object
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

#[derive(clap::ValueEnum, Debug, Default, Clone, PartialEq, Eq)]
enum CaptureBackend {
    #[default]
    Auto,
    WlrScreencopy,
    ExtImageCopyCapture,
}

struct FpsCounter {
    last_ct: u64,
    ct: u64,
    next_report: Instant,
}

impl FpsCounter {
    const PER: Duration = Duration::from_secs(1);

    fn new() -> Self {
        Self {
            last_ct: 0,
            ct: 0,
            next_report: Instant::now() + Self::PER,
        }
    }
    fn on_frame(&mut self) {
        self.ct += 1;
    }

    fn report(&mut self) {
        if Instant::now() > self.next_report {
            let _ = writeln!(stdout().lock(), "{} fps", self.ct - self.last_ct); // ignore errors, can indicate stdout was closed
            self.next_report += Self::PER;
            self.last_ct = self.ct;
        }
    }

    fn time_until_next_report(&self) -> Duration {
        let now = Instant::now();
        if now > self.next_report {
            Duration::from_secs(0)
        } else {
            self.next_report - now
        }
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
    global_name: u32,
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
    fn complete(&self) -> Option<OutputInfo> {
        if let (Some(name), Some(loc), Some(logical_size), Some(size_pixels), Some(refresh)) = (
            &self.name,
            &self.loc,
            &self.logical_size,
            &self.size_pixels,
            &self.refresh,
        ) {
            Some(OutputInfo {
                global_name: self.global_name,
                loc: *loc,
                name: name.clone(),
                logical_size: *logical_size,
                refresh: *refresh,
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
    global_name: u32,
    name: String,
    loc: (i32, i32),
    logical_size: (i32, i32),
    size_pixels: (i32, i32),
    refresh: Rational,
    output: WlOutput,
    transform: Transform,
}

impl OutputInfo {
    fn logical_to_pixel(&self, logical: i32) -> i32 {
        (f64::from(logical) * self.fractional_scale()).round() as i32
    }

    fn fractional_scale(&self) -> f64 {
        f64::from(self.size_pixels.0) / f64::from(self.logical_size.0)
    }

    fn size_screen_space(&self) -> (i32, i32) {
        transpose_if_transform_transposed(self.size_pixels, self.transform)
    }
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
#[repr(transparent)]
struct DrmModifier(u64);

impl DrmModifier {
    const LINEAR: DrmModifier = DrmModifier(0);
    const INVALID: DrmModifier = DrmModifier(0xffffffffffffff);
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

#[derive(Clone, Debug)]
struct DmabufPotentialFormat {
    fourcc: DrmFourcc,
    modifiers: Vec<DrmModifier>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DmabufFormat {
    width: i32,
    height: i32,
    fourcc: DrmFourcc,
    modifiers: Vec<DrmModifier>,
}

#[link(name = "drm")]
unsafe extern "C" {
    pub fn drmGetFormatModifierVendor(modifier: u64) -> *mut libc::c_char;
    pub fn drmGetFormatModifierName(modifier: u64) -> *mut libc::c_char;
}

struct State<S: CaptureSource> {
    in_flight_surface: InFlightSurface<S>,
    dma: ZwpLinuxDmabufV1,
    enc: EncConstructionStage<S>,
    starting_timestamp: Option<i64>,
    args: Args,
    errored: bool,
    gm: GlobalList,
    xdg_output_manager: ZxdgOutputManagerV1,
}

enum InFlightSurface<S: CaptureSource> {
    None,
    AllocQueued,
    Allocd(S::Frame),
    CopyQueued {
        av_surface: frame::Video,
        av_mapping: video::Video,
        wl_frame: S::Frame,
        wl_buffer: WlBuffer,
    },
}
impl<S: CaptureSource> InFlightSurface<S> {
    fn take(&mut self) -> InFlightSurface<S> {
        mem::replace(self, InFlightSurface::None)
    }
}

struct ProbingOutputsState {
    partial_outputs: HashMap<TypedObjectId<WlOutput>, PartialOutputInfo>, // key is xdg-output name (wayland object ID)
    outputs: HashMap<TypedObjectId<WlOutput>, Option<OutputInfo>>,        // none for disabled
    history_already_triggered: bool,
}

struct CompleteState<S> {
    enc: EncState,
    cap: S,
    output: OutputInfo,
    output_went_away: bool,
}

struct OutputWentAwayState {
    enc: EncState,
    waiting_for_output_name: String,
    partial_outputs: HashMap<TypedObjectId<WlOutput>, PartialOutputInfo>, // key is xdg-output name (wayland object ID)
}

enum EncConstructionStage<S> {
    ProbingOutputs(ProbingOutputsState),
    EverythingButFormat {
        roi: Rect,
        cap: S,
        output: OutputInfo,
        history_already_triggered: bool,
    },
    Complete(CompleteState<S>),
    OutputWentAway(OutputWentAwayState),
    Intermediate,
}
impl<S> EncConstructionStage<S> {
    #[track_caller]
    fn unwrap(&mut self) -> &mut CompleteState<S> {
        if let EncConstructionStage::Complete(e) = self {
            e
        } else {
            panic!("unwrap on non-complete EncConstructionStage")
        }
    }
    fn take_enc(self) -> EncState {
        match self {
            EncConstructionStage::Complete(e) => e.enc,
            EncConstructionStage::OutputWentAway(e) => e.enc,
            _ => panic!("unwrap on non-complete EncConstructionStage"),
        }
    }
    fn enc_mut(&mut self) -> Option<&mut EncState> {
        match self {
            EncConstructionStage::Complete(e) => Some(&mut e.enc),
            EncConstructionStage::OutputWentAway(e) => Some(&mut e.enc),
            _ => None,
        }
    }

    #[track_caller]
    fn unwrap_cap(&mut self) -> &mut S {
        match self {
            EncConstructionStage::EverythingButFormat { cap, .. } => cap,
            EncConstructionStage::Complete(e) => &mut e.cap,
            _ => panic!("no capture source yet"),
        }
    }

    fn on_sigusr1(&mut self) {
        match self {
            EncConstructionStage::ProbingOutputs(probing_outputs_state) => {
                probing_outputs_state.history_already_triggered = true
            }
            EncConstructionStage::EverythingButFormat {
                history_already_triggered,
                ..
            } => *history_already_triggered = true,
            EncConstructionStage::Complete(complete_state) => complete_state.enc.trigger_history(),
            EncConstructionStage::OutputWentAway(output_went_away_state) => {
                output_went_away_state.enc.trigger_history()
            }
            EncConstructionStage::Intermediate => unreachable!("enc left in intermediate state"),
        }
    }
}

enum HistoryState {
    RecordingHistory(Duration, VecDeque<Packet>), // --history specified, but SIGUSR1 not received yet. State is (duration of history, history)
    Recording(i64), // --history not specified OR (--history specified and SIGUSR1 has been sent). Data is the PTS offset (in nanoseconds), which is required when using history. If a stream is not present, then assume 0 offset
}

impl OutputWentAwayState {
    fn new_wl_output<S: CaptureSource + 'static>(
        &mut self,
        registry: &WlRegistry,
        xdg_output_manager: &ZxdgOutputManagerV1,
        global: Global,
        qhandle: &QueueHandle<State<S>>,
    ) {
        assert!(global.interface == WlOutput::interface().name);
        let output: WlOutput = registry.bind(global.name, global.version, qhandle, ());
        let _xdg = xdg_output_manager.get_xdg_output(&output, qhandle, TypedObjectId::new(&output));

        self.partial_outputs.insert(
            TypedObjectId::new(&output),
            PartialOutputInfo {
                global_name: global.name,
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

impl<S: CaptureSource + 'static> Dispatch<WlRegistry, GlobalListContents> for State<S> {
    fn event(
        state: &mut Self,
        proxy: &WlRegistry,
        event: <WlRegistry as Proxy>::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_registry::Event;
        debug!("wl-registry event: {event:?}");
        match event {
            Event::GlobalRemove { name } => {
                if let EncConstructionStage::Complete(c) = &mut state.enc {
                    if c.output.global_name == name {
                        c.output_went_away = true;
                    }
                }
            }
            Event::Global {
                name,
                interface,
                version,
            } => {
                if interface == WlOutput::interface().name {
                    if let EncConstructionStage::OutputWentAway(owa) = &mut state.enc {
                        owa.new_wl_output(
                            proxy,
                            &state.xdg_output_manager,
                            Global {
                                name,
                                interface,
                                version,
                            },
                            qhandle,
                        );
                    }
                }
            }
            _ => todo!(),
        }
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
        qhandle: &QueueHandle<Self>,
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
            zxdg_output_v1::Event::Done => {
                state.done_output_info_wl_output(out_id.clone(), qhandle);
            }
            _ => {}
        }
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
    fn new(conn: &Connection, args: Args) -> anyhow::Result<(Self, EventQueue<Self>)> {
        let display = conn.display();

        let (gm, queue) = registry_queue_init(conn).unwrap();
        let eq: QueueHandle<State<S>> = queue.handle();

        let dma: ZwpLinuxDmabufV1 = gm
            .bind(&eq, 4..=ZwpLinuxDmabufV1::interface().version, ())
            .context("your compositor does not support zwp-linux-dmabuf and therefore is not support by wl-screenrec. See the README for supported compositors")?;

        let registry = display.get_registry(&eq, ());

        let xdg_output_manager: ZxdgOutputManagerV1 = gm
            .bind(&eq, 3..=ZxdgOutputManagerV1::interface().version, ())
            .context("your compositor does not support zxdg-output-manager and therefore is not support by wl-screenrec. See the README for supported compositors")?;

        let mut partial_outputs = HashMap::new();
        for g in gm.contents().clone_list() {
            if g.interface == WlOutput::interface().name {
                let output: WlOutput =
                    registry.bind(g.name, WlOutput::interface().version, &eq, ());

                // query so we get the dispatch callbacks
                let _xdg =
                    xdg_output_manager.get_xdg_output(&output, &eq, TypedObjectId::new(&output));

                partial_outputs.insert(
                    TypedObjectId::new(&output),
                    PartialOutputInfo {
                        global_name: g.name,
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
                in_flight_surface: InFlightSurface::None,
                dma,
                enc: EncConstructionStage::ProbingOutputs(ProbingOutputsState {
                    partial_outputs,
                    outputs: HashMap::new(),
                    history_already_triggered: false,
                }),
                starting_timestamp: None,
                args,
                errored: false,
                gm,
                xdg_output_manager,
            },
            queue,
        ))
    }

    fn on_frame_allocd(&mut self, qhandle: &QueueHandle<State<S>>, frame: &S::Frame) {
        assert!(matches!(
            self.in_flight_surface,
            InFlightSurface::AllocQueued
        ));
        self.in_flight_surface = InFlightSurface::Allocd(frame.clone());

        match &mut self.enc {
            EncConstructionStage::ProbingOutputs { .. } => unreachable!(
                "Oops, somehow created a screencopy frame without initial enc state stuff?"
            ),
            EncConstructionStage::EverythingButFormat { .. } => {
                // queue_frame_capture will be called in negotiate_format
            }
            EncConstructionStage::OutputWentAway(_) => {
                panic!("copy_src_ready called when the output went away??")
            }
            EncConstructionStage::Intermediate => panic!("enc left in intermediate state"),
            EncConstructionStage::Complete(_) => {
                self.queue_frame_capture(qhandle);
            }
        }
    }
    fn queue_frame_capture(&mut self, qhandle: &QueueHandle<Self>) {
        let CompleteState { enc, cap, .. } = self.enc.unwrap();

        let InFlightSurface::Allocd(frame) = &self.in_flight_surface else {
            panic!("queue_frame_capture called in a strange state");
        };

        let mut av_surface = enc.frames_rgb.alloc().unwrap();
        av_surface.set_color_space(ffmpeg::color::Space::RGB);

        let (desc, av_mapping) = map_drm(&av_surface);

        assert_eq!(desc.nb_layers, 1);

        let wl_buffer_params = self.dma.create_params(qhandle, ());

        for i in 0..desc.layers[0].nb_planes {
            let oid = desc.layers[0].planes[i as usize].object_index;
            assert!(oid < desc.nb_objects);
            let object = &desc.objects[oid as usize];
            let plane = &desc.layers[0].planes[i as usize];
            let modifier = object.format_modifier.to_be_bytes();
            let fd = unsafe { BorrowedFd::borrow_raw(object.fd) };
            wl_buffer_params.add(
                fd,
                i as u32,
                plane.offset as u32,
                plane.pitch as u32,
                u32::from_be_bytes(modifier[..4].try_into().unwrap()),
                u32::from_be_bytes(modifier[4..].try_into().unwrap()),
            );
        }
        let wl_buffer = wl_buffer_params.create_immed(
            enc.selected_format.width,
            enc.selected_format.height,
            enc.selected_format.fourcc as u32,
            zwp_linux_buffer_params_v1::Flags::empty(),
            qhandle,
            (),
        );

        cap.queue_copy(
            self.args.damage,
            &wl_buffer,
            (enc.selected_format.width, enc.selected_format.height),
            frame,
        );

        self.in_flight_surface = InFlightSurface::CopyQueued {
            av_surface,
            av_mapping,
            wl_frame: frame.clone(),
            wl_buffer,
        };
    }

    fn on_new_capture_format(
        &mut self,
        mut cs: CompleteState<S>,
        new_format: &DmabufFormat,
    ) -> anyhow::Result<CompleteState<S>> {
        if *new_format == cs.enc.selected_format {
            return Ok(cs);
        }
        info!("compositor gave new format {new_format:?}");

        // destroy old frames
        match &self.in_flight_surface {
            InFlightSurface::Allocd(_) => {} // these frames are format independent, the previously allocated one is fine
            InFlightSurface::CopyQueued {
                wl_frame,
                wl_buffer,
                ..
            } => {
                cs.cap.on_done_with_frame(wl_frame.clone());
                wl_buffer.destroy();
                self.in_flight_surface = InFlightSurface::None;
            }
            InFlightSurface::None => {}
            InFlightSurface::AllocQueued => {}
        }

        let capture_pixfmt = dmabuf_to_av(new_format.fourcc);

        // make sure bounds are still valid, as size may have changed
        cs.enc.roi_screen_coord = cs
            .enc
            .roi_screen_coord
            .fit_inside_bounds(new_format.width, new_format.height);

        if cs.enc.roi_screen_coord.w == 0 || cs.enc.roi_screen_coord.h == 0 {
            bail!("new capture surface is zero-sized, bailing");
        }

        cs.enc.frames_rgb = cs.enc.hw_device_ctx
            .create_frame_ctx(capture_pixfmt, new_format.width, new_format.height, &new_format.modifiers, Usage::Capture)
            .with_context(|| format!("Failed to create {} frame context for capture surfaces of format {capture_pixfmt:?} {new_format:?}", if self.args.vulkan { "vulkan" } else { "vaapi" }))?;

        // todo: proper size here
        let enc_pixfmt_av = match cs.enc.enc_pixfmt {
            EncodePixelFormat::Vaapi(fmt) => fmt,
            EncodePixelFormat::Sw(fmt) => fmt,
            EncodePixelFormat::Vulkan(fmt) => fmt,
        };

        cs.enc.selected_format = new_format.clone();

        // flush old filter & encoder
        cs.enc
            .video_filter
            .get("in")
            .unwrap()
            .source()
            .flush()
            .unwrap();
        cs.enc.process_ready();
        if cs.enc.enc_video_has_been_fed_any_frames {
            // ffmpeg bug--if you call send_eof before feeding any frames it will crash
            cs.enc.enc_video.send_eof().unwrap();
        }
        cs.enc.process_ready();

        // create a new encoder
        // TODO: correct scaling
        let mut frames_yuv = cs.enc.hw_device_ctx
            .create_frame_ctx(enc_pixfmt_av, cs.enc.roi_screen_coord.w, cs.enc.roi_screen_coord.h, &[DrmModifier::LINEAR], Usage::Enc)
            .with_context(|| {
                format!("Failed to create a vaapi frame context for encode surfaces of format {enc_pixfmt_av:?} {}x{}", cs.enc.roi_screen_coord.w, cs.enc.roi_screen_coord.h)
            })?;

        let encoder = cs.enc.enc_video.codec().unwrap();
        let framerate = cs.enc.enc_video.frame_rate();
        let global_header = cs
            .enc
            .octx
            .format()
            .flags()
            .contains(format::Flags::GLOBAL_HEADER);
        let enc = make_video_params(
            &self.args,
            cs.enc.enc_pixfmt,
            &encoder,
            (cs.enc.roi_screen_coord.w, cs.enc.roi_screen_coord.h),
            framerate,
            global_header,
            &mut cs.enc.hw_device_ctx,
            &mut frames_yuv,
        )?;

        cs.enc.enc_video = enc.open_with(cs.enc.enc_video_options.clone())?;
        cs.enc.enc_video_has_been_fed_any_frames = false;

        let (filter, filter_timebase) = video_filter(
            &mut cs.enc.frames_rgb,
            cs.enc.enc_pixfmt,
            (new_format.width, new_format.height),
            cs.enc.roi_screen_coord,
            (cs.enc.roi_screen_coord.w, cs.enc.roi_screen_coord.h),
            cs.enc.transform,
            self.args.vulkan,
        );
        cs.enc.video_filter = filter;
        cs.enc.filter_output_timebase = filter_timebase;
        cs.enc.format_change = true;

        Ok(cs)
    }

    fn update_output_info_wl_output(
        &mut self,
        id: &TypedObjectId<WlOutput>,
        f: impl FnOnce(&mut PartialOutputInfo),
    ) {
        match &mut self.enc {
            EncConstructionStage::ProbingOutputs(ProbingOutputsState {
                partial_outputs, ..
            })
            | EncConstructionStage::OutputWentAway(OutputWentAwayState {
                partial_outputs, ..
            }) => {
                let output = partial_outputs.get_mut(id).unwrap();
                f(output);
            }
            _ => (),
        }
    }

    fn done_output_info_wl_output(
        &mut self,
        id: TypedObjectId<WlOutput>,
        qhandle: &QueueHandle<Self>,
    ) {
        let p = match &mut self.enc {
            EncConstructionStage::ProbingOutputs(p) => &mut p.partial_outputs,
            EncConstructionStage::OutputWentAway(p) => &mut p.partial_outputs,
            _ => {
                // got this event because of some dispaly changes, ignore...
                return;
            }
        };

        let output = p.get_mut(&id).unwrap();

        // for each output, we will get 2 done events
        // * when we create the WlOutput the first time
        // * then again when we probe the xdg output
        // we only care about the second one, as we want the xdg output info
        if !output.has_recvd_done {
            output.has_recvd_done = true;
            return;
        }

        if output.name.is_none() {
            warn!(
                "compositor did not provide name for wl_output {}, strange",
                id.0.protocol_id()
            );
        }
        let complete_output = output.complete();

        match &mut self.enc {
            EncConstructionStage::ProbingOutputs(probing_outputs_state) => {
                if let Some(info) = complete_output {
                    probing_outputs_state.outputs.insert(id, Some(info));
                }

                self.start_if_output_probe_complete(qhandle);
            }
            EncConstructionStage::OutputWentAway(output_went_away_state) => {
                if let Some(info) = complete_output {
                    if info.name == output_went_away_state.waiting_for_output_name {
                        info!(
                            "output {} came back, continuing screenrecording..",
                            info.name
                        );
                        let enc = mem::replace(&mut self.enc, EncConstructionStage::Intermediate)
                            .take_enc();
                        let cap = S::new(&self.gm, qhandle, info.output.clone()).unwrap();
                        self.enc = EncConstructionStage::Complete(CompleteState {
                            enc,
                            cap,
                            output: info,
                            output_went_away: false,
                        });
                        self.queue_alloc_frame(qhandle);
                    }
                }
            }
            _ => unreachable!(),
        }
    }

    fn start_if_output_probe_complete(&mut self, qhandle: &QueueHandle<Self>) {
        let p = if let EncConstructionStage::ProbingOutputs(p) = &self.enc {
            p
        } else {
            panic!("bad precondition: is still constructing");
        };

        if p.outputs.len() != p.partial_outputs.len() {
            // probe not complete
            debug!(
                "output probe not yet complete, still waiting for {}",
                p.partial_outputs
                    .iter()
                    .filter(|(id, _)| !p.outputs.contains_key(id))
                    .map(|(id, po)| format!("({id:?}, {po:?})"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
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
                    self.errored = true;
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
                    eprintln!("display {disp} not found, bailing");
                    self.errored = true;
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
                    eprintln!("region {x},{y} {w}x{h} is not entirely within one output, bailing",);
                    self.errored = true;
                    return;
                }
            }
            (Some(_), _) => {
                eprintln!(
                    "both --geometry and --output were passed, which is not allowed, bailing"
                );
                self.errored = true;
                return;
            }
        };

        info!("Using output {}", output.name);

        let cap = match S::new(&self.gm, qhandle, output.output.clone()) {
            Ok(cap) => cap,
            Err(err) => {
                eprintln!("failed to create capture state: {err}");
                self.errored = true;
                return;
            }
        };
        self.enc = EncConstructionStage::EverythingButFormat {
            roi,
            cap,
            output: output.clone(),
            history_already_triggered: p.history_already_triggered,
        };

        self.queue_alloc_frame(qhandle);
    }

    fn on_copy_complete(
        &mut self,
        qhandle: &QueueHandle<Self>,
        tv_sec_hi: u32,
        tv_sec_lo: u32,
        tv_nsec: u32,
    ) {
        let CompleteState { enc, cap, .. } = self.enc.unwrap();

        let mut surf = if let InFlightSurface::CopyQueued {
            av_surface,
            av_mapping,
            wl_frame,
            wl_buffer,
        } = self.in_flight_surface.take()
        {
            drop(av_mapping);
            cap.on_done_with_frame(wl_frame);
            wl_buffer.destroy();
            av_surface
        } else {
            panic!("on_copy_complete called in a strange state")
        };

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

        enc.push_with_fpslimit(surf);

        self.queue_alloc_frame(qhandle);
    }

    fn on_copy_fail(&mut self, qhandle: &QueueHandle<Self>) {
        let CompleteState {
            output_went_away,
            output,
            cap,
            enc,
        } = self.enc.unwrap();

        if let InFlightSurface::CopyQueued {
            av_surface,
            av_mapping,
            wl_frame,
            wl_buffer,
        } = self.in_flight_surface.take()
        {
            drop(av_mapping);
            cap.on_done_with_frame(wl_frame);
            wl_buffer.destroy();
            drop(av_surface);
        } else {
            panic!("on_copy_fail called in strange state");
        }

        if *output_went_away {
            info!(
                "copy failed because output {} went away. Waiting for it to come back...",
                output.name
            );
            let waiting_for_output_name = output.name.clone();
            let enc = mem::replace(&mut self.enc, EncConstructionStage::Intermediate).take_enc();

            let mut owa = OutputWentAwayState {
                enc,
                waiting_for_output_name,
                partial_outputs: Default::default(),
            };
            for g in self.gm.contents().clone_list() {
                if g.interface == WlOutput::interface().name {
                    owa.new_wl_output(self.gm.registry(), &self.xdg_output_manager, g, qhandle);
                }
            }
            self.enc = EncConstructionStage::OutputWentAway(owa);
        } else if enc.format_change {
            enc.format_change = false;
            debug!(
                "failed transfer, but just did a format change so not surprising. trying to capture a new frame..."
            );
            self.queue_alloc_frame(qhandle);
        } else {
            error!("unknown copy fail reason, trying to capture a new frame...");
            self.queue_alloc_frame(qhandle);
        }
    }

    fn negotiate_format(
        &mut self,
        capture_formats: &[DmabufPotentialFormat],
        (w, h): (u32, u32),
        dri_device: Option<&Path>,
        eq: &QueueHandle<State<S>>,
    ) {
        debug!("Supported capture formats are {w}x{h} {capture_formats:?}");
        let dri_device = if let Some(dev) = &self.args.dri_device {
            Path::new(dev)
        } else if let Some(dev) = dri_device {
            dev
        } else {
            warn!(
                "dri device could not be auto-detected, using /dev/dri/renderD128. Pass --dri-device if this isn't correct or to suppress this warning"
            );
            Path::new("/dev/dri/renderD128")
        };

        fn negotiate_format_impl(
            width: i32,
            height: i32,
            capture_formats: &[DmabufPotentialFormat],
        ) -> anyhow::Result<DmabufFormat> {
            for preferred_format in [
                DrmFourcc::Xrgb8888,
                DrmFourcc::Xbgr8888,
                DrmFourcc::Xrgb2101010,
            ] {
                let find = capture_formats.iter().find(|p| {
                    p.fourcc == preferred_format
                        && (p.modifiers.contains(&DrmModifier::LINEAR)
                            || p.modifiers.contains(&DrmModifier::INVALID)) // NVidia seems to only report INVALID & tiled formats, not LINEAR...
                });

                if let Some(find) = find {
                    return Ok(DmabufFormat {
                        width,
                        height,
                        fourcc: find.fourcc,
                        modifiers: find.modifiers.clone(),
                    });
                }
            }
            bail!(
                "failed to select a viable capture format. This is probably a bug. Availabe capture formats are {:?}",
                capture_formats
            )
        }

        let selected_format = match negotiate_format_impl(w as i32, h as i32, capture_formats) {
            Ok(f) => f,
            Err(e) => {
                error!("Failed to negotiate format: {e}");
                return;
            }
        };

        match mem::replace(&mut self.enc, EncConstructionStage::Intermediate) {
            EncConstructionStage::EverythingButFormat {
                output,
                roi,
                cap,
                history_already_triggered,
            } => {
                let enc = match EncState::new(
                    &self.args,
                    selected_format,
                    output.refresh,
                    output.transform,
                    roi,
                    history_already_triggered,
                    dri_device,
                ) {
                    Ok(enc) => enc,
                    Err(e) => {
                        error!("failed to create encoder(s): {e:?}");
                        self.errored = true;
                        return;
                    }
                };

                self.enc = EncConstructionStage::Complete(CompleteState {
                    enc,
                    cap,
                    output,
                    output_went_away: false,
                });
            }
            EncConstructionStage::Complete(mut c) => {
                // can happen on dispaly disconnect & reconnect OR output resize
                c = match self.on_new_capture_format(c, &selected_format) {
                    Ok(enc) => enc,
                    Err(e) => {
                        error!("failed to renegotiate new format {selected_format:?}: {e}");
                        self.errored = true;
                        return;
                    }
                };
                self.enc = EncConstructionStage::Complete(c);
            }
            _ => panic!("called negotiate_format in a strange state"),
        }

        // make the next sensible step in capture
        match &self.in_flight_surface {
            InFlightSurface::None => {
                self.queue_alloc_frame(eq);
            }
            InFlightSurface::AllocQueued => {} // nothing to do
            InFlightSurface::Allocd(_) => {
                self.queue_frame_capture(eq);
            }
            InFlightSurface::CopyQueued { .. } => {}
        }
    }

    fn queue_alloc_frame(&mut self, eq: &QueueHandle<State<S>>) {
        assert!(matches!(self.in_flight_surface, InFlightSurface::None));
        let f = self.enc.unwrap_cap().alloc_frame(eq);
        self.in_flight_surface = InFlightSurface::AllocQueued;
        if let Some(f) = f {
            self.on_frame_allocd(eq, &f);
        }
    }

    fn fps_counter(&mut self) -> Option<&mut FpsCounter> {
        self.enc.enc_mut().map(|enc| &mut enc.fps_counter)
    }
}

struct EncState {
    video_filter: filter::Graph,
    enc_video: encoder::Video,
    enc_video_has_been_fed_any_frames: bool,
    octx: format::context::Output,
    frames_rgb: AvHwFrameCtx,
    filter_output_timebase: Rational,
    vid_stream_idx: usize,
    history_state: HistoryState,
    audio: Option<AudioHandle>,
    selected_format: DmabufFormat,
    hw_device_ctx: AvHwDevCtx,
    enc_pixfmt: EncodePixelFormat,
    roi_screen_coord: Rect,
    transform: Transform,
    enc_video_options: dictionary::Owned<'static>,
    format_change: bool,
    fps_counter: FpsCounter,
    fps_limit: Option<FpsLimit<frame::Video>>,
}

#[derive(Copy, Clone, Debug)]
enum EncodePixelFormat {
    Vaapi(Pixel),
    Vulkan(Pixel),
    Sw(Pixel),
}

fn hw_codec_id(codec: codec::Id, vulkan: bool) -> Option<&'static str> {
    if vulkan {
        match codec {
            codec::Id::H264 => Some("h264_vulkan"),
            codec::Id::H265 | codec::Id::HEVC => Some("hevc_vulkan"),
            codec::Id::AV1 => Some("av1_vulkan"),
            _ => None,
        }
    } else {
        match codec {
            codec::Id::H264 => Some("h264_vaapi"),
            codec::Id::H265 | codec::Id::HEVC => Some("hevc_vaapi"),
            codec::Id::VP8 => Some("vp8_vaapi"),
            codec::Id::VP9 => Some("vp9_vaapi"),
            codec::Id::AV1 => Some("av1_vaapi"),
            _ => None,
        }
    }
}
#[allow(clippy::too_many_arguments)]
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

    enc.set_bit_rate((args.bitrate.into::<Byte>().value() * 8.) as usize);
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
        EncodePixelFormat::Vulkan(_) => Pixel::VULKAN,
        EncodePixelFormat::Sw(sw) => sw,
    });

    if let EncodePixelFormat::Vaapi(sw_pix_fmt) | EncodePixelFormat::Vulkan(sw_pix_fmt) =
        enc_pix_fmt
    {
        unsafe {
            (*enc.as_mut_ptr()).hw_device_ctx = av_buffer_ref(hw_device_ctx.as_mut_ptr());
            (*enc.as_mut_ptr()).hw_frames_ctx = av_buffer_ref(frames_yuv.as_mut_ptr());
            (*enc.as_mut_ptr()).sw_pix_fmt = sw_pix_fmt.into();
        }
    }

    Ok(enc)
}

fn parse_dict<'a>(dict: &str) -> Result<dictionary::Owned<'a>, ffmpeg::Error> {
    let cstr = CString::new(dict).unwrap();

    let mut ptr = null_mut();
    unsafe {
        let res = av_dict_parse_string(
            &mut ptr,
            cstr.as_ptr(),
            c"=:".as_ptr().cast(),
            c",".as_ptr().cast(),
            0,
        );
        if res != 0 {
            return Err(ffmpeg::Error::from(res));
        }

        Ok(dictionary::Owned::own(ptr))
    }
}

fn get_encoder(args: &Args, format: &Output) -> anyhow::Result<ffmpeg::Codec> {
    Ok(if let Some(encoder_name) = &args.ffmpeg_encoder {
        ffmpeg_next::encoder::find_by_name(encoder_name).ok_or_else(|| {
            format_err!(
                "Encoder {encoder_name} specified with --ffmpeg-encoder could not be instantiated"
            )
        })?
    } else {
        let codec_id = match args.codec {
            Codec::Auto => format.codec(&args.filename, media::Type::Video),
            Codec::Avc => codec::Id::H264,
            Codec::Hevc => codec::Id::HEVC,
            Codec::VP8 => codec::Id::VP8,
            Codec::VP9 => codec::Id::VP9,
            Codec::AV1 => codec::Id::AV1,
        };

        let maybe_hw_codec = if args.hw {
            if let Some(hw_codec_name) = hw_codec_id(codec_id, args.vulkan) {
                if let Some(codec) = ffmpeg_next::encoder::find_by_name(hw_codec_name) {
                    Some(codec)
                } else {
                    warn!(
                        "there is a known vaapi codec ({hw_codec_name}) for codec {codec_id:?}, but it's not available. Using a generic encoder..."
                    );
                    None
                }
            } else {
                warn!(
                    "hw flag is specified, but there's no known vaapi codec for {codec_id:?}. Using a generic encoder..."
                );
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
    })
}

fn get_enc_pixfmt(args: &Args, encoder: &ffmpeg::Codec) -> anyhow::Result<EncodePixelFormat> {
    let supported_formats = supported_formats(encoder);
    Ok(if supported_formats.is_empty() {
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
    } else if supported_formats.contains(&Pixel::VULKAN) {
        EncodePixelFormat::Vulkan(args.encode_pixfmt.unwrap_or(Pixel::NV12))
    } else {
        match args.encode_pixfmt {
            None => EncodePixelFormat::Sw(supported_formats[0]),
            Some(fmt) if supported_formats.contains(&fmt) => EncodePixelFormat::Sw(fmt),
            Some(fmt) => bail!("Encoder does not support pixel format {fmt:?}"),
        }
    })
}

impl EncState {
    // assumed that capture_{w,h}
    fn new(
        args: &Args,
        capture_format: DmabufFormat,
        refresh: Rational,
        transform: Transform,
        roi_screen_coord: Rect, // roi in screen coordinates (0, 0 is screen upper left, which is not necessarily captured frame upper left)
        history_alreday_triggered: bool,
        dri_device: &Path,
    ) -> anyhow::Result<Self> {
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

        let encoder = get_encoder(args, &octx.format())?;

        // format selection: naive version, should actually see what the ffmpeg filter supports...
        info!("capture pixel format is {}", capture_format.fourcc);

        let enc_pixfmt = get_enc_pixfmt(args, &encoder)?;
        info!("encode pixel format is {enc_pixfmt:?}");

        let codec_id = encoder.id();
        match unsafe {
            avformat_query_codec(
                octx.format().as_ptr(),
                codec_id.into(),
                FF_COMPLIANCE_STRICT,
            )
        } {
            0 => bail!(
                "Format {} does not support {:?} codec",
                octx.format().name(),
                codec_id
            ),
            1 => (),
            e => {
                warn!(
                    "Format {} might not support {:?} codec ({})",
                    octx.format().name(),
                    codec_id,
                    ffmpeg::Error::from(e)
                )
            }
        }

        let global_header = octx.format().flags().contains(format::Flags::GLOBAL_HEADER);

        let mut hw_device_ctx = if args.vulkan {
            error!(
                "Vulkan is buggy and isn't known to work well yet. See https://github.com/russelltg/wl-screenrec/issues/95"
            );

            #[allow(unreachable_code)]
            {
                info!("Opening vulkan device from {}", dri_device.display());
                AvHwDevCtx::new_vulkan(
                    dri_device, false, /* set to true to enable vulkan validation */
                )
                .map_err(|e| anyhow!("Failed to open vulkan device: {e}"))?
            }
        } else {
            info!(
                "Opening libva device from DRM device {}",
                dri_device.display()
            );
            AvHwDevCtx::new_libva(dri_device).map_err(
            |e| anyhow!("Failed to load vaapi device: {e}. This is likely *not* a bug in wl-screenrec, but an issue with your vaapi installation. Follow your distribution's instructions. If you're pretty sure you've done this correctly, create a new issue with the output of `vainfo` and if `wf-recorder -c h264_vaapi -d {}` works.", dri_device.display()))?
        };

        let mut frames_rgb = hw_device_ctx
            .create_frame_ctx(dmabuf_to_av(capture_format.fourcc), capture_format.width, capture_format.height, &capture_format.modifiers, Usage::Capture)
            .with_context(|| format!("Failed to create vaapi frame context for capture surfaces of format {capture_format:?}"))?;

        let (enc_w_screen_coord, enc_h_screen_coord) = match args.encode_resolution {
            Some((x, y)) => (x as i32, y as i32),
            None => (roi_screen_coord.w, roi_screen_coord.h),
        };

        let (video_filter, filter_timebase) = video_filter(
            &mut frames_rgb,
            enc_pixfmt,
            (capture_format.width, capture_format.height),
            roi_screen_coord,
            (enc_w_screen_coord, enc_h_screen_coord),
            transform,
            args.vulkan, // xx enum
        );

        let enc_pixfmt_av = match enc_pixfmt {
            EncodePixelFormat::Vaapi(fmt) => fmt,
            EncodePixelFormat::Vulkan(fmt) => fmt,
            EncodePixelFormat::Sw(fmt) => fmt,
        };
        let mut frames_yuv = hw_device_ctx
            .create_frame_ctx(enc_pixfmt_av, enc_w_screen_coord, enc_h_screen_coord, &[DrmModifier::LINEAR], Usage::Enc)
            .with_context(|| {
                format!("Failed to create a vaapi frame context for encode surfaces of format {enc_pixfmt_av:?} {enc_w_screen_coord}x{enc_h_screen_coord}")
            })?;

        info!("{}", video_filter.dump());

        let mut enc = make_video_params(
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

        let (enc_video, enc_video_options) = if args.hw {
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

            unsafe {
                (*enc.as_mut_ptr()).hw_frames_ctx = av_buffer_ref(frames_yuv.as_mut_ptr());
            }

            match args.low_power {
                LowPowerMode::Auto => match enc.open_with(low_power_opts.clone()) {
                    Ok(enc) => (enc, low_power_opts),
                    Err(e) => {
                        eprintln!(
                            "failed to open encoder in low_power mode ({e}), trying non low_power mode. if you have an intel iGPU, set enable_guc=2 in the i915 module to use the fixed function encoder. pass --low-power=off to suppress this warning"
                        );
                        (
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
                            .open_with(regular_opts.clone())?,
                            regular_opts,
                        )
                    }
                },
                LowPowerMode::On => (enc.open_with(low_power_opts.clone())?, low_power_opts),
                LowPowerMode::Off => (enc.open_with(regular_opts.clone())?, regular_opts),
            }
        } else {
            let mut enc_options = passed_enc_options.clone();
            if encoder.name() == "x264" && enc_options.get("preset").is_none() {
                enc_options.set("preset", "ultrafast");
            }
            (enc.open_with(enc_options.clone()).unwrap(), enc_options)
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
            Some(_) if history_alreday_triggered => HistoryState::Recording(0), // SIGUSR1 triggered before negotiation complete
            Some(history) => HistoryState::RecordingHistory(history, VecDeque::new()),
            None => HistoryState::Recording(0), // recording since the beginnging, no PTS offset
        };

        Ok(EncState {
            video_filter,
            enc_video,
            enc_video_has_been_fed_any_frames: false,
            filter_output_timebase: filter_timebase,
            octx,
            vid_stream_idx,
            hw_device_ctx,
            enc_pixfmt,
            roi_screen_coord,
            transform,
            enc_video_options,
            frames_rgb,
            history_state,
            audio,
            selected_format: capture_format,
            format_change: false,
            fps_counter: FpsCounter::new(),
            fps_limit: args.max_fps.map(FpsLimit::new),
        })
    }

    fn process_ready(&mut self) {
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
            self.enc_video_has_been_fed_any_frames = true;
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
                                current_history_size,
                                history_dur,
                                removed_bytes,
                                removed_packets,
                                self.octx
                                    .stream(last_in_stream.stream())
                                    .unwrap()
                                    .parameters()
                                    .medium()
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
        if let Some(limit) = &mut self.fps_limit {
            if let Some(f) = limit.flush() {
                self.push(f);
            }
        }

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
        self.fps_counter.on_frame();
        self.video_filter
            .get("in")
            .unwrap()
            .source()
            .add(&surf)
            .unwrap();

        self.process_ready();
    }

    fn push_with_fpslimit(&mut self, surf: frame::Video) {
        if let Some(limit) = &mut self.fps_limit {
            let ts = Duration::from_nanos(surf.pts().unwrap() as u64);
            if let Some(to_enc) = limit.on_new_frame(surf, ts) {
                self.push(to_enc);
            }
        } else {
            self.push(surf);
        }
    }

    fn trigger_history(&mut self) {
        // if we were recording history and got the SIGUSR1 flag
        if let HistoryState::RecordingHistory(_, hist) = &mut self.history_state {
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
            info!("pts offset is {pts_offset_ns:?}ns");

            // grab this before we set history_state
            let mut hist_moved = VecDeque::new();
            swap(hist, &mut hist_moved);

            // transition history state
            self.history_state = HistoryState::Recording(pts_offset_ns);

            for packet in hist_moved.drain(..) {
                self.on_encoded_packet(packet);
            }
        }
    }
}

fn video_filter(
    inctx: &mut AvHwFrameCtx,
    pix_fmt: EncodePixelFormat,
    (capture_width, capture_height): (i32, i32),
    roi_screen_coord: Rect,                               // size (pixels)
    (enc_w_screen_coord, enc_h_screen_coord): (i32, i32), // size (pixels) to encode. if not same as roi_{w,h}, the image will be scaled.
    transform: Transform,
    vulkan: bool,
) -> (filter::Graph, Rational) {
    let mut g = ffmpeg::filter::graph::Graph::new();

    let pixfmt_int = if vulkan {
        AVPixelFormat::AV_PIX_FMT_VULKAN as c_int
    } else {
        AVPixelFormat::AV_PIX_FMT_VAAPI as c_int
    };

    // src
    unsafe {
        let buffersrc_ctx = avfilter_graph_alloc_filter(
            g.as_mut_ptr(),
            filter::find("buffer").unwrap().as_mut_ptr(),
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
    let mut out = g
        .add(&filter::find("buffersink").unwrap(), "out", "")
        .unwrap();

    out.set_pixel_format(match pix_fmt {
        EncodePixelFormat::Sw(sw) => sw,
        EncodePixelFormat::Vaapi(_) => Pixel::VAAPI,
        EncodePixelFormat::Vulkan(_) => Pixel::VULKAN,
    });

    let output_real_pixfmt_name = unsafe {
        from_utf8_unchecked(
            CStr::from_ptr(av_get_pix_fmt_name(
                match pix_fmt {
                    EncodePixelFormat::Vaapi(fmt) => fmt,
                    EncodePixelFormat::Sw(fmt) => fmt,
                    EncodePixelFormat::Vulkan(fmt) => fmt,
                }
                .into(),
            ))
            .to_bytes(),
        )
    };

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
    let transpose_filter = transpose_dir
        .map(|transpose_dir| {
            if vulkan {
                format!("transpose_vulkan=dir={transpose_dir}")
            } else {
                format!("transpose_vaapi=dir={transpose_dir}")
            }
        })
        .unwrap_or_default();

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

    if vulkan {
        g.output("in", 0)
            .unwrap()
            .input("out", 0)
            .unwrap()
            .parse(&format!(
                "crop={roi_w}:{roi_h}:{roi_x}:{roi_y}:exact=1,scale_vulkan=format={output_real_pixfmt_name}:w={enc_w}:h={enc_h}{transpose_filter}{}",
                if let EncodePixelFormat::Vulkan(_) = pix_fmt {
                    ""
                } else {
                    ", hwdownload"
                },
            ))
            .unwrap();
    } else {
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
    }

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

struct InitialProbeState;
impl Dispatch<WlRegistry, GlobalListContents> for InitialProbeState {
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

fn main() {
    let args = Args::parse();

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

    if let Some(generator) = args.completions_generator {
        let mut command = Args::command();
        let bin_name = command.get_name().to_string();
        clap_complete::generate(generator, &mut command, bin_name, &mut io::stdout());
        return;
    }

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
        error!(
            "`--encode-pixfmt vaapi` passed, this is nonsense. It will automatically be transformed into a vaapi pixel format if the selected encoder supports vaapi memory input"
        );
        exit(1);
    }
    if let Some(max_fps) = args.max_fps {
        if max_fps <= 0. {
            error!("`--max-fps` must be a positive and nonzero number");
            exit(1);
        }
    }

    let conn = match Connection::connect_to_env() {
        Ok(conn) => conn,
        Err(e @ ConnectError::NoCompositor) => {
            error!(
                "WAYLAND_DISPLAY or XDG_RUNTIME_DIR environment variables are not set or are set to an invalid value: {e}"
            );
            exit(1);
        }
        Err(e) => {
            error!("{e}");
            exit(1)
        }
    };

    match args.capture_backend {
        CaptureBackend::Auto => {
            let (gm, _queue) = registry_queue_init::<InitialProbeState>(&conn).unwrap();
            let ext_image_copy_cap_name = ExtOutputImageCaptureSourceManagerV1::interface().name;
            let has_ext_image_copy_cap = gm
                .contents()
                .with_list(|l| l.iter().any(|g| g.interface == ext_image_copy_cap_name));
            if has_ext_image_copy_cap {
                info!(
                    "Protocol {ext_image_copy_cap_name} found in globals, defaulting to it (use `--capture-backend` to override)"
                );
                execute::<CapExtImageCopy>(args, conn);
            } else {
                info!(
                    "Protocol {ext_image_copy_cap_name} not found in globals, defaulting to {} (use `--capture-backend` to override)",
                    ZwlrScreencopyManagerV1::interface().name
                );
                execute::<CapWlrScreencopy>(args, conn);
            }
        }
        CaptureBackend::WlrScreencopy => {
            execute::<CapWlrScreencopy>(args, conn);
        }
        CaptureBackend::ExtImageCopyCapture => {
            execute::<CapExtImageCopy>(args, conn);
        }
    }
}

fn execute<S: CaptureSource + 'static>(args: Args, conn: Connection) {
    let mut sigs = Signals::new([SIGINT, SIGTERM, SIGHUP, SIGUSR1]).unwrap();

    if args.verbose >= 3 {
        ffmpeg_next::log::set_level(ffmpeg::log::Level::Trace);
    }

    ffmpeg_next::init().unwrap();

    info!("FFmpeg version {}", unsafe {
        CStr::from_ptr(ffmpeg_sys_next::av_version_info())
            .to_str()
            .unwrap()
    });

    let (mut state, mut queue) = match State::<S>::new(&conn, args) {
        Ok(res) => res,
        Err(e) => {
            eprintln!("{e}");
            exit(EXIT_FAILURE);
        }
    };

    const TOKEN_SIGS: Token = Token(0);
    const TOKEN_WAYLAND: Token = Token(1);

    let mut poll = mio::Poll::new().unwrap();
    poll.registry()
        .register(&mut sigs, TOKEN_SIGS, Interest::READABLE)
        .unwrap();

    poll.registry()
        .register(
            &mut SourceFd(&conn.as_fd().as_raw_fd()),
            TOKEN_WAYLAND,
            Interest::READABLE,
        )
        .unwrap();

    let mut events = Events::with_capacity(2);

    let exit_code = 'outer: loop {
        queue.flush().unwrap();
        let mut rg = Some(queue.prepare_read().unwrap());

        match poll.poll(
            &mut events,
            state.fps_counter().map(|f| f.time_until_next_report()),
        ) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                continue 'outer;
            }
            Err(e) => panic!("{e:?}"),
            Ok(()) => {}
        }

        for ev in events.iter() {
            match ev.token() {
                TOKEN_SIGS if ev.is_readable() => {
                    for sig in sigs.pending() {
                        match sig {
                            SIGINT | SIGTERM | SIGHUP => {
                                break 'outer EXIT_SUCCESS;
                            }
                            SIGUSR1 => {
                                state.enc.on_sigusr1();
                            }
                            _ => unreachable!(),
                        }
                    }
                }
                TOKEN_WAYLAND if ev.is_readable() => {
                    if let Err(wayland_backend::client::WaylandError::Io(e)) =
                        rg.take().unwrap().read()
                    {
                        if e.kind() == io::ErrorKind::WouldBlock {
                            continue;
                        } else {
                            panic!("Error reading from wayland connection: {e}");
                        }
                    }
                    queue.dispatch_pending(&mut state).unwrap();
                }
                _ => {}
            }
        }

        if let Some(f) = state.fps_counter() {
            f.report();
        }

        if state.errored {
            break EXIT_FAILURE;
        }
    };
    if let EncConstructionStage::Complete(c) = &mut state.enc {
        c.enc.flush();
    }

    exit(exit_code);
}
