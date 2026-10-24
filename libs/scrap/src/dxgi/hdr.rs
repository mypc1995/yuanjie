//! 把桌面复制（Desktop Duplication）得到的帧从 HDR 桌面归一化到 SDR。
//!
//! 开启 HDR 后，Windows 会以线性 scRGB、
//! R16G16B16A16_FLOAT 的形式合成桌面，而 SDR 的“白色”位于用户 SDR 内容的
//! 亮度（DISPLAYCONFIG_SDR_WHITE_LEVEL）上，而不是 1.0。旧的
//! DuplicateOutput 会通过裁剪把它转成 BGRA8，这正是
//! HDR 主机上所报告的灰暗画面的来源。这一步用 SDR 白点电平做除法，
//! 再做截断，并应用 sRGB 传递函数，于是 SDR 内容的输出效果完全
//! 等同于在 SDR 桌面上得到的效果。
//!
//! 这是归一化，而不是色调映射：任何比 SDR 白色更亮的内容
//! （HDR 视频、HDR 游戏）在 SDR 观看端会被裁剪为纯白，而在本地的
//! HDR 显示器上本可以显示得比白色更亮。而做 roll-off 就必须
//! 把 SDR 白色降到 1.0 以下来腾出余量，这会牺牲这一步所追求的 SDR
//! 内容的准确性，因此刻意不做。
//!
//! Windows 11 22H2 也会以 FP16 合成 Advanced Color SDR（WCG）桌面，
//! 但那时 1.0 是显示器的参考白，而不是 80 尼特，也不存在
//! SDR 白点电平这回事。IDXGIOutput6 能把两者区分开，而对于
//! 非 HDR 输出，这一步只应用 scRGB -> sRGB 传递函数。
//!
//! 这个转换是自动进行的，而且刻意只留在可控范围内：
//! 控制器通过 Flutter external texture 渲染，而它在所有桌面平台上都是 8-bit
//! 的，所以传送 HDR 没有收益。
//! 如果渲染器将来支持真正的 HDR 直通，应当遵循
//! Sunshine/Moonlight 的做法：由控制器在明确的用户开关之后声明一个 `hdr` 能力位，
//! 并像 i444 一样协商。

use super::ComPtr;
use hbb_common::log;
use std::{
    io, mem, ptr,
    sync::{atomic::AtomicBool, OnceLock},
    time::{Duration, Instant},
};
use winapi::{
    ctypes::c_void,
    shared::{
        basetsd::SIZE_T,
        dxgi::{CreateDXGIFactory1, IDXGIFactory1, IID_IDXGIFactory1, DXGI_OUTPUT_DESC},
        dxgi1_2::IDXGIOutput1,
        dxgi1_6::{IDXGIOutput6, IID_IDXGIOutput6, DXGI_OUTPUT_DESC1},
        dxgiformat::DXGI_FORMAT_B8G8R8A8_UNORM,
        dxgitype::{DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020, DXGI_SAMPLE_DESC},
        minwindef::{FALSE, LPCVOID, UINT, ULONG},
        ntdef::{LONG, LPCSTR, WCHAR},
        winerror::S_OK,
    },
    um::{
        d3d11::*,
        d3dcommon::{ID3DBlob, ID3DInclude, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D_SHADER_MACRO},
        libloaderapi::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32},
        unknwnbase::IUnknown,
        wingdi::{
            DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_HEADER,
            DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
            DISPLAYCONFIG_TOPOLOGY_ID,
        },
        winnt::HRESULT,
    },
};

/// 当色调映射在这个进程里已不可能生效时（没有 d3dcompiler，
/// 着色器无法编译）就设置一次。此后各捕获器就不再向 DXGI 请求浮点帧了。
/// 设备特定的失败不在这里记录；遇到这类失败的捕获器
/// 会自行在不做色调映射的情况下重新复制。
pub static UNAVAILABLE: AtomicBool = AtomicBool::new(false);

/// 本机上任何捕获器都无法恢复的失败，与
/// 重建后的捕获器可能不会再遇到的设备特定失败不同。
pub fn is_permanent(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::Unsupported
}

