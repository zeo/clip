use windows::core::{s, Error, Interface, Result, PCSTR};
use windows::Win32::Devices::Display::*;
use windows::Win32::Foundation::{E_FAIL, LUID, RECT};
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D_SHADER_MACRO};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::HMONITOR;

/// scRGB 1.0 is defined as 80 nits
pub const SCRGB_WHITE: f32 = 80.0;

/// A monitor's brightness setup, read once per recording so the tonemap never changes mid clip.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Display {
    pub hdr: bool,
    /// nits that windows maps sdr white to, from the "sdr content brightness" slider
    pub sdr_white: f32,
    /// panel peak in nits as the driver reports it
    pub peak: f32,
}

pub fn display(monitor: HMONITOR) -> Result<Display> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut a = 0;
        while let Ok(adapter) = factory.EnumAdapters1(a) {
            let mut o = 0;
            while let Ok(output) = adapter.EnumOutputs(o) {
                let desc = output.cast::<IDXGIOutput6>()?.GetDesc1()?;
                if desc.Monitor == monitor {
                    let hdr = desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
                    let sdr_white = if hdr { sdr_white(&desc.DeviceName).unwrap_or(200.0) } else { SCRGB_WHITE };
                    let peak = if desc.MaxLuminance > 0.0 { desc.MaxLuminance } else { 1000.0 };
                    return Ok(Display { hdr, sdr_white, peak: peak.max(sdr_white) });
                }
                o += 1;
            }
            a += 1;
        }
    }
    Err(Error::new(DXGI_ERROR_NOT_FOUND, "the monitor is not on any adapter"))
}

fn sdr_white(device: &[u16; 32]) -> Option<f32> {
    let name = |s: &[u16]| s[..s.iter().position(|&c| c == 0).unwrap_or(s.len())].to_vec();
    let header = |kind, size: usize, adapter: LUID, id| DISPLAYCONFIG_DEVICE_INFO_HEADER {
        r#type: kind,
        size: size as u32,
        adapterId: adapter,
        id,
    };
    unsafe {
        let (mut path_count, mut mode_count) = (0, 0);
        GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count).ok().ok()?;
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut path_count,
            paths.as_mut_ptr(),
            &mut mode_count,
            modes.as_mut_ptr(),
            None,
        )
        .ok()
        .ok()?;
        paths.truncate(path_count as usize);
        paths.iter().find_map(|path| {
            let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
                header: header(
                    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                    size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>(),
                    path.sourceInfo.adapterId,
                    path.sourceInfo.id,
                ),
                ..Default::default()
            };
            if DisplayConfigGetDeviceInfo(&mut source.header) != 0 || name(&source.viewGdiDeviceName) != name(device) {
                return None;
            }
            let mut white = DISPLAYCONFIG_SDR_WHITE_LEVEL {
                header: header(
                    DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
                    size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>(),
                    path.targetInfo.adapterId,
                    path.targetInfo.id,
                ),
                ..Default::default()
            };
            // the level is in thousandths of scRGB white
            let nits = white_level(DisplayConfigGetDeviceInfo(&mut white.header), white.SDRWhiteLevel);
            nits.filter(|n| (SCRGB_WHITE..=10_000.0).contains(n))
        })
    }
}

fn white_level(status: i32, level: u32) -> Option<f32> {
    (status == 0 && level > 0).then(|| level as f32 * SCRGB_WHITE / 1000.0)
}

const VERTEX: &str = r#"
struct Vertex {
    float4 position : SV_Position;
    float2 uv : TEXCOORD;
};

// one triangle covering the viewport, uv runs 0 to 1 across it
Vertex vs(uint id : SV_VertexID) {
    Vertex v;
    v.uv = float2((id << 1) & 2, id & 2);
    v.position = float4(v.uv * float2(2, -2) + float2(-1, 1), 0, 1);
    return v;
}
"#;

const TONEMAP: &str = r#"
Texture2D<float4> frame : register(t0);

float4 sdr(float4 position : SV_Position) : SV_Target {
    float3 c = max(frame.Load(int3(position.xy, 0)).rgb * SCALE, 0);
    float m = max(c.r, max(c.g, c.b));
    // one factor for all channels keeps the hue of bright colors
    if (m > KNEE) {
        float e = m - KNEE;
        c *= (KNEE + e / (1 + e / SOFT)) / m;
    }
    c = saturate(c);
    return float4(c <= 0.0031308 ? c * 12.92 : 1.055 * pow(c, 1 / 2.4) - 0.055, 1);
}
"#;

