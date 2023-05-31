extern crate ffmpeg_next as ffmpeg;

use std::{
    collections::{BTreeMap, VecDeque},
    ffi::{c_int, CString},
    num::ParseIntError,
    ptr::null_mut,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    thread::{sleep, spawn},
    time::Duration,
};

use clap::{command, ArgAction, Parser};
use ffmpeg::{
    codec::{self},
    dict, encoder,
    ffi::{
        av_buffer_ref, av_buffer_unref, av_buffersrc_parameters_alloc, av_buffersrc_parameters_set,
        av_free, av_hwdevice_ctx_create, av_hwframe_ctx_alloc, av_hwframe_ctx_init,
        av_hwframe_get_buffer, av_hwframe_map, avcodec_alloc_context3, AVDRMFrameDescriptor,
        AVHWFramesContext, AVPixelFormat, AV_HWFRAME_MAP_READ, AV_HWFRAME_MAP_WRITE,
    },
    filter,
    format::{self, Pixel},
    frame::{self, video},
    Error, Packet, Rational,
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
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
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

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[clap(long="no-hw", default_value = "true", action=ArgAction::SetFalse, help="don't use the GPU encoder, download the frames onto the CPU and use x264")]
    hw: bool,

    #[clap(
        long,
        short,
        default_value = "screenrecord.avi",
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

    #[clap(long, short, help = "add very loud logging")]
    verbose: bool,

    #[clap(long, default_value = "/dev/dri/renderD128")]
    dri_device: String,

    #[clap(long, value_enum, default_value_t)]
    low_power: LowPowerMode,

    #[clap(long, short, default_value_t=SpecificSize::new(5, Megabyte).unwrap().into(), help="bitrate to encode at. Unit is bytes per second, so 5 MB is 40 Mbps")]
    bitrate: Size,

    #[clap(long,
        help="run in a mode where the screen is recorded, but nothing is written to the output file until SIGUSR1 is sent to the process. Then, it writes the most recent N seconds to a file and continues recording", 
        value_parser=parse_duration
    )]
    history: Option<Duration>,
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
            AV_HWFRAME_MAP_WRITE as c_int | AV_HWFRAME_MAP_READ as c_int,
        );
        assert_eq!(sts, 0);

        (
            *((*dst.as_ptr()).data[0] as *const AVDRMFrameDescriptor),
            dst,
        )
    }
}

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
    enc: Option<EncState>,
    starting_timestamp: Option<i64>,
    fps_counter: FpsCounter,
    args: Args,
    output_fractional_scales: BTreeMap<u32, (Option<String>, Option<f64>)>, // key is zwlr_output_head name (object ID) -> (name property, fractional scale)
    partial_outputs: BTreeMap<u32, PartialOutputInfo>, // key is xdg-output name (wayland object ID)
    outputs: BTreeMap<u32, OutputInfo>,
    quit_flag: Arc<AtomicBool>,
    sigusr1_flag: Arc<AtomicBool>,
}

enum HistoryState {
    RecordingHistory(Duration, VecDeque<Packet>), // --history specified, but SIGUSR1 not received yet. State is (duration of history, history)
    Recording(i64), // --history not specified OR (--history specified and SIGUSR1 has been sent). Data is the PTS offset, which is required when using history
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

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrScreencopyFrameV1,
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

                if state.starting_timestamp.is_none() {
                    state.starting_timestamp = Some(pts_abs);
                }
                let pts = pts_abs - state.starting_timestamp.unwrap();
                surf.set_pts(Some(pts));