const VS_SRC: &str = "\
float4 main(uint id : SV_VertexID) : SV_Position {
    float2 uv = float2((id << 1) & 2, id & 2);
    return float4(uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
}";

const PS_SRC: &str = "\
Texture2D<float4> src : register(t0);
cbuffer Params : register(b0) { float inv_sdr_white; float3 pad; };
float4 main(float4 pos : SV_Position) : SV_Target {
    float3 lin = saturate(src.Load(int3(pos.xy, 0)).rgb * inv_sdr_white);
    float3 lo = lin * 12.92;
    float3 hi = 1.055 * pow(lin, 1.0 / 2.4) - 0.055;
    return float4(lerp(hi, lo, step(lin, 0.0031308)), 1.0);
}";

const OUTPUT_STATE_REFRESH: Duration = Duration::from_secs(1);

pub struct HdrToSdr {
    device: ComPtr<ID3D11Device>,
    context: ComPtr<ID3D11DeviceContext>,
    vs: ComPtr<ID3D11VertexShader>,
    ps: ComPtr<ID3D11PixelShader>,
    params: ComPtr<ID3D11Buffer>,
    target: ComPtr<ID3D11Texture2D>,
    rtv: ComPtr<ID3D11RenderTargetView>,
    srv: ComPtr<ID3D11ShaderResourceView>,
    // 创建纹理 `srv` 时所用的。视图会保持它的存活，因此该地址
    // 不会在我们背后被回收再用。
    srv_source: *mut ID3D11Texture2D,
    width: u32,
    height: u32,
    device_name: [WCHAR; 32],
    // Advanced Color 状态是从 `output6` 读取的，而每当旧的 factory 不再是当前 factory 时，就会
    // 从一个新的 `factory` 重新枚举。
    factory: ComPtr<IDXGIFactory1>,
    output6: ComPtr<IDXGIOutput6>,
    is_hdr: bool,
    // 以 DISPLAYCONFIG 单位表示（1000 == 80 尼特 == scRGB 1.0）。读取失败时为 `None`，
    // 此时在能读到之前按 80 尼特处理。
    sdr_white_level: Option<u32>,
    queried_at: Instant,
}

impl HdrToSdr {
    pub fn new(
        device: *mut ID3D11Device,
        context: *mut ID3D11DeviceContext,
        output: *mut IDXGIOutput1,
        device_name: &[WCHAR; 32],
    ) -> io::Result<Self> {
        unsafe {
            if device.is_null() || context.is_null() {
                return Err(other("no d3d11 device"));
            }
            (*device).AddRef();
            let device = ComPtr(device);
            (*context).AddRef();
            let context = ComPtr(context);

            let compile = load_d3d_compile()?;
            let vs_code = compile_shader(compile, VS_SRC, b"vs_4_0\0")?;
            let ps_code = compile_shader(compile, PS_SRC, b"ps_4_0\0")?;
            let mut vs = ptr::null_mut();
            check(
                (*device.0).CreateVertexShader(
                    (*vs_code.0).GetBufferPointer(),
                    (*vs_code.0).GetBufferSize(),
                    ptr::null_mut(),
                    &mut vs,
                ),
                "CreateVertexShader",
            )?;
            let vs = ComPtr(vs);
            let mut ps = ptr::null_mut();
            check(
                (*device.0).CreatePixelShader(
                    (*ps_code.0).GetBufferPointer(),
                    (*ps_code.0).GetBufferSize(),
                    ptr::null_mut(),
                    &mut ps,
                ),
                "CreatePixelShader",
            )?;
            let ps = ComPtr(ps);

            // 未找到时会让 factory 保持为空，因此首次刷新会重新枚举，
            // 而不是信任捕获器里可能已经过期的输出。
            let (factory, mut output6) = enumerate_output6(device_name);
            if output6.is_null() {
                output6 = query_output6(output as *mut IUnknown);
            }
            // 只有在存在 IDXGIOutput6 的地方才会请求浮点帧，因此一个
            // 读不出描述信息的输出仍然来自支持 HDR 的技术栈。
            let is_hdr = output_is_hdr(output6.0).unwrap_or(true);
            let sdr_white_level = if is_hdr {
                query_sdr_white_level(device_name)
            } else {
                None
            };
            if is_hdr && sdr_white_level.is_none() {
                log::warn!(
                    "HDR output but the SDR white level cannot be read (needs Windows 10 1709+), \
                     assuming 80 nits until it can"
                );
            }
            let init = params_data(sdr_white_level);
            let desc = D3D11_BUFFER_DESC {
                ByteWidth: mem::size_of_val(&init) as _,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER,
                CPUAccessFlags: 0,
                MiscFlags: 0,
                StructureByteStride: 0,
            };
            let data = D3D11_SUBRESOURCE_DATA {
                pSysMem: init.as_ptr() as _,
                SysMemPitch: 0,
                SysMemSlicePitch: 0,
            };
            let mut params = ptr::null_mut();
            check(
                (*device.0).CreateBuffer(&desc, &data, &mut params),
                "CreateBuffer",
            )?;
            let params = ComPtr(params);
            log::info!(
                "scRGB desktop conversion ready, hdr {is_hdr}, sdr white level {sdr_white_level:?}"
            );

            Ok(Self {
                device,
                context,
                vs,
                ps,
                params,
                target: ComPtr(ptr::null_mut()),
                rtv: ComPtr(ptr::null_mut()),
                srv: ComPtr(ptr::null_mut()),
                srv_source: ptr::null_mut(),
                width: 0,
                height: 0,
                device_name: *device_name,
                factory,
                output6,
                is_hdr,
                sdr_white_level,
                queried_at: Instant::now(),
            })
        }
    }