const PQ: &str = r#"
Texture2D<float4> frame : register(t0);
SamplerState bilinear : register(s0);
cbuffer Crop : register(b0) {
    float2 scale;
    float shift;
};

static const float3x3 BT709_TO_BT2020 = {
    0.627404, 0.329283, 0.043313,
    0.069097, 0.919540, 0.011362,
    0.016391, 0.088013, 0.895595,
};

float3 pq(float2 uv) {
    float3 nits = mul(BT709_TO_BT2020, frame.SampleLevel(bilinear, uv * scale, 0).rgb) * 80;
    float3 y = pow(saturate(nits / 10000), 0.1593017578125);
    return pow((0.8359375 + 18.8515625 * y) / (1 + 18.6875 * y), 78.84375);
}

// p010 keeps limited range 10 bit codes in the top bits of each 16 bit sample
float code(float v) {
    return round(v) * 64 / 65535;
}

float bt2020_luma(float3 c) {
    return dot(c, float3(0.2627, 0.6780, 0.0593));
}

float luma(float4 position : SV_Position, float2 uv : TEXCOORD) : SV_Target {
    return code(64 + 876 * bt2020_luma(pq(uv)));
}

// left chroma siting: in line with the left luma column, halfway between the two rows
float2 chroma(float4 position : SV_Position, float2 uv : TEXCOORD) : SV_Target {
    float3 c = pq(uv - float2(shift, 0));
    float y = bt2020_luma(c);
    return float2(code(512 + 896 * (c.b - y) / 1.8814), code(512 + 896 * (c.r - y) / 1.4746));
}
"#;

/// Full viewport pixel shader passes from one texture, compiled at startup with the
/// d3dcompiler that ships with windows.
struct Program {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vertex: ID3D11VertexShader,
    pixels: Vec<ID3D11PixelShader>,
}

impl Program {
    fn new(device: &ID3D11Device, source: &str, entries: &[PCSTR], defines: &[(PCSTR, f32)]) -> Result<Self> {
        let source = [VERTEX, source].concat();
        let values: Vec<String> = defines.iter().map(|(_, v)| format!("{v:e}\0")).collect();
        let mut macros: Vec<D3D_SHADER_MACRO> = defines
            .iter()
            .zip(&values)
            .map(|((name, _), value)| D3D_SHADER_MACRO { Name: *name, Definition: PCSTR(value.as_ptr()) })
            .collect();
        macros.push(D3D_SHADER_MACRO::default());
        unsafe {
            let vertex = compile(&source, s!("vs"), s!("vs_5_0"), &macros)?;
            let mut vs = None;
            device.CreateVertexShader(bytes(&vertex), None, Some(&mut vs))?;
            let mut pixels = Vec::with_capacity(entries.len());
            for &entry in entries {
                let pixel = compile(&source, entry, s!("ps_5_0"), &macros)?;
                let mut ps = None;
                device.CreatePixelShader(bytes(&pixel), None, Some(&mut ps))?;
                pixels.push(ps.ok_or(Error::from(E_FAIL))?);
            }
            Ok(Self {
                device: device.clone(),
                context: device.GetImmediateContext()?,
                vertex: vs.ok_or(Error::from(E_FAIL))?,
                pixels,
            })
        }
    }

    /// Runs pixel shader `i` into each `(i, target, viewport)` with `source` bound to t0.
    fn draw(
        &self,
        source: &ID3D11Texture2D,
        setup: impl FnOnce(&ID3D11DeviceContext),
        passes: &[(usize, &ID3D11RenderTargetView, D3D11_VIEWPORT)],
    ) -> Result<()> {
        unsafe {
            let mut input = None;
            self.device.CreateShaderResourceView(source, None, Some(&mut input))?;
            // media foundation uses this context from its own threads, so hold it until the state is unbound
            let lock: ID3D11Multithread = self.context.cast()?;
            lock.Enter();
            let c = &self.context;
            c.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            c.IASetInputLayout(None);
            c.VSSetShader(&self.vertex, None);
            c.PSSetShaderResources(0, Some(&[input]));
            c.RSSetState(None);
            c.OMSetBlendState(None, None, u32::MAX);
            setup(c);
            for &(i, target, viewport) in passes {
                c.PSSetShader(&self.pixels[i], None);
                c.RSSetViewports(Some(&[viewport]));
                c.OMSetRenderTargets(Some(&[Some(target.clone())]), None);
                c.Draw(3, 0);
            }
            c.OMSetRenderTargets(None, None);
            c.PSSetShaderResources(0, Some(&[None]));
            lock.Leave();
            Ok(())
        }
    }
}

