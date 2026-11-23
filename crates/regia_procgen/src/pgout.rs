// SPDX-License-Identifier: MPL-2.0
//! Baked output files (`<generator guid>.pgout`, format `pgout`, version 4; spec 62 § 8).
//!
//! Layout, little endian, f32/f64 IEEE: magic `RPGO`, `version: u32`, graph hash, parameter hash,
//! `seed: u32`, input hash, output hash (32 bytes each), `quality: u8` + 3 pad bytes, a cell table
//! (`count: u32`, then per cell `i: i32, j: i32, tier: u8` + 3 pad bytes, `offset: u64, len: u64`
//! and the cell's output hash (32 bytes); one cell `(0, 0)` tier 0 for untiled generators, spec 66
//! § 2 for tiled ones), then the cell sections: instance sets, prefab records, tables, point sets,
//! masks, generated meshes, terrain edit records, road decals (spec 64 §§ 6, 7.2, 7.3) and the data
//! layers of the instance sets that have one (`set_index: u32` + name; spec 63 § 13.1, ADR-7214),
//! then, only when a prefab record has `spawn.prefab` overrides (spec 63 § 13.2), the optional
//! override section: `count: u32` (at least 1), then per overridden record its index into the
//! prefab records (`u32`, strictly increasing) and `count: u32` (at least 1) overrides, each the
//! component and field strings and the value as a one-element attribute column (ADR-7750, the
//! ADR-7724 (h) fold). A section that ends after the set layers has no overrides, so every early
//! version 4 file reads unchanged.
//! The header's output hash is the whole output's (the cells merged as `tiles::merge_outputs`).
//! Strings are `len: u32` + UTF-8. Readers reject unknown versions; a loaded file is re-hashed and
//! must match its header (a hand edit fails with `pgout_corrupt`).
//!
//! Version 1 (v0.2.6, before spec 66) had one untiled cell whose table entry was `i: i32, j: i32,
//! offset: u64, len: u64` (no tier, no cell hash). Version 2 (v0.2.7) has this layout but its
//! sections end after the masks. Version 3 (v0.2.8 to v0.5) ends its sections after the road
//! decals (no set layers). Version 4 files written before ADR-7750 end after the set layers; that is
//! the same layout with no override section. [`decode`] reads all three in memory and [`migrate`] rewrites them as the
//! current version (`regia upgrade-project`).

use std::collections::BTreeMap;

use crate::data::{AttrColumn, AttrType, PointSet, Table, Xform};
use regia_terrain::RoadDecal;

use crate::exec::{
    Collision, GeneratedMesh, Instance, InstanceSet, PrefabFieldOverride, PrefabSpawn, RunError, RunOutput,
    TerrainEditRecord,
};

/// File magic.
pub const PGOUT_MAGIC: [u8; 4] = *b"RPGO";
/// The version this build reads and writes.
pub const PGOUT_VERSION: u32 = 4;
/// The type path a cooked `.pgout` carries in a pak (spec 62 § 8, ADR-7214): the file's bytes,
/// stored unchanged by the cook, read back by the bake loader through the asset server.
pub const PGOUT_COOKED_TYPE_PATH: &str = "regia_procgen::pgout::PgoutBytes";
/// Whether this build can write bakes (the `pipeline` feature, spec 62 § 11).
pub const PIPELINE: bool = cfg!(feature = "pipeline");

/// The layout fingerprint of `pgout` for `cargo xtask check-formats`. Edit it with [`encode`].
pub fn pgout_schema_fingerprint() -> String {
    "RPGO,version:u32,graph_hash:32,param_hash:32,seed:u32,input_hash:32,output_hash:32,quality:u8+3;\
     cells:u32[(i:i32,j:i32,tier:u8+3,offset:u64,len:u64,output_hash:32)];section(instance_sets:u32[(mesh,material?,shadows:u8,\
     collision:u8,draw,fade,cull:f32,instances:u64[(t3,r4,s3,color4,custom4:f32)])],prefabs:u32[(prefab,\
     name,t3,r4,s3:f32,tags:u32[str])],tables:u32[(name,rows:u64,columns:u32[(name,column)])],\
     points:u32[(name,points:u64[(position3,rotation4,scale3,color4,density:f32,seed:u32,bmin3,bmax3,\
     hardness:f32)],attrs:u32[(name,column)])],masks:u32[(name,rect4:f32,width:u32,height:u32,bilinear:u8,\
     values:u64[f32])],meshes:u32[(material,collision:u8,positions:u64[f32x3],normals:u64[f32x3],\
     uvs:u64[f32x2],colors:u64[f32x4],indices:u64[u32])],terrain_edits:u32[(rect:u32x4,samples:u64[f32],\
     original:u64[f32])],road_decals:u32[(a2,b2,half_width,color3:f32)],set_layers:u32[(set:u32,layer)],\
     prefab_overrides?:u32[(prefab:u32,overrides:u32[(component,field,value:column1)])]);\
     column=(type,len:u64,values)"
        .to_string()
}

/// `pgout` migration sources: version 1 (the untiled cell table), version 2 (no meshes, terrain
/// edits or road decals) and version 3 (no set layers), all through [`migrate`].
pub fn pgout_migration_sources() -> Vec<u32> {
    vec![1, 2, 3]
}

/// The `.pgout` header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PgoutHeader {
    /// Graph hash (spec 62 § 6.4).
    pub graph_hash: [u8; 32],
    /// Hash of the effective parameters.
    pub param_hash: [u8; 32],
    /// Generator seed.
    pub seed: u32,
    /// Hash of every dependency (inputs) in order.
    pub input_hash: [u8; 32],
    /// [`RunOutput::hash`] of the stored output.
    pub output_hash: [u8; 32],
    /// Quality tier index (`MobileLow` = 0 … `Ultra` = 6).
    pub quality: u8,
}

/// A read failure.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct PgoutError {
    /// `pgout_magic`, `pgout_version`, `pgout_truncated`, `pgout_corrupt`, `pgout_io`.
    pub code: &'static str,
    /// Detail.
    pub message: String,
}

fn err(code: &'static str, message: impl Into<String>) -> PgoutError {
    PgoutError {
        code,
        message: message.into(),
    }
}

struct W(Vec<u8>);

impl W {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn i64(&mut self, v: i64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn f32s(&mut self, v: &[f32]) {
        for x in v {
            self.0.extend_from_slice(&x.to_le_bytes());
        }
    }
    fn vecs<const N: usize>(&mut self, v: &[[f32; N]]) {
        self.u64(v.len() as u64);
        v.iter().for_each(|x| self.f32s(x));
    }
    fn f64s(&mut self, v: &[f64]) {
        for x in v {
            self.0.extend_from_slice(&x.to_le_bytes());
        }
    }
    fn str(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.0.extend_from_slice(s.as_bytes());
    }
    fn column(&mut self, c: &AttrColumn) {
        self.str(c.ty().name());
        self.u64(c.len() as u64);
        match c {
            AttrColumn::Bool(v) => v.iter().for_each(|b| self.u8(u8::from(*b))),
            AttrColumn::Int(v) => v.iter().for_each(|x| self.i64(*x)),
            AttrColumn::Float(v) => self.f64s(v),
            AttrColumn::Vec2(v) => v.iter().for_each(|x| self.f64s(x)),
            AttrColumn::Vec3(v) => v.iter().for_each(|x| self.f64s(x)),
            AttrColumn::Vec4(v) | AttrColumn::Quat(v) => v.iter().for_each(|x| self.f64s(x)),
            AttrColumn::Transform(v) => v.iter().for_each(|x| {
                self.f64s(&x.t);
                self.f64s(&x.r);
                self.f64s(&x.s);
            }),
            AttrColumn::Name(v) | AttrColumn::Asset(v) => v.iter().for_each(|s| self.str(s)),
        }
    }
}

struct R<'a> {
    b: &'a [u8],
    i: usize,
}

