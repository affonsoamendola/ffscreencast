use anyhow::{anyhow, Result};
use bytes::Bytes;
use nvenc::bitstream::BitStream;
use nvenc::encoder::{Encoder, RegisteredResource};
use nvenc::session::{InitParams, NeedsConfig, Session};
use nvenc::sys::enums::{
    NVencBufferFormat, NVencParamsRcMode, NVencPicStruct, NVencPicType, NVencTuningInfo,
};
use nvenc::sys::guids::{
    NV_ENC_CODEC_H264_GUID, NV_ENC_H264_PROFILE_HIGH_GUID, NV_ENC_PRESET_P4_GUID,
};
use nvenc::sys::structs::NVencConfig;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_CPU_ACCESS_WRITE, D3D11_MAP_WRITE, D3D11_MAPPED_SUBRESOURCE,
    D3D11_SDK_VERSION, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, D3D11_USAGE, ID3D11Device,
    ID3D11DeviceContext, ID3D11Texture2D, D3D11_TEXTURE2D_DESC, D3D11_BIND_SHADER_RESOURCE,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC};

use crate::capture::Frame;

#[allow(dead_code)]
pub struct NvencH264Encoder {
    encoder: Encoder,
    registered: RegisteredResource,
    default_texture: ID3D11Texture2D,
    output_bitstream: BitStream,
    staging_texture: ID3D11Texture2D,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    width: u32,
    height: u32,
}

unsafe impl Send for NvencH264Encoder {}

#[allow(dead_code)]
impl NvencH264Encoder {
    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
}

fn create_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    usage: D3D11_USAGE,
    bind_flags: u32,
    cpu_access: u32,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: usage,
        BindFlags: bind_flags,
        CPUAccessFlags: cpu_access,
        MiscFlags: 0,
    };

    let mut texture = None;
    unsafe {
        device
            .CreateTexture2D(&desc, None, Some(&mut texture))
            .map_err(|e| anyhow!("CreateTexture2D failed: {e}"))?;
    }
    texture.ok_or_else(|| anyhow!("CreateTexture2D returned null"))
}

fn create_d3d11_device_and_context() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    logln!("[nvenc] creating D3D11 device...");
    let mut device = None;
    let mut context = None;
    let feature_levels = [D3D_FEATURE_LEVEL_11_0];

    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE(std::ptr::null_mut()),
            Default::default(),
            Some(&feature_levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .map_err(|e| {
            logln!("[nvenc] D3D11CreateDevice returned error: {e}");
            anyhow!("D3D11CreateDevice failed: {e}")
        })?;
    }

    logln!(
        "[nvenc] D3D11CreateDevice succeeded, device is_some={}",
        device.is_some()
    );
    let device = device.ok_or_else(|| {
        logln!("[nvenc] D3D11CreateDevice returned null device");
        anyhow!("D3D11CreateDevice returned null device")
    })?;
    let context = context.ok_or_else(|| {
        logln!("[nvenc] D3D11CreateDevice returned null context");
        anyhow!("D3D11CreateDevice returned null context")
    })?;

    Ok((device, context))
}

unsafe fn configure_config(config: &mut NVencConfig, fps: u32) {
    config.profile_guid = NV_ENC_H264_PROFILE_HIGH_GUID;
    config.gop_len = fps * 2;
    config.frame_interval_p = 1;

    config.rc_params.rate_control_mode = NVencParamsRcMode::CBR;
    config.rc_params.average_bit_rate = 8_000_000;
}