/// Turns scRGB FP16 frames into sRGB BGRA on the GPU: sdr content keeps its normal brightness
/// and highlights roll off above a knee instead of clipping. Parameters come from the display
/// once, so the result is the same in every frame.
pub struct Tonemap {
    program: Program,
    target: Option<(ID3D11Texture2D, ID3D11RenderTargetView, (u32, u32))>,
}

/// Linear sdr-relative level below which sdr content passes through untouched, about sRGB 231
const KNEE: f32 = 0.8;

impl Tonemap {
    pub fn new(device: &ID3D11Device, display: Display) -> Result<Self> {
        let (knee, soft) = rolloff(display);
        let defines = [(s!("SCALE"), SCRGB_WHITE / display.sdr_white), (s!("KNEE"), knee), (s!("SOFT"), soft)];
        Ok(Self { program: Program::new(device, TONEMAP, &[s!("sdr")], &defines)?, target: None })
    }

    pub fn apply(&mut self, source: &ID3D11Texture2D) -> Result<ID3D11Texture2D> {
        let size = size(source);
        if self.target.as_ref().is_none_or(|t| t.2 != size) {
            let texture = texture(&self.program.device, size, DXGI_FORMAT_B8G8R8A8_UNORM)?;
            let view = view(&self.program.device, &texture, DXGI_FORMAT_B8G8R8A8_UNORM)?;
            self.target = Some((texture, view, size));
        }
        let (texture, view, _) = self.target.as_ref().expect("created");
        self.program.draw(source, |_| {}, &[(0, view, viewport(0, 0, size))])?;
        Ok(texture.clone())
    }
}

/// Writes scRGB FP16 frames straight into P010 as hdr10: bt.2020 primaries, the PQ curve in
/// absolute nits, limited range. Video processors are free to refuse PQ output (NVIDIA's does),
/// so the color math and the scaling happen here. Targets rotate like the converter's.
pub struct Pq {
    program: Program,
    sampler: ID3D11SamplerState,
    crop: ID3D11Buffer,
    targets: Vec<(ID3D11Texture2D, ID3D11RenderTargetView, ID3D11RenderTargetView)>,
    next: usize,
}

impl Pq {
    pub fn new(device: &ID3D11Device, output: (u32, u32), count: usize) -> Result<Self> {
        let program = Program::new(device, PQ, &[s!("luma"), s!("chroma")], &[])?;
        unsafe {
            let mut sampler = None;
            device.CreateSamplerState(
                &D3D11_SAMPLER_DESC {
                    Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                    AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                    ComparisonFunc: D3D11_COMPARISON_NEVER,
                    MaxLOD: f32::MAX,
                    ..Default::default()
                },
                Some(&mut sampler),
            )?;
            let mut crop = None;
            device.CreateBuffer(
                &D3D11_BUFFER_DESC {
                    ByteWidth: 16,
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                    ..Default::default()
                },
                None,
                Some(&mut crop),
            )?;
            let mut targets = Vec::with_capacity(count);
            for _ in 0..count {
                let texture = texture(device, output, DXGI_FORMAT_P010)?;
                // the view format picks the plane: 16 bit luma, then 16 bit pairs of half size chroma
                let luma = view(device, &texture, DXGI_FORMAT_R16_UNORM)?;
                let chroma = view(device, &texture, DXGI_FORMAT_R16G16_UNORM)?;
                targets.push((texture, luma, chroma));
            }
            Ok(Self {
                program,
                sampler: sampler.ok_or(Error::from(E_FAIL))?,
                crop: crop.ok_or(Error::from(E_FAIL))?,
                targets,
                next: 0,
            })
        }
    }