                unsafe {
                    (*surf.as_mut_ptr()).time_base.num = 1;
                    (*surf.as_mut_ptr()).time_base.den = 1_000_000_000;
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
        match event {
            wl_output::Event::Mode {
                refresh,
                flags: WEnum::Value(flags),
                width,
                height,
            } => {
                if flags.contains(Mode::Current) {
                    state.update_output_info_wl_output(*data, qhandle, |info| {
                        info.refresh = Some(Rational(refresh, 1000));
                        info.size_pixels = Some((width, height));
                    });
                }
            }
            _ => {}
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
        _state: &mut Self,
        _proxy: &ZwlrOutputManagerV1,
        _event: <ZwlrOutputManagerV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
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
            .unwrap();

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
                enc: None,
                starting_timestamp: None,
                fps_counter: FpsCounter::new(),
                args,
                wl_output: None,
                partial_outputs,
                outputs: BTreeMap::new(),
                output_fractional_scales: BTreeMap::new(),
                quit_flag,
                sigusr1_flag,
            },
            queue,
        )
    }

    fn queue_copy(&mut self, eq: &QueueHandle<State>) {
        let enc = self.enc.as_mut().unwrap();
        let surf = enc.frames_rgb.alloc().unwrap();

        let (desc, mapping) = map_drm(&surf);

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
            drm_fourcc::DrmFourcc::Xrgb8888 as u32,
            zwp_linux_buffer_params_v1::Flags::empty(),
            eq,
            (),
        );

        let capture =
            self.screencopy_manager
                .capture_output(1, self.wl_output.as_ref().unwrap(), eq, ());

        capture.copy_with_damage(&buf);

        self.surfaces_owned_by_compositor
            .push_back((surf, mapping, dma_params, capture, buf));
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

    fn start_if_output_probe_complete(&mut self, qhandle: &QueueHandle<State>) {
        assert!(self.enc.is_none());

        if self.outputs.len() != self.partial_outputs.len() {
            // probe not complete
            return;
        }

        let (output, (x, y), (w, h)) = match (self.args.geometry, self.args.output.as_str()) {
            (None, "") => {
                // default case, capture whole monitor
                if self.outputs.len() != 1 {
                    println!("multiple displays and no --geometry or --output supplied, bailing");
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
                    println!("display {} not found, bailing", disp);
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
                    println!(
                        "region {},{} {}x{} is not entirely within one output, bailing",
                        x, y, w, h
                    );
                    self.quit_flag.store(true, Ordering::SeqCst);
                    return;
                }
            }
            (Some(_), _) => {
                println!("both --geometry and --output were passed, which is not allowed, bailing");
                self.quit_flag.store(true, Ordering::SeqCst);
                return;
            }
        };

        println!("Using output {}", output.name);

        self.wl_output = Some(output.output.clone());
        self.enc = Some(EncState::new(
            &self.args,
            output.refresh,
            output.size_pixels.0,
            output.size_pixels.1,
            x,
            y,
            w,
            h,
            Arc::clone(&self.sigusr1_flag),
        ));
        self.queue_copy(qhandle);
    }
}

struct EncState {
    filter: filter::Graph,
    enc: encoder::Video,
    octx: format::context::Output,
    frames_rgb: AvHwFrameCtx,
    filter_output_timebase: Rational,
    octx_time_base: Rational,
    vid_stream_idx: usize,
    capture_size: (i32, i32),
    verbose: bool,
    history_state: HistoryState,
    sigusr1_flag: Arc<AtomicBool>,
}

fn make_video_params(
    args: &Args,
    encode_w: i32,
    encode_h: i32,
    framerate: Rational,
    global_header: bool,
    hw_device_ctx: &AvHwDevCtx,
    frames_yuv: &AvHwFrameCtx,
) -> encoder::video::Video {
    let codec =
        ffmpeg_next::encoder::find_by_name(if args.hw { "h264_vaapi" } else { "libx264" }).unwrap();

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

    if args.hw {
        enc.set_format(Pixel::VAAPI);

        unsafe {
            (*enc.as_mut_ptr()).hw_device_ctx = av_buffer_ref(hw_device_ctx.ptr as *mut _);
            (*enc.as_mut_ptr()).hw_frames_ctx = av_buffer_ref(frames_yuv.ptr as *mut _);
            (*enc.as_mut_ptr()).sw_pix_fmt = AVPixelFormat::AV_PIX_FMT_NV12;
        }
    } else {
        enc.set_format(Pixel::NV12);
    }

    enc
}

