// SPDX-License-Identifier: MPL-2.0
#import regia::utils

@group(0) @binding(0) var hdr_tex: texture_2d<f32>;
@group(0) @binding(1) var<storage, read_write> bins: array<atomic<u32>>;

// Per-workgroup bins. The frame's luminance falls into a handful of the 64 bins, so one
// global atomic per pixel serialised on those few addresses. Each workgroup counts in
// shared memory and adds its non-zero bins once; integer sums, so the histogram is
// bit-identical. 64 scalar atomics, not an array: naga gives a workgroup array an
// ArrayStride and SPIR-V 1.6 rejects that (ADR-026, VUID-StandaloneSpirv-None-10684).
#ifdef HIST_WORKGROUP_ARRAY
// Metal (`HIST_WORKGROUP_ARRAY`): the 64 scalar variables below become 64 threadgroup arguments of
// the entry point and Metal allows 31, so the bins are one array there. The ArrayStride problem
// above is a SPIR-V one; Metal never sees SPIR-V.
var<workgroup> wbins: array<atomic<u32>, 64>;

fn local_clear(bin: u32) {
    atomicStore(&wbins[bin], 0u);
}

fn local_add(bin: u32, w: u32) {
    atomicAdd(&wbins[bin], w);
}

fn local_load(bin: u32) -> u32 {
    return atomicLoad(&wbins[bin]);
}
#else
var<workgroup> wb0: atomic<u32>;
var<workgroup> wb1: atomic<u32>;
var<workgroup> wb2: atomic<u32>;
var<workgroup> wb3: atomic<u32>;
var<workgroup> wb4: atomic<u32>;
var<workgroup> wb5: atomic<u32>;
var<workgroup> wb6: atomic<u32>;
var<workgroup> wb7: atomic<u32>;
var<workgroup> wb8: atomic<u32>;
var<workgroup> wb9: atomic<u32>;
var<workgroup> wb10: atomic<u32>;
var<workgroup> wb11: atomic<u32>;
var<workgroup> wb12: atomic<u32>;
var<workgroup> wb13: atomic<u32>;
var<workgroup> wb14: atomic<u32>;
var<workgroup> wb15: atomic<u32>;
var<workgroup> wb16: atomic<u32>;
var<workgroup> wb17: atomic<u32>;
var<workgroup> wb18: atomic<u32>;
var<workgroup> wb19: atomic<u32>;
var<workgroup> wb20: atomic<u32>;
var<workgroup> wb21: atomic<u32>;
var<workgroup> wb22: atomic<u32>;
var<workgroup> wb23: atomic<u32>;
var<workgroup> wb24: atomic<u32>;
var<workgroup> wb25: atomic<u32>;
var<workgroup> wb26: atomic<u32>;
var<workgroup> wb27: atomic<u32>;
var<workgroup> wb28: atomic<u32>;
var<workgroup> wb29: atomic<u32>;
var<workgroup> wb30: atomic<u32>;
var<workgroup> wb31: atomic<u32>;
var<workgroup> wb32: atomic<u32>;
var<workgroup> wb33: atomic<u32>;
var<workgroup> wb34: atomic<u32>;
var<workgroup> wb35: atomic<u32>;
var<workgroup> wb36: atomic<u32>;
var<workgroup> wb37: atomic<u32>;
var<workgroup> wb38: atomic<u32>;
var<workgroup> wb39: atomic<u32>;
var<workgroup> wb40: atomic<u32>;
var<workgroup> wb41: atomic<u32>;
var<workgroup> wb42: atomic<u32>;
var<workgroup> wb43: atomic<u32>;
var<workgroup> wb44: atomic<u32>;
var<workgroup> wb45: atomic<u32>;
var<workgroup> wb46: atomic<u32>;
var<workgroup> wb47: atomic<u32>;
var<workgroup> wb48: atomic<u32>;
var<workgroup> wb49: atomic<u32>;
var<workgroup> wb50: atomic<u32>;
var<workgroup> wb51: atomic<u32>;
var<workgroup> wb52: atomic<u32>;
var<workgroup> wb53: atomic<u32>;
var<workgroup> wb54: atomic<u32>;
var<workgroup> wb55: atomic<u32>;
var<workgroup> wb56: atomic<u32>;
var<workgroup> wb57: atomic<u32>;
var<workgroup> wb58: atomic<u32>;
var<workgroup> wb59: atomic<u32>;
var<workgroup> wb60: atomic<u32>;
var<workgroup> wb61: atomic<u32>;
var<workgroup> wb62: atomic<u32>;
var<workgroup> wb63: atomic<u32>;

