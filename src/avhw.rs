use std::{ffi::CString, path::Path, ptr::null_mut};
#[cfg(feature = "experimental-vulkan")]
use std::{os::raw::c_void, pin::Pin};

use ffmpeg::{
    dict,
    ffi::{
        av_buffer_ref, av_buffer_unref, av_hwdevice_ctx_create, av_hwframe_ctx_alloc,
        av_hwframe_ctx_init, av_hwframe_get_buffer, AVHWFramesContext,
    },
    format::Pixel,
    frame, Dictionary,
};
use ffmpeg_sys_next::av_hwdevice_ctx_create_derived_opts;

use crate::DrmModifier;
use log::error;

pub struct AvHwDevCtx {
    ptr: *mut ffmpeg::sys::AVBufferRef,
    fmt: Pixel,
}

impl AvHwDevCtx {
    pub fn new_libva(dri_device: &Path) -> Result<Self, ffmpeg::Error> {
        unsafe {
            let mut hw_device_ctx = null_mut();

            let opts = dict! {
                "connection_type" => "drm"
            };

            let dev_cstr = CString::new(dri_device.to_str().unwrap()).unwrap();
            let sts = av_hwdevice_ctx_create(
                &mut hw_device_ctx,
                ffmpeg_next::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                dev_cstr.as_ptr(),
                opts.as_mut_ptr(),
                0,
            );

            if sts != 0 {
                Err(ffmpeg::Error::from(sts))
            } else {
                Ok(Self {
                    ptr: hw_device_ctx,
                    fmt: Pixel::VAAPI,
                })
            }
        }
    }

    pub fn new_vulkan(dri_device: &Path, validtion: bool) -> Result<Self, ffmpeg::Error> {
        unsafe {
            let mut hw_device_ctx_drm = null_mut();
            let mut hw_device_ctx = null_mut();

            let dev_cstr = CString::new(dri_device.to_str().unwrap()).unwrap();

            let mut d = Dictionary::new();
            if validtion {
                d.set("debug", "validate");
            }

            let sts = av_hwdevice_ctx_create(
                &mut hw_device_ctx_drm,
                ffmpeg_sys_next::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM,
                dev_cstr.as_ptr(),
                null_mut(),
                0,
            );
            if sts != 0 {
                return Err(ffmpeg::Error::from(sts));
            }

            let sts = av_hwdevice_ctx_create_derived_opts(
                &mut hw_device_ctx,
                ffmpeg_next::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VULKAN,
                hw_device_ctx_drm,
                d.as_mut_ptr(),
                0,
            );

            av_buffer_unref(&mut hw_device_ctx_drm);

            if sts != 0 {
                Err(ffmpeg::Error::from(sts))
            } else {
                Ok(Self {
                    ptr: hw_device_ctx,
                    fmt: Pixel::VULKAN,
                })
            }
        }
    }

    pub fn create_frame_ctx(
        &mut self,
        pixfmt: Pixel,
        width: i32,
        height: i32,
        modifiers: &[DrmModifier],
    ) -> Result<AvHwFrameCtx, ffmpeg::Error> {
        unsafe {
            let mut hwframe = av_hwframe_ctx_alloc(self.ptr as *mut _);
            let hwframe_casted = &mut *((*hwframe).data as *mut AVHWFramesContext);

            // ffmpeg does not expose RGB vaapi
            hwframe_casted.format = self.fmt.into();
            hwframe_casted.sw_format = pixfmt.into();
            hwframe_casted.width = width;
            hwframe_casted.height = height;
            hwframe_casted.initial_pool_size = 5;

            #[cfg(feature = "experimental-vulkan")]
            let mut vk: Option<Pin<Box<AvHwDevCtxVulkanBuffers>>> = None;

            let sts = if self.fmt == Pixel::VULKAN {
                #[cfg(feature = "experimental-vulkan")]
                {
                    use ash::vk;
                    use ffmpeg::ffi::{
                        AVHWDeviceContext, AVVulkanDeviceContext, AVVulkanFramesContext,
                    };

                    let av_devctx = &(*((*self.as_mut_ptr()).data as *mut AVHWDeviceContext));
                    let vk_hwctx = &*(av_devctx.hwctx as *mut AVVulkanDeviceContext);

                    let inst = ash::Instance::load(
                        &ash::StaticFn {
                            get_instance_proc_addr: vk_hwctx.get_proc_addr,
                        },
                        vk_hwctx.inst,
                    );

                    let usage = vk::ImageUsageFlags::TRANSFER_DST
                        | vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR
                        | vk::ImageUsageFlags::SAMPLED; // TODO: could split usage based on if this is output of the filter graph or not

                    let pixfmt_vk = vkfmt_from_pixfmt(pixfmt)?;
                    let modifiers_filtered = vk_filter_drm_modifiers(
                        inst,
                        vk_hwctx.phys_dev,
                        pixfmt_vk,
                        usage,
                        modifiers,
                        width,
                        height,
                    );

                    let mut vk_bufs = AvHwDevCtxVulkanBuffers::new(
                        modifiers_filtered.into_boxed_slice(),
                        pixfmt_vk,
                    );

                    let vk_ptr = &mut *(hwframe_casted.hwctx as *mut AVVulkanFramesContext);

                    vk_ptr.tiling = vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT;
                    vk_ptr.usage = usage;
                    vk_ptr.create_pnext = vk_bufs.as_mut().chain_ptr();

                    vk = Some(vk_bufs);
                    av_hwframe_ctx_init(hwframe)
                }
                #[cfg(not(feature = "experimental-vulkan"))]
                panic!("vulkan requested but built without vulkan support")
            } else {
                if !modifiers.contains(&DrmModifier::LINEAR) {
                    error!("unknown how to request non-linear frames in vaapi");
                }
                av_hwframe_ctx_init(hwframe)
            };
            if sts != 0 {
                return Err(ffmpeg::Error::from(sts));
            }

            let ret = Ok(AvHwFrameCtx {
                ptr: av_buffer_ref(hwframe),

                #[cfg(feature = "experimental-vulkan")]
                _vk: vk,
            });

            av_buffer_unref(&mut hwframe);

            ret
        }
    }

