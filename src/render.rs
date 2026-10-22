//! Rendering D3D11 del video nella swapchain che ReShade aggancia.
//!
//! La swapchain e' FLIP_SEQUENTIAL a 2 buffer: subito dopo il Present del frame N il back
//! buffer contiene il frame N-1 gia' elaborato da ReShade. Da li' lo rileggiamo per l'export
//! e per il confronto. Il frame "originale" del confronto viene disegnato con lo stesso shader
//! in una texture a parte, che ReShade non tocca.

use windows::core::{s, Interface, Result, HRESULT, PCSTR};
use windows::Win32::Foundation::{HMODULE, HWND, RECT};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;

use crate::framing::Framing;

const SHADER: &str = r#"
Texture2D video : register(t0);
SamplerState samp : register(s0);
cbuffer C : register(b0) {
    float4 map;     // uv sorgente = uv * map.xy + map.zw
    float4 cover;   // idem, per le bande sfocate
    float4 params;  // x: bande (-1 no, 0 nere, 1 sfocate)  y: lod sfocatura  z: bicubico
    float4 tex;     // xy: dimensioni del video
};
struct VSOut { float4 pos : SV_Position; float2 uv : TEXCOORD0; };
VSOut vs_main(uint id : SV_VertexID) {
    VSOut o;
    o.uv = float2((id << 1) & 2, id & 2);
    o.pos = float4(o.uv * float2(2, -2) + float2(-1, 1), 0, 1);
    return o;
}
// Catmull-Rom in 9 campioni bilineari
float3 bicubic(float2 uv) {
    float2 size = tex.xy;
    float2 p = uv * size;
    float2 t1 = floor(p - 0.5) + 0.5;
    float2 f = p - t1;
    float2 w0 = f * (-0.5 + f * (1.0 - 0.5 * f));
    float2 w1 = 1.0 + f * f * (-2.5 + 1.5 * f);
    float2 w2 = f * (0.5 + f * (2.0 - 1.5 * f));
    float2 w3 = f * f * (-0.5 + 0.5 * f);
    float2 w12 = w1 + w2;
    float2 t0 = (t1 - 1) / size, t3 = (t1 + 2) / size, t12 = (t1 + w2 / w12) / size;
    float3 c = 0;
    c += video.SampleLevel(samp, float2(t0.x,  t0.y),  0).rgb * w0.x  * w0.y;
    c += video.SampleLevel(samp, float2(t12.x, t0.y),  0).rgb * w12.x * w0.y;
    c += video.SampleLevel(samp, float2(t3.x,  t0.y),  0).rgb * w3.x  * w0.y;
    c += video.SampleLevel(samp, float2(t0.x,  t12.y), 0).rgb * w0.x  * w12.y;
    c += video.SampleLevel(samp, float2(t12.x, t12.y), 0).rgb * w12.x * w12.y;
    c += video.SampleLevel(samp, float2(t3.x,  t12.y), 0).rgb * w3.x  * w12.y;
    c += video.SampleLevel(samp, float2(t0.x,  t3.y),  0).rgb * w0.x  * w3.y;
    c += video.SampleLevel(samp, float2(t12.x, t3.y),  0).rgb * w12.x * w3.y;
    c += video.SampleLevel(samp, float2(t3.x,  t3.y),  0).rgb * w3.x  * w3.y;
    return saturate(c);
}
float4 ps_main(VSOut i) : SV_Target {
    float2 uv = i.uv * map.xy + map.zw;
    if (any(uv < 0) || any(uv > 1)) {
        if (params.x < 0.5) return float4(0, 0, 0, 1);
        float2 cu = i.uv * cover.xy + cover.zw;
        float2 px = exp2(params.y) / tex.xy;
        float3 c = 0;
        [unroll] for (int y = -1; y <= 1; y++)
            [unroll] for (int x = -1; x <= 1; x++)
                c += video.SampleLevel(samp, cu + float2(x, y) * px, params.y).rgb;
        return float4(c / 9 * 0.55, 1);
    }
    float3 c = params.z > 0.5 ? bicubic(uv) : video.Sample(samp, uv).rgb;
    return float4(c, 1);
}
"#;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Constants {
    map: [f32; 4],
    cover: [f32; 4],
    params: [f32; 4],
    tex: [f32; 4],
}

