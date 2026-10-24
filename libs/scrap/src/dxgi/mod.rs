use std::{io, mem, ptr, slice};
pub mod gdi;
pub use gdi::CapturerGDI;
pub mod hdr;
pub mod mag;

use winapi::{
    shared::{
        dxgi::*,
        dxgi1_2::*,
        dxgi1_6::*,
        dxgiformat::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT},
        dxgitype::*,
        minwindef::{DWORD, FALSE, TRUE, UINT},
        ntdef::LONG,
        windef::{HMONITOR, RECT},
        winerror::*,
    },
    um::{
        d3d11::*, d3dcommon::D3D_DRIVER_TYPE_UNKNOWN, unknwnbase::IUnknown, wingdi::*,
        winnt::HRESULT, winuser::*,
    },
};

use crate::RotationMode::*;

use crate::{AdapterDevice, Frame, PixelBuffer};
use std::ffi::c_void;

pub struct ComPtr<T>(*mut T);
impl<T> ComPtr<T> {
    fn is_null(&self) -> bool {
        self.0.is_null()
    }
}
impl<T> Drop for ComPtr<T> {
    fn drop(&mut self) {
        unsafe {
            if !self.is_null() {
                (*(self.0 as *mut IUnknown)).Release();
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum FrameState {
    Idle,
    Acquired,
    Mapped,
}

pub struct Capturer {
    device: ComPtr<ID3D11Device>,
    display: Display,
    context: ComPtr<ID3D11DeviceContext>,
    duplication: ComPtr<IDXGIOutputDuplication>,
    fastlane: bool,
    surface: ComPtr<IDXGISurface>,
    texture: ComPtr<ID3D11Texture2D>,
    width: usize,
    height: usize,
    rotated: Vec<u8>,
    gdi_capturer: Option<CapturerGDI>,
    gdi_buffer: Vec<u8>,
    saved_raw_data: Vec<u8>, // for faster compare and copy
    output_texture: bool,
    adapter_desc1: DXGI_ADAPTER_DESC1,
    rotate: Rotate,
    frame_state: FrameState,
    hdr: Option<hdr::HdrToSdr>,
}

impl Capturer {
    pub fn new(display: Display) -> io::Result<Capturer> {
        let mut device = ptr::null_mut();
        let mut context = ptr::null_mut();
        let mut duplication = ptr::null_mut();
        #[allow(invalid_value)]
        let mut desc = unsafe { mem::MaybeUninit::uninit().assume_init() };
        #[allow(invalid_value)]
        let mut adapter_desc1 = unsafe { mem::MaybeUninit::uninit().assume_init() };
        let mut gdi_capturer = None;

        let mut res = if display.gdi {
            wrap_hresult(1)
        } else {
            let res = wrap_hresult(unsafe {
                D3D11CreateDevice(
                    display.adapter.0 as *mut _,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    ptr::null_mut(), // No software rasterizer.
                    0,               // No device flags.
                    ptr::null_mut(), // Feature levels.
                    0,               // Feature levels' length.
                    D3D11_SDK_VERSION,
                    &mut device,
                    ptr::null_mut(),
                    &mut context,
                )
            });
            if res.is_ok() {
                wrap_hresult(unsafe { (*display.adapter.0).GetDesc1(&mut adapter_desc1) })
            } else {
                res
            }
        };
        let device = ComPtr(device);
        let context = ComPtr(context);

        if res.is_err() {
            gdi_capturer = display.create_gdi();
            println!("Fallback to GDI");
            if gdi_capturer.is_some() {
                res = Ok(());
            }
        } else {
            res = wrap_hresult(unsafe {
                let hres = Self::duplicate_output(&display, device.0, &mut duplication);
                if hres != S_OK {
                    gdi_capturer = display.create_gdi();
                    println!("Fallback to GDI");
                    if gdi_capturer.is_some() {
                        S_OK
                    } else {
                        hres
                    }
                } else {
                    hres
                }

                // NVFBC（NVIDIA Capture SDK），xpra 曾使用过，现已废弃，https://developer.nvidia.com/capture-sdk

                // 也可以尝试更高版本的 DXGI 以获得更好的性能，例如
                // https://docs.microsoft.com/zh-cn/windows/win32/direct3ddxgi/dxgi-1-2-improvements
                // dxgi-1-6 可能太高，只支持 win10 (2018)
                // https://docs.microsoft.com/zh-cn/windows/win32/api/dxgiformat/ne-dxgiformat-dxgi_format
                // DXGI_FORMAT_420_OPAQUE
                // IDXGIOutputDuplication::GetFrameDirtyRects 和 IDXGIOutputDuplication::GetFrameMoveRects
                // 可以帮助我们增量更新屏幕

                /* 在我的电脑上不支持，以后再试
                use winapi::shared::dxgiformat::DXGI_FORMAT_B8G8R8A8_UNORM;

                let format : Vec<DXGI_FORMAT> = vec![DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_420_OPAQUE];
                (*display.inner).DuplicateOutput1(
                    device as *mut _,
                    0 as UINT,
                    2 as UINT,
                    format.as_ptr(),
                    &mut duplication
                )
                */

                // 如果上面那招不奏效，我认为下面的也不会奏效，以后再试
                // https://developer.nvidia.com/capture-sdk 已废弃
                // 使用 directx + nvideo sdk 实现 GPU 加速的视频编解码的示例
                // https://github.com/NVIDIA/video-sdk-samples
            });
        }

        res?;

        if !duplication.is_null() {
            unsafe {
                (*duplication).GetDesc(&mut desc);
            }
        }
        let rotate = Self::create_rotations(device.0, context.0, &display);

        Ok(Capturer {
            device,
            context,
            duplication: ComPtr(duplication),
            fastlane: desc.DesktopImageInSystemMemory == TRUE
                && desc.ModeDesc.Format != DXGI_FORMAT_R16G16B16A16_FLOAT,
            surface: ComPtr(ptr::null_mut()),
            texture: ComPtr(ptr::null_mut()),
            width: display.width() as usize,
            height: display.height() as usize,
            display,
            rotated: Vec::new(),
            gdi_capturer,
            gdi_buffer: Vec::new(),
            saved_raw_data: Vec::new(),
            output_texture: false,
            adapter_desc1,
            rotate,
            frame_state: FrameState::Idle,
            hdr: None,
        })
    }

    // 请求 HDR 模式合成的浮点桌面，以便对其做色调映射；
    // 而旧的调用返回的会是 DXGI 裁剪后的 BGRA8 转换结果。
    // 仅在存在 IDXGIOutput6（Windows 10 1703）的地方，因为正是它
    // 能区分 HDR 桌面和 WCG 桌面；微软的复制示例也是用同样的方式
    // 来限定浮点请求。
    unsafe fn duplicate_output(
        display: &Display,
        device: *mut ID3D11Device,
        duplication: &mut *mut IDXGIOutputDuplication,
    ) -> HRESULT {
        if !hdr::UNAVAILABLE.load(std::sync::atomic::Ordering::Relaxed) {
            let mut output6: *mut IDXGIOutput6 = ptr::null_mut();
            (*display.inner.0).QueryInterface(
                &IID_IDXGIOutput6,
                &mut output6 as *mut *mut _ as *mut *mut _,
            );
            if !output6.is_null() {
                let output6 = ComPtr(output6);
                let formats = [DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_B8G8R8A8_UNORM];
                let hres = (*output6.0).DuplicateOutput1(
                    device as *mut _,
                    0,
                    formats.len() as UINT,
                    formats.as_ptr(),
                    duplication,
                );
                if hres == S_OK {
                    return hres;
                }
                hbb_common::log::warn!(
                    "HDR DuplicateOutput1 failed: hr={:#x}, fallback=DuplicateOutput",
                    hres as u32
                );
            }
        }
        (*display.inner.0).DuplicateOutput(device as *mut _, duplication)
    }

    unsafe fn tonemap(
        &mut self,
        source: *mut ID3D11Texture2D,
        desc: &D3D11_TEXTURE2D_DESC,
    ) -> io::Result<*mut ID3D11Texture2D> {
        if self.hdr.is_none() {
            match hdr::HdrToSdr::new(
                self.device.0,
                self.context.0,
                self.display.inner.0,
                &self.display.desc.DeviceName,
            ) {
                Ok(hdr) => self.hdr = Some(hdr),
                Err(err) => return self.abandon_tonemap(err),
            }
        }
        let converted = match self.hdr.as_mut() {
            Some(hdr) => hdr.convert(source, desc),
            None => Err(io::Error::new(io::ErrorKind::Other, "no tone-map")),
        };
        match converted {
            Ok(texture) => Ok(texture),
            Err(err) => self.abandon_tonemap(err),
        }
    }

    // 去掉色调映射，按旧方式重新复制输出，于是 DXGI
    // 交出裁剪后的 BGRA8（HDR 之前的行为）。如果重新复制失败，
    // 则先切换到 GDI 再返回。调用方会看到 WouldBlock 并再次询问。
    unsafe fn abandon_tonemap<T>(&mut self, err: io::Error) -> io::Result<T> {
        if hdr::is_permanent(&err) {
            hdr::UNAVAILABLE.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        hbb_common::log::error!("HDR tone-map failed, re-duplicating without it: {err}");
        self.hdr = None;
        if let Err(release_err) = self.release_frame() {
            hbb_common::log::warn!("DXGI frame cleanup failed while disabling HDR: {release_err}");
        }
        self.duplication = ComPtr(ptr::null_mut());
        let mut duplication = ptr::null_mut();
        let result = wrap_hresult(
            (*self.display.inner.0).DuplicateOutput(self.device.0 as *mut _, &mut duplication),
        );
        if let Err(err) = result {
            if self.set_gdi() {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            return Err(err);
        }
        self.duplication = ComPtr(duplication);
        let mut desc: DXGI_OUTDUPL_DESC = mem::zeroed();
        (*duplication).GetDesc(&mut desc);
        self.fastlane = desc.DesktopImageInSystemMemory == TRUE;
        Err(io::ErrorKind::WouldBlock.into())
    }

    fn create_rotations(
        device: *mut ID3D11Device,
        context: *mut ID3D11DeviceContext,
        display: &Display,
    ) -> Rotate {
        let mut video_context: *mut ID3D11VideoContext = ptr::null_mut();
        let mut video_device: *mut ID3D11VideoDevice = ptr::null_mut();
        let mut video_processor_enum: *mut ID3D11VideoProcessorEnumerator = ptr::null_mut();
        let mut video_processor: *mut ID3D11VideoProcessor = ptr::null_mut();
        let processor_rotation = match display.rotation() {
            DXGI_MODE_ROTATION_ROTATE90 => Some(D3D11_VIDEO_PROCESSOR_ROTATION_90),
            DXGI_MODE_ROTATION_ROTATE180 => Some(D3D11_VIDEO_PROCESSOR_ROTATION_180),
            DXGI_MODE_ROTATION_ROTATE270 => Some(D3D11_VIDEO_PROCESSOR_ROTATION_270),
            _ => None,
        };
        if let Some(processor_rotation) = processor_rotation {
            println!("create rotations");
            if !device.is_null() && !context.is_null() {
                unsafe {
                    (*context).QueryInterface(
                        &IID_ID3D11VideoContext,
                        &mut video_context as *mut *mut _ as *mut *mut _,
                    );
                    if !video_context.is_null() {
                        (*device).QueryInterface(
                            &IID_ID3D11VideoDevice,
                            &mut video_device as *mut *mut _ as *mut *mut _,
                        );
                        if !video_device.is_null() {
                            let (input_width, input_height) = match display.rotation() {
                                DXGI_MODE_ROTATION_ROTATE90 | DXGI_MODE_ROTATION_ROTATE270 => {
                                    (display.height(), display.width())
                                }
                                _ => (display.width(), display.height()),
                            };
                            let (output_width, output_height) = (display.width(), display.height());
                            let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                                InputFrameRate: DXGI_RATIONAL {
                                    Numerator: 30,
                                    Denominator: 1,
                                },
                                InputWidth: input_width as _,
                                InputHeight: input_height as _,
                                OutputFrameRate: DXGI_RATIONAL {
                                    Numerator: 30,
                                    Denominator: 1,
                                },
                                OutputWidth: output_width as _,
                                OutputHeight: output_height as _,
                                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
                            };
                            (*video_device).CreateVideoProcessorEnumerator(
                                &content_desc,
                                &mut video_processor_enum,
                            );
                            if !video_processor_enum.is_null() {
                                let mut caps: D3D11_VIDEO_PROCESSOR_CAPS = mem::zeroed();
                                if S_OK == (*video_processor_enum).GetVideoProcessorCaps(&mut caps)
                                {
                                    if caps.FeatureCaps
                                        & D3D11_VIDEO_PROCESSOR_FEATURE_CAPS_ROTATION
                                        != 0
                                    {
                                        (*video_device).CreateVideoProcessor(
                                            video_processor_enum,
                                            0,
                                            &mut video_processor,
                                        );
                                        if !video_processor.is_null() {
                                            (*video_context).VideoProcessorSetStreamRotation(
                                                video_processor,
                                                0,
                                                TRUE,
                                                processor_rotation,
                                            );
                                            (*video_context)
                                                .VideoProcessorSetStreamAutoProcessingMode(
                                                    video_processor,
                                                    0,
                                                    FALSE,
                                                );
                                            (*video_context).VideoProcessorSetStreamFrameFormat(
                                                video_processor,
                                                0,
                                                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                                            );
                                            (*video_context).VideoProcessorSetStreamSourceRect(
                                                video_processor,
                                                0,
                                                TRUE,
                                                &RECT {
                                                    left: 0,
                                                    top: 0,
                                                    right: input_width as _,
                                                    bottom: input_height as _,
                                                },
                                            );
                                            (*video_context).VideoProcessorSetStreamDestRect(
                                                video_processor,
                                                0,
                                                TRUE,
                                                &RECT {
                                                    left: 0,
                                                    top: 0,
                                                    right: output_width as _,
                                                    bottom: output_height as _,
                                                },
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        let video_context = ComPtr(video_context);
        let video_device = ComPtr(video_device);
        let video_processor_enum = ComPtr(video_processor_enum);
        let video_processor = ComPtr(video_processor);
        let rotated_texture = ComPtr(ptr::null_mut());
        Rotate {
            video_context,
            video_device,
            video_processor_enum,
            video_processor,
            texture: (rotated_texture, false),
        }
    }

    pub fn is_gdi(&self) -> bool {
        self.gdi_capturer.is_some()
    }

    pub fn set_gdi(&mut self) -> bool {
        self.gdi_capturer = self.display.create_gdi();
        self.is_gdi()
    }

    pub fn cancel_gdi(&mut self) {
        self.gdi_buffer = Vec::new();
        self.gdi_capturer.take();
    }

    #[cfg(feature = "vram")]
    pub fn set_output_texture(&mut self, texture: bool) {
        self.output_texture = texture;
    }

    unsafe fn load_frame(&mut self, timeout: UINT) -> io::Result<(*const u8, i32)> {
        if self.duplication.0.is_null() {
            return Err(io::ErrorKind::AddrNotAvailable.into());
        }
        let mut frame = ptr::null_mut();
        #[allow(invalid_value)]
        let mut info = mem::MaybeUninit::uninit().assume_init();

        wrap_hresult((*self.duplication.0).AcquireNextFrame(timeout, &mut info, &mut frame))?;
        self.frame_state = FrameState::Acquired;
        let frame = ComPtr(frame);

        if *info.LastPresentTime.QuadPart() == 0 {
            return Err(std::io::ErrorKind::WouldBlock.into());
        }

        #[allow(invalid_value)]
        let mut rect = mem::MaybeUninit::uninit().assume_init();
        if self.fastlane {
            wrap_hresult((*self.duplication.0).MapDesktopSurface(&mut rect))?;
            self.frame_state = FrameState::Mapped;
        } else {
            self.surface = ComPtr(self.ohgodwhat(frame.0)?);
            wrap_hresult((*self.surface.0).Map(&mut rect, DXGI_MAP_READ))?;
            self.frame_state = FrameState::Mapped;
        }
        Ok((rect.pBits, rect.Pitch))
    }

    // 从 GPU 内存复制到系统内存
    unsafe fn ohgodwhat(&mut self, frame: *mut IDXGIResource) -> io::Result<*mut IDXGISurface> {
        let mut texture: *mut ID3D11Texture2D = ptr::null_mut();
        (*frame).QueryInterface(
            &IID_ID3D11Texture2D,
            &mut texture as *mut *mut _ as *mut *mut _,
        );
        let texture = ComPtr(texture);

        #[allow(invalid_value)]
        let mut texture_desc = mem::MaybeUninit::uninit().assume_init();
        (*texture.0).GetDesc(&mut texture_desc);

        let mut source = texture.0;
        if texture_desc.Format == DXGI_FORMAT_R16G16B16A16_FLOAT {
            source = self.tonemap(texture.0, &texture_desc)?;
            (*source).GetDesc(&mut texture_desc);
        }

        texture_desc.Usage = D3D11_USAGE_STAGING;
        texture_desc.BindFlags = 0;
        texture_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
        texture_desc.MiscFlags = 0;

        let mut readable = ptr::null_mut();
        wrap_hresult((*self.device.0).CreateTexture2D(
            &mut texture_desc,
            ptr::null(),
            &mut readable,
        ))?;
        (*readable).SetEvictionPriority(DXGI_RESOURCE_PRIORITY_MAXIMUM);
        let readable = ComPtr(readable);

        let mut surface = ptr::null_mut();
        (*readable.0).QueryInterface(
            &IID_IDXGISurface,
            &mut surface as *mut *mut _ as *mut *mut _,
        );

        (*self.context.0).CopyResource(readable.0 as *mut _, source as *mut _);

        Ok(surface)
    }

    pub fn frame<'a>(&'a mut self, timeout: UINT) -> io::Result<Frame<'a>> {
        if self.output_texture {
            Ok(Frame::Texture(self.get_texture(timeout)?))
        } else {
            let width = self.width;
            let height = self.height;
            Ok(Frame::PixelBuffer(PixelBuffer::with_BGRA(
                self.get_pixelbuffer(timeout)?,
                width,
                height,
            )))
        }
    }

    fn get_pixelbuffer<'a>(&'a mut self, timeout: UINT) -> io::Result<&'a [u8]> {
        unsafe {
            // 释放上一帧。
            // 不需要检查错误，因为我们并不在意。
            // 反正这些错误都不会导致崩溃。
            let result = {
                if let Some(gdi_capturer) = &self.gdi_capturer {
                    match gdi_capturer.frame(&mut self.gdi_buffer) {
                        Ok(_) => {
                            crate::would_block_if_equal(
                                &mut self.saved_raw_data,
                                &self.gdi_buffer,
                            )?;
                            &self.gdi_buffer
                        }
                        Err(err) => {
                            return Err(io::Error::new(io::ErrorKind::Other, err.to_string()));
                        }
                    }
                } else {
                    self.release_frame()?;
                    let r = self.load_frame(timeout)?;
                    let rotate = match self.display.rotation() {
                        DXGI_MODE_ROTATION_IDENTITY | DXGI_MODE_ROTATION_UNSPECIFIED => kRotate0,
                        DXGI_MODE_ROTATION_ROTATE90 => kRotate90,
                        DXGI_MODE_ROTATION_ROTATE180 => kRotate180,
                        DXGI_MODE_ROTATION_ROTATE270 => kRotate270,
                        _ => {
                            return Err(io::Error::new(
                                io::ErrorKind::Other,
                                "Unknown rotation".to_string(),
                            ));
                        }
                    };
                    if rotate == kRotate0 {
                        slice::from_raw_parts(r.0, r.1 as usize * self.height)
                    } else {
                        self.rotated.resize(self.width * self.height * 4, 0);
                        crate::common::ARGBRotate(
                            r.0,
                            r.1,
                            self.rotated.as_mut_ptr(),
                            4 * self.width as i32,
                            if rotate == kRotate180 {
                                self.width
                            } else {
                                self.height
                            } as _,
                            if rotate != kRotate180 {
                                self.width
                            } else {
                                self.height
                            } as _,
                            rotate,
                        );
                        &self.rotated[..]
                    }
                }
            };
            Ok(result)
        }
    }

    fn get_texture(&mut self, timeout: UINT) -> io::Result<(*mut c_void, usize)> {
        unsafe {
            if self.duplication.0.is_null() {
                return Err(std::io::ErrorKind::AddrNotAvailable.into());
            }
            self.release_frame()?;
            let mut frame = ptr::null_mut();
            #[allow(invalid_value)]
            let mut info = mem::MaybeUninit::uninit().assume_init();

            wrap_hresult((*self.duplication.0).AcquireNextFrame(timeout, &mut info, &mut frame))?;
            self.frame_state = FrameState::Acquired;
            let frame = ComPtr(frame);

            if info.AccumulatedFrames == 0 || *info.LastPresentTime.QuadPart() == 0 {
                return Err(std::io::ErrorKind::WouldBlock.into());
            }

            let mut texture: *mut ID3D11Texture2D = ptr::null_mut();
            (*frame.0).QueryInterface(
                &IID_ID3D11Texture2D,
                &mut texture as *mut *mut _ as *mut *mut _,
            );
            let texture = ComPtr(texture);
            self.texture = texture;

            let mut frame_desc: D3D11_TEXTURE2D_DESC = mem::zeroed();
            (*self.texture.0).GetDesc(&mut frame_desc);
            if frame_desc.Format == DXGI_FORMAT_R16G16B16A16_FLOAT {
                let converted = self.tonemap(self.texture.0, &frame_desc)?;
                (*converted).AddRef();
                self.texture = ComPtr(converted);
            }

            let mut final_texture = self.texture.0 as *mut c_void;
            let mut rotation = match self.display.rotation() {
                DXGI_MODE_ROTATION_ROTATE90 => 90,
                DXGI_MODE_ROTATION_ROTATE180 => 180,
                DXGI_MODE_ROTATION_ROTATE270 => 270,
                _ => 0,
            };
            if rotation != 0
                && !self.texture.is_null()
                && !self.rotate.video_context.is_null()
                && !self.rotate.video_device.is_null()
                && !self.rotate.video_processor_enum.is_null()
                && !self.rotate.video_processor.is_null()
            {
                let mut desc: D3D11_TEXTURE2D_DESC = mem::zeroed();
                (*self.texture.0).GetDesc(&mut desc);
                if rotation == 90 || rotation == 270 {
                    let tmp = desc.Width;
                    desc.Width = desc.Height;
                    desc.Height = tmp;
                }
                if !self.rotate.texture.1 {
                    self.rotate.texture.1 = true;
                    let mut rotated_texture: *mut ID3D11Texture2D = ptr::null_mut();
                    desc.MiscFlags = D3D11_RESOURCE_MISC_SHARED;
                    (*self.device.0).CreateTexture2D(&desc, ptr::null(), &mut rotated_texture);
                    self.rotate.texture.0 = ComPtr(rotated_texture);
                }
                if !self.rotate.texture.0.is_null()
                    && desc.Width == self.width as u32
                    && desc.Height == self.height as u32
                {
                    let input_view_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                        FourCC: 0,
                        ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                        Texture2D: D3D11_TEX2D_VPIV {
                            ArraySlice: 0,
                            MipSlice: 0,
                        },
                    };
                    let mut input_view = ptr::null_mut();
                    (*self.rotate.video_device.0).CreateVideoProcessorInputView(
                        self.texture.0 as *mut _,
                        self.rotate.video_processor_enum.0 as *mut _,
                        &input_view_desc,
                        &mut input_view,
                    );
                    if !input_view.is_null() {
                        let input_view = ComPtr(input_view);
                        let mut output_view_desc: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC =
                            mem::zeroed();
                        output_view_desc.ViewDimension = D3D11_VPOV_DIMENSION_TEXTURE2D;
                        output_view_desc.u.Texture2D_mut().MipSlice = 0;
                        let mut output_view = ptr::null_mut();
                        (*self.rotate.video_device.0).CreateVideoProcessorOutputView(
                            self.rotate.texture.0 .0 as *mut _,
                            self.rotate.video_processor_enum.0 as *mut _,
                            &output_view_desc,
                            &mut output_view,
                        );
                        if !output_view.is_null() {
                            let output_view = ComPtr(output_view);
                            let mut stream_data: D3D11_VIDEO_PROCESSOR_STREAM = mem::zeroed();
                            stream_data.Enable = TRUE;
                            stream_data.pInputSurface = input_view.0;
                            (*self.rotate.video_context.0).VideoProcessorBlt(
                                self.rotate.video_processor.0,
                                output_view.0,
                                0,
                                1,
                                &stream_data,
                            );
                            final_texture = self.rotate.texture.0 .0 as *mut c_void;
                            rotation = 0;
                        }
                    }
                }
            }
            Ok((final_texture, rotation))
        }
    }

    fn release_frame(&mut self) -> io::Result<()> {
        if self.duplication.is_null() {
            return Ok(());
        }
        let mut first_error = None;
        // 在 ReleaseFrame 使桌面表面失效之前先 Unmap；对 staging 表面也用同样的
        // 顺序。清理过程按 Mapped -> Acquired -> Idle 推进，
        // 其中 Idle 是空操作。即使出错也要推进状态，以免重复尝试
        // 清理，但如果 unmap 失败，仍然要尝试 ReleaseFrame。
        unsafe {
            if self.frame_state == FrameState::Mapped {
                let result = if self.fastlane {
                    wrap_hresult((*self.duplication.0).UnMapDesktopSurface())
                } else if !self.surface.is_null() {
                    wrap_hresult((*self.surface.0).Unmap())
                } else {
                    Ok(())
                };
                self.frame_state = FrameState::Acquired;
                if let Err(err) = result {
                    first_error = Some(err);
                }
            }
            if self.frame_state == FrameState::Acquired {
                let result = wrap_hresult((*self.duplication.0).ReleaseFrame());
                self.frame_state = FrameState::Idle;
                if first_error.is_none() {
                    if let Err(err) = result {
                        first_error = Some(err);
                    }
                }
            }
        }
        match first_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    pub fn device(&self) -> AdapterDevice {
        AdapterDevice {
            device: self.device.0 as _,
            vendor_id: self.adapter_desc1.VendorId,
            luid: ((self.adapter_desc1.AdapterLuid.HighPart as i64) << 32)
                | self.adapter_desc1.AdapterLuid.LowPart as i64,
        }
    }
}

impl Drop for Capturer {
    fn drop(&mut self) {
        if let Err(err) = self.release_frame() {
            eprintln!("DXGI frame cleanup failed: {err}");
        }
    }
}

pub struct Displays {
    factory: ComPtr<IDXGIFactory1>,
    adapter: ComPtr<IDXGIAdapter1>,
    /// 当前适配器的索引。
    nadapter: UINT,
    /// 下一个要获取的显示器的索引。
    ndisplay: UINT,
}

impl Displays {
    pub fn new() -> io::Result<Displays> {
        let mut factory = ptr::null_mut();
        wrap_hresult(unsafe { CreateDXGIFactory1(&IID_IDXGIFactory1, &mut factory) })?;

        let factory = factory as *mut IDXGIFactory1;
        let mut adapter = ptr::null_mut();
        unsafe {
            // 出错时我们的适配器为空，所以这样没问题。
            (*factory).EnumAdapters1(0, &mut adapter);
        };

        Ok(Displays {
            factory: ComPtr(factory),
            adapter: ComPtr(adapter),
            nadapter: 0,
            ndisplay: 0,
        })
    }

    pub fn get_from_gdi() -> Vec<Display> {
        let mut all = Vec::new();
        let mut i: DWORD = 0;
        loop {
            #[allow(invalid_value)]
            let mut d: DISPLAY_DEVICEW = unsafe { std::mem::MaybeUninit::uninit().assume_init() };
            d.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as _;
            let ok = unsafe { EnumDisplayDevicesW(std::ptr::null(), i, &mut d as _, 0) };
            if ok == FALSE {
                break;
            }
            i += 1;
            if 0 == (d.StateFlags & DISPLAY_DEVICE_ACTIVE)
                || (d.StateFlags & DISPLAY_DEVICE_MIRRORING_DRIVER) > 0
            {
                continue;
            }
            // let is_primary = (d.StateFlags & DISPLAY_DEVICE_PRIMARY_DEVICE) > 0;
            let mut disp = Display {
                inner: ComPtr(std::ptr::null_mut()),
                adapter: ComPtr(std::ptr::null_mut()),
                desc: unsafe { std::mem::zeroed() },
                gdi: true,
            };
            disp.desc.DeviceName = d.DeviceName;
            #[allow(invalid_value)]
            let mut m: DEVMODEW = unsafe { std::mem::MaybeUninit::uninit().assume_init() };
            m.dmSize = std::mem::size_of::<DEVMODEW>() as _;
            m.dmDriverExtra = 0;
            let ok = unsafe {
                EnumDisplaySettingsExW(
                    disp.desc.DeviceName.as_ptr(),
                    ENUM_CURRENT_SETTINGS,
                    &mut m as _,
                    0,
                )
            };
            if ok == FALSE {
                continue;
            }
            disp.desc.DesktopCoordinates.left = unsafe { m.u1.s2().dmPosition.x };
            disp.desc.DesktopCoordinates.top = unsafe { m.u1.s2().dmPosition.y };
            disp.desc.DesktopCoordinates.right =
                disp.desc.DesktopCoordinates.left + m.dmPelsWidth as i32;
            disp.desc.DesktopCoordinates.bottom =
                disp.desc.DesktopCoordinates.top + m.dmPelsHeight as i32;
            disp.desc.AttachedToDesktop = 1;
            all.push(disp);
        }
        all
    }

    // 没有适配器 => Some(None)
    // 非空适配器 => Some(Some(OUTPUT))
    // 适配器结束 => None
    fn read_and_invalidate(&mut self) -> Option<Option<Display>> {
        // 如果没有适配器，我们就没什么可做的了。

        if self.adapter.is_null() {
            return Some(None);
        }

        // 否则，我们取当前适配器的下一个输出。

        let output = unsafe {
            let mut output = ptr::null_mut();
            (*self.adapter.0).EnumOutputs(self.ndisplay, &mut output);
            ComPtr(output)
        };

        // 如果当前适配器已遍历完，就释放它。
        // 我们返回 None，让调用方去取下一个适配器并重试。

        if output.is_null() {
            self.adapter = ComPtr(ptr::null_mut());
            return None;
        }

        // 前进到下一个显示器。

        self.ndisplay += 1;

        // 我们获取该显示器的详细信息。

        let desc = unsafe {
            #[allow(invalid_value)]
            let mut desc = mem::MaybeUninit::uninit().assume_init();
            (*output.0).GetDesc(&mut desc);
            desc
        };

        // 我们把它向上转型到桌面复制所需的版本。

        let mut inner: *mut IDXGIOutput1 = ptr::null_mut();
        unsafe {
            (*output.0).QueryInterface(&IID_IDXGIOutput1, &mut inner as *mut *mut _ as *mut *mut _);
        }

        // 如果它为空，说明出错了。
        // 所以我们就当作适配器已经遍历完了。

        if inner.is_null() {
            self.adapter = ComPtr(ptr::null_mut());
            return None;
        }

        unsafe {
            (*self.adapter.0).AddRef();
        }

        Some(Some(Display {
            inner: ComPtr(inner),
            adapter: ComPtr(self.adapter.0),
            desc,
            gdi: false,
        }))
    }
}

impl Iterator for Displays {
    type Item = Display;
    fn next(&mut self) -> Option<Display> {
        if let Some(res) = self.read_and_invalidate() {
            res
        } else {
            // 我们需要替换这个适配器。

            self.ndisplay = 0;
            self.nadapter += 1;

            self.adapter = unsafe {
                let mut adapter = ptr::null_mut();
                (*self.factory.0).EnumAdapters1(self.nadapter, &mut adapter);
                ComPtr(adapter)
            };

            if let Some(res) = self.read_and_invalidate() {
                res
            } else {
                // 后续所有适配器也会是空的。
                None
            }
        }
    }
}

pub struct Display {
    inner: ComPtr<IDXGIOutput1>,
    adapter: ComPtr<IDXGIAdapter1>,
    desc: DXGI_OUTPUT_DESC,
    gdi: bool,
}

// 针对更新区域做了优化
// https://github.com/dchapyshev/aspia/blob/master/source/base/desktop/win/dxgi_output_duplicator.cc
// rotation
// https://github.com/bryal/dxgcap-rs/blob/master/src/lib.rs

impl Display {
    pub fn width(&self) -> LONG {
        self.desc.DesktopCoordinates.right - self.desc.DesktopCoordinates.left
    }

    pub fn height(&self) -> LONG {
        self.desc.DesktopCoordinates.bottom - self.desc.DesktopCoordinates.top
    }

    pub fn attached_to_desktop(&self) -> bool {
        self.desc.AttachedToDesktop != 0
    }

    pub fn rotation(&self) -> DXGI_MODE_ROTATION {
        self.desc.Rotation
    }

    fn create_gdi(&self) -> Option<CapturerGDI> {
        if let Ok(res) = CapturerGDI::new(self.name(), self.width(), self.height()) {
            Some(res)
        } else {
            None
        }
    }

    pub fn hmonitor(&self) -> HMONITOR {
        self.desc.Monitor
    }

    pub fn name(&self) -> &[u16] {
        let s = &self.desc.DeviceName;
        let i = s.iter().position(|&x| x == 0).unwrap_or(s.len());
        &s[..i]
    }

    pub fn is_online(&self) -> bool {
        self.desc.AttachedToDesktop != 0
    }

    pub fn origin(&self) -> (LONG, LONG) {
        (
            self.desc.DesktopCoordinates.left,
            self.desc.DesktopCoordinates.top,
        )
    }

    #[cfg(feature = "vram")]
    pub fn adapter_luid(&self) -> Option<i64> {
        unsafe {
            if !self.adapter.is_null() {
                #[allow(invalid_value)]
                let mut adapter_desc1 = mem::MaybeUninit::uninit().assume_init();
                if wrap_hresult((*self.adapter.0).GetDesc1(&mut adapter_desc1)).is_ok() {
                    let luid = ((adapter_desc1.AdapterLuid.HighPart as i64) << 32)
                        | adapter_desc1.AdapterLuid.LowPart as i64;
                    return Some(luid);
                }
            }
            None
        }
    }
}

fn wrap_hresult(x: HRESULT) -> io::Result<()> {
    use std::io::ErrorKind::*;
    Err((match x {
        S_OK => return Ok(()),
        DXGI_ERROR_ACCESS_LOST => ConnectionReset,
        DXGI_ERROR_WAIT_TIMEOUT => TimedOut,
        DXGI_ERROR_INVALID_CALL => InvalidData,
        E_ACCESSDENIED => PermissionDenied,
        DXGI_ERROR_UNSUPPORTED => ConnectionRefused,
        DXGI_ERROR_NOT_CURRENTLY_AVAILABLE => Interrupted,
        DXGI_ERROR_SESSION_DISCONNECTED => ConnectionAborted,
        E_INVALIDARG => InvalidInput,
        _ => {
            // 0x8000ffff https://www.auslogics.com/en/articles/windows-10-update-error-0x8000ffff-fixed/
            return Err(io::Error::new(Other, format!("Error code: {:#X}", x)));
        }
    })
    .into())
}

struct Rotate {
    video_context: ComPtr<ID3D11VideoContext>,
    video_device: ComPtr<ID3D11VideoDevice>,
    video_processor_enum: ComPtr<ID3D11VideoProcessorEnumerator>,
    video_processor: ComPtr<ID3D11VideoProcessor>,
    texture: (ComPtr<ID3D11Texture2D>, bool),
}
