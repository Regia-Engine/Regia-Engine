// SPDX-License-Identifier: MPL-2.0
//! Counted (GPU-only) meshes and textures in an App (ADR-7330, `docs/specs/12-assets.md` § 3).
//!
//! [`CountedMeshPlugin`] gives the App's [`regia_asset::AssetServer`] the renderer's
//! [`crate::PrepareAsset`](regia_asset::PrepareAsset)s once a [`crate::RenderDevice`] exists:
//! [`crate::AssetMeshUploads`] for counted `CpuMesh` loads and [`crate::AssetTextureUploads`] for
//! counted `CookedTexture` / `CookedImage` loads. It hands the mesh uploader to the
//! [`crate::RuntimeRenderer`], so a [`crate::MeshRenderer`] on a [`crate::counted_mesh`] stand-in
//! draws the uploaded buffers with no CPU copy anywhere, and [`CountedAssetUploads`] (a resource)
//! is where a host builds [`crate::MaterialImage::Asset`] slots from counted texture handles.
//!
//! Registering the uploaders changes what a counted load keeps: the CPU streams go after the
//! upload unless the load asked for `keep_cpu_data` (spec 12 § 3). Typed loads are not touched.
//! Assets loaded before the uploaders were registered keep their CPU copy and are not uploaded:
//! add the plugin before the first counted load (the uploaders register in the first `PreUpdate`
//! that sees a device, before the asset server's update).

use regia_app::prelude::*;

/// The renderer's uploaders of counted assets, registered with the App's asset server
/// ([`CountedMeshPlugin`]). Clones share their tables.
#[derive(Resource, Clone, Debug)]
pub struct CountedAssetUploads {
    /// Counted `CpuMesh` loads: vertex and index buffers in the frame's layout.
    pub meshes: crate::AssetMeshUploads,
    /// Counted `CookedTexture` and `CookedImage` loads: one sampled texture each.
    pub textures: crate::AssetTextureUploads,
}

impl CountedAssetUploads {
    /// Uploaders on `device`'s device and queue.
    #[must_use]
    pub fn new(device: &crate::RenderDevice) -> Self {
        Self {
            meshes: crate::AssetMeshUploads::new(device),
            textures: crate::AssetTextureUploads::new(device),
        }
    }

    /// Registers the uploaders with `server`: counted mesh, texture and image loads upload from
    /// now on and drop their CPU copy unless they asked to keep it.
    pub fn register(&self, server: &mut regia_asset::AssetServer) {
        server.register_prepare(self.meshes.clone());
        server.register_prepare(self.textures.cooked_textures());
        server.register_prepare(self.textures.images());
    }
}

/// Registers [`CountedAssetUploads`] with the App's asset server and the runtime renderer
/// (ADR-7330). Add it with `RenderPlugin` and `regia_asset::AssetPlugin`.
#[derive(Debug, Clone, Copy, Default)]
pub struct CountedMeshPlugin;

impl Plugin for CountedMeshPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            PreUpdate,
            (
                wire_counted_uploads.before(regia_asset::asset_server_update_system),
                release_unloaded_counted_meshes.after(regia_asset::asset_server_update_system),
            ),
        );
    }
}

/// Makes the uploaders once a device and an asset server exist, registers them, and keeps the
/// runtime renderer pointed at the mesh uploader (a renderer rebuilt after a device loss gets
/// it again). Does nothing per frame once wired beyond one pointer check.
pub fn wire_counted_uploads(
    mut commands: Commands,
    device: Option<Res<crate::RenderDevice>>,
    server: Option<ResMut<regia_asset::AssetServer>>,
    uploads: Option<Res<CountedAssetUploads>>,
    runtime: Option<ResMut<crate::RuntimeRenderer>>,
) {
    if let Some(uploads) = uploads {
        if let Some(mut runtime) = runtime
            && runtime.0.asset_meshes().is_none()
        {
            runtime.0.set_asset_meshes(Some(uploads.meshes.clone()));
        }
        return;
    }
    let (Some(device), Some(mut server)) = (device, server) else {
        return;
    };
    let uploads = CountedAssetUploads::new(&device);
    uploads.register(&mut server);
    if let Some(mut runtime) = runtime {
        runtime.0.set_asset_meshes(Some(uploads.meshes.clone()));
    }
    commands.insert_resource(uploads);
}

/// Frees the renderer's hold on counted meshes the asset server unloaded this frame
/// ([`crate::FrameRenderer::release_unloaded_asset_meshes`]), so their GPU memory goes even when
/// no frame is drawn (an empty scene draws none).
pub fn release_unloaded_counted_meshes(runtime: Option<ResMut<crate::RuntimeRenderer>>) {
    if let Some(mut runtime) = runtime
        && runtime.0.adopted_asset_mesh_count() > 0
    {
        runtime.0.release_unloaded_asset_meshes();
    }
}