impl R<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], PgoutError> {
        let end = self
            .i
            .checked_add(n)
            .filter(|e| *e <= self.b.len())
            .ok_or_else(|| err("pgout_truncated", "file ends early"))?;
        let s = &self.b[self.i..end];
        self.i = end;
        Ok(s)
    }
    fn arr<const N: usize>(&mut self) -> Result<[u8; N], PgoutError> {
        let mut a = [0u8; N];
        a.copy_from_slice(self.take(N)?);
        Ok(a)
    }
    fn u8(&mut self) -> Result<u8, PgoutError> {
        Ok(self.arr::<1>()?[0])
    }
    fn u32(&mut self) -> Result<u32, PgoutError> {
        Ok(u32::from_le_bytes(self.arr()?))
    }
    fn u64(&mut self) -> Result<u64, PgoutError> {
        Ok(u64::from_le_bytes(self.arr()?))
    }
    fn i32(&mut self) -> Result<i32, PgoutError> {
        Ok(i32::from_le_bytes(self.arr()?))
    }
    fn i64(&mut self) -> Result<i64, PgoutError> {
        Ok(i64::from_le_bytes(self.arr()?))
    }
    fn f32(&mut self) -> Result<f32, PgoutError> {
        Ok(f32::from_le_bytes(self.arr()?))
    }
    fn f64(&mut self) -> Result<f64, PgoutError> {
        Ok(f64::from_le_bytes(self.arr()?))
    }
    fn f32n<const N: usize>(&mut self) -> Result<[f32; N], PgoutError> {
        let mut a = [0.0f32; N];
        for x in &mut a {
            *x = self.f32()?;
        }
        Ok(a)
    }
    fn f64n<const N: usize>(&mut self) -> Result<[f64; N], PgoutError> {
        let mut a = [0.0f64; N];
        for x in &mut a {
            *x = self.f64()?;
        }
        Ok(a)
    }
    fn vecs<const N: usize>(&mut self) -> Result<Vec<[f32; N]>, PgoutError> {
        let n = self.count(4 * N)?;
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            v.push(self.f32n()?);
        }
        Ok(v)
    }
    fn f32_list(&mut self) -> Result<Vec<f32>, PgoutError> {
        let n = self.count(4)?;
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            v.push(self.f32()?);
        }
        Ok(v)
    }
    /// A count, bounded by the bytes left (each element takes at least `min` bytes).
    fn count(&mut self, min: usize) -> Result<usize, PgoutError> {
        let n = self.u64()? as usize;
        if n.saturating_mul(min.max(1)) > self.b.len() - self.i {
            return Err(err("pgout_truncated", format!("count {n} exceeds the file")));
        }
        Ok(n)
    }
    fn str(&mut self) -> Result<String, PgoutError> {
        let n = self.u32()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| err("pgout_corrupt", "string is not UTF-8"))
    }
    fn column(&mut self) -> Result<AttrColumn, PgoutError> {
        let ty_name = self.str()?;
        let ty = AttrType::from_name(&ty_name)
            .ok_or_else(|| err("pgout_corrupt", format!("unknown type `{ty_name}`")))?;
        let n = self.count(1)?;
        let mut c = AttrColumn::new(ty);
        for _ in 0..n {
            use crate::data::AttrValue as V;
            let v = match ty {
                AttrType::Bool => V::Bool(self.u8()? != 0),
                AttrType::Int => V::Int(self.i64()?),
                AttrType::Float => V::Float(self.f64()?),
                AttrType::Vec2 => V::Vec2(self.f64n()?),
                AttrType::Vec3 => V::Vec3(self.f64n()?),
                AttrType::Vec4 => V::Vec4(self.f64n()?),
                AttrType::Quat => V::Quat(self.f64n()?),
                AttrType::Transform => V::Transform(Xform {
                    t: self.f64n()?,
                    r: self.f64n()?,
                    s: self.f64n()?,
                }),
                AttrType::Name => V::Name(self.str()?),
                AttrType::Asset => V::Asset(self.str()?),
            };
            c.push(v);
        }
        Ok(c)
    }
}

fn encode_points(w: &mut W, p: &PointSet) {
    w.u64(p.len() as u64);
    for i in 0..p.len() {
        let q = p.point(i);
        w.f32s(&q.position);
        w.f32s(&q.rotation);
        w.f32s(&q.scale);
        w.f32s(&q.color);
        w.f32s(&[q.density]);
        w.u32(q.seed);
        w.f32s(&q.bounds_min);
        w.f32s(&q.bounds_max);
        w.f32s(&[q.hardness]);
    }
    w.u32(p.attrs.len() as u32);
    for (k, c) in &p.attrs {
        w.str(k);
        w.column(c);
    }
}

fn decode_points(r: &mut R<'_>) -> Result<PointSet, PgoutError> {
    let n = r.count(92)?;
    let mut p = PointSet::new();
    let w = p.writer();
    for _ in 0..n {
        w.position.push(r.f32n()?);
        w.rotation.push(r.f32n()?);
        w.scale.push(r.f32n()?);
        w.color.push(r.f32n()?);
        w.density.push(r.f32()?);
        w.seed.push(r.u32()?);
        w.bounds_min.push(r.f32n()?);
        w.bounds_max.push(r.f32n()?);
        w.hardness.push(r.f32()?);
    }
    for _ in 0..r.u32()? {
        let k = r.str()?;
        let c = r.column()?;
        if c.len() != n {
            return Err(err(
                "pgout_corrupt",
                format!("attribute `{k}` has {} values for {n} points", c.len()),
            ));
        }
        p.attrs.insert(k, c);
    }
    Ok(p)
}

fn encode_section(output: &RunOutput) -> Vec<u8> {
    let mut w = W(Vec::new());
    w.u32(output.instance_sets.len() as u32);
    for s in &output.instance_sets {
        w.str(&s.mesh);
        match &s.material {
            Some(m) => {
                w.u8(1);
                w.str(m);
            }
            None => w.u8(0),
        }
        w.u8(u8::from(s.cast_shadows));
        w.u8(s.collision as u8);
        w.f32s(&[s.draw_distance_m, s.fade_m, s.cull_below_px]);
        w.u64(s.instances.len() as u64);
        for i in &s.instances {
            w.f32s(&i.translation);
            w.f32s(&i.rotation);
            w.f32s(&i.scale);
            w.f32s(&i.color);
            w.f32s(&i.custom);
        }
    }
    w.u32(output.prefabs.len() as u32);
    for p in &output.prefabs {
        w.str(&p.prefab);
        w.str(&p.name);
        w.f32s(&p.translation);
        w.f32s(&p.rotation);
        w.f32s(&p.scale);
        w.u32(p.tags.len() as u32);
        p.tags.iter().for_each(|t| w.str(t));
    }
    w.u32(output.tables.len() as u32);
    for (k, t) in &output.tables {
        w.str(k);
        w.u64(t.rows as u64);
        w.u32(t.columns.len() as u32);
        for (name, c) in &t.columns {
            w.str(name);
            w.column(c);
        }
    }
    w.u32(output.points.len() as u32);
    for (k, p) in &output.points {
        w.str(k);
        encode_points(&mut w, p);
    }
    w.u32(output.masks.len() as u32);
    for (k, m) in &output.masks {
        w.str(k);
        w.f32s(&m.rect);
        w.u32(m.width);
        w.u32(m.height);
        w.u8(u8::from(m.bilinear));
        w.u64(m.values.len() as u64);
        w.f32s(&m.values);
    }
    w.u32(output.meshes.len() as u32);
    for m in &output.meshes {
        w.str(&m.material);
        w.u8(u8::from(m.collision));
        w.vecs(&m.positions);
        w.vecs(&m.normals);
        w.vecs(&m.uvs);
        w.vecs(&m.colors);
        w.u64(m.indices.len() as u64);
        m.indices.iter().for_each(|i| w.u32(*i));
    }
    w.u32(output.terrain_edits.len() as u32);
    for t in &output.terrain_edits {
        t.rect.iter().for_each(|v| w.u32(*v));
        w.u64(t.samples.len() as u64);
        w.f32s(&t.samples);
        w.u64(t.original.len() as u64);
        w.f32s(&t.original);
    }
    w.u32(output.road_decals.len() as u32);
    for d in &output.road_decals {
        w.f32s(&d.a);
        w.f32s(&d.b);
        w.f32s(&[d.half_width]);
        w.f32s(&d.color);
    }
    // Version 4: the data layer of each set that has one (`target_layer`).
    let layered: Vec<(usize, &str)> = output
        .instance_sets
        .iter()
        .enumerate()
        .filter_map(|(k, s)| s.layer.as_deref().map(|l| (k, l)))
        .collect();
    w.u32(layered.len() as u32);
    for (k, layer) in layered {
        w.u32(k as u32);
        w.str(layer);
    }
    // Version 4, optional (ADR-7750): the overrides of the prefab records that have any. Absent
    // when none has, so an output without overrides keeps the early version 4 bytes.
    let overridden: Vec<(usize, &[PrefabFieldOverride])> = output
        .prefabs
        .iter()
        .enumerate()
        .filter(|(_, p)| !p.overrides.is_empty())
        .map(|(k, p)| (k, p.overrides.as_slice()))
        .collect();
    if !overridden.is_empty() {
        w.u32(overridden.len() as u32);
        for (k, overrides) in overridden {
            w.u32(k as u32);
            w.u32(overrides.len() as u32);
            for o in overrides {
                w.str(&o.component);
                w.str(&o.field);
                // The value in the attribute-column encoding the sections already use.
                let mut value = AttrColumn::new(o.value.ty());
                value.push(o.value.clone());
                w.column(&value);
            }
        }
    }
    w.0
}

