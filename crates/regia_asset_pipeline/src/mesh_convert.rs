// SPDX-License-Identifier: MPL-2.0
//! Shared pieces of the single-mesh converters (`ply_import`, `stl_import`): the up-axis choice,
//! the unit scale and the one-node GLB the glTF importer cooks (ADR-3244, the OBJ path of ADR-1602).

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::gltf_write::{GlbMesh, GlbNode, GlbPrimitive, GlbScene, GlbWriteError};

/// Which source axis points up. The glTF convention is +Y up; a Z-up file (CAD, 3D printing,
/// most scans) is turned by -90 degrees about X, `(x, y, z)` to `(x, z, -y)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpAxis {
    /// +Y is up: the file is used as written.
    #[default]
    Y,
    /// +Z is up: the file is turned to +Y up.
    Z,
}

impl UpAxis {
    /// Applies the axis choice to one vector (positions and normals alike).
    #[must_use]
    pub(crate) fn apply(self, v: [f32; 3]) -> [f32; 3] {
        match self {
            UpAxis::Y => v,
            UpAxis::Z => [v[0], v[2], -v[1]],
        }
    }
}

/// A usable scale: finite and positive, else 1.
pub(crate) fn sane_scale(scale: f32) -> f32 {
    if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    }
}

/// Opens `path` for reading only when it is a regular file (symlinks followed). A FIFO, socket or
/// device is an `InvalidInput` error before `open`: opening a FIFO for reading waits for a writer,
/// which would block the caller (the editor's world thread for an agent inspect) forever.
///
/// # Errors
/// The metadata or open error, or `InvalidInput` for a path that is not a regular file.
pub(crate) fn open_regular(path: &Path) -> std::io::Result<std::fs::File> {
    if !std::fs::metadata(path)?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    std::fs::File::open(path)
}

/// Reads a file refusing one larger than `limit` bytes. `Err` is `(size, io message)`: `Ok(Err)`
/// for a read failure or a path that is not a regular file (`open_regular`), `Err(size)` for a
/// file over the limit.
pub(crate) fn read_capped(path: &Path, limit: u64) -> Result<Result<Vec<u8>, String>, u64> {
    use std::io::Read;
    let file = match open_regular(path) {
        Ok(f) => f,
        Err(e) => return Ok(Err(format!("{}: {e}", path.display()))),
    };
    let size = match file.metadata() {
        Ok(m) => m.len(),
        Err(e) => return Ok(Err(format!("{}: {e}", path.display()))),
    };
    if size > limit {
        return Err(size);
    }
    let mut bytes = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
    if let Err(e) = file.take(limit.saturating_add(1)).read_to_end(&mut bytes) {
        return Ok(Err(format!("{}: {e}", path.display())));
    }
    if bytes.len() as u64 > limit {
        return Err(bytes.len() as u64);
    }
    Ok(Ok(bytes))
}

/// A one-mesh, one-node GLB around `primitive`.
///
/// # Errors
///
/// [`GlbWriteError`] when a bounded accessor (the positions) holds a non-finite component.
pub(crate) fn single_primitive_glb(
    generator: &str,
    name: &str,
    primitive: GlbPrimitive,
) -> Result<Vec<u8>, GlbWriteError> {
    let mut scene = GlbScene {
        generator: generator.into(),
        ..GlbScene::default()
    };
    scene.meshes.push(GlbMesh {
        name: name.to_string(),
        primitives: vec![primitive],
        ..GlbMesh::default()
    });
    scene.nodes.push(GlbNode {
        name: name.to_string(),
        mesh: Some(0),
        ..GlbNode::default()
    });
    scene.roots.push(0);
    scene.to_glb()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn z_up_turns_to_y_up() {
        assert_eq!(UpAxis::Z.apply([1.0, 2.0, 3.0]), [1.0, 3.0, -2.0]);
        assert_eq!(UpAxis::Y.apply([1.0, 2.0, 3.0]), [1.0, 2.0, 3.0]);
    }

    #[test]
    fn a_bad_scale_is_one() {
        for bad in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            assert_eq!(sane_scale(bad), 1.0);
        }
        assert_eq!(sane_scale(0.001), 0.001);
    }

    /// Review rc1-rev-editor-k finding 1 (the FIFO part): `editor.ply_inspect` and its siblings read
    /// through these helpers on the world thread, and `File::open` on a FIFO waits for a writer.
    /// Every importer's file reader refuses a FIFO with its `*_io` code instead of blocking. (On the
    /// old code this test does not fail, it hangs in the first `open`.)
    #[cfg(all(unix, feature = "usd_import"))]
    #[test]
    fn every_importer_file_reader_refuses_a_fifo_without_blocking() {
        use crate::{fbx_import, obj_import, ply_import, psd_import, stl_import, usd_import};
        let dir = regia_test_utils::unique_temp_dir("mesh-convert-fifo");
        let mut codes = Vec::new();
        for ext in ["ply", "stl", "psd", "fbx", "obj", "usda"] {
            let fifo = dir.join(format!("pipe.{ext}"));
            let made = std::process::Command::new("mkfifo").arg(&fifo).status();
            if !made.is_ok_and(|s| s.success()) {
                let _ = std::fs::remove_dir_all(&dir);
                return; // no mkfifo on this host
            }
            let code = match ext {
                "ply" => ply_import::read_ply_file(&fifo, &Default::default())
                    .map(drop)
                    .map_err(|e| e.code()),
                "stl" => stl_import::read_stl_file(&fifo, &Default::default())
                    .map(drop)
                    .map_err(|e| e.code()),
                "psd" => psd_import::read_psd_file(&fifo, &Default::default())
                    .map(drop)
                    .map_err(|e| e.code()),
                "fbx" => fbx_import::read_fbx_file(&fifo, &Default::default())
                    .map(drop)
                    .map_err(|e| e.code()),
                "obj" => obj_import::read_obj_file(&fifo, &Default::default())
                    .map(drop)
                    .map_err(|e| e.code()),
                _ => usd_import::convert_usd_to_glb(
                    usd_import::UsdSource::File(&fifo),
                    &usd_import::UsdImportSettings::default(),
                )
                .map(drop)
                .map_err(|e| e.code()),
            };
            codes.push((ext, code));
        }
        assert_eq!(
            codes,
            [
                ("ply", Err("ply_io")),
                ("stl", Err("stl_io")),
                ("psd", Err("psd_io")),
                ("fbx", Err("fbx_io")),
                ("obj", Err("obj_io")),
                ("usda", Err("usd_io")),
            ]
        );
        // A regular file still reads.
        let plain = dir.join("plain.obj");
        std::fs::write(&plain, "v 0 0 0\n").expect("write");
        assert_eq!(
            obj_import::read_obj_file(&plain, &Default::default()).expect("reads"),
            b"v 0 0 0\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