fn local_clear(bin: u32) {
    switch bin {
        case 0u: { atomicStore(&wb0, 0u); }
        case 1u: { atomicStore(&wb1, 0u); }
        case 2u: { atomicStore(&wb2, 0u); }
        case 3u: { atomicStore(&wb3, 0u); }
        case 4u: { atomicStore(&wb4, 0u); }
        case 5u: { atomicStore(&wb5, 0u); }
        case 6u: { atomicStore(&wb6, 0u); }
        case 7u: { atomicStore(&wb7, 0u); }
        case 8u: { atomicStore(&wb8, 0u); }
        case 9u: { atomicStore(&wb9, 0u); }
        case 10u: { atomicStore(&wb10, 0u); }
        case 11u: { atomicStore(&wb11, 0u); }
        case 12u: { atomicStore(&wb12, 0u); }
        case 13u: { atomicStore(&wb13, 0u); }
        case 14u: { atomicStore(&wb14, 0u); }
        case 15u: { atomicStore(&wb15, 0u); }
        case 16u: { atomicStore(&wb16, 0u); }
        case 17u: { atomicStore(&wb17, 0u); }
        case 18u: { atomicStore(&wb18, 0u); }
        case 19u: { atomicStore(&wb19, 0u); }
        case 20u: { atomicStore(&wb20, 0u); }
        case 21u: { atomicStore(&wb21, 0u); }
        case 22u: { atomicStore(&wb22, 0u); }
        case 23u: { atomicStore(&wb23, 0u); }
        case 24u: { atomicStore(&wb24, 0u); }
        case 25u: { atomicStore(&wb25, 0u); }
        case 26u: { atomicStore(&wb26, 0u); }
        case 27u: { atomicStore(&wb27, 0u); }
        case 28u: { atomicStore(&wb28, 0u); }
        case 29u: { atomicStore(&wb29, 0u); }
        case 30u: { atomicStore(&wb30, 0u); }
        case 31u: { atomicStore(&wb31, 0u); }
        case 32u: { atomicStore(&wb32, 0u); }
        case 33u: { atomicStore(&wb33, 0u); }
        case 34u: { atomicStore(&wb34, 0u); }
        case 35u: { atomicStore(&wb35, 0u); }
        case 36u: { atomicStore(&wb36, 0u); }
        case 37u: { atomicStore(&wb37, 0u); }
        case 38u: { atomicStore(&wb38, 0u); }
        case 39u: { atomicStore(&wb39, 0u); }
        case 40u: { atomicStore(&wb40, 0u); }
        case 41u: { atomicStore(&wb41, 0u); }
        case 42u: { atomicStore(&wb42, 0u); }
        case 43u: { atomicStore(&wb43, 0u); }
        case 44u: { atomicStore(&wb44, 0u); }
        case 45u: { atomicStore(&wb45, 0u); }
        case 46u: { atomicStore(&wb46, 0u); }
        case 47u: { atomicStore(&wb47, 0u); }
        case 48u: { atomicStore(&wb48, 0u); }
        case 49u: { atomicStore(&wb49, 0u); }
        case 50u: { atomicStore(&wb50, 0u); }
        case 51u: { atomicStore(&wb51, 0u); }
        case 52u: { atomicStore(&wb52, 0u); }
        case 53u: { atomicStore(&wb53, 0u); }
        case 54u: { atomicStore(&wb54, 0u); }
        case 55u: { atomicStore(&wb55, 0u); }
        case 56u: { atomicStore(&wb56, 0u); }
        case 57u: { atomicStore(&wb57, 0u); }
        case 58u: { atomicStore(&wb58, 0u); }
        case 59u: { atomicStore(&wb59, 0u); }
        case 60u: { atomicStore(&wb60, 0u); }
        case 61u: { atomicStore(&wb61, 0u); }
        case 62u: { atomicStore(&wb62, 0u); }
        default: { atomicStore(&wb63, 0u); }
    }
}