/// The optional override section of a version 4 section (ADR-7750); `out.prefabs` is complete.
fn decode_overrides(r: &mut R<'_>, out: &mut RunOutput) -> Result<(), PgoutError> {
    let records = r.u32()?;
    if records == 0 {
        return Err(err(
            "pgout_corrupt",
            "an empty override section (it is written only when a prefab has overrides)",
        ));
    }
    let mut next = 0usize;
    for _ in 0..records {
        let index = r.u32()? as usize;
        if index < next || index >= out.prefabs.len() {
            return Err(err(
                "pgout_corrupt",
                format!(
                    "override record for prefab {index} (of {}) out of order or out of range",
                    out.prefabs.len()
                ),
            ));
        }
        next = index + 1;
        let count = r.u32()?;
        if count == 0 {
            return Err(err(
                "pgout_corrupt",
                format!("prefab {index} is listed with no override"),
            ));
        }
        let mut overrides = Vec::new();
        for _ in 0..count {
            let component = r.str()?;
            let field = r.str()?;
            let column = r.column()?;
            let value = match (column.len(), column.get(0)) {
                (1, Some(v)) => v,
                (n, _) => {
                    return Err(err(
                        "pgout_corrupt",
                        format!("override `{component}.{field}` has {n} values (one expected)"),
                    ));
                }
            };
            overrides.push(PrefabFieldOverride {
                component,
                field,
                value,
            });
        }
        out.prefabs[index].overrides = overrides;
    }
    Ok(())
}

/// Generated meshes, terrain edits and road decals, which version 3 sections append.
fn decode_v3_tail(r: &mut R<'_>, out: &mut RunOutput) -> Result<(), PgoutError> {
    for _ in 0..r.u32()? {
        let material = r.str()?;
        let collision = r.u8()? != 0;
        let positions = r.vecs()?;
        let normals = r.vecs()?;
        let uvs = r.vecs()?;
        let colors: Vec<[f32; 4]> = r.vecs()?;
        let n = r.count(4)?;
        let mut indices = Vec::with_capacity(n);
        for _ in 0..n {
            indices.push(r.u32()?);
        }
        let v = positions.len();
        if normals.len() != v
            || uvs.len() != v
            || !(colors.is_empty() || colors.len() == v)
            || !indices.len().is_multiple_of(3)
            || indices.iter().any(|i| *i as usize >= v)
        {
            return Err(err("pgout_corrupt", "a generated mesh has ragged streams"));
        }
        out.meshes.push(GeneratedMesh {
            material,
            positions,
            normals,
            uvs,
            indices,
            colors,
            collision,
        });
    }
    for _ in 0..r.u32()? {
        let rect = [r.u32()?, r.u32()?, r.u32()?, r.u32()?];
        let samples = r.f32_list()?;
        let original = r.f32_list()?;
        let area = u64::from(rect[2].saturating_sub(rect[0])) * u64::from(rect[3].saturating_sub(rect[1]));
        if samples.len() as u64 != area || original.len() as u64 != area {
            return Err(err(
                "pgout_corrupt",
                "a terrain edit's samples do not fill its rectangle",
            ));
        }
        out.terrain_edits.push(TerrainEditRecord {
            rect,
            samples,
            original,
        });
    }
    for _ in 0..r.u32()? {
        let (a, b) = (r.f32n()?, r.f32n()?);
        let half_width = r.f32()?;
        let color = r.f32n()?;
        out.road_decals.push(RoadDecal {
            a,
            b,
            half_width,
            color,
        });
    }
    Ok(())
}

fn decode_section(r: &mut R<'_>, version: u32) -> Result<RunOutput, PgoutError> {
    let mut out = RunOutput::default();
    for _ in 0..r.u32()? {
        let mesh = r.str()?;
        let material = if r.u8()? == 1 { Some(r.str()?) } else { None };
        let cast_shadows = r.u8()? != 0;
        let collision = match r.u8()? {
            0 => Collision::None,
            1 => Collision::Simple,
            2 => Collision::Mesh,
            c => return Err(err("pgout_corrupt", format!("unknown collision {c}"))),
        };
        let [draw_distance_m, fade_m, cull_below_px] = r.f32n()?;
        let n = r.count(72)?;
        let mut instances = Vec::with_capacity(n);
        for _ in 0..n {
            instances.push(Instance {
                translation: r.f32n()?,
                rotation: r.f32n()?,
                scale: r.f32n()?,
                color: r.f32n()?,
                custom: r.f32n()?,
            });
        }
        out.instance_sets.push(InstanceSet {
            mesh,
            material,
            cast_shadows,
            collision,
            draw_distance_m,
            fade_m,
            cull_below_px,
            instances,
            layer: None,
        });
    }
    for _ in 0..r.u32()? {
        let prefab = r.str()?;
        let name = r.str()?;
        let (translation, rotation, scale) = (r.f32n()?, r.f32n()?, r.f32n()?);
        let mut tags = Vec::new();
        for _ in 0..r.u32()? {
            tags.push(r.str()?);
        }
        out.prefabs.push(PrefabSpawn {
            prefab,
            name,
            translation,
            rotation,
            scale,
            tags,
            // Filled from the optional override section at the end (ADR-7750).
            overrides: Vec::new(),
        });
    }
    for _ in 0..r.u32()? {
        let k = r.str()?;
        // A column-less table carries no bytes per row, so the file size cannot bound `rows`; no run
        // emits more rows than the largest element budget (audit: zai M5).
        let rows = r.u64()?;
        let max_rows = crate::exec::RunLimits::DESKTOP.total_elements as u64;
        if rows > max_rows {
            return Err(err(
                "pgout_corrupt",
                format!("table `{k}` has {rows} rows (at most {max_rows})"),
            ));
        }
        let rows = rows as usize;
        let mut t = Table {
            rows,
            columns: Vec::new(),
        };
        for _ in 0..r.u32()? {
            let name = r.str()?;
            let c = r.column()?;
            if c.len() != rows {
                return Err(err(
                    "pgout_corrupt",
                    format!("table `{k}` column `{name}` is ragged"),
                ));
            }
            t.columns.push((name, c));
        }
        out.tables.insert(k, t);
    }
    let mut points = BTreeMap::new();
    for _ in 0..r.u32()? {
        let k = r.str()?;
        points.insert(k, decode_points(r)?);
    }
    out.points = points;
    for _ in 0..r.u32()? {
        let k = r.str()?;
        let rect = r.f32n()?;
        let (width, height) = (r.u32()?, r.u32()?);
        let bilinear = r.u8()? != 0;
        let n = r.count(4)?;
        let mut values = Vec::with_capacity(n);
        for _ in 0..n {
            values.push(r.f32()?);
        }
        let m = crate::data::MaskData::new(rect, width, height, values, bilinear)
            .map_err(|e| err("pgout_corrupt", e))?;
        out.masks.insert(k, m);
    }
    if version >= 3 {
        decode_v3_tail(r, &mut out)?;
    }
    if version >= 4 {
        for _ in 0..r.u32()? {
            let index = r.u32()? as usize;
            let layer = r.str()?;
            let Some(set) = out.instance_sets.get_mut(index) else {
                return Err(err(
                    "pgout_corrupt",
                    "a set layer names a set the section does not have",
                ));
            };
            set.layer = Some(layer);
        }
        // `r` holds exactly one cell section (`decode_section_bytes`): bytes left after the set
        // layers are the optional override section; none left is an early v4 section (ADR-7750).
        if r.i < r.b.len() {
            decode_overrides(r, &mut out)?;
        }
    }
    Ok(out)
}

/// One entry of the cell table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PgoutCell {
    /// Tier index.
    pub tier: u8,
    /// Cell index along X.
    pub i: i32,
    /// Cell index along Z.
    pub j: i32,
    /// Byte offset of the cell's section.
    pub offset: u64,
    /// Section length in bytes.
    pub len: u64,
    /// [`RunOutput::hash`] of the cell's section.
    pub output_hash: [u8; 32],
    /// The version of the file the section came from.
    pub version: u32,
}

/// One section to assemble: `(tier, i, j, output hash, section bytes)`.
type Section<'a> = (u8, i32, i32, [u8; 32], &'a [u8]);
/// A section with owned bytes.
type OwnedSection = (u8, i32, i32, [u8; 32], Vec<u8>);

/// Bytes of one cell table entry.
const CELL_ENTRY: usize = 4 + 4 + 4 + 8 + 8 + 32;
/// Bytes before the cell table.
const HEADER_LEN: usize = 4 + 4 + 32 + 32 + 4 + 32 + 32 + 4;

fn write_header(w: &mut W, header: &PgoutHeader) {
    w.0.extend_from_slice(&PGOUT_MAGIC);
    w.u32(PGOUT_VERSION);
    w.0.extend_from_slice(&header.graph_hash);
    w.0.extend_from_slice(&header.param_hash);
    w.u32(header.seed);
    w.0.extend_from_slice(&header.input_hash);
    w.0.extend_from_slice(&header.output_hash);
    w.0.extend_from_slice(&[header.quality, 0, 0, 0]);
}