    /// 把 `source`（R16G16B16A16_FLOAT）渲染到一张自持有的 B8G8R8A8_UNORM
    /// 纹理中（尺寸相同）并返回它。该纹理在下一次调用之前
    /// 保持有效。
    pub fn convert(
        &mut self,
        source: *mut ID3D11Texture2D,
        desc: &D3D11_TEXTURE2D_DESC,
    ) -> io::Result<*mut ID3D11Texture2D> {
        unsafe {
            self.refresh_output_state();
            self.ensure_target(desc.Width, desc.Height)?;
            self.ensure_source_view(source)?;

            let ctx = self.context.0;
            let rtv = self.rtv.0;
            let srv = self.srv.0;
            let params = self.params.0;
            let viewport = D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: self.width as f32,
                Height: self.height as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            (*ctx).OMSetRenderTargets(1, &rtv, ptr::null_mut());
            (*ctx).OMSetBlendState(ptr::null_mut(), &[0.0; 4], 0xffff_ffff);
            (*ctx).OMSetDepthStencilState(ptr::null_mut(), 0);
            (*ctx).RSSetState(ptr::null_mut());
            (*ctx).RSSetViewports(1, &viewport);
            (*ctx).IASetInputLayout(ptr::null_mut());
            (*ctx).IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            (*ctx).VSSetShader(self.vs.0, ptr::null(), 0);
            (*ctx).PSSetShader(self.ps.0, ptr::null(), 0);
            (*ctx).PSSetConstantBuffers(0, 1, &params);
            (*ctx).PSSetShaderResources(0, 1, &srv);
            (*ctx).Draw(3, 0);
            // 解除绑定，这样下一帧的拷贝和编码器都不会看到
            // 目标被当作活动渲染目标，或桌面图像被当作已绑定的
            // 着色器输入。
            let no_srv: *mut ID3D11ShaderResourceView = ptr::null_mut();
            (*ctx).PSSetShaderResources(0, 1, &no_srv);
            (*ctx).OMSetRenderTargets(0, ptr::null(), ptr::null_mut());
            Ok(self.target.0)
        }
    }

    unsafe fn ensure_target(&mut self, width: u32, height: u32) -> io::Result<()> {
        if !self.target.is_null() && self.width == width && self.height == height {
            return Ok(());
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE,
            CPUAccessFlags: 0,
            MiscFlags: D3D11_RESOURCE_MISC_SHARED,
        };
        let mut target = ptr::null_mut();
        check(
            (*self.device.0).CreateTexture2D(&desc, ptr::null(), &mut target),
            "CreateTexture2D",
        )?;
        let target = ComPtr(target);
        let mut rtv = ptr::null_mut();
        check(
            (*self.device.0).CreateRenderTargetView(target.0 as *mut _, ptr::null(), &mut rtv),
            "CreateRenderTargetView",
        )?;
        self.rtv = ComPtr(rtv);
        self.target = target;
        self.width = width;
        self.height = height;
        Ok(())
    }