impl EncState {
    // assumed that capture_{w,h}
    fn new(
        args: &Args,
        refresh: Rational,
        capture_w: i32, // pixels
        capture_h: i32, // pixels
        encode_x: i32,
        encode_y: i32,
        encode_w: i32,
        encode_h: i32,
        sigusr1_flag: Arc<AtomicBool>,
    ) -> Self {
        let mut octx = ffmpeg_next::format::output(&args.filename).unwrap();

        let global_header = octx.format().flags().contains(format::Flags::GLOBAL_HEADER);

        let mut hw_device_ctx = AvHwDevCtx::new_libva(&args.dri_device);
        let frames_rgb = hw_device_ctx
            .create_frame_ctx(AVPixelFormat::AV_PIX_FMT_BGR0, capture_w, capture_h)
            .unwrap();

        let (filter, filter_timebase) = filter(
            &frames_rgb,
            args.hw,
            capture_w,
            capture_h,
            encode_x,
            encode_y,
            encode_w,
            encode_h,
        );

        let frames_yuv = hw_device_ctx
            .create_frame_ctx(AVPixelFormat::AV_PIX_FMT_NV12, encode_w, encode_h)
            .unwrap();

        if args.verbose {
            println!("{}", filter.dump());
        }

        let enc = make_video_params(
            args,
            encode_w,
            encode_h,
            refresh,
            global_header,
            &hw_device_ctx,
            &frames_yuv,
        );

        let enc = if args.hw {
            let low_power_opts = dict! {
                "low_power" => "1"
            };
            let regular_opts = dict! {
                "level" => "30"
            };

            match args.low_power {
                LowPowerMode::Auto => match enc.open_with(low_power_opts) {
                    Ok(enc) => enc,
                    Err(e) => {
                        println!("failed to open encoder in low_power mode ({}), trying non low_power mode. if you have an intel iGPU, set enable_guc=2 in the i915 module to use the fixed function encoder", e);
                        make_video_params(
                            args,
                            encode_w,
                            encode_h,
                            refresh,
                            global_header,
                            &hw_device_ctx,
                            &frames_yuv,
                        )
                        .open_with(regular_opts)
                        .unwrap()
                    }
                },
                LowPowerMode::On => enc.open_with(low_power_opts).unwrap(),
                LowPowerMode::Off => enc.open_with(regular_opts).unwrap(),
            }
        } else {
            enc.open_with(dict! {
                "preset" => "ultrafast"
            })
            .unwrap()
        };

        let mut ost = octx
            .add_stream(ffmpeg_next::encoder::find(codec::Id::H264))
            .unwrap();

        let vid_stream_idx = ost.index();
        ost.set_parameters(&enc);
        octx.write_header().unwrap();
        let octx_time_base = octx.stream(vid_stream_idx).unwrap().time_base();

        if args.verbose {
            ffmpeg_next::format::context::output::dump(&octx, 0, Some(&args.filename));
        }

        let history_state = match args.history {
            Some(history) => HistoryState::RecordingHistory(history, VecDeque::new()),
            None => HistoryState::Recording(0), // recording since the beginnging, no PTS offset
        };

        EncState {
            filter,
            enc,
            filter_output_timebase: filter_timebase,
            octx_time_base,
            octx,
            vid_stream_idx,
            frames_rgb,
            capture_size: (capture_w, capture_h),
            verbose: args.verbose,
            history_state,
            sigusr1_flag,
        }
    }
    fn process_ready(&mut self) {
        // if we were recording history and got the SIGUSR1 flag
        if let (HistoryState::RecordingHistory(_, hist), true) = (
            &mut self.history_state,
            self.sigusr1_flag.load(Ordering::SeqCst),
        ) {
            // write history to container
            let pts_offset = hist.front().map(|first| first.pts().unwrap()).unwrap_or(0);

            for packet in hist {
                packet.set_pts(Some(packet.pts().unwrap() - pts_offset));
                packet.set_dts(packet.dts().map(|dts| dts - pts_offset));
                let _ = packet.write_interleaved(&mut self.octx).unwrap();
            }

            eprintln!("SIGUSR1 received, flushing history");

            // transition history state
            self.history_state = HistoryState::Recording(pts_offset);
        }

        let mut yuv_frame = frame::Video::empty();
        while self
            .filter
            .get("out")
            .unwrap()
            .sink()
            .frame(&mut yuv_frame)
            .is_ok()
        {
            // encoder has same time base as the filter, so don't do any time scaling
            self.enc.send_frame(&yuv_frame).unwrap();
        }

        let mut encoded = Packet::empty();
        while self.enc.receive_packet(&mut encoded).is_ok() {
            encoded.set_stream(self.vid_stream_idx);
            encoded.rescale_ts(self.filter_output_timebase, self.octx_time_base);

            match &mut self.history_state {
                HistoryState::Recording(pts_offset) => {
                    encoded.set_pts(Some(encoded.pts().unwrap() - *pts_offset));
                    encoded.set_dts(encoded.dts().map(|dts| dts - *pts_offset));
                    let _ = encoded.write_interleaved(&mut self.octx).unwrap();
                }
                HistoryState::RecordingHistory(history_dur, history) => {
                    // discard old history if necessary

                    // find first key packet (other than the first one)
                    // we want to make sure the first packet is always a key packet
                    while let Some(key_idx_minus_one) =
                        history.iter().skip(1).position(|packet| packet.is_key())
                    {
                        let key_idx = key_idx_minus_one + 1;
                        let key_pts = history[key_idx].pts().unwrap();

                        let current_history_size_pts =
                            u64::try_from(encoded.pts().unwrap() - key_pts).unwrap();
                        let current_history_size = Duration::from_nanos(
                            current_history_size_pts * self.octx_time_base.0 as u64 * 1_000_000_000
                                / self.octx_time_base.1 as u64,
                        );

                        if current_history_size > *history_dur {
                            if self.verbose {
                                eprintln!(
                                    "history is {:?} > {:?}, popping from history buffer",
                                    current_history_size, history_dur
                                );
                            }
                            history.drain(0..key_idx);
                        } else {
                            break;
                        }
                    }

                    history.push_back(encoded.clone());
                }
            }

            encoded = Packet::empty();
        }
    }