/// Assembles a file from a header and `(tier, i, j, output hash, section bytes)` in table order.
fn assemble(header: &PgoutHeader, sections: &[Section<'_>]) -> Vec<u8> {
    let body: usize = sections.iter().map(|s| s.4.len()).sum();
    let mut w = W(Vec::with_capacity(
        HEADER_LEN + 4 + sections.len() * CELL_ENTRY + body,
    ));
    write_header(&mut w, header);
    w.u32(sections.len() as u32);
    let mut offset = (HEADER_LEN + 4 + sections.len() * CELL_ENTRY) as u64;
    for (tier, i, j, hash, bytes) in sections {
        w.0.extend_from_slice(&i.to_le_bytes());
        w.0.extend_from_slice(&j.to_le_bytes());
        w.0.extend_from_slice(&[*tier, 0, 0, 0]);
        w.u64(offset);
        w.u64(bytes.len() as u64);
        w.0.extend_from_slice(hash);
        offset += bytes.len() as u64;
    }
    for s in sections {
        w.0.extend_from_slice(s.4);
    }
    w.0
}

/// Encodes a bake (one untiled cell).
pub fn encode(header: &PgoutHeader, output: &RunOutput) -> Vec<u8> {
    let section = encode_section(output);
    assemble(header, &[(0, 0, 0, output.hash(), &section)])
}

/// One cell to encode: `(tier, i, j, owned output)`.
pub type CellInput<'a> = (u8, i32, i32, &'a RunOutput);

/// Encodes a tiled bake: one section per `(tier, i, j, owned output)`, sorted by `(tier, i, j)`.
/// `header.output_hash` must be the merged output's hash ([`crate::tiles::merge_outputs`]).
pub fn encode_cells(header: &PgoutHeader, cells: &[CellInput<'_>]) -> Vec<u8> {
    let mut sorted: Vec<&CellInput<'_>> = cells.iter().collect();
    sorted.sort_by_key(|c| (c.0, c.1, c.2));
    let sections: Vec<OwnedSection> = sorted
        .iter()
        .map(|c| (c.0, c.1, c.2, c.3.hash(), encode_section(c.3)))
        .collect();
    let refs: Vec<Section<'_>> = sections
        .iter()
        .map(|s| (s.0, s.1, s.2, s.3, s.4.as_slice()))
        .collect();
    assemble(header, &refs)
}

/// Reads the header and the cell table (no section is decoded).
///
/// # Errors
///
/// [`PgoutError`] for a wrong magic, an unknown version, a truncated file or a table whose ranges do
/// not tile the file.
pub fn read_table(bytes: &[u8]) -> Result<(PgoutHeader, Vec<PgoutCell>), PgoutError> {
    read_table_at(bytes, bytes.len() as u64)
}

/// [`read_table`] over the file's leading bytes (at least the whole table); `size` is the file's.
fn read_table_at(bytes: &[u8], size: u64) -> Result<(PgoutHeader, Vec<PgoutCell>), PgoutError> {
    let mut r = R { b: bytes, i: 0 };
    if r.arr::<4>()? != PGOUT_MAGIC {
        return Err(err("pgout_magic", "not a .pgout file"));
    }
    let version = r.u32()?;
    if !(2..=PGOUT_VERSION).contains(&version) {
        let hint = if version == 1 {
            " (run `regia upgrade-project`)"
        } else {
            ""
        };
        return Err(err(
            "pgout_version",
            format!("version {version}; this build reads {PGOUT_VERSION}{hint}"),
        ));
    }
    let header = PgoutHeader {
        graph_hash: r.arr()?,
        param_hash: r.arr()?,
        seed: r.u32()?,
        input_hash: r.arr()?,
        output_hash: r.arr()?,
        quality: r.arr::<4>()?[0],
    };
    let count = r.u32()? as usize;
    if count == 0 || count as u64 > (size / CELL_ENTRY as u64).max(1) {
        return Err(err("pgout_corrupt", format!("{count} cells")));
    }
    let mut cells = Vec::with_capacity(count);
    for _ in 0..count {
        let (i, j) = (r.i32()?, r.i32()?);
        let tier = r.arr::<4>()?[0];
        let (offset, len) = (r.u64()?, r.u64()?);
        let output_hash = r.arr()?;
        cells.push(PgoutCell {
            tier,
            i,
            j,
            offset,
            len,
            output_hash,
            version,
        });
    }
    // A cell listed twice would stream one copy (the map keeps the last) and merge both in a
    // full decode (audit wave 6, stream M2).
    let mut seen = std::collections::BTreeSet::new();
    for c in &cells {
        if !seen.insert((c.tier, c.i, c.j)) {
            return Err(err(
                "pgout_corrupt",
                format!("cell (tier {}, {}, {}) is listed twice", c.tier, c.i, c.j),
            ));
        }
    }
    let mut at = r.i as u64;
    for c in &cells {
        if c.offset != at {
            return Err(err("pgout_corrupt", "cell table does not match the file"));
        }
        at = at
            .checked_add(c.len)
            .ok_or_else(|| err("pgout_corrupt", "cell length overflows"))?;
    }
    if at != size {
        return Err(err("pgout_corrupt", "cell table does not match the file"));
    }
    Ok((header, cells))
}

/// Decodes one cell's section and checks it against its table hash.
///
/// # Errors
///
/// `pgout_truncated` / `pgout_corrupt` for a bad section or a hash mismatch.
pub fn decode_cell(bytes: &[u8], cell: &PgoutCell) -> Result<RunOutput, PgoutError> {
    let start = usize::try_from(cell.offset).map_err(|_| err("pgout_corrupt", "offset"))?;
    let end = start
        .checked_add(usize::try_from(cell.len).map_err(|_| err("pgout_corrupt", "length"))?)
        .filter(|e| *e <= bytes.len())
        .ok_or_else(|| err("pgout_truncated", "cell section past the end"))?;
    decode_section_bytes(&bytes[start..end], cell)
}

/// Decodes a cell from its section bytes alone (what a streamed read loads).
fn decode_section_bytes(section: &[u8], cell: &PgoutCell) -> Result<RunOutput, PgoutError> {
    let mut r = R { b: section, i: 0 };
    let output = decode_section(&mut r, cell.version)?;
    if r.i != section.len() {
        return Err(err("pgout_corrupt", "cell section has trailing bytes"));
    }
    if output.hash() != cell.output_hash {
        return Err(err("pgout_corrupt", "cell content does not match its table hash"));
    }
    Ok(output)
}

/// A baked file opened for streaming (spec 66 § 4.3 "Baked + anchors"): the header and table are in
/// memory, each cell's section is read from disk on request.
#[derive(Clone, Debug)]
pub struct PgoutFile {
    path: std::path::PathBuf,
    /// The whole file when it came from a pak ([`Self::from_bytes`]); `None` reads from `path`.
    bytes: Option<std::sync::Arc<[u8]>>,
    /// The header.
    pub header: PgoutHeader,
    /// The cell table by `(tier, i, j)`.
    pub cells: BTreeMap<(u8, i32, i32), PgoutCell>,
}

impl PgoutFile {
    /// Opens a file: reads the header and the cell table, no section.
    ///
    /// # Errors
    ///
    /// `pgout_io` when it cannot be read, or the [`read_table`] errors. A version 1 file fails with
    /// `pgout_version` (run `regia upgrade-project`).
    pub fn open(path: &std::path::Path) -> Result<Self, PgoutError> {
        use std::io::Read;
        let io = |e: std::io::Error| err("pgout_io", format!("{}: {e}", path.display()));
        let mut f = std::fs::File::open(path).map_err(io)?;
        let size = f.metadata().map_err(io)?.len();
        let mut head = vec![0u8; (HEADER_LEN + 4).min(size as usize)];
        f.read_exact(&mut head).map_err(io)?;
        if head.len() < HEADER_LEN + 4 {
            // Too short for a header: the table reader names what is wrong.
            read_table_at(&head, size)?;
        }
        let count = head
            .last_chunk::<4>()
            .map_or(0, |c| u32::from_le_bytes(*c) as u64);
        // A table that cannot fit in the file fails here, before any allocation of its size.
        let table = count.saturating_mul(CELL_ENTRY as u64);
        if table > size {
            return Err(err("pgout_corrupt", format!("{count} cells")));
        }
        head.resize(HEADER_LEN + 4 + table as usize, 0);
        f.read_exact(&mut head[HEADER_LEN + 4..]).map_err(io)?;
        let (header, cells) = read_table_at(&head, size)?;
        Ok(Self {
            path: path.to_path_buf(),
            bytes: None,
            header,
            cells: cells.into_iter().map(|c| ((c.tier, c.i, c.j), c)).collect(),
        })
    }

    /// A file already in memory (a bake read from a pak, ADR-7214), named `path` in messages: the
    /// header and table are read now, each cell's section is decoded on request.
    ///
    /// # Errors
    ///
    /// The [`read_table`] errors.
    pub fn from_bytes(path: &std::path::Path, bytes: std::sync::Arc<[u8]>) -> Result<Self, PgoutError> {
        let (header, cells) = read_table(&bytes)?;
        Ok(Self {
            path: path.to_path_buf(),
            bytes: Some(bytes),
            header,
            cells: cells.into_iter().map(|c| ((c.tier, c.i, c.j), c)).collect(),
        })
    }

    /// Reads and decodes one cell (`None`: the table has no such cell).
    ///
    /// # Errors
    ///
    /// `pgout_io`, or `pgout_truncated` / `pgout_corrupt` for a bad section or a hash mismatch.
    pub fn read_cell(&self, tier: u8, i: i32, j: i32) -> Result<Option<RunOutput>, PgoutError> {
        use std::io::{Read, Seek};
        let Some(cell) = self.cells.get(&(tier, i, j)) else {
            return Ok(None);
        };
        if let Some(bytes) = &self.bytes {
            return decode_cell(bytes, cell).map(Some);
        }
        let io = |e: std::io::Error| err("pgout_io", format!("{}: {e}", self.path.display()));
        let mut f = std::fs::File::open(&self.path).map_err(io)?;
        f.seek(std::io::SeekFrom::Start(cell.offset)).map_err(io)?;
        let len = usize::try_from(cell.len).map_err(|_| err("pgout_corrupt", "length"))?;
        let mut section = vec![0u8; len];
        f.read_exact(&mut section).map_err(io)?;
        decode_section_bytes(&section, cell).map(Some)
    }
}

/// Decodes a bake (every cell, merged) and checks it against the header's output hash.
///
/// # Errors
///
/// [`PgoutError`] for a wrong magic, an unknown version, a truncated or corrupt file, content
/// whose hash differs from the header (`pgout_corrupt`), or cells whose point sets disagree on an
/// attribute's type (`attr_type_conflict`).
pub fn decode(bytes: &[u8]) -> Result<(PgoutHeader, RunOutput), PgoutError> {
    if file_version(bytes) == Some(1) {
        return decode_v1(bytes);
    }
    let (header, cells) = read_table(bytes)?;
    // One cell holding the whole output: an untiled bake (a tiled bake of one cell names its tables
    // per cell, so its hash differs from the header's).
    let output = if let [only] = cells.as_slice()
        && only.output_hash == header.output_hash
    {
        decode_cell(bytes, only)?
    } else {
        let decoded: Vec<(crate::tiles::TileKey, RunOutput)> = cells
            .iter()
            .map(|c| decode_cell(bytes, c).map(|o| (cell_key(c), o)))
            .collect::<Result<_, _>>()?;
        merge_decoded(decoded.iter().map(|(k, o)| (k, o)))?
    };
    if output.hash() != header.output_hash {
        return Err(err(
            "pgout_corrupt",
            "content does not match the header's output hash",
        ));
    }
    Ok((header, output))
}

/// Merges decoded cells the way the bake was written (see [`crate::tiles::merge_outputs`]).
///
/// # Errors
///
/// `attr_type_conflict` when two cells give the same point-set name an attribute of a different
/// type (deep-zai-procgen-runtime finding 9: the second cell's points used to be dropped
/// silently).
fn merge_decoded<'a>(
    cells: impl IntoIterator<Item = (&'a crate::tiles::TileKey, &'a RunOutput)>,
) -> Result<RunOutput, PgoutError> {
    crate::tiles::merge_outputs(cells).map_err(|e| match e {
        RunError::Node { message, .. } => err("attr_type_conflict", message),
        // A merge never cancels; the mapping keeps the helper total.
        RunError::Cancelled => err("pgout_corrupt", "cancelled during merge"),
    })
}