    unsafe fn ensure_source_view(&mut self, source: *mut ID3D11Texture2D) -> io::Result<()> {
        if !self.srv.is_null() && self.srv_source == source {
            return Ok(());
        }
        let mut srv = ptr::null_mut();
        check(
            (*self.device.0).CreateShaderResourceView(source as *mut _, ptr::null(), &mut srv),
            "CreateShaderResourceView",
        )?;
        self.srv = ComPtr(srv);
        self.srv_source = source;
        Ok(())
    }

    // Advanced Color 状态是动态的：HDR 可以被打开或关闭，WCG
    // 桌面也可能变成 HDR 桌面，而复制不会因此丢失。输出
    // 的描述信息只是一个快照，所以一旦 factory 不再是当前
    // factory，就需要新的 factory 和输出才能看到新状态，正如
    // GetDesc1 文档所要求的那样。
    unsafe fn refresh_output_state(&mut self) {
        if self.queried_at.elapsed() < OUTPUT_STATE_REFRESH {
            return;
        }
        self.queried_at = Instant::now();
        if self.factory.is_null() || (*self.factory.0).IsCurrent() == FALSE {
            let (factory, output6) = enumerate_output6(&self.device_name);
            if !output6.is_null() {
                self.factory = factory;
                self.output6 = output6;
            } else {
                // 继续读取旧的输出，但下次重新枚举。
                self.factory = ComPtr(ptr::null_mut());
            }
        }
        let is_hdr = output_is_hdr(self.output6.0).unwrap_or(self.is_hdr);
        let level = if is_hdr {
            query_sdr_white_level(&self.device_name)
        } else {
            None
        };
        // 电平暂时读不到时保留上一次已知的值。
        if is_hdr == self.is_hdr && (level.is_none() || level == self.sdr_white_level) {
            return;
        }
        log::info!(
            "output changed: hdr {} -> {is_hdr}, sdr white level {:?} -> {level:?}",
            self.is_hdr,
            self.sdr_white_level
        );
        if is_hdr && level.is_none() {
            log::warn!(
                "HDR output but the SDR white level cannot be read, assuming 80 nits until it can"
            );
        }
        self.is_hdr = is_hdr;
        self.sdr_white_level = level;
        let data = params_data(level);
        (*self.context.0).UpdateSubresource(
            self.params.0 as *mut _,
            0,
            ptr::null(),
            data.as_ptr() as _,
            0,
            0,
        );
    }
}

// `None` 表示要么是非 HDR（WCG）输出（那里 1.0 本身就是显示器的
// 参考白），要么是电平未知的 HDR 输出；两者都按 1.0 处理。
fn params_data(sdr_white_level: Option<u32>) -> [f32; 4] {
    [
        1000.0 / sdr_white_level.unwrap_or(1000) as f32,
        0.0,
        0.0,
        0.0,
    ]
}

// 以 FP16 合成桌面要么意味着 HDR（场景参考，1.0 == 80 尼特），
// 要么意味着（自 Windows 11 22H2 起）Advanced Color SDR（显示参考），而且只有
// IDXGIOutput6（Windows 10 1703）才能把两者区分开。无法读取时为 `None`，
// 此刻就读不到。
unsafe fn output_is_hdr(output6: *mut IDXGIOutput6) -> Option<bool> {
    if output6.is_null() {
        return None;
    }
    let mut desc: DXGI_OUTPUT_DESC1 = mem::zeroed();
    if (*output6).GetDesc1(&mut desc) != S_OK {
        return None;
    }
    Some(desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020)
}

unsafe fn query_output6(object: *mut IUnknown) -> ComPtr<IDXGIOutput6> {
    let mut output6: *mut IDXGIOutput6 = ptr::null_mut();
    if !object.is_null() {
        (*object).QueryInterface(
            &IID_IDXGIOutput6,
            &mut output6 as *mut *mut _ as *mut *mut _,
        );
    }
    ComPtr(output6)
}

