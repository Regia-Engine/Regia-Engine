// SPDX-License-Identifier: MPL-2.0
//! The build without the NGX SDK: the same API, every call [`DlssError::NotLinked`].

use crate::{AddedExtensions, CreateFlags, DlssError, EvalInputs, NgxConfig, OptimalSettings, PerfQuality};

fn not_linked<T>() -> Result<T, DlssError> {
    Err(DlssError::NotLinked)
}

fn request_device_stub<T>() -> Result<T, DlssError> {
    not_linked()
}

fn ngx_new_stub<T>() -> Result<T, DlssError> {
    not_linked()
}

fn super_resolution_new_stub<T>() -> Result<T, DlssError> {
    not_linked()
}

pub(crate) fn create_instance(
    _descriptor: &wgpu::InstanceDescriptor,
    _config: &NgxConfig,
) -> Result<(wgpu::Instance, AddedExtensions), DlssError> {
    not_linked()
}

pub(crate) fn request_device(
    _adapter: &wgpu::Adapter,
    _descriptor: &wgpu::DeviceDescriptor<'_>,
    _config: &NgxConfig,
) -> Result<(wgpu::Device, wgpu::Queue, AddedExtensions), DlssError> {
    request_device_stub()
}

pub(crate) fn device_has_extensions(
    _device: &wgpu::Device,
    _instance: &[String],
    _device_wanted: &[String],
) -> bool {
    false
}

pub(crate) fn device_has_ngx_extensions(
    _device: &wgpu::Device,
    _config: &NgxConfig,
) -> Result<bool, DlssError> {
    Err(DlssError::NotLinked)
}

/// Never constructed in the stub.
pub(crate) struct Ngx;

impl Ngx {
    pub(crate) fn new(_config: &NgxConfig, _device: &wgpu::Device) -> Result<Self, DlssError> {
        ngx_new_stub()
    }

    pub(crate) fn is_on(&self, _device: &wgpu::Device) -> bool {
        false
    }

    pub(crate) fn optimal_settings(
        &self,
        _display: [u32; 2],
        _quality: PerfQuality,
    ) -> Result<OptimalSettings, DlssError> {
        not_linked()
    }

    pub(crate) fn vram_bytes(&self) -> Result<u64, DlssError> {
        not_linked()
    }
}

/// Never constructed in the stub.
pub(crate) struct Lease;

/// Never constructed in the stub.
pub(crate) struct SuperResolution;

impl SuperResolution {
    pub(crate) fn new(
        _ngx: &Ngx,
        _device: &wgpu::Device,
        _queue: &wgpu::Queue,
        _display: [u32; 2],
        _quality: PerfQuality,
        _flags: CreateFlags,
        _render_max: [u32; 2],
    ) -> Result<Self, DlssError> {
        super_resolution_new_stub()
    }

    pub(crate) fn evaluate(
        &mut self,
        _device: &wgpu::Device,
        _encoder: &mut wgpu::CommandEncoder,
        _inputs: &EvalInputs<'_>,
    ) -> Result<(wgpu::CommandBuffer, Lease), DlssError> {
        not_linked()
    }

    pub(crate) fn display(&self) -> [u32; 2] {
        [0, 0]
    }

    pub(crate) fn is_on(&self, _device: &wgpu::Device) -> bool {
        false
    }
}

#[cfg(test)]
pub(crate) mod stub_entry_tests {
    use crate::DlssError;

    pub(crate) fn request_device() -> Result<(), DlssError> {
        super::request_device_stub()
    }

    pub(crate) fn ngx_new() -> Result<(), DlssError> {
        super::ngx_new_stub()
    }

    pub(crate) fn super_resolution_new() -> Result<(), DlssError> {
        super::super_resolution_new_stub()
    }
}