#[allow(dead_code)]
impl NvencH264Encoder {
    pub fn new(width: u32, height: u32, fps: u32) -> Result<Self> {
        logln!("[nvenc] creating encoder: {width}x{height} @ {fps}fps");
        let (device, context) = create_d3d11_device_and_context()?;
        logln!("[nvenc] D3D11 device ready, opening NVENC session...");

        let session: Session<NeedsConfig> = Session::open_dx(&device).map_err(|e| {
            logln!("[nvenc] NVENC open_dx failed: {e:?}");
            anyhow!("NVENC open_dx failed: {e:?}")
        })?;
        logln!("[nvenc] NVENC session opened, getting preset config...");

        let (session, preset_config) = session
            .get_encode_preset_config_ex(
                NV_ENC_CODEC_H264_GUID,
                NV_ENC_PRESET_P4_GUID,
                NVencTuningInfo::UltraLowLatency,
            )
            .map_err(|e| {
                logln!("[nvenc] get preset config failed: {e:?}");
                anyhow!("get preset config failed: {e:?}")
            })?;
        logln!("[nvenc] preset config obtained, configuring...");

        let mut encode_config: NVencConfig =
            unsafe { std::ptr::read(&preset_config.preset_cfg as *const NVencConfig) };
        unsafe {
            configure_config(&mut encode_config, fps);
        }

        let init_params = InitParams {
            encode_guid: NV_ENC_CODEC_H264_GUID,
            preset_guid: NV_ENC_PRESET_P4_GUID,
            resolution: [width, height],
            aspect_ratio: [width, height],
            frame_rate: [fps, 1],
            tuning_info: NVencTuningInfo::UltraLowLatency,
            buffer_format: NVencBufferFormat::ABGR,
            encode_config: &mut encode_config,
            enable_ptd: true,
            max_encoder_resolution: [width, height],
        };

        logln!("[nvenc] initializing encoder...");
        let encoder = session.init_encoder(init_params).map_err(|e| {
            logln!("[nvenc] init_encoder failed: {e:?}");
            anyhow!("NVENC init_encoder failed: {e:?}")
        })?;
        logln!("[nvenc] encoder initialized, creating textures...");

        let staging_texture = create_texture(
            &device,
            width,
            height,
            D3D11_USAGE_STAGING,
            0,
            D3D11_CPU_ACCESS_WRITE.0 as u32,
        )
        .map_err(|e| {
            logln!("[nvenc] create staging texture failed: {e}");
            e
        })?;
        logln!("[nvenc] staging texture created");

        let default_texture = create_texture(
            &device,
            width,
            height,
            D3D11_USAGE_DEFAULT,
            D3D11_BIND_SHADER_RESOURCE.0 as u32,
            0,
        )
        .map_err(|e| {
            logln!("[nvenc] create default texture failed: {e}");
            e
        })?;
        logln!("[nvenc] default texture created, registering with NVENC...");

        let registered = encoder
            .register_resource_dx11(&default_texture, NVencBufferFormat::ABGR, 0)
            .map_err(|e| {
                logln!("[nvenc] register_resource_dx11 failed: {e:?}");
                anyhow!("register_resource_dx11 failed: {e:?}")
            })?;
        logln!("[nvenc] texture registered, creating bitstream buffer...");

        let output_bitstream = encoder.create_bitstream_buffer().map_err(|e| {
            logln!("[nvenc] create_bitstream_buffer failed: {e:?}");
            anyhow!("create_bitstream_buffer failed: {e:?}")
        })?;

        let result = Ok(Self {
            registered,
            default_texture,
            output_bitstream,
            staging_texture,
            device,
            context,
            encoder,
            width,
            height,
        });
        logln!("[nvenc] encoder created successfully");
        result
    }

    fn copy_frame_to_texture(&self, frame: &Frame) -> Result<()> {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context
                .Map(
                    &self.staging_texture,
                    0,
                    D3D11_MAP_WRITE,
                    0,
                    Some(&mut mapped),
                )
                .map_err(|e| anyhow!("map staging texture failed: {e}"))?;
        }

        let src_pitch = self.width as usize * 4;
        let dst_pitch = mapped.RowPitch as usize;
        let dst = mapped.pData as *mut u8;

        if src_pitch == dst_pitch {
            let total = src_pitch * self.height as usize;
            let dst_slice = unsafe { std::slice::from_raw_parts_mut(dst, total) };
            dst_slice.copy_from_slice(&frame.data[..total]);
        } else {
            for y in 0..self.height as usize {
                let src_row = &frame.data[y * src_pitch..(y + 1) * src_pitch];
                let dst_row = unsafe {
                    std::slice::from_raw_parts_mut(dst.add(y * dst_pitch), src_pitch)
                };
                dst_row.copy_from_slice(src_row);
            }
        }

        unsafe {
            self.context.Unmap(&self.staging_texture, 0);
        }

        unsafe {
            self.context
                .CopyResource(&self.default_texture, &self.staging_texture);
        }