// 新的 factory 能看到当前的显示配置；输出是通过
// GDI 名称查找的，因为过期 factory 的输出仍带着过期的描述信息。
// 两者要么都返回要么都不返回：非空的 factory 保证该输出来自
// 它的拓扑，因此看到空 factory 的调用方知道需要重新枚举。
unsafe fn enumerate_output6(
    device_name: &[WCHAR; 32],
) -> (ComPtr<IDXGIFactory1>, ComPtr<IDXGIOutput6>) {
    let mut factory: *mut c_void = ptr::null_mut();
    if CreateDXGIFactory1(&IID_IDXGIFactory1, &mut factory) != S_OK {
        return (ComPtr(ptr::null_mut()), ComPtr(ptr::null_mut()));
    }
    let factory = ComPtr(factory as *mut IDXGIFactory1);
    let mut adapter_index = 0;
    loop {
        let mut adapter = ptr::null_mut();
        if (*factory.0).EnumAdapters1(adapter_index, &mut adapter) != S_OK {
            break;
        }
        let adapter = ComPtr(adapter);
        adapter_index += 1;
        let mut output_index = 0;
        loop {
            let mut output = ptr::null_mut();
            if (*adapter.0).EnumOutputs(output_index, &mut output) != S_OK {
                break;
            }
            let output = ComPtr(output);
            output_index += 1;
            let mut desc: DXGI_OUTPUT_DESC = mem::zeroed();
            if (*output.0).GetDesc(&mut desc) == S_OK && wide_eq(&desc.DeviceName, device_name) {
                let output6 = query_output6(output.0 as *mut IUnknown);
                if output6.is_null() {
                    return (ComPtr(ptr::null_mut()), ComPtr(ptr::null_mut()));
                }
                return (factory, output6);
            }
        }
    }
    (ComPtr(ptr::null_mut()), ComPtr(ptr::null_mut()))
}

fn other(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::Other, msg.into())
}

fn check(hr: HRESULT, what: &str) -> io::Result<()> {
    if hr == S_OK {
        Ok(())
    } else {
        Err(other(format!("{what} failed: {hr:#x}")))
    }
}

// D3DCompile(pSrcData, SrcDataSize, pSourceName, pDefines, pInclude,
//            pEntrypoint, pTarget, Flags1, Flags2, ppCode, ppErrorMsgs)
type D3DCompileFn = unsafe extern "system" fn(
    LPCVOID,
    SIZE_T,
    LPCSTR,
    * const D3D_SHADER_MACRO,
    * mut ID3DInclude,
    LPCSTR,
    LPCSTR,
    UINT,
    UINT,
    * mut *mut ID3DBlob,
    * mut *mut ID3DBlob,
) -> HRESULT;

static D3D_COMPILE: OnceLock<Result<D3DCompileFn, String>> = OnceLock::new();

// 每个进程只加载一次并保留：编译器 DLL 只在 HDR
// 桌面上才需要，而在导入期链接会让每次安装都依赖它。
fn load_d3d_compile() -> io::Result<D3DCompileFn> {
    D3D_COMPILE
        .get_or_init(|| unsafe { find_d3d_compile() })
        .clone()
        .map_err(|e| io::Error::new(io::ErrorKind::Unsupported, e))
}

unsafe fn find_d3d_compile() -> Result<D3DCompileFn, String> {
    let name: Vec<u16> = "d3dcompiler_47.dll\0".encode_utf16().collect();
    let module = LoadLibraryExW(name.as_ptr(), ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32);
    if module.is_null() {
        return Err("d3dcompiler_47.dll not available".into());
    }
    let f = GetProcAddress(module, b"D3DCompile\0".as_ptr() as _);
    if f.is_null() {
        return Err("D3DCompile not exported".into());
    }
    Ok(mem::transmute::<_, D3DCompileFn>(f))
}