pub struct Renderer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    swapchain: IDXGISwapChain1,
    rtv: Option<ID3D11RenderTargetView>,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    cbuffer: ID3D11Buffer,
    video_tex: Option<ID3D11Texture2D>,
    video_srv: Option<ID3D11ShaderResourceView>,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    orig_tex: Option<ID3D11Texture2D>,
    orig_rtv: Option<ID3D11RenderTargetView>,
    orig_staging: Option<(ID3D11Texture2D, u32, u32)>,
    /// Riquadro del back buffer in cui va il video (x, y, w, h); None = tutto. In anteprima il
    /// back buffer copre l'intera area, cosi' il menu di ReShade ha spazio ai lati del video.
    pub view: Option<[u32; 4]>,
    pub video_w: u32,
    pub video_h: u32,
    pub back_w: u32,
    pub back_h: u32,
}

fn compile(entry: PCSTR, target: PCSTR) -> Result<ID3DBlob> {
    let mut blob = None;
    let mut errors = None;
    unsafe {
        D3DCompile(
            SHADER.as_ptr() as _,
            SHADER.len(),
            PCSTR::null(),
            None,
            None,
            entry,
            target,
            0,
            0,
            &mut blob,
            Some(&mut errors),
        )?;
    }
    Ok(blob.unwrap())
}

fn blob_bytes(b: &ID3DBlob) -> &[u8] {
    unsafe { std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()) }
}

impl Renderer {
    pub fn new(hwnd: HWND) -> Result<Self> {
        unsafe {
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let device: ID3D11Device = device.unwrap();
            let context = context.unwrap();

            let factory: IDXGIFactory2 = device.cast::<IDXGIDevice>()?.GetAdapter()?.GetParent()?;
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let (w, h) = ((rc.right - rc.left).max(16) as u32, (rc.bottom - rc.top).max(16) as u32);
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: w,
                Height: h,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2, // con FLIP_SEQUENTIAL: vedi in cima al file
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                ..Default::default()
            };
            let swapchain = factory.CreateSwapChainForHwnd(&device, hwnd, &desc, None, None)?;
            let _ = factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER);

            let vs_blob = compile(s!("vs_main"), s!("vs_5_0"))?;
            let ps_blob = compile(s!("ps_main"), s!("ps_5_0"))?;
            let mut vs = None;
            let mut ps = None;
            device.CreateVertexShader(blob_bytes(&vs_blob), None, Some(&mut vs))?;
            device.CreatePixelShader(blob_bytes(&ps_blob), None, Some(&mut ps))?;