    pub fn as_mut_ptr(&mut self) -> *mut ffmpeg::sys::AVBufferRef {
        self.ptr
    }
}

impl Drop for AvHwDevCtx {
    fn drop(&mut self) {
        unsafe {
            av_buffer_unref(&mut self.ptr);
        }
    }
}

#[cfg(feature = "experimental-vulkan")]
fn vkfmt_from_pixfmt(pix: Pixel) -> Result<ash::vk::Format, ffmpeg::Error> {
    use ffmpeg_sys_next::av_vkfmt_from_pixfmt;

    // Safety: av_vkfmt_from_pixfmt is safe with any argument
    // if it returns a value, it will be a valid pointer to an ash::vk::Format
    unsafe {
        let res = av_vkfmt_from_pixfmt(pix.into());
        if res.is_null() {
            Err(ffmpeg::Error::InvalidData)
        } else {
            Ok(*res)
        }
    }
}

#[cfg(feature = "experimental-vulkan")]
fn vk_filter_drm_modifiers(
    inst: ash::Instance,
    phys_dev: ash::vk::PhysicalDevice,
    pixfmt_vk: ash::vk::Format,
    usage: ash::vk::ImageUsageFlags,
    in_modifiers: &[DrmModifier],
    width: i32,
    height: i32,
) -> Vec<DrmModifier> {
    use ash::vk;

    #[cfg(not(ffmpeg_8_0))]
    let drm_modifier_props = get_drm_format_modifier_properties(&inst, phys_dev, pixfmt_vk);

    let mut modifiers_filtered: Vec<DrmModifier> = Vec::new();

    'outer: for modifier in in_modifiers {
        let mut drm_info = ash::vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
            .drm_format_modifier(modifier.0);

        let mut image_format_prop = ash::vk::ImageFormatProperties2::default();

        if let Ok(()) = unsafe {
            inst.get_physical_device_image_format_properties2(
                phys_dev,
                &vk::PhysicalDeviceImageFormatInfo2::default()
                    .format(pixfmt_vk)
                    .usage(usage)
                    .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                    .push_next(&mut drm_info),
                &mut image_format_prop,
            )
        } {
            if image_format_prop.image_format_properties.max_extent.width < width as u32
                || image_format_prop.image_format_properties.max_extent.height < height as u32
            {
                log::debug!(
                    "modifier {:?} not supported for size {}x{} (max extents {}x{})",
                    modifier,
                    width,
                    height,
                    image_format_prop.image_format_properties.max_extent.width,
                    image_format_prop.image_format_properties.max_extent.height
                );
                continue; // modifier not supported for this size
            }

            #[cfg(not(ffmpeg_8_0))]
            for m in &drm_modifier_props {
                if m.drm_format_modifier == modifier.0 && m.drm_format_modifier_plane_count > 1 {
                    log::warn!("ffmpeg < 8.0 buggy and does not support multi-plane modifier export (modifier {modifier:?} has {} planes), skipping", 
                            m.drm_format_modifier_plane_count);
                    continue 'outer;
                }
            }

            modifiers_filtered.push(*modifier);
        }
    }
    modifiers_filtered
}