    /// Scales the `content` region of `source` into `dest` (even pixels) of the next target, black around it.
    pub fn convert(&mut self, source: &ID3D11Texture2D, content: (u32, u32), dest: RECT) -> Result<ID3D11Texture2D> {
        let size = size(source);
        let content = (content.0.clamp(1, size.0), content.1.clamp(1, size.1));
        let width = (dest.right - dest.left).max(2) as f32;
        let crop = [content.0 as f32 / size.0 as f32, content.1 as f32 / size.1 as f32, 0.5 / width, 0.0];
        let (texture, luma, chroma) = &self.targets[self.next];
        self.next = (self.next + 1) % self.targets.len();
        let at = |divisor: i32| {
            let corner = (dest.left / divisor, dest.top / divisor);
            let extent = ((dest.right - dest.left) / divisor, (dest.bottom - dest.top) / divisor);
            viewport(corner.0, corner.1, (extent.0 as u32, extent.1 as u32))
        };
        let setup = |c: &ID3D11DeviceContext| unsafe {
            let black = 64.0 * 64.0 / 65535.0;
            let neutral = 512.0 * 64.0 / 65535.0;
            c.ClearRenderTargetView(luma, &[black, 0.0, 0.0, 0.0]);
            c.ClearRenderTargetView(chroma, &[neutral, neutral, 0.0, 0.0]);
            c.UpdateSubresource(&self.crop, 0, None, crop.as_ptr() as _, 0, 0);
            c.PSSetConstantBuffers(0, Some(&[Some(self.crop.clone())]));
            c.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
        };
        self.program.draw(source, setup, &[(0, luma, at(1)), (1, chroma, at(2))])?;
        Ok(texture.clone())
    }
}

fn size(texture: &ID3D11Texture2D) -> (u32, u32) {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };
    (desc.Width, desc.Height)
}

fn viewport(left: i32, top: i32, size: (u32, u32)) -> D3D11_VIEWPORT {
    D3D11_VIEWPORT {
        TopLeftX: left as f32,
        TopLeftY: top as f32,
        Width: size.0 as f32,
        Height: size.1 as f32,
        MinDepth: 0.0,
        MaxDepth: 1.0,
    }
}

fn texture(device: &ID3D11Device, size: (u32, u32), format: DXGI_FORMAT) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: size.0,
        Height: size.1,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        // the video processor and the encoders read through render target binding
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture))? };
    texture.ok_or(Error::from(E_FAIL))
}

fn view(device: &ID3D11Device, texture: &ID3D11Texture2D, format: DXGI_FORMAT) -> Result<ID3D11RenderTargetView> {
    let desc = D3D11_RENDER_TARGET_VIEW_DESC {
        Format: format,
        ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 } },
    };
    let mut view = None;
    unsafe { device.CreateRenderTargetView(texture, Some(&desc), Some(&mut view))? };
    view.ok_or(Error::from(E_FAIL))
}

unsafe fn compile(source: &str, entry: PCSTR, target: PCSTR, macros: &[D3D_SHADER_MACRO]) -> Result<ID3DBlob> {
    let (mut code, mut errors) = (None, None);
    let compiled = unsafe {
        D3DCompile(
            source.as_ptr() as _,
            source.len(),
            None,
            Some(macros.as_ptr()),
            None,
            entry,
            target,
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = compiled {
        let log = errors.map(|b| String::from_utf8_lossy(bytes(&b)).into_owned()).unwrap_or_default();
        return Err(Error::new(e.code(), format!("shader didn't compile: {log}")));
    }
    code.ok_or(Error::from(E_FAIL))
}

fn bytes(blob: &ID3DBlob) -> &[u8] {
    unsafe { std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize()) }
}

/// Knee and softness of the highlight curve in sdr-relative linear light, where sdr white is 1.0.
/// Above the knee `e / (1 + e / soft)` bends the panel peak exactly onto white without a kink.
/// A panel no brighter than sdr white has nothing to roll off, so the knee moves out of reach.
fn rolloff(display: Display) -> (f32, f32) {
    let peak = display.peak / display.sdr_white;
    let (range, room) = (peak - KNEE, 1.0 - KNEE);
    if range <= room {
        return (1e30, 1.0);
    }
    (KNEE, range * room / (range - room))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn white_level_is_thousandths_of_scrgb_white() {
        assert_eq!(white_level(0, 2500), Some(200.0));
        assert_eq!(white_level(0, 0), None);
        assert_eq!(white_level(87, 2500), None);
    }

    #[test]
    fn rolloff_lands_the_peak_on_white() {
        let (knee, soft) = rolloff(Display { hdr: true, sdr_white: 200.0, peak: 1000.0 });
        let excess = 1000.0 / 200.0 - knee;
        assert!((knee + excess / (1.0 + excess / soft) - 1.0).abs() < 1e-6);
        let (knee, _) = rolloff(Display { hdr: true, sdr_white: 400.0, peak: 300.0 });
        assert!(knee > 1e20);
    }
}