            let sd = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                MaxLOD: f32::MAX,
                ..Default::default()
            };
            let mut sampler = None;
            device.CreateSamplerState(&sd, Some(&mut sampler))?;

            let bd = D3D11_BUFFER_DESC {
                ByteWidth: std::mem::size_of::<Constants>() as u32,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                ..Default::default()
            };
            let mut cbuffer = None;
            device.CreateBuffer(&bd, None, Some(&mut cbuffer))?;

            let mut r = Self {
                device,
                context,
                swapchain,
                rtv: None,
                vs: vs.unwrap(),
                ps: ps.unwrap(),
                sampler: sampler.unwrap(),
                cbuffer: cbuffer.unwrap(),
                video_tex: None,
                video_srv: None,
                staging: None,
                orig_tex: None,
                orig_rtv: None,
                orig_staging: None,
                view: None,
                video_w: 0,
                video_h: 0,
                back_w: w,
                back_h: h,
            };
            r.create_rtv()?;
            Ok(r)
        }
    }

    fn create_rtv(&mut self) -> Result<()> {
        unsafe {
            let back: ID3D11Texture2D = self.swapchain.GetBuffer(0)?;
            let mut rtv = None;
            self.device.CreateRenderTargetView(&back, None, Some(&mut rtv))?;
            self.rtv = rtv;
        }
        Ok(())
    }

    /// Cambia le dimensioni del back buffer. ReShade ricarica gli effetti: farlo di rado.
    pub fn resize(&mut self, w: u32, h: u32) {
        if w == 0 || h == 0 || (w == self.back_w && h == self.back_h) {
            return;
        }
        unsafe {
            self.context.OMSetRenderTargets(None, None);
            self.rtv = None;
            self.staging = None;
            self.orig_tex = None;
            self.orig_rtv = None;
            self.orig_staging = None;
            if self.swapchain.ResizeBuffers(0, w, h, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(0)).is_ok() {
                self.back_w = w;
                self.back_h = h;
            }
        }
        let _ = self.create_rtv();
    }

    /// Prepara la texture per un video di queste dimensioni (con mipmap per le bande sfocate).
    pub fn set_video(&mut self, w: u32, h: u32) -> Result<()> {
        let td = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 0,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            MiscFlags: D3D11_RESOURCE_MISC_GENERATE_MIPS.0 as u32,
            ..Default::default()
        };
        unsafe {
            let mut tex = None;
            self.device.CreateTexture2D(&td, None, Some(&mut tex))?;
            let tex = tex.unwrap();
            let mut srv = None;
            self.device.CreateShaderResourceView(&tex, None, Some(&mut srv))?;
            self.video_tex = Some(tex);
            self.video_srv = srv;
        }
        self.video_w = w;
        self.video_h = h;
        Ok(())
    }

    pub fn upload(&self, frame: &[u8]) {
        let (Some(tex), Some(srv)) = (&self.video_tex, &self.video_srv) else { return };
        unsafe {
            self.context.UpdateSubresource(tex, 0, None, frame.as_ptr() as _, self.video_w * 4, 0);
            self.context.GenerateMips(srv);
        }
    }


    /// Riquadro del video nel back buffer, limitato alle sue dimensioni.
    fn view_rect(&self) -> [u32; 4] {
        let [x, y, w, h] = self.view.unwrap_or([0, 0, self.back_w, self.back_h]);
        let x = x.min(self.back_w - 1);
        let y = y.min(self.back_h - 1);
        [x, y, w.clamp(1, self.back_w - x), h.clamp(1, self.back_h - y)]
    }

    fn draw(&self, rtv: &ID3D11RenderTargetView, v: [u32; 4], f: &Framing) {
        let Some(srv) = &self.video_srv else { return };
        let [x, y, w, h] = v;
        // Bicubico quando ingrandiamo, mipmap trilineari quando rimpiccioliamo
        let texels_shown = self.video_w as f32 * f.map[0];
        let upscale = w as f32 >= texels_shown * 0.99;
        let blur_lod = ((self.video_w.max(self.video_h) as f32) / 48.0).log2().max(0.0);
        let c = Constants {
            map: f.map,
            cover: f.cover,
            params: [f.bars as f32, blur_lod, if upscale { 1.0 } else { 0.0 }, 0.0],
            tex: [self.video_w as f32, self.video_h as f32, 0.0, 0.0],
        };
        unsafe {
            let ctx = &self.context;
            ctx.UpdateSubresource(&self.cbuffer, 0, None, &c as *const _ as _, 0, 0);
            // stesso colore del fondo dell'anteprima nell'interfaccia
            ctx.ClearRenderTargetView(rtv, &[10.0 / 255.0, 10.0 / 255.0, 13.0 / 255.0, 1.0]);
            ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            let vp = D3D11_VIEWPORT {
                TopLeftX: x as f32,
                TopLeftY: y as f32,
                Width: w as f32,
                Height: h as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            ctx.RSSetViewports(Some(&[vp]));
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.IASetInputLayout(None);
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetShader(&self.ps, None);
            ctx.PSSetShaderResources(0, Some(&[Some(srv.clone())]));
            ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            ctx.PSSetConstantBuffers(0, Some(&[Some(self.cbuffer.clone())]));
            ctx.Draw(3, 0);
        }
    }

    /// Disegna il video nel back buffer e lo presenta (ReShade lo elabora qui).
    pub fn present(&self, f: &Framing, vsync: bool) -> HRESULT {
        if let Some(rtv) = &self.rtv {
            self.draw(rtv, self.view_rect(), f);
        }
        unsafe { self.swapchain.Present(if vsync { 1 } else { 0 }, DXGI_PRESENT(0)) }
    }

    fn staging_for(&self, w: u32, h: u32) -> Option<(ID3D11Texture2D, u32, u32)> {
        let td = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            ..Default::default()
        };
        let mut t = None;
        unsafe { self.device.CreateTexture2D(&td, None, Some(&mut t)).ok()? };
        t.map(|t| (t, w, h))
    }

    /// Copia il riquadro del video di `src` in `out` (righe compatte BGRA), usando una staging
    /// delle stesse dimensioni del riquadro, ricreata se cambia.
    fn read_region(&self, src: &ID3D11Texture2D, slot: &mut Option<(ID3D11Texture2D, u32, u32)>, out: &mut Vec<u8>) -> bool {
        let [x, y, w, h] = self.view_rect();
        if !matches!(slot, Some((_, sw, sh)) if *sw == w && *sh == h) {
            *slot = self.staging_for(w, h);
        }
        let Some((staging, _, _)) = slot else { return false };
        let (w, h) = (w as usize, h as usize);
        unsafe {
            let bx = D3D11_BOX { left: x, top: y, front: 0, right: x + w as u32, bottom: y + h as u32, back: 1 };
            self.context.CopySubresourceRegion(&*staging, 0, 0, 0, 0, src, 0, Some(&bx));
            let mut map = D3D11_MAPPED_SUBRESOURCE::default();
            if self.context.Map(&*staging, 0, D3D11_MAP_READ, 0, Some(&mut map)).is_err() {
                return false;
            }
            let row = w * 4;
            out.resize(row * h, 0);
            for r in 0..h {
                let src_row = (map.pData as *const u8).add(r * map.RowPitch as usize);
                std::ptr::copy_nonoverlapping(src_row, out.as_mut_ptr().add(r * row), row);
            }
            self.context.Unmap(&*staging, 0);
        }
        true
    }

    /// Dimensioni dell'immagine restituita da `read_back` / `read_original`.
    pub fn view_size(&self) -> (u32, u32) {
        let [_, _, w, h] = self.view_rect();
        (w, h)
    }

    /// Copia il back buffer attuale (= ultimo frame elaborato da ReShade), solo il riquadro del video.
    pub fn read_back(&mut self, out: &mut Vec<u8>) -> bool {
        let Ok(back) = (unsafe { self.swapchain.GetBuffer::<ID3D11Texture2D>(0) }) else { return false };
        let mut slot = self.staging.take();
        let ok = self.read_region(&back, &mut slot, out);
        self.staging = slot;
        ok
    }

    /// Lo stesso fotogramma senza ReShade, per il confronto.
    pub fn read_original(&mut self, f: &Framing, out: &mut Vec<u8>) -> bool {
        if self.orig_tex.is_none() {
            let td = D3D11_TEXTURE2D_DESC {
                Width: self.back_w,
                Height: self.back_h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                ..Default::default()
            };
            unsafe {
                let mut tex = None;
                if self.device.CreateTexture2D(&td, None, Some(&mut tex)).is_err() {
                    return false;
                }
                let tex = tex.unwrap();
                let mut rtv = None;
                if self.device.CreateRenderTargetView(&tex, None, Some(&mut rtv)).is_err() {
                    return false;
                }
                self.orig_tex = Some(tex);
                self.orig_rtv = rtv;
            }
        }
        let (Some(tex), Some(rtv)) = (self.orig_tex.clone(), self.orig_rtv.clone()) else { return false };
        self.draw(&rtv, self.view_rect(), f);
        let mut slot = self.orig_staging.take();
        let ok = self.read_region(&tex, &mut slot, out);
        self.orig_staging = slot;
        ok
    }
}