    fn flush(&mut self) {
        self.filter.get("in").unwrap().source().flush().unwrap();
        self.process_ready();
        self.enc.send_eof().unwrap();
        self.process_ready();
        self.octx.write_trailer().unwrap();
    }

    fn push(&mut self, surf: frame::Video) {
        self.filter.get("in").unwrap().source().add(&surf).unwrap();

        self.process_ready();
    }
}

struct AvHwDevCtx {
    ptr: *mut ffmpeg::sys::AVBufferRef,
}

impl AvHwDevCtx {
    fn new_libva(dri_device: &str) -> Self {
        unsafe {
            let mut hw_device_ctx = null_mut();

            let opts = dict! {
                "connection_type" => "drm"
            };

            let dev_cstr = CString::new(dri_device).unwrap();
            let sts = av_hwdevice_ctx_create(
                &mut hw_device_ctx,
                ffmpeg_next::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                dev_cstr.as_ptr(),
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

impl AvHwFrameCtx {
    fn alloc(&mut self) -> Result<frame::Video, Error> {
        let mut frame = ffmpeg_next::frame::video::Video::empty();
        match unsafe { av_hwframe_get_buffer(self.ptr, frame.as_mut_ptr(), 0) } {
            0 => Ok(frame),
            e => Err(Error::from(e)),
        }
    }
}

fn filter(
    inctx: &AvHwFrameCtx,
    hw: bool,
    capture_width: i32,
    capture_height: i32,
    enc_x: i32,
    enc_y: i32,
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
            "crop={}:{}:{}:{},scale_vaapi=format=nv12:w={}:h={}{}",
            enc_width,
            enc_height,
            enc_x,
            enc_y,
            enc_width,
            enc_height,
            if hw { "" } else { ", hwdownload" },
        ))
        .unwrap();

    g.validate().unwrap();

    (g, Rational::new(1, 1_000_000_000))
}

fn main() {
    let quit_flag = Arc::new(AtomicBool::new(false));
    let sigusr1_flag = Arc::new(AtomicBool::new(false));

    signal_hook::flag::register(SIGINT, Arc::clone(&quit_flag)).unwrap();
    signal_hook::flag::register(SIGUSR1, Arc::clone(&sigusr1_flag)).unwrap();

    let args = Args::parse();

    ffmpeg_next::init().unwrap();

    if args.verbose {
        ffmpeg_next::log::set_level(ffmpeg::log::Level::Trace);
    }

    let conn = Connection::connect_to_env().unwrap();

    let (mut state, mut queue) = State::new(&conn, args, quit_flag.clone(), sigusr1_flag);

    while !quit_flag.load(Ordering::SeqCst) {
        queue.blocking_dispatch(&mut state).unwrap();
    }

    if let Some(enc) = &mut state.enc {
        enc.flush();
    }
}