fn local_add(bin: u32, w: u32) {
    switch bin {
        case 0u: { atomicAdd(&wb0, w); }
        case 1u: { atomicAdd(&wb1, w); }
        case 2u: { atomicAdd(&wb2, w); }
        case 3u: { atomicAdd(&wb3, w); }
        case 4u: { atomicAdd(&wb4, w); }
        case 5u: { atomicAdd(&wb5, w); }
        case 6u: { atomicAdd(&wb6, w); }
        case 7u: { atomicAdd(&wb7, w); }
        case 8u: { atomicAdd(&wb8, w); }
        case 9u: { atomicAdd(&wb9, w); }
        case 10u: { atomicAdd(&wb10, w); }
        case 11u: { atomicAdd(&wb11, w); }
        case 12u: { atomicAdd(&wb12, w); }
        case 13u: { atomicAdd(&wb13, w); }
        case 14u: { atomicAdd(&wb14, w); }
        case 15u: { atomicAdd(&wb15, w); }
        case 16u: { atomicAdd(&wb16, w); }
        case 17u: { atomicAdd(&wb17, w); }
        case 18u: { atomicAdd(&wb18, w); }
        case 19u: { atomicAdd(&wb19, w); }
        case 20u: { atomicAdd(&wb20, w); }
        case 21u: { atomicAdd(&wb21, w); }
        case 22u: { atomicAdd(&wb22, w); }
        case 23u: { atomicAdd(&wb23, w); }
        case 24u: { atomicAdd(&wb24, w); }
        case 25u: { atomicAdd(&wb25, w); }
        case 26u: { atomicAdd(&wb26, w); }
        case 27u: { atomicAdd(&wb27, w); }
        case 28u: { atomicAdd(&wb28, w); }
        case 29u: { atomicAdd(&wb29, w); }
        case 30u: { atomicAdd(&wb30, w); }
        case 31u: { atomicAdd(&wb31, w); }
        case 32u: { atomicAdd(&wb32, w); }
        case 33u: { atomicAdd(&wb33, w); }
        case 34u: { atomicAdd(&wb34, w); }
        case 35u: { atomicAdd(&wb35, w); }
        case 36u: { atomicAdd(&wb36, w); }
        case 37u: { atomicAdd(&wb37, w); }
        case 38u: { atomicAdd(&wb38, w); }
        case 39u: { atomicAdd(&wb39, w); }
        case 40u: { atomicAdd(&wb40, w); }
        case 41u: { atomicAdd(&wb41, w); }
        case 42u: { atomicAdd(&wb42, w); }
        case 43u: { atomicAdd(&wb43, w); }
        case 44u: { atomicAdd(&wb44, w); }
        case 45u: { atomicAdd(&wb45, w); }
        case 46u: { atomicAdd(&wb46, w); }
        case 47u: { atomicAdd(&wb47, w); }
        case 48u: { atomicAdd(&wb48, w); }
        case 49u: { atomicAdd(&wb49, w); }
        case 50u: { atomicAdd(&wb50, w); }
        case 51u: { atomicAdd(&wb51, w); }
        case 52u: { atomicAdd(&wb52, w); }
        case 53u: { atomicAdd(&wb53, w); }
        case 54u: { atomicAdd(&wb54, w); }
        case 55u: { atomicAdd(&wb55, w); }
        case 56u: { atomicAdd(&wb56, w); }
        case 57u: { atomicAdd(&wb57, w); }
        case 58u: { atomicAdd(&wb58, w); }
        case 59u: { atomicAdd(&wb59, w); }
        case 60u: { atomicAdd(&wb60, w); }
        case 61u: { atomicAdd(&wb61, w); }
        case 62u: { atomicAdd(&wb62, w); }
        default: { atomicAdd(&wb63, w); }
    }
}

