#[cfg(feature = "experimental-vulkan")]
use std::pin::Pin;
use std::{ffi::CString, path::Path, ptr::null_mut};

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
                        av_vkfmt_from_pixfmt, AVHWDeviceContext, AVVulkanDeviceContext,
                        AVVulkanFramesContext,
                    };

                    let av_devctx = &(*((*self.as_mut_ptr()).data as *mut AVHWDeviceContext));
                    let vk_hwctx = &*(av_devctx.hwctx as *mut AVVulkanDeviceContext);

                    let inst = ash::Instance::load(
                        &ash::StaticFn {
                            get_instance_proc_addr: vk_hwctx.get_proc_addr,
                        },
                        vk_hwctx.inst,
                    );

                    let mut modifiers_filtered: Vec<DrmModifier> = Vec::new();
                    for modifier in modifiers {
                        let mut drm_info =
                            ash::vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
                                .drm_format_modifier(modifier.0);
                        let mut image_format_prop = ash::vk::ImageFormatProperties2::default();

                        if let Ok(()) = inst.get_physical_device_image_format_properties2(
                            vk_hwctx.phys_dev,
                            &vk::PhysicalDeviceImageFormatInfo2 {
                                format: *av_vkfmt_from_pixfmt(pixfmt.into()),
                                usage: vk::ImageUsageFlags::TRANSFER_DST
                                    | vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR
                                    | vk::ImageUsageFlags::SAMPLED,
                                tiling: vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT,
                                p_next: <*mut _>::cast(&mut drm_info),
                                ..Default::default()
                            },
                            &mut image_format_prop,
                        ) {
                            if image_format_prop.image_format_properties.max_extent.width
                                < width as u32
                                || image_format_prop.image_format_properties.max_extent.height
                                    < height as u32
                            {
                                log::debug!(
                                    "modifier {:?} not supported for size {}x{} (max extents {}x{})",
                                    modifier, width, height, image_format_prop.image_format_properties.max_extent.width,
                                    image_format_prop.image_format_properties.max_extent.height
                                );
                                continue; // modifier not supported for this size
                            }
                            modifiers_filtered.push(*modifier);
                        }
                    }

                    vk = Some(Pin::new(Box::new(AvHwDevCtxVulkanBuffers {
                        drm_info: ash::vk::ImageDrmFormatModifierListCreateInfoEXT::default(),
                        vk_modifiers: Pin::new(Box::new([])),
                        image_fmt_list_info: ash::vk::ImageFormatListCreateInfo::default(),
                        image_fmt_list_info_fmts: [*av_vkfmt_from_pixfmt(pixfmt.into())],
                    })));
                    let vk = vk.as_mut().unwrap();

                    vk.image_fmt_list_info = ash::vk::ImageFormatListCreateInfo {
                        view_format_count: vk.image_fmt_list_info_fmts.len() as u32,
                        p_view_formats: vk.image_fmt_list_info_fmts.as_ptr(),
                        ..Default::default()
                    };
                    vk.drm_info = ash::vk::ImageDrmFormatModifierListCreateInfoEXT::default();
                    vk.drm_info.p_next = <*mut _>::cast(&mut vk.image_fmt_list_info);

                    // some buffer requirements are complex, just start removing modifiers until it works
                    let mut sts = -1;
                    while sts != 0 && !modifiers_filtered.is_empty() {
                        vk.drm_info.drm_format_modifier_count = modifiers_filtered.len() as u32;
                        vk.drm_info.p_drm_format_modifiers = modifiers_filtered.as_ptr() as _;

                        let vk_ptr = hwframe_casted.hwctx as *mut AVVulkanFramesContext;

                        (*vk_ptr).tiling = vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT;
                        (*vk_ptr).usage |= vk::ImageUsageFlags::TRANSFER_DST
                            | vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR
                            | vk::ImageUsageFlags::SAMPLED; // TODO: could split usage based on if this is output of the filter graph or not
                        (*vk_ptr).create_pnext = &mut vk.drm_info
                            as *mut ash::vk::ImageDrmFormatModifierListCreateInfoEXT
                            as _;

                        sts = av_hwframe_ctx_init(hwframe);

                        if sts != 0 {
                            modifiers_filtered.pop();
                        }
                    }

                    // NOTE: safe because this can't change the address of the array
                    vk.vk_modifiers = Pin::new(modifiers_filtered.into_boxed_slice());

                    sts
                }
                #[cfg(not(feature = "experimental-vulkan"))]
                panic!("vulkan requested but built without vulkan support")
            } else {
                if modifiers != &[DrmModifier::LINEAR] {
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

// self-referencing struct of Vulkan buffers
// 'static in here is a hack, it's really the lifetime of the AvHwDevCtxVulkanBuffers
#[cfg(feature = "experimental-vulkan")]
struct AvHwDevCtxVulkanBuffers {
    drm_info: ash::vk::ImageDrmFormatModifierListCreateInfoEXT<'static>, // points to _image_fmt_list_info & _vk_modifiers
    vk_modifiers: Pin<Box<[DrmModifier]>>,
    image_fmt_list_info: ash::vk::ImageFormatListCreateInfo<'static>, // points to _image_fmt_list_info_fmts
    image_fmt_list_info_fmts: [ash::vk::Format; 1],
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
