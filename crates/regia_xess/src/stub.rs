// SPDX-License-Identifier: MPL-2.0
//! SDK-free implementation on every target.
use crate::{CreateDesc, Error, EvalInputs, OptimalSettings, Quality, Version};
#[derive(Clone)]
pub(crate) struct Runtime;
impl Runtime {
    pub(crate) fn load(_: &std::path::Path) -> Result<Self, Error> {
        Err(Error::NotLinked)
    }
    pub(crate) fn version(&self) -> Version {
        Version {
            major: 0,
            minor: 0,
            patch: 0,
        }
    }
    pub(crate) fn create_instance(
        &self,
        _: &wgpu::InstanceDescriptor,
    ) -> Result<(wgpu::Instance, Vec<String>), Error> {
        Err(Error::NotLinked)
    }
    pub(crate) fn request_device(
        &self,
        _: &wgpu::Adapter,
        _: &wgpu::DeviceDescriptor<'_>,
    ) -> Result<(wgpu::Device, wgpu::Queue, Vec<String>), Error> {
        Err(Error::NotLinked)
    }
    pub(crate) fn context(&self, _: &wgpu::Device) -> Result<Context, Error> {
        Err(Error::NotLinked)
    }
}
pub(crate) struct Context;
impl Context {
    pub(crate) fn optimal_settings(&self, _: [u32; 2], _: Quality) -> Result<OptimalSettings, Error> {
        Err(Error::NotLinked)
    }
    pub(crate) fn zeroed_temp_heaps(&self) -> bool {
        false
    }
    pub(crate) fn temp_heap_zeroing(&self) -> Option<[crate::HeapZeroing; 2]> {
        None
    }
    pub(crate) fn initialize(&mut self, _: CreateDesc) -> Result<(), Error> {
        Err(Error::NotLinked)
    }
    pub(crate) fn evaluate(
        &mut self,
        _: &wgpu::Device,
        _: &mut wgpu::CommandEncoder,
        _: &EvalInputs<'_>,
    ) -> Result<(wgpu::CommandBuffer, Lease), Error> {
        Err(Error::NotLinked)
    }
}
pub(crate) struct Lease;

pub(crate) fn device_prepared(_: &wgpu::Device) -> bool {
    false
}