/// The version field of a file with the `RPGO` magic.
fn file_version(bytes: &[u8]) -> Option<u32> {
    let (magic, rest) = bytes.split_first_chunk::<4>()?;
    let version = rest.first_chunk::<4>()?;
    (*magic == PGOUT_MAGIC).then_some(u32::from_le_bytes(*version))
}

/// Reads a version 1 file: one untiled cell, table entry `i, j, offset, len`.
fn decode_v1(bytes: &[u8]) -> Result<(PgoutHeader, RunOutput), PgoutError> {
    let mut r = R { b: bytes, i: 8 };
    let header = PgoutHeader {
        graph_hash: r.arr()?,
        param_hash: r.arr()?,
        seed: r.u32()?,
        input_hash: r.arr()?,
        output_hash: r.arr()?,
        quality: r.arr::<4>()?[0],
    };
    let cells = r.u32()?;
    if cells != 1 {
        return Err(err(
            "pgout_corrupt",
            format!("{cells} cells; version 1 bakes have one"),
        ));
    }
    let (_i, _j) = (r.i32()?, r.i32()?);
    let (offset, len) = (r.u64()?, r.u64()?);
    if offset != r.i as u64 || offset.checked_add(len) != Some(bytes.len() as u64) {
        return Err(err("pgout_corrupt", "cell table does not match the file"));
    }
    let output = decode_section(&mut r, 1)?;
    if r.i != bytes.len() {
        return Err(err("pgout_corrupt", "trailing bytes"));
    }
    if output.hash() != header.output_hash {
        return Err(err(
            "pgout_corrupt",
            "content does not match the header's output hash",
        ));
    }
    Ok((header, output))
}

/// Rewrites an older file as the current version: version 1 as one cell `(0, 0)` tier 0 with its
/// output hash, versions 2 and 3 with their cells' sections re-encoded (same header, same hashes;
/// no set has a layer, which hashes as before). A current
/// file comes back unchanged.
///
/// A version 2 bake of a graph with generated meshes or terrain edits cannot be kept: those were not
/// written, so the file fails its hash check (`pgout_corrupt`) and the generator needs a re-bake.
///
/// # Errors
///
/// The [`decode`] errors of the input.
pub fn migrate(bytes: &[u8]) -> Result<Vec<u8>, PgoutError> {
    match file_version(bytes) {
        Some(1) => {
            let (header, output) = decode_v1(bytes)?;
            Ok(encode(&header, &output))
        }
        Some(2 | 3) => {
            decode(bytes)?;
            let (header, cells) = read_table(bytes)?;
            let sections: Vec<OwnedSection> = cells
                .iter()
                .map(|c| decode_cell(bytes, c).map(|o| (c.tier, c.i, c.j, c.output_hash, encode_section(&o))))
                .collect::<Result<_, _>>()?;
            let refs: Vec<Section<'_>> = sections
                .iter()
                .map(|s| (s.0, s.1, s.2, s.3, s.4.as_slice()))
                .collect();
            Ok(assemble(&header, &refs))
        }
        _ => {
            decode(bytes)?;
            Ok(bytes.to_vec())
        }
    }
}

fn cell_key(c: &PgoutCell) -> crate::tiles::TileKey {
    crate::tiles::TileKey {
        tier: c.tier,
        cell: crate::tiles::CellCoord::new(i64::from(c.i), i64::from(c.j)),
    }
}

/// Re-bakes one cell (spec 66 § 2): the other cells' sections are copied byte for byte, the
/// table and the header's output hash are rewritten. A cell not in the table is added. The result
/// is the current version, so a section of an older file is re-encoded instead of copied.
///
/// # Errors
///
/// The [`read_table`] / [`decode_cell`] errors of the existing file, or `attr_type_conflict` when
/// a kept cell and the re-baked one disagree on a point attribute's type.
pub fn replace_cell(
    bytes: &[u8],
    tier: u8,
    i: i32,
    j: i32,
    output: &RunOutput,
) -> Result<Vec<u8>, PgoutError> {
    let (mut header, cells) = read_table(bytes)?;
    let new_section = encode_section(output);
    let new_hash = output.hash();
    let mut sections: Vec<Section<'_>> = Vec::with_capacity(cells.len() + 1);
    let mut merged: Vec<(crate::tiles::TileKey, RunOutput)> = Vec::with_capacity(cells.len() + 1);
    let mut placed = false;
    let decoded: Vec<Option<RunOutput>> = cells
        .iter()
        .map(|c| {
            ((c.tier, c.i, c.j) != (tier, i, j))
                .then(|| decode_cell(bytes, c))
                .transpose()
        })
        .collect::<Result<_, _>>()?;
    let upgraded: Vec<Option<Vec<u8>>> = cells
        .iter()
        .zip(&decoded)
        .map(|(c, d)| {
            d.as_ref()
                .filter(|_| c.version != PGOUT_VERSION)
                .map(encode_section)
        })
        .collect();
    for (k, c) in cells.iter().enumerate() {
        let (s, e) = (c.offset as usize, (c.offset + c.len) as usize);
        match &decoded[k] {
            None => {
                sections.push((tier, i, j, new_hash, &new_section));
                merged.push((cell_key(c), output.clone()));
                placed = true;
            }
            Some(d) => {
                let section = upgraded[k].as_deref().unwrap_or(&bytes[s..e]);
                sections.push((c.tier, c.i, c.j, c.output_hash, section));
                merged.push((cell_key(c), d.clone()));
            }
        }
    }
    if !placed {
        sections.push((tier, i, j, new_hash, &new_section));
        merged.push((
            crate::tiles::TileKey {
                tier,
                cell: crate::tiles::CellCoord::new(i64::from(i), i64::from(j)),
            },
            output.clone(),
        ));
        sections.sort_by_key(|s| (s.0, s.1, s.2));
        merged.sort_by_key(|m| m.0);
    }
    header.output_hash = merge_decoded(merged.iter().map(|(k, o)| (k, o)))?.hash();
    Ok(assemble(&header, &sections))
}