        Ok(())
    }

    pub fn encode(&mut self, frame: &Frame) -> Result<Vec<Bytes>> {
        if frame.width != self.width || frame.height != self.height {
            let msg = format!(
                "resolution mismatch: expected {}x{}, got {}x{}",
                self.width, self.height, frame.width, frame.height,
            );
            logln!("[nvenc] {msg}");
            return Err(anyhow!(msg));
        }

        self.copy_frame_to_texture(frame)?;

        self.encoder
            .encode_picture(
                &self.registered,
                &self.output_bitstream,
                0,
                0,
                NVencBufferFormat::ABGR,
                NVencPicStruct::Frame,
                NVencPicType::P,
                None,
            )
            .map_err(|e| anyhow!("encode_picture failed: {e:?}"))?;

        let lock = self
            .output_bitstream
            .try_lock(true)
            .map_err(|e| anyhow!("lock bitstream failed: {e:?}"))?;

        let data = lock.as_slice();
        Ok(split_annex_b(data).into_iter().map(Bytes::from).collect())
    }

    pub fn encode_keyframe(&mut self, frame: &Frame) -> Result<Vec<Bytes>> {
        if frame.width != self.width || frame.height != self.height {
            let msg = format!(
                "resolution mismatch: expected {}x{}, got {}x{}",
                self.width, self.height, frame.width, frame.height,
            );
            logln!("[nvenc] {msg}");
            return Err(anyhow!(msg));
        }

        self.copy_frame_to_texture(frame)?;

        self.encoder
            .encode_picture(
                &self.registered,
                &self.output_bitstream,
                0,
                0,
                NVencBufferFormat::ABGR,
                NVencPicStruct::Frame,
                NVencPicType::IDR,
                None,
            )
            .map_err(|e| anyhow!("encode IDR failed: {e:?}"))?;

        let lock = self
            .output_bitstream
            .try_lock(true)
            .map_err(|e| anyhow!("lock bitstream failed: {e:?}"))?;

        let data = lock.as_slice();
        Ok(split_annex_b(data).into_iter().map(Bytes::from).collect())
    }

    pub fn reconfigure(&mut self, width: u32, height: u32, fps: u32) -> Result<()> {
        logln!("[nvenc] reconfiguring: {width}x{height} @ {fps}fps");
        self.width = width;
        self.height = height;

        let (device, context) = create_d3d11_device_and_context()?;
        let session: Session<NeedsConfig> =
            Session::open_dx(&device).map_err(|e| anyhow!("NVENC open_dx failed: {e:?}"))?;

        let (session, preset_config) = session
            .get_encode_preset_config_ex(
                NV_ENC_CODEC_H264_GUID,
                NV_ENC_PRESET_P4_GUID,
                NVencTuningInfo::UltraLowLatency,
            )
            .map_err(|e| anyhow!("get preset config failed: {e:?}"))?;

        let mut encode_config: NVencConfig =
            unsafe { std::ptr::read(&preset_config.preset_cfg as *const NVencConfig) };
        unsafe {
            configure_config(&mut encode_config, fps);
        }

        let init_params = InitParams {
            encode_guid: NV_ENC_CODEC_H264_GUID,
            preset_guid: NV_ENC_PRESET_P4_GUID,
            resolution: [width, height],
            aspect_ratio: [width, height],
            frame_rate: [fps, 1],
            tuning_info: NVencTuningInfo::UltraLowLatency,
            buffer_format: NVencBufferFormat::ABGR,
            encode_config: &mut encode_config,
            enable_ptd: true,
            max_encoder_resolution: [width, height],
        };

        let encoder = session
            .init_encoder(init_params)
            .map_err(|e| anyhow!("NVENC init_encoder failed: {e:?}"))?;

        let staging_texture = create_texture(
            &device,
            width,
            height,
            D3D11_USAGE_STAGING,
            0,
            D3D11_CPU_ACCESS_WRITE.0 as u32,
        )
        .map_err(|e| anyhow!("create staging texture failed: {e}"))?;

        let default_texture = create_texture(
            &device,
            width,
            height,
            D3D11_USAGE_DEFAULT,
            D3D11_BIND_SHADER_RESOURCE.0 as u32,
            0,
        )
        .map_err(|e| anyhow!("create default texture failed: {e}"))?;

        let registered = encoder
            .register_resource_dx11(&default_texture, NVencBufferFormat::ABGR, 0)
            .map_err(|e| anyhow!("register_resource_dx11 failed: {e:?}"))?;

        let output_bitstream = encoder
            .create_bitstream_buffer()
            .map_err(|e| anyhow!("create_bitstream_buffer failed: {e:?}"))?;

        self.encoder = encoder;
        self.registered = registered;
        self.default_texture = default_texture;
        self.output_bitstream = output_bitstream;
        self.staging_texture = staging_texture;
        self.device = device;
        self.context = context;

        Ok(())
    }
}

fn split_annex_b(bs: &[u8]) -> Vec<Vec<u8>> {
    let mut nals = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bs.len() {
        if i + 3 < bs.len()
            && bs[i] == 0
            && bs[i + 1] == 0
            && bs[i + 2] == 0
            && bs[i + 3] == 1
        {
            if i > start {
                nals.push(bs[start..i].to_vec());
            }
            start = i + 4;
            i += 4;
        } else if i + 2 < bs.len() && bs[i] == 0 && bs[i + 1] == 0 && bs[i + 2] == 1 {
            if i > start {
                nals.push(bs[start..i].to_vec());
            }
            start = i + 3;
            i += 3;
        } else {
            i += 1;
        }
    }
    if start < bs.len() {
        nals.push(bs[start..].to_vec());
    }
    nals.retain(|n| n.len() > 1);
    nals
}