unsafe fn compile_shader(
    compile: D3DCompileFn,
    src: &str,
    target: &[u8],
) -> io::Result<ComPtr<ID3DBlob>> {
    let mut code = ptr::null_mut();
    let mut errors = ptr::null_mut();
    let hr = compile(
        src.as_ptr() as _,
        src.len(),
        ptr::null(),
        ptr::null(),
        ptr::null_mut(),
        b"main\0".as_ptr() as _,
        target.as_ptr() as _,
        0,
        0,
        &mut code,
        &mut errors,
    );
    let errors = ComPtr(errors);
    if hr != S_OK || code.is_null() {
        let msg = if errors.is_null() {
            String::new()
        } else {
            let bytes = std::slice::from_raw_parts(
                (*errors.0).GetBufferPointer() as *const u8,
                (*errors.0).GetBufferSize(),
            );
            String::from_utf8_lossy(bytes).into_owned()
        };
        if !code.is_null() {
            (*(code as *mut IUnknown)).Release();
        }
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("D3DCompile failed: {hr:#x} {msg}"),
        ));
    }
    Ok(ComPtr(code))
}

const DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL: u32 = 11;
const QDC_ONLY_ACTIVE_PATHS: u32 = 2;

#[repr(C)]
#[allow(non_snake_case)]
struct DISPLAYCONFIG_SDR_WHITE_LEVEL {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    SDRWhiteLevel: ULONG,
}

#[link(name = "user32")]
extern "system" {
    fn GetDisplayConfigBufferSizes(
        flags: u32,
        numPathArrayElements: *mut u32,
        numModeInfoArrayElements: *mut u32,
    ) -> LONG;
    fn QueryDisplayConfig(
        flags: u32,
        numPathArrayElements: *mut u32,
        pathArray: *mut DISPLAYCONFIG_PATH_INFO,
        numModeInfoArrayElements: *mut u32,
        modeInfoArray: *mut DISPLAYCONFIG_MODE_INFO,
        currentTopologyId: *mut DISPLAYCONFIG_TOPOLOGY_ID,
    ) -> LONG;
    fn DisplayConfigGetDeviceInfo(requestPacket: *mut DISPLAYCONFIG_DEVICE_INFO_HEADER) -> LONG;
}

/// GDI 名称为 `device_name` 的那个输出的 SDR 白点电平
/// （例如 `\\.\DISPLAY1`），以 DISPLAYCONFIG 单位表示（1000 == 80 尼特）。`None`
/// 表示查询失败（Windows 10 1709 之前）或返回 0。
fn query_sdr_white_level(device_name: &[WCHAR; 32]) -> Option<u32> {
    unsafe {
        let mut n_paths = 0u32;
        let mut n_modes = 0u32;
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut n_paths, &mut n_modes) != 0 {
            return None;
        }
        let mut paths: Vec<DISPLAYCONFIG_PATH_INFO> = vec![mem::zeroed(); n_paths as usize];
        let mut modes: Vec<DISPLAYCONFIG_MODE_INFO> = vec![mem::zeroed(); n_modes as usize];
        if QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut n_paths,
            paths.as_mut_ptr(),
            &mut n_modes,
            modes.as_mut_ptr(),
            ptr::null_mut(),
        ) != 0
        {
            return None;
        }
        for path in &paths[..n_paths as usize] {
            let mut source: DISPLAYCONFIG_SOURCE_DEVICE_NAME = mem::zeroed();
            source.header._type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            source.header.size = mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as _;
            source.header.adapterId = path.sourceInfo.adapterId;
            source.header.id = path.sourceInfo.id;
            if DisplayConfigGetDeviceInfo(&mut source.header) != 0
                || !wide_eq(&source.viewGdiDeviceName, device_name)
            {
                continue;
            }
            let mut white: DISPLAYCONFIG_SDR_WHITE_LEVEL = mem::zeroed();
            white.header._type = DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL;
            white.header.size = mem::size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as _;
            white.header.adapterId = path.targetInfo.adapterId;
            white.header.id = path.targetInfo.id;
            if DisplayConfigGetDeviceInfo(&mut white.header) == 0 && white.SDRWhiteLevel != 0 {
                return Some(white.SDRWhiteLevel);
            }
        }
        None
    }
}

fn wide_eq(a: &[WCHAR], b: &[WCHAR]) -> bool {
    let end = |s: &[WCHAR]| s.iter().position(|&c| c == 0).unwrap_or(s.len());
    a[..end(a)] == b[..end(b)]
}