/// Writes bytes atomically (temporary file + rename). Needs the `pipeline` feature.
///
/// # Errors
///
/// `pgout_io` on a write failure, `procgen_editor_only` without the `pipeline` feature.
pub fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> Result<(), PgoutError> {
    #[cfg(feature = "pipeline")]
    {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| err("pgout_io", e.to_string()))?;
        }
        let tmp = path.with_extension(format!("pgout.tmp{}", std::process::id()));
        let written = std::fs::File::create(&tmp).and_then(|mut f| {
            std::io::Write::write_all(&mut f, bytes)?;
            f.sync_all()
        });
        written.map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            err("pgout_io", e.to_string())
        })?;
        std::fs::rename(&tmp, path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            err("pgout_io", e.to_string())
        })
    }
    #[cfg(not(feature = "pipeline"))]
    {
        let _ = (path, bytes);
        Err(err(
            "procgen_editor_only",
            "writing bakes needs the `pipeline` feature (editor, CLI)",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::Point;

    fn sample() -> RunOutput {
        let mut out = RunOutput::default();
        out.instance_sets.push(InstanceSet {
            mesh: "m.glb".into(),
            material: Some("mat".into()),
            cast_shadows: true,
            collision: Collision::Simple,
            draw_distance_m: 300.0,
            fade_m: 20.0,
            cull_below_px: 2.0,
            instances: vec![Instance {
                translation: [1.0, 2.0, 3.0],
                rotation: [0.0, 0.0, 0.0, 1.0],
                scale: [1.0; 3],
                color: [1.0; 4],
                custom: [0.5, 0.0, 0.0, 0.0],
            }],
            layer: None,
        });
        out.prefabs.push(PrefabSpawn {
            prefab: "p".into(),
            name: "n_0".into(),
            translation: [0.0; 3],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
            tags: vec!["t".into()],
            overrides: Vec::new(),
        });
        out.tables.insert(
            "t".into(),
            Table::one_row([("k".to_string(), crate::data::AttrValue::Int(4))]),
        );
        let mut p = PointSet::new();
        p.push(Point::default());
        p.attrs
            .insert("w".into(), AttrColumn::Name(vec!["x".into()].into()));
        out.points.insert("pts".into(), p);
        out
    }

    #[test]
    fn round_trips_and_rejects_edits_versions_and_truncation() {
        let out = sample();
        let header = PgoutHeader {
            graph_hash: [1; 32],
            param_hash: [2; 32],
            seed: 9,
            input_hash: [3; 32],
            output_hash: out.hash(),
            quality: 5,
        };
        let bytes = encode(&header, &out);
        let (h, o) = decode(&bytes).expect("decode");
        assert_eq!(h, header);
        assert_eq!(o, out);
        let mut edited = bytes.clone();
        let at = edited.len() - 30;
        edited[at] ^= 0x40;
        assert!(decode(&edited).is_err(), "a hand edit is caught");
        let mut future = bytes.clone();
        future[4] = 99;
        assert_eq!(
            decode(&future).map(|_| ()).expect_err("version").code,
            "pgout_version"
        );
        assert_eq!(
            decode(&bytes[..bytes.len() - 1])
                .map(|_| ())
                .expect_err("short")
                .code,
            "pgout_corrupt"
        );
        assert_eq!(
            decode(b"NOPE").map(|_| ()).expect_err("magic").code,
            "pgout_magic"
        );
    }

    /// Audit wave 6 (stream M2): a table that lists one `(tier, i, j)` twice is refused by the
    /// table reader, so streaming and a full decode cannot disagree about that cell.
    #[test]
    fn a_cell_listed_twice_is_refused() {
        let (a, mut b) = (sample(), sample());
        b.prefabs.clear();
        let header = PgoutHeader {
            graph_hash: [1; 32],
            param_hash: [2; 32],
            seed: 9,
            input_hash: [3; 32],
            output_hash: a.hash(),
            quality: 5,
        };
        let two = encode_cells(&header, &[(0, 0, 0, &a), (0, 1, 0, &b)]);
        assert!(read_table(&two).is_ok(), "distinct cells read");
        let twice = encode_cells(&header, &[(0, 0, 0, &a), (0, 0, 0, &b)]);
        let error = read_table(&twice).map(|_| ()).expect_err("a repeated cell");
        assert_eq!(error.code, "pgout_corrupt");
        assert!(decode(&twice).is_err());
    }

    /// A version 1 file (v0.2.6 layout: one cell, no tier, no cell hash) still decodes, and
    /// `migrate` rewrites it as the current version with the same header and content.
    #[test]
    fn version_1_files_decode_and_migrate() {
        let out = sample();
        let header = PgoutHeader {
            graph_hash: [4; 32],
            param_hash: [5; 32],
            seed: 3,
            input_hash: [6; 32],
            output_hash: out.hash(),
            quality: 2,
        };
        let mut section = encode_section(&out);
        // Version 1 sections end after the masks: drop the v3 tail's three zero counts and the v4
        // set-layer count.
        section.truncate(section.len() - 16);
        let mut w = W(Vec::new());
        w.0.extend_from_slice(&PGOUT_MAGIC);
        w.u32(1);
        w.0.extend_from_slice(&header.graph_hash);
        w.0.extend_from_slice(&header.param_hash);
        w.u32(header.seed);
        w.0.extend_from_slice(&header.input_hash);
        w.0.extend_from_slice(&header.output_hash);
        w.0.extend_from_slice(&[header.quality, 0, 0, 0]);
        w.u32(1);
        let table_end = w.0.len() + 4 + 4 + 8 + 8;
        w.0.extend_from_slice(&0i32.to_le_bytes());
        w.0.extend_from_slice(&0i32.to_le_bytes());
        w.u64(table_end as u64);
        w.u64(section.len() as u64);
        w.0.extend_from_slice(&section);
        let v1 = w.0;
        let (h, o) = decode(&v1).expect("v1 decodes");
        assert_eq!((h, &o), (header, &out));
        assert_eq!(
            read_table(&v1).map(|_| ()).expect_err("v1 table").code,
            "pgout_version"
        );
        let migrated = migrate(&v1).expect("migrate");
        assert_eq!(migrated, encode(&header, &out), "same bytes as a fresh bake");
        assert_eq!(migrate(&migrated).expect("current"), migrated);
        let mut edited = v1.clone();
        let at = edited.len() - 30;
        edited[at] ^= 0x40;
        assert!(migrate(&edited).is_err(), "a hand-edited v1 file is refused");
    }

    /// Re-baking a cell of a version 2 file writes a version 3 header, so the other cells' sections
    /// cannot be copied as they are: read back, they would run out at the v3 tail.
    #[test]
    fn replacing_a_cell_of_a_version_2_file_upgrades_the_others() {
        let out = sample();
        let header = PgoutHeader {
            graph_hash: [7; 32],
            param_hash: [8; 32],
            seed: 5,
            input_hash: [9; 32],
            output_hash: [0; 32],
            quality: 1,
        };
        let mut section = encode_section(&out);
        // A version 2 section: no v3 tail (three zero counts), no v4 set-layer count.
        section.truncate(section.len() - 16);
        let start = (HEADER_LEN + 4 + 2 * CELL_ENTRY) as u64;
        let mut w = W(Vec::new());
        write_header(&mut w, &header);
        w.0[4..8].copy_from_slice(&2u32.to_le_bytes());
        w.u32(2);
        for i in 0..2i32 {
            w.0.extend_from_slice(&i.to_le_bytes());
            w.0.extend_from_slice(&0i32.to_le_bytes());
            w.0.extend_from_slice(&[0, 0, 0, 0]);
            w.u64(start + i as u64 * section.len() as u64);
            w.u64(section.len() as u64);
            w.0.extend_from_slice(&out.hash());
        }
        w.0.extend_from_slice(&section);
        w.0.extend_from_slice(&section);
        let rebaked = replace_cell(&w.0, 0, 0, 0, &out).expect("re-bake");
        let (_, cells) = read_table(&rebaked).expect("table");
        assert!(cells.iter().all(|c| c.version == PGOUT_VERSION));
        for c in &cells {
            assert_eq!(decode_cell(&rebaked, c).expect("cell"), out);
        }
        assert!(decode(&rebaked).is_ok());
    }

    /// A clear writes `original` back, so a flipped bit in it must fail the file's hash.
    #[test]
    fn the_output_hash_covers_terrain_originals() {
        let record = |original: f32| crate::exec::TerrainEditRecord {
            rect: [0, 0, 1, 1],
            samples: vec![1.0],
            original: vec![original],
        };
        let mut a = RunOutput::default();
        a.terrain_edits.push(record(2.0));
        let mut b = RunOutput::default();
        b.terrain_edits.push(record(3.0));
        assert_ne!(a.hash(), b.hash());
    }

    /// A mesh of a loaded bake reaches trimesh and tangent code, which take three indices a triangle.
    #[test]
    fn a_mesh_with_a_partial_triangle_is_refused() {
        let mut out = RunOutput::default();
        out.meshes.push(GeneratedMesh {
            material: "m".into(),
            positions: vec![[0.0; 3]; 3],
            normals: vec![[0.0, 1.0, 0.0]; 3],
            uvs: vec![[0.0; 2]; 3],
            colors: Vec::new(),
            indices: vec![0, 1],
            collision: false,
        });
        let header = PgoutHeader {
            graph_hash: [7; 32],
            param_hash: [8; 32],
            seed: 5,
            input_hash: [9; 32],
            output_hash: out.hash(),
            quality: 1,
        };
        let e = decode(&encode(&header, &out)).expect_err("refused");
        assert_eq!(e.code, "pgout_corrupt");
    }

    /// A version 2 file (v0.2.7: sections end after the masks) reads through the same table and
    /// `migrate` rewrites it with the v3 section tail.
    #[test]
    fn version_2_files_decode_and_migrate() {
        let out = sample();
        let header = PgoutHeader {
            graph_hash: [7; 32],
            param_hash: [8; 32],
            seed: 5,
            input_hash: [9; 32],
            output_hash: out.hash(),
            quality: 1,
        };
        let mut section = encode_section(&out);
        // The v3 tail of an output without meshes, edits or decals (three zero counts) and the v4
        // set-layer count.
        section.truncate(section.len() - 16);
        let table_end = (HEADER_LEN + 4 + CELL_ENTRY) as u64;
        let mut w = W(Vec::new());
        write_header(&mut w, &header);
        w.0[4..8].copy_from_slice(&2u32.to_le_bytes());
        w.u32(1);
        w.0.extend_from_slice(&0i32.to_le_bytes());
        w.0.extend_from_slice(&0i32.to_le_bytes());
        w.0.extend_from_slice(&[0, 0, 0, 0]);
        w.u64(table_end);
        w.u64(section.len() as u64);
        w.0.extend_from_slice(&out.hash());
        w.0.extend_from_slice(&section);
        let v2 = w.0;
        let (h, o) = decode(&v2).expect("v2 decodes");
        assert_eq!((h, &o), (header, &out));
        let migrated = migrate(&v2).expect("migrate");
        assert_eq!(migrated, encode(&header, &out), "same bytes as a fresh bake");
    }

    /// A version 3 file (v0.2.8 to v0.5: no set layers) decodes with every set unlayered, and
    /// `migrate` rewrites it as version 4 with the same header and hashes (ADR-7214).
    #[test]
    fn version_3_files_decode_and_migrate() {
        let out = sample();
        let header = PgoutHeader {
            graph_hash: [7; 32],
            param_hash: [8; 32],
            seed: 5,
            input_hash: [9; 32],
            output_hash: out.hash(),
            quality: 1,
        };
        let mut section = encode_section(&out);
        // A version 3 section ends at the road decals: drop the v4 set-layer count.
        section.truncate(section.len() - 4);
        let table_end = (HEADER_LEN + 4 + CELL_ENTRY) as u64;
        let mut w = W(Vec::new());
        write_header(&mut w, &header);
        w.0[4..8].copy_from_slice(&3u32.to_le_bytes());
        w.u32(1);
        w.0.extend_from_slice(&0i32.to_le_bytes());
        w.0.extend_from_slice(&0i32.to_le_bytes());
        w.0.extend_from_slice(&[0, 0, 0, 0]);
        w.u64(table_end);
        w.u64(section.len() as u64);
        w.0.extend_from_slice(&out.hash());
        w.0.extend_from_slice(&section);
        let v3 = w.0;
        assert_eq!(file_version(&v3), Some(3));
        let (h, o) = decode(&v3).expect("v3 decodes");
        assert_eq!((h, &o), (header, &out));
        assert!(o.instance_sets.iter().all(|s| s.layer.is_none()));
        let migrated = migrate(&v3).expect("migrate");
        assert_eq!(file_version(&migrated), Some(PGOUT_VERSION));
        assert_eq!(migrated, encode(&header, &out), "same bytes as a fresh bake");
    }

    /// A set's data layer (`target_layer`) round trips, changes the output hash only when set, and
    /// a layer row naming a set the section does not have is refused (ADR-7214).
    #[test]
    fn set_layers_round_trip_and_hash_only_when_set() {
        let plain = sample();
        let mut layered = plain.clone();
        layered.instance_sets[0].layer = Some("quest_a".into());
        assert_ne!(plain.hash(), layered.hash());
        let header = PgoutHeader {
            graph_hash: [1; 32],
            param_hash: [2; 32],
            seed: 3,
            input_hash: [4; 32],
            output_hash: layered.hash(),
            quality: 0,
        };
        let bytes = encode(&header, &layered);
        let (_, back) = decode(&bytes).expect("decodes");
        assert_eq!(back, layered);
        assert_eq!(back.instance_sets[0].layer.as_deref(), Some("quest_a"));
        // A row pointing past the sets: the last 4 bytes before the name are the set index.
        let mut section = encode_section(&layered);
        let name_len = 4 + "quest_a".len();
        let at = section.len() - name_len - 4;
        section[at..at + 4].copy_from_slice(&99u32.to_le_bytes());
        let mut r = R { b: &section, i: 0 };
        let e = decode_section(&mut r, PGOUT_VERSION).expect_err("refused");
        assert_eq!(e.code, "pgout_corrupt");
    }

    /// Generated meshes, terrain edits and road decals round trip, and a ragged mesh is refused.
    #[test]
    fn meshes_edits_and_decals_round_trip() {
        let mut out = sample();
        let mesh = GeneratedMesh {
            material: "m/road.material.ron".into(),
            positions: vec![[0.0; 3], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]],
            normals: vec![[0.0, 1.0, 0.0]; 3],
            uvs: vec![[0.0, 0.25]; 3],
            indices: vec![0, 1, 2],
            colors: vec![[1.0, 0.5, 0.25, 1.0]; 3],
            collision: true,
        };
        out.meshes.push(mesh.clone());
        out.terrain_edits.push(TerrainEditRecord {
            rect: [1, 2, 3, 3],
            samples: vec![1.5, 2.5],
            original: vec![1.0, 2.0],
        });
        out.road_decals
            .push(RoadDecal::new([0.0, 1.0], [2.0, 3.0], 3.5, [0.1, 0.1, 0.1]));
        let header = PgoutHeader {
            graph_hash: [1; 32],
            param_hash: [2; 32],
            seed: 9,
            input_hash: [3; 32],
            output_hash: out.hash(),
            quality: 5,
        };
        let bytes = encode(&header, &out);
        let (_, o) = decode(&bytes).expect("decode");
        assert_eq!(o, out);

        let mut bad = out.clone();
        bad.meshes[0].indices = vec![0, 1, 9];
        let header = PgoutHeader {
            output_hash: bad.hash(),
            ..header
        };
        assert_eq!(
            decode(&encode(&header, &bad))
                .map(|_| ())
                .expect_err("ragged")
                .code,
            "pgout_corrupt"
        );
        assert_ne!(out.hash(), sample().hash(), "meshes, edits and decals are hashed");
    }

    /// A streamed file reads one section from disk and answers the same cell as the whole file.
    #[cfg(feature = "pipeline")]
    #[test]
    fn a_streamed_file_reads_single_cells() {
        let a = sample();
        let mut b = RunOutput::default();
        b.terrain_edits.push(TerrainEditRecord {
            rect: [0, 0, 1, 1],
            samples: vec![3.0],
            original: vec![2.0],
        });
        let key = |i| crate::tiles::TileKey {
            tier: 1,
            cell: crate::tiles::CellCoord::new(i, 0),
        };
        let merged = crate::tiles::merge_outputs([(&key(0), &a), (&key(1), &b)])
            .expect("the sample cells agree on attribute types");
        let header = PgoutHeader {
            graph_hash: [1; 32],
            param_hash: [2; 32],
            seed: 9,
            input_hash: [3; 32],
            output_hash: merged.hash(),
            quality: 5,
        };
        let bytes = encode_cells(&header, &[(1, 0, 0, &a), (1, 1, 0, &b)]);
        let dir = regia_test_utils::unique_temp_dir("pgout-stream");
        let path = dir.join("g.pgout");
        write_atomic(&path, &bytes).expect("write");
        let f = PgoutFile::open(&path).expect("open");
        assert_eq!(f.header, header);
        assert_eq!(f.cells.len(), 2);
        assert_eq!(f.read_cell(1, 1, 0).expect("read"), Some(b));
        assert_eq!(f.read_cell(1, 0, 0).expect("read"), Some(a));
        assert_eq!(f.read_cell(0, 0, 0).expect("read"), None);
        std::fs::remove_file(&path).expect("rm");
        assert_eq!(f.read_cell(1, 0, 0).expect_err("gone").code, "pgout_io");
    }

    /// A column-less table cannot claim more rows than any run emits (a crafted file with a
    /// recomputed hash used to decode `rows: u64::MAX`, and the first reader of its `index` column
    /// aborted on allocation).
    #[test]
    fn column_less_tables_are_bounded() {
        let encode_rows = |rows: usize| {
            let mut out = RunOutput::default();
            out.tables.insert(
                "t".into(),
                Table {
                    rows,
                    columns: Vec::new(),
                },
            );
            let header = PgoutHeader {
                graph_hash: [0; 32],
                param_hash: [0; 32],
                seed: 1,
                input_hash: [0; 32],
                output_hash: out.hash(),
                quality: 0,
            };
            (encode(&header, &out), out)
        };
        let (bytes, out) = encode_rows(5);
        assert_eq!(decode(&bytes).expect("small").1, out);
        let too_many = crate::exec::RunLimits::DESKTOP.total_elements + 1;
        let (bytes, _) = encode_rows(too_many);
        assert_eq!(
            decode(&bytes).map(|_| ()).expect_err("huge").code,
            "pgout_corrupt"
        );
    }

    fn header_for(out: &RunOutput) -> PgoutHeader {
        PgoutHeader {
            graph_hash: [1; 32],
            param_hash: [2; 32],
            seed: 9,
            input_hash: [3; 32],
            output_hash: out.hash(),
            quality: 5,
        }
    }

    /// [`sample`] plus a second prefab with three overrides (a float, a vector and a name).
    fn with_overrides() -> RunOutput {
        use crate::data::AttrValue;
        use crate::exec::PrefabFieldOverride;
        let mut out = sample();
        out.prefabs.push(PrefabSpawn {
            prefab: "q".into(),
            name: "n_1".into(),
            translation: [4.0, 0.0, 2.0],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
            tags: Vec::new(),
            overrides: vec![
                PrefabFieldOverride {
                    component: "Glow".into(),
                    field: "intensity".into(),
                    value: AttrValue::Float(2.5),
                },
                PrefabFieldOverride {
                    component: "my_game::Tint".into(),
                    field: "color".into(),
                    value: AttrValue::Vec3([0.1, 0.2, 0.3]),
                },
                PrefabFieldOverride {
                    component: "Label".into(),
                    field: "text".into(),
                    value: AttrValue::Name("north".into()),
                },
            ],
        });
        out
    }

    /// `out` with every prefab's overrides dropped.
    fn without_overrides(out: &RunOutput) -> RunOutput {
        let mut plain = out.clone();
        plain.prefabs.iter_mut().for_each(|p| p.overrides.clear());
        plain
    }

    /// ADR-7750 (the ADR-7724 (h) fold): `spawn.prefab` overrides round trip in the optional
    /// trailing section of a version 4 section, and an output without overrides writes exactly
    /// the early version 4 section (the override section is appended, never interleaved).
    #[test]
    fn prefab_overrides_round_trip_in_the_optional_v4_section() {
        let out = with_overrides();
        let header = header_for(&out);
        let bytes = encode(&header, &out);
        assert_eq!(decode(&bytes).expect("decode"), (header, out.clone()));
        assert_eq!(
            migrate(&bytes).expect("migrate"),
            bytes,
            "a current file is unchanged"
        );
        let full = encode_section(&out);
        let early = encode_section(&without_overrides(&out));
        assert!(
            full.len() > early.len() && full.starts_with(&early),
            "the override section follows the early v4 sections"
        );
        let plain = without_overrides(&out);
        assert_ne!(plain.hash(), out.hash(), "the output hash covers the overrides");
        let plain_bytes = encode(&header_for(&plain), &plain);
        assert_eq!(decode(&plain_bytes).expect("plain").1, plain);
    }

    /// A tiled bake keeps each cell's overrides; the merged decode and a streamed read agree.
    #[test]
    fn tiled_bakes_keep_overrides_per_cell() {
        let a = with_overrides();
        let b = sample();
        let key = |i| crate::tiles::TileKey {
            tier: 0,
            cell: crate::tiles::CellCoord::new(i, 0),
        };
        let merged = crate::tiles::merge_outputs([(&key(0), &a), (&key(1), &b)]).expect("merge");
        let header = header_for(&merged);
        let bytes = encode_cells(&header, &[(0, 0, 0, &a), (0, 1, 0, &b)]);
        assert_eq!(decode(&bytes).expect("decode").1, merged);
        let f = PgoutFile::from_bytes(std::path::Path::new("g.pgout"), bytes.into()).expect("table");
        assert_eq!(f.read_cell(0, 0, 0).expect("read"), Some(a));
        assert_eq!(f.read_cell(0, 1, 0).expect("read"), Some(b));
    }

    /// The override section's own checks: one encoding only (no empty section, no prefab without
    /// overrides, prefab indices strictly increasing and in range, one value per override), and a
    /// section cut anywhere inside it is refused or reads as the early layout, whose hash then
    /// differs.
    #[test]
    fn malformed_override_sections_are_refused() {
        let out = with_overrides();
        let early = encode_section(&without_overrides(&out));
        let full = encode_section(&out);
        let decode_tail = |tail: &[u8]| {
            let mut s = early.clone();
            s.extend_from_slice(tail);
            let mut r = R { b: &s, i: 0 };
            decode_section(&mut r, PGOUT_VERSION).map(|_| ())
        };
        let entry = |w: &mut W, prefab: u32, values: &[AttrColumn]| {
            w.u32(prefab);
            w.u32(values.len() as u32);
            for v in values {
                w.str("Glow");
                w.str("intensity");
                w.column(v);
            }
        };
        let one = AttrColumn::Float(vec![1.0].into());
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("an empty section", {
                let mut w = W(Vec::new());
                w.u32(0);
                w.0
            }),
            ("a prefab out of range", {
                let mut w = W(Vec::new());
                w.u32(1);
                entry(&mut w, 2, std::slice::from_ref(&one));
                w.0
            }),
            ("indices not increasing", {
                let mut w = W(Vec::new());
                w.u32(2);
                entry(&mut w, 1, std::slice::from_ref(&one));
                entry(&mut w, 0, std::slice::from_ref(&one));
                w.0
            }),
            ("a prefab listed twice", {
                let mut w = W(Vec::new());
                w.u32(2);
                entry(&mut w, 0, std::slice::from_ref(&one));
                entry(&mut w, 0, std::slice::from_ref(&one));
                w.0
            }),
            ("a prefab with no override", {
                let mut w = W(Vec::new());
                w.u32(1);
                entry(&mut w, 0, &[]);
                w.0
            }),
            ("two values for one override", {
                let mut w = W(Vec::new());
                w.u32(1);
                entry(&mut w, 0, &[AttrColumn::Float(vec![1.0, 2.0].into())]);
                w.0
            }),
        ];
        for (what, tail) in &cases {
            let e = decode_tail(tail).expect_err(what);
            assert_eq!(e.code, "pgout_corrupt", "{what}: {e}");
        }
        let mut good = W(Vec::new());
        good.u32(1);
        entry(&mut good, 0, std::slice::from_ref(&one));
        decode_tail(&good.0).expect("a well-formed section");
        for n in early.len()..full.len() {
            let mut r = R { b: &full[..n], i: 0 };
            match decode_section(&mut r, PGOUT_VERSION) {
                Ok(o) => {
                    assert_eq!(n, early.len(), "only the early layout decodes");
                    assert_ne!(o.hash(), out.hash(), "and fails its hash check");
                }
                Err(e) => assert!(
                    ["pgout_truncated", "pgout_corrupt"].contains(&e.code),
                    "cut at {n}: {e}"
                ),
            }
        }
    }

    /// Migrating a version 3 file writes no override section (none of its prefabs has one): the
    /// result is the early version 4 layout.
    #[test]
    fn migrating_a_version_3_file_writes_no_override_section() {
        let out = sample();
        let header = header_for(&out);
        let v4 = encode(&header, &out);
        // The early v4 section minus its empty set-layer count, with the version word set to 3.
        let mut v3 = v4[..v4.len() - 4].to_vec();
        v3[4..8].copy_from_slice(&3u32.to_le_bytes());
        let table_len = HEADER_LEN + 4 + CELL_ENTRY;
        let len = (v3.len() - table_len) as u64;
        v3[HEADER_LEN + 4 + 20..HEADER_LEN + 4 + 28].copy_from_slice(&len.to_le_bytes());
        assert_eq!(decode(&v3).expect("v3").1, out);
        assert_eq!(migrate(&v3).expect("migrate"), v4);
    }
}