fn local_load(bin: u32) -> u32 {
    switch bin {
        case 0u: { return atomicLoad(&wb0); }
        case 1u: { return atomicLoad(&wb1); }
        case 2u: { return atomicLoad(&wb2); }
        case 3u: { return atomicLoad(&wb3); }
        case 4u: { return atomicLoad(&wb4); }
        case 5u: { return atomicLoad(&wb5); }
        case 6u: { return atomicLoad(&wb6); }
        case 7u: { return atomicLoad(&wb7); }
        case 8u: { return atomicLoad(&wb8); }
        case 9u: { return atomicLoad(&wb9); }
        case 10u: { return atomicLoad(&wb10); }
        case 11u: { return atomicLoad(&wb11); }
        case 12u: { return atomicLoad(&wb12); }
        case 13u: { return atomicLoad(&wb13); }
        case 14u: { return atomicLoad(&wb14); }
        case 15u: { return atomicLoad(&wb15); }
        case 16u: { return atomicLoad(&wb16); }
        case 17u: { return atomicLoad(&wb17); }
        case 18u: { return atomicLoad(&wb18); }
        case 19u: { return atomicLoad(&wb19); }
        case 20u: { return atomicLoad(&wb20); }
        case 21u: { return atomicLoad(&wb21); }
        case 22u: { return atomicLoad(&wb22); }
        case 23u: { return atomicLoad(&wb23); }
        case 24u: { return atomicLoad(&wb24); }
        case 25u: { return atomicLoad(&wb25); }
        case 26u: { return atomicLoad(&wb26); }
        case 27u: { return atomicLoad(&wb27); }
        case 28u: { return atomicLoad(&wb28); }
        case 29u: { return atomicLoad(&wb29); }
        case 30u: { return atomicLoad(&wb30); }
        case 31u: { return atomicLoad(&wb31); }
        case 32u: { return atomicLoad(&wb32); }
        case 33u: { return atomicLoad(&wb33); }
        case 34u: { return atomicLoad(&wb34); }
        case 35u: { return atomicLoad(&wb35); }
        case 36u: { return atomicLoad(&wb36); }
        case 37u: { return atomicLoad(&wb37); }
        case 38u: { return atomicLoad(&wb38); }
        case 39u: { return atomicLoad(&wb39); }
        case 40u: { return atomicLoad(&wb40); }
        case 41u: { return atomicLoad(&wb41); }
        case 42u: { return atomicLoad(&wb42); }
        case 43u: { return atomicLoad(&wb43); }
        case 44u: { return atomicLoad(&wb44); }
        case 45u: { return atomicLoad(&wb45); }
        case 46u: { return atomicLoad(&wb46); }
        case 47u: { return atomicLoad(&wb47); }
        case 48u: { return atomicLoad(&wb48); }
        case 49u: { return atomicLoad(&wb49); }
        case 50u: { return atomicLoad(&wb50); }
        case 51u: { return atomicLoad(&wb51); }
        case 52u: { return atomicLoad(&wb52); }
        case 53u: { return atomicLoad(&wb53); }
        case 54u: { return atomicLoad(&wb54); }
        case 55u: { return atomicLoad(&wb55); }
        case 56u: { return atomicLoad(&wb56); }
        case 57u: { return atomicLoad(&wb57); }
        case 58u: { return atomicLoad(&wb58); }
        case 59u: { return atomicLoad(&wb59); }
        case 60u: { return atomicLoad(&wb60); }
        case 61u: { return atomicLoad(&wb61); }
        case 62u: { return atomicLoad(&wb62); }
        default: { return atomicLoad(&wb63); }
    }
}
#endif

@compute @workgroup_size(16, 16)
fn cs_hist(
    @builtin(global_invocation_id) id: vec3<u32>,
    @builtin(local_invocation_index) li: u32,
) {
    if (li < 64u) {
        local_clear(li);
    }
    workgroupBarrier();
    let size = textureDimensions(hdr_tex);
    if (id.x < size.x && id.y < size.y) {
        let c = textureLoad(hdr_tex, vec2<i32>(id.xy), 0).rgb;
        // Slot 65 holds the HDR target's pre-exposure (f32 bits): meter radiance.
        let y = max(regia::utils::luminance(c) / bitcast<f32>(atomicLoad(&bins[65])), 1e-6);
        // Range [2^-6, 2^18) cd/m², matching `readback::HIST_LOG2_MIN` / `HIST_LOG2_RANGE`.
        let t = clamp((log2(y) + 6.0) / 24.0, 0.0, 0.999);
        let bin = min(u32(t * 64.0), 63u);
        // Centre-weighted metering: slot 64 holds the weight (f32 bits, 0 = even). A
        // Gaussian over the middle third of the frame counts up to 9× (`ExposureMeter`).
        let center_weight = bitcast<f32>(atomicLoad(&bins[64]));
        let p = (vec2<f32>(id.xy) + vec2<f32>(0.5)) / vec2<f32>(size) - vec2<f32>(0.5);
        let aspect = f32(size.x) / max(f32(size.y), 1.0);
        let d2 = p.x * p.x * aspect * aspect + p.y * p.y;
        let w = 1u + u32(round(8.0 * clamp(center_weight, 0.0, 1.0) * exp(-d2 / 0.08)));
        local_add(bin, w);
    }
    workgroupBarrier();
    if (li < 64u) {
        let n = local_load(li);
        if (n > 0u) {
            atomicAdd(&bins[li], n);
        }
    }
}