#[cfg(feature = "experimental-vulkan")]
fn get_drm_format_modifier_properties(
    inst: &ash::Instance,
    phys_dev: ash::vk::PhysicalDevice,
    pixfmt: ash::vk::Format,
) -> Vec<ash::vk::DrmFormatModifierPropertiesEXT> {
    let mut drm_props = ash::vk::DrmFormatModifierPropertiesListEXT::default();
    unsafe {
        use ash::vk::{DrmFormatModifierPropertiesEXT, FormatProperties2};

        inst.get_physical_device_format_properties2(
            phys_dev,
            pixfmt,
            &mut FormatProperties2::default().push_next(&mut drm_props),
        );
        let mut props_storage = vec![
            DrmFormatModifierPropertiesEXT::default();
            drm_props.drm_format_modifier_count as usize
        ];
        drm_props.p_drm_format_modifier_properties = props_storage.as_mut_ptr();
        inst.get_physical_device_format_properties2(
            phys_dev,
            pixfmt,
            &mut FormatProperties2::default().push_next(&mut drm_props),
        );
        props_storage
    }
}

// self-referencing struct of Vulkan buffers
// also, the frames context will store a pointer to this struct, so more reaons it's !Unpin
// 'static in here is a hack, it's really the lifetime of the AvHwDevCtxVulkanBuffers
#[cfg(feature = "experimental-vulkan")]
struct AvHwDevCtxVulkanBuffers {
    drm_info: ash::vk::ImageDrmFormatModifierListCreateInfoEXT<'static>, // points to _image_fmt_list_info & _vk_modifiers
    vk_modifiers: Pin<Box<[DrmModifier]>>,
    image_fmt_list_info: ash::vk::ImageFormatListCreateInfo<'static>, // points to _image_fmt_list_info_fmts
    image_fmt_list_info_fmts: [ash::vk::Format; 1],
    _pin: std::marker::PhantomPinned, // to make this struct !Unpin
}

#[cfg(feature = "experimental-vulkan")]
impl AvHwDevCtxVulkanBuffers {
    pub fn new(modifiers_filtered: Box<[DrmModifier]>, pixfmt: ash::vk::Format) -> Pin<Box<Self>> {
        let mut vk = Box::pin(AvHwDevCtxVulkanBuffers {
            drm_info: ash::vk::ImageDrmFormatModifierListCreateInfoEXT::default(),
            vk_modifiers: Pin::new(modifiers_filtered),
            image_fmt_list_info: ash::vk::ImageFormatListCreateInfo::default(),
            image_fmt_list_info_fmts: [pixfmt],
            _pin: std::marker::PhantomPinned,
        });

        // SAFETY: we are not moving out of any of the fields, so this is safe
        // Also, this sets up the self-referencing pointers correctly
        unsafe {
            let vk = vk.as_mut().get_unchecked_mut();

            vk.image_fmt_list_info.view_format_count = vk.image_fmt_list_info_fmts.len() as u32;
            vk.image_fmt_list_info.p_view_formats = vk.image_fmt_list_info_fmts.as_ptr();

            vk.drm_info.p_next = <*mut _>::cast(&mut vk.image_fmt_list_info);
            vk.drm_info.drm_format_modifier_count = vk.vk_modifiers.len() as u32;
            vk.drm_info.p_drm_format_modifiers = vk.vk_modifiers.as_ptr() as *const _;
        }
        vk
    }

    pub fn chain_ptr(self: std::pin::Pin<&mut Self>) -> *mut c_void {
        // drm_info is the beginning of the chain
        &self.as_ref().drm_info as *const _ as *mut _
    }
}

pub struct AvHwFrameCtx {
    ptr: *mut ffmpeg::sys::AVBufferRef,

    // the frame context continues to references these pointeres, so allocate them on the heap
    #[cfg(feature = "experimental-vulkan")]
    _vk: Option<Pin<Box<AvHwDevCtxVulkanBuffers>>>,
}

impl Drop for AvHwFrameCtx {
    fn drop(&mut self) {
        unsafe {
            av_buffer_unref(&mut self.ptr);
        }
    }
}

impl AvHwFrameCtx {
    pub fn alloc(&mut self) -> Result<frame::Video, ffmpeg::Error> {
        let mut frame = ffmpeg_next::frame::video::Video::empty();
        match unsafe { av_hwframe_get_buffer(self.ptr, frame.as_mut_ptr(), 0) } {
            0 => Ok(frame),
            e => Err(ffmpeg::Error::from(e)),
        }
    }
    pub fn as_mut_ptr(&mut self) -> *mut ffmpeg::sys::AVBufferRef {
        self.ptr
    }
}
