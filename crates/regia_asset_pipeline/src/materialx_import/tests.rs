// SPDX-License-Identifier: MPL-2.0
//! Unit tests of the MaterialX importer: value parsing, path resolution, error codes, static
//! switches and their varying fallback, swizzles, custom-node parameters and the limits.

use regia_matgraph::{
    ColorSpace, GraphAlphaMode, Node, NormalSpace, ParamValue, compile, compile_unvalidated,
};

use super::translate::parse_value;
use super::*;

fn wrap(body: &str) -> String {
    format!(
        r#"<?xml version="1.0"?>
<materialx version="1.38">
{body}
  <surfacematerial name="M" type="material">
    <input name="surfaceshader" type="surfaceshader" nodename="SR" />
  </surfacematerial>
</materialx>"#
    )
}

fn import(text: &str) -> MaterialXImport {
    import_materialx_str(text, "assets/materials", &MaterialXSettings::default()).expect("imports")
}

fn err(text: &str) -> MaterialXError {
    import_materialx_str(text, "assets/materials", &MaterialXSettings::default()).expect_err("fails")
}

/// The node an output slot names, after following nothing (outputs name emitted nodes).
fn out_node<'a>(i: &'a MaterialXImport, slot: Option<&'a String>) -> &'a Node {
    &i.graph.nodes[slot.expect("output set")]
}

/// A colour value honours `colorspace` like an image file: `0.5` marked `srgb_texture` cooks as
/// the linear ≈0.214, not the raw 0.5 (corr-low pipeline-b #17). A non-colour input with the
/// attribute is left alone.
#[test]
fn a_color_value_decodes_its_colorspace() {
    let i = import(&wrap(
        r#"<standard_surface name="SR" type="surfacemeshader">
      <input name="base_color" type="color3" value="0.5, 0.5, 0.5" colorspace="srgb_texture" />
      <input name="specular_roughness" type="float" value="0.5" colorspace="srgb_texture" />
    </standard_surface>"#,
    ));
    let p = |name: &str| i.graph.params.iter().find(|p| p.name == name).map(|p| p.value);
    let ParamValue::Color(c) = p("base_color").expect("base_color param") else {
        panic!("base_color is a color");
    };
    assert!(
        (c[0] - 0.214_04).abs() < 1e-4,
        "srgb 0.5 decodes to linear ~0.214, got {c:?}"
    );
    // The colour attribute on a float input is meaningless: the raw value stands.
    assert_eq!(p("specular_roughness"), Some(ParamValue::Float(0.5)));
}

/// corr-low pipeline-b #19: past `MAX_EMITTED_NODES`, `add` used to keep inserting (only the
/// error was recorded), so a flood grew the graph without end. The graph must stay bounded:
/// every overflow add points at the one placeholder node.
#[test]
fn the_emitted_node_limit_bounds_the_graph_not_just_the_error() {
    let doc = doc::Document::default();
    let settings = MaterialXSettings::default();
    let shader = doc::NodeElem::default();
    let mut t = translate::Translator::new(&doc, &settings, String::new(), &shader);
    for i in 0..(MAX_EMITTED_NODES + 1000) {
        t.add(&format!("flood_{i}"), Node::Float(0.0));
    }
    assert!(t.fatal.is_some(), "the limit stays a fatal error");
    assert!(
        t.graph.nodes.len() <= MAX_EMITTED_NODES + 1,
        "overflow adds stop at one placeholder, got {} nodes",
        t.graph.nodes.len()
    );
}

#[test]
fn values_parse_by_width() {
    assert_eq!(parse_value("0.5", 1), Some([0.5, 0.0, 0.0, 0.0]));
    assert_eq!(parse_value(" 0.1, 0.2,0.3 ", 3), Some([0.1, 0.2, 0.3, 0.0]));
    assert_eq!(parse_value("1 2", 2), Some([1.0, 2.0, 0.0, 0.0]));
    assert_eq!(parse_value("true", 1), Some([1.0, 0.0, 0.0, 0.0]));
    assert_eq!(parse_value("false", 1), Some([0.0; 4]));
    assert_eq!(parse_value("0.1, 0.2", 3), None, "too few components");
    assert_eq!(parse_value("1, 2, 3, 4", 3), None, "too many components");
    assert_eq!(parse_value("nan", 1), None);
    assert_eq!(parse_value("inf", 1), None);
    assert_eq!(parse_value("abc", 1), None);
}

#[test]
fn a_bare_standard_surface_maps_to_its_defaults() {
    let i = import(&wrap(r#"<standard_surface name="SR" type="surfaceshader" />"#));
    assert_eq!(i.report.shader, "standard_surface");
    assert_eq!(i.report.material, "M");
    // base 1 × base_color 0.8 (the Standard Surface default base colour), metalness 0,
    // roughness 0.2: exposed as parameters with those defaults.
    let p = |name: &str| i.graph.params.iter().find(|p| p.name == name).map(|p| p.value);
    assert_eq!(p("base_color"), Some(ParamValue::Color([0.8, 0.8, 0.8])));
    assert_eq!(p("metalness"), Some(ParamValue::Float(0.0)));
    assert_eq!(p("specular_roughness"), Some(ParamValue::Float(0.2)));
    assert!(i.graph.output.alpha.is_none() && i.graph.output.emissive.is_none());
    assert_eq!(i.graph.shading.alpha_mode, GraphAlphaMode::Opaque);
    assert!(i.report.warnings.is_empty(), "{:?}", i.report.warnings);
    compile(&i.graph).expect("compiles and validates");
}

#[test]
fn dropped_shader_inputs_warn_each_with_the_spec_default() {
    let text = wrap(
        r#"<standard_surface name="SR" type="surfaceshader">
             <input name="sheen" type="float" value="0.4" />
             <input name="sheen_color" type="color3" value="1.0, 0.0, 0.0" />
             <input name="thin_film_IOR" type="float" value="1.3" />
           </standard_surface>"#,
    );
    let i = import(&text);
    let lossy: Vec<&str> = i
        .report
        .warnings
        .iter()
        .filter(|w| w.code == WARN_LOSSY_INPUT)
        .map(|w| w.node.as_str())
        .collect();
    assert_eq!(lossy, ["SR.sheen", "SR.sheen_color", "SR.thin_film_IOR"]);
    // Inputs at their spec defaults stay silent.
    let quiet = wrap(
        r#"<standard_surface name="SR" type="surfaceshader">
             <input name="sheen" type="float" value="0.0" />
             <input name="sheen_roughness" type="float" value="0.3" />
           </standard_surface>"#,
    );
    let i = import(&quiet);
    assert!(i.report.warnings.is_empty(), "{:?}", i.report.warnings);
    compile(&i.graph).expect("compiles and validates");
}

#[test]
fn without_parameter_exposure_the_inputs_are_constants() {
    let settings = MaterialXSettings {
        expose_surface_params: false,
        ..MaterialXSettings::default()
    };
    let text = wrap(
        r#"<standard_surface name="SR" type="surfaceshader">
             <input name="base_color" type="color3" value="0.1, 0.2, 0.3" />
             <input name="base" type="float" value="0.5" />
           </standard_surface>"#,
    );
    let i = import_materialx_str(&text, "assets", &settings).expect("imports");
    assert!(i.graph.params.is_empty());
    // base × base_color folds to one constant.
    assert_eq!(
        out_node(&i, i.graph.output.base_color.as_ref()),
        &Node::Vec3([0.05, 0.1, 0.15])
    );
}

#[test]
fn error_codes_name_each_failure() {
    assert_eq!(err("<materialx version=\"1.38\"><nodegraph").code(), "mtlx_parse");
    assert_eq!(err("<scene version=\"1.38\"/>").code(), "mtlx_not_materialx");
    assert_eq!(err("<materialx version=\"2.0\"/>").code(), "mtlx_version");
    assert_eq!(err("<materialx/>").code(), "mtlx_version");
    assert_eq!(err("<materialx version=\"1.38\"/>").code(), "mtlx_no_material");
    assert_eq!(
        err(&wrap(r#"<UsdPreviewSurface name="SR" type="surfaceshader" />"#)).code(),
        "mtlx_unsupported_shader"
    );
    assert_eq!(
        err("<materialx version=\"1.38\"><image type=\"color3\"/></materialx>").code(),
        "mtlx_structure"
    );
    let dtd = "<?xml version=\"1.0\"?><!DOCTYPE m [<!ENTITY a \"aaaa\">]><materialx version=\"1.38\">&a;</materialx>";
    assert_eq!(err(dtd).code(), "mtlx_parse", "DTDs are refused");
    let twice = wrap(
        r#"<standard_surface name="SR" type="surfaceshader" />
           <constant name="SR" type="float" />"#,
    );
    assert_eq!(err(&twice).code(), "mtlx_structure");
    let big = vec![b' '; MAX_SOURCE_BYTES + 1];
    let e = import_materialx_bytes(&big, "", &MaterialXSettings::default()).expect_err("too large");
    assert_eq!(e.code(), "mtlx_too_large");
    let e = import_materialx_bytes(&[0xff, 0xfe], "", &MaterialXSettings::default()).expect_err("utf8");
    assert_eq!(e.code(), "mtlx_not_utf8");
    for e in [
        MaterialXError::Io(String::new()),
        MaterialXError::Cycle(String::new()),
        MaterialXError::Limit(String::new()),
        MaterialXError::Settings(String::new()),
    ] {
        let code = e.code();
        assert!(
            code.starts_with("mtlx_") && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
            "{code}"
        );
    }
}

#[test]
fn a_named_material_is_picked_and_the_others_are_reported() {
    let text = r#"<materialx version="1.38">
      <standard_surface name="A" type="surfaceshader"><input name="metalness" type="float" value="0.1"/></standard_surface>
      <open_pbr_surface name="B" type="surfaceshader"><input name="base_metalness" type="float" value="0.9"/></open_pbr_surface>
      <surfacematerial name="MA" type="material"><input name="surfaceshader" type="surfaceshader" nodename="A"/></surfacematerial>
      <surfacematerial name="MB" type="material"><input name="surfaceshader" type="surfaceshader" nodename="B"/></surfacematerial>
    </materialx>"#;
    let first = import(text);
    assert_eq!(first.report.material, "MA");
    assert!(
        first
            .report
            .warnings
            .iter()
            .any(|w| w.code == WARN_EXTRA_MATERIAL && w.node == "MB")
    );
    let settings = MaterialXSettings {
        material: Some("MB".into()),
        ..MaterialXSettings::default()
    };
    let second = import_materialx_str(text, "assets", &settings).expect("MB");
    assert_eq!(second.report.shader, "open_pbr_surface");
    let missing = MaterialXSettings {
        material: Some("MC".into()),
        ..MaterialXSettings::default()
    };
    let e = import_materialx_str(text, "assets", &missing).expect_err("no MC");
    assert_eq!(e.code(), "mtlx_no_material");
    assert!(e.to_string().contains("MA, MB"), "{e}");
}

#[test]
fn image_paths_resolve_against_the_document_and_its_prefixes() {
    let text = wrap(
        r#"<nodegraph name="NG" fileprefix="tex/">
             <image name="a" type="color3"><input name="file" type="filename" value="../shared/a.png" colorspace="srgb_texture"/></image>
             <image name="b" type="float"><input name="file" type="filename" value="b.png" colorspace="srgb_texture"/></image>
             <image name="c" type="color3"><input name="file" type="filename" value="/abs/c.png"/></image>
             <image name="d" type="color3"><input name="file" type="filename" value="../../../../d.png"/></image>
             <image name="e" type="color3"><input name="file" type="filename" value="e.&lt;UDIM&gt;.png"/></image>
             <add name="ab" type="color3"><input name="in1" type="color3" nodename="a"/><input name="in2" type="float" nodename="b"/></add>
             <add name="abc" type="color3"><input name="in1" type="color3" nodename="ab"/><input name="in2" type="color3" nodename="c"/></add>
             <add name="abcd" type="color3"><input name="in1" type="color3" nodename="abc"/><input name="in2" type="color3" nodename="d"/></add>
             <add name="all" type="color3"><input name="in1" type="color3" nodename="abcd"/><input name="in2" type="color3" nodename="e"/></add>
             <output name="out" type="color3" nodename="all"/>
           </nodegraph>
           <standard_surface name="SR" type="surfaceshader"><input name="base_color" type="color3" nodegraph="NG" output="out"/></standard_surface>"#,
    );
    let i = import(&text);
    let textures: Vec<(&str, ColorSpace)> = i
        .graph
        .textures
        .iter()
        .map(|t| (t.path.as_str(), t.color_space))
        .collect();
    assert_eq!(
        textures,
        vec![
            ("assets/materials/shared/a.png", ColorSpace::Srgb),
            // A float image is data: read raw whatever its colour space says.
            ("assets/materials/tex/b.png", ColorSpace::Linear),
        ]
    );
    let refused: Vec<&str> = i
        .report
        .warnings
        .iter()
        .filter(|w| w.code == WARN_TEXTURE_PATH)
        .map(|w| w.node.as_str())
        .collect();
    assert_eq!(refused, vec!["NG/c", "NG/d", "NG/e"]);
    compile(&i.graph).expect("compiles");
}

/// A folded image path still has to be a portable project path. `...` is a parent directory on
/// Windows, `nul.png` is a device name, and `C:secret.png` is a drive prefix. `check_relative`
/// refuses all three; the lexical fold alone stores them.
#[test]
fn image_paths_refuse_dot_segments_devices_and_colons() {
    let text = wrap(
        r#"<nodegraph name="NG">
             <image name="dots" type="color3"><input name="file" type="filename" value=".../secret.png"/></image>
             <image name="dev" type="color3"><input name="file" type="filename" value="nul.png"/></image>
             <image name="drv" type="color3"><input name="file" type="filename" value="C:secret.png"/></image>
             <add name="ab" type="color3"><input name="in1" type="color3" nodename="dots"/><input name="in2" type="color3" nodename="dev"/></add>
             <add name="all" type="color3"><input name="in1" type="color3" nodename="ab"/><input name="in2" type="color3" nodename="drv"/></add>
             <output name="out" type="color3" nodename="all"/>
           </nodegraph>
           <standard_surface name="SR" type="surfaceshader"><input name="base_color" type="color3" nodegraph="NG" output="out"/></standard_surface>"#,
    );
    let i = import(&text);
    assert!(
        i.graph.textures.is_empty(),
        "windows-special paths must not become texture paths: {:?}",
        i.graph
            .textures
            .iter()
            .map(|t| t.path.as_str())
            .collect::<Vec<_>>()
    );
    let refused: Vec<&str> = i
        .report
        .warnings
        .iter()
        .filter(|w| w.code == WARN_TEXTURE_PATH)
        .map(|w| w.node.as_str())
        .collect();
    assert_eq!(refused, vec!["NG/dots", "NG/dev", "NG/drv"]);
}

#[test]
fn more_images_than_slots_warn_and_fall_back() {
    let mut nodes = String::new();
    let mut prev = String::from("i0");
    for k in 0..9 {
        nodes.push_str(&format!(
            r#"<image name="i{k}" type="float"><input name="file" type="filename" value="t{k}.png"/></image>"#
        ));
        if k > 0 {
            nodes.push_str(&format!(
                r#"<add name="s{k}" type="float"><input name="in1" type="float" nodename="{prev}"/><input name="in2" type="float" nodename="i{k}"/></add>"#
            ));
            prev = format!("s{k}");
        }
    }
    let text = wrap(&format!(
        r#"<nodegraph name="NG">{nodes}<output name="out" type="float" nodename="{prev}"/></nodegraph>
           <standard_surface name="SR" type="surfaceshader"><input name="specular_roughness" type="float" nodegraph="NG" output="out"/></standard_surface>"#
    ));
    let i = import(&text);
    assert_eq!(i.graph.textures.len(), 7);
    assert_eq!(
        i.report
            .warnings
            .iter()
            .filter(|w| w.code == WARN_TEXTURE_LIMIT)
            .count(),
        2
    );
    assert!(i.report.matgraph_gaps.iter().any(|g| g.contains("7 images")));
    compile(&i.graph).expect("compiles");
}

#[test]
fn texture_coordinates_flip_v_between_conventions() {
    // A plain image samples the mesh UV as is (the two flips cancel).
    let text = wrap(
        r#"<image name="img" type="color3"><input name="file" type="filename" value="a.png"/></image>
           <standard_surface name="SR" type="surfaceshader"><input name="base_color" type="color3" nodename="img"/></standard_surface>"#,
    );
    let i = import(&text);
    let sample = i
        .graph
        .nodes
        .values()
        .find_map(|n| match n {
            Node::Sample { uv, .. } => Some(uv.clone()),
            _ => None,
        })
        .expect("a sample");
    assert_eq!(i.graph.nodes[&sample], Node::TexCoord);

    // A tiled image with constant tiling becomes one UV transform with the flipped offset:
    // v' = (1 - v) * 3 + 0.25 sampled at 1 - v' = 3v - 2.25.
    let text = wrap(
        r#"<tiledimage name="img" type="color3">
             <input name="file" type="filename" value="a.png"/>
             <input name="uvtiling" type="vector2" value="2, 3"/>
             <input name="uvoffset" type="vector2" value="0.5, 0.25"/>
           </tiledimage>
           <standard_surface name="SR" type="surfaceshader"><input name="base_color" type="color3" nodename="img"/></standard_surface>"#,
    );
    let i = import(&text);
    let transform = i
        .graph
        .nodes
        .values()
        .find(|n| matches!(n, Node::UvTransform { .. }))
        .expect("uv transform");
    assert_eq!(
        transform,
        &Node::UvTransform {
            uv: "uv0".into(),
            scale: [2.0, 3.0],
            offset: [0.5, -2.25],
            rotation: 0.0,
        }
    );
    // An explicit texcoord read by arithmetic is flipped into MaterialX space and back.
    let text = wrap(
        r#"<texcoord name="tc" type="vector2"/>
           <multiply name="twice" type="vector2"><input name="in1" type="vector2" nodename="tc"/><input name="in2" type="float" value="2"/></multiply>
           <image name="img" type="color3"><input name="file" type="filename" value="a.png"/><input name="texcoord" type="vector2" nodename="twice"/></image>
           <standard_surface name="SR" type="surfaceshader"><input name="base_color" type="color3" nodename="img"/></standard_surface>"#,
    );
    let i = import(&text);
    let flips = i
        .graph
        .nodes
        .values()
        .filter(|n| matches!(n, Node::OneMinus(_)))
        .count();
    assert_eq!(flips, 2, "one flip into MaterialX space, one back");
    compile(&i.graph).expect("compiles");
}

#[test]
fn constant_conditions_are_static_and_varying_ones_blend() {
    let text = wrap(
        r#"<ifequal name="pick" type="color3">
             <input name="value1" type="integer" value="2"/>
             <input name="value2" type="integer" value="2"/>
             <input name="in1" type="color3" value="1, 0, 0"/>
             <input name="in2" type="color3" value="0, 0, 1"/>
           </ifequal>
           <standard_surface name="SR" type="surfaceshader"><input name="base_color" type="color3" nodename="pick"/></standard_surface>"#,
    );
    let i = import(&text);
    assert_eq!(
        out_node(&i, i.graph.output.base_color.as_ref()),
        &Node::Vec3([1.0, 0.0, 0.0])
    );
    assert!(
        i.report
            .mapped
            .iter()
            .any(|m| m.node == "pick" && m.mapped_to == "static switch: in1")
    );

    let text = wrap(
        r#"<texcoord name="tc" type="vector2"/>
           <extract name="u" type="float"><input name="in" type="vector2" nodename="tc"/><input name="index" type="integer" value="0"/></extract>
           <ifgreater name="pick" type="float">
             <input name="value1" type="float" nodename="u"/>
             <input name="value2" type="float" value="0.5"/>
             <input name="in1" type="float" value="0.9"/>
             <input name="in2" type="float" value="0.1"/>
           </ifgreater>
           <standard_surface name="SR" type="surfaceshader"><input name="specular_roughness" type="float" nodename="pick"/></standard_surface>"#,
    );
    let i = import(&text);
    assert!(matches!(
        out_node(&i, i.graph.output.roughness.as_ref()),
        Node::Lerp { .. }
    ));
    assert!(
        i.report
            .warnings
            .iter()
            .any(|w| w.code == WARN_LOSSY_INPUT && w.node == "pick")
    );
    assert!(i.report.matgraph_gaps.iter().any(|g| g.contains("ifgreater")));
    compile(&i.graph).expect("compiles");
}

#[test]
fn swizzles_combines_and_separates_map_to_swizzle_and_append() {
    let text = wrap(
        r#"<constant name="c" type="color3"><input name="value" type="color3" value="0.1, 0.2, 0.3"/></constant>
           <geomcolor name="vc" type="color4"/>
           <swizzle name="bgr" type="color3"><input name="in" type="color4" nodename="vc"/><input name="channels" type="string" value="bgr"/></swizzle>
           <swizzle name="r01" type="color3"><input name="in" type="color4" nodename="vc"/><input name="channels" type="string" value="r01"/></swizzle>
           <separate3 name="sep" type="multioutput"><input name="in" type="color3" nodename="bgr"/></separate3>
           <combine3 name="cmb" type="color3">
             <input name="in1" type="float" nodename="sep" output="outb"/>
             <input name="in2" type="float" nodename="sep" output="outg"/>
             <input name="in3" type="float" nodename="sep" output="outr"/>
           </combine3>
           <add name="sum" type="color3"><input name="in1" type="color3" nodename="cmb"/><input name="in2" type="color3" nodename="r01"/></add>
           <multiply name="m" type="color3"><input name="in1" type="color3" nodename="sum"/><input name="in2" type="color3" nodename="c"/></multiply>
           <standard_surface name="SR" type="surfaceshader"><input name="base_color" type="color3" nodename="m"/></standard_surface>"#,
    );
    let i = import(&text);
    assert!(
        i.graph
            .nodes
            .values()
            .any(|n| matches!(n, Node::Swizzle { channels, .. } if channels == "zyx"))
    );
    assert!(i.graph.nodes.values().any(|n| matches!(n, Node::Append(..))));
    assert!(i.graph.nodes.values().any(|n| matches!(n, Node::VertexColor)));
    assert!(i.report.unsupported().next().is_none(), "{:?}", i.report.warnings);
    compile(&i.graph).expect("compiles");
}

#[test]
fn custom_nodes_expose_their_inputs_with_document_defaults() {
    let text = r#"<materialx version="1.38">
      <nodedef name="ND_tint" node="tint">
        <input name="color" type="color3" value="0.2, 0.4, 0.6" />
        <input name="gain" type="float" value="1.5" />
        <input name="mode" type="integer" value="1" />
        <output name="out" type="color3" />
      </nodedef>
      <nodegraph name="NG_tint" nodedef="ND_tint">
        <multiply name="m" type="color3"><input name="in1" type="color3" interfacename="color"/><input name="in2" type="float" interfacename="gain"/></multiply>
        <ifequal name="pick" type="color3">
          <input name="value1" type="integer" interfacename="mode"/><input name="value2" type="integer" value="1"/>
          <input name="in1" type="color3" nodename="m"/><input name="in2" type="color3" value="0,0,0"/>
        </ifequal>
        <output name="out" type="color3" nodename="pick" />
      </nodegraph>
      <tint name="t1" type="color3"><input name="gain" type="float" value="0.75" /></tint>
      <standard_surface name="SR" type="surfaceshader"><input name="base_color" type="color3" nodename="t1"/></standard_surface>
      <surfacematerial name="M" type="material"><input name="surfaceshader" type="surfaceshader" nodename="SR"/></surfacematerial>
    </materialx>"#;
    let i = import(text);
    let p = |name: &str| i.graph.params.iter().find(|p| p.name == name).map(|p| p.value);
    assert_eq!(
        p("t1_color"),
        Some(ParamValue::Color([0.2, 0.4, 0.6])),
        "the nodedef default"
    );
    assert_eq!(
        p("t1_gain"),
        Some(ParamValue::Float(0.75)),
        "the instance's value"
    );
    assert_eq!(p("t1_mode"), None, "integers stay constants (they pick branches)");
    assert!(
        i.report
            .mapped
            .iter()
            .any(|m| m.node == "t1/pick" && m.mapped_to == "static switch: in1")
    );
    assert!(
        i.report
            .params
            .iter()
            .any(|q| q.name == "t1_gain" && q.source == "t1.gain")
    );
    compile(&i.graph).expect("compiles");
}

#[test]
fn cycles_and_runaway_documents_are_errors() {
    let cycle = wrap(
        r#"<add name="a" type="float"><input name="in1" type="float" nodename="b"/></add>
           <add name="b" type="float"><input name="in1" type="float" nodename="a"/></add>
           <standard_surface name="SR" type="surfaceshader"><input name="metalness" type="float" nodename="a"/></standard_surface>"#,
    );
    assert_eq!(err(&cycle).code(), "mtlx_cycle");

    let mut chain = String::from(
        r#"<constant name="n0" type="float"><input name="value" type="float" value="0.5"/></constant>"#,
    );
    for k in 1..400 {
        chain.push_str(&format!(
            r#"<sin name="n{k}" type="float"><input name="in" type="float" nodename="n{}"/></sin>"#,
            k - 1
        ));
    }
    let deep = wrap(&format!(
        r#"{chain}<standard_surface name="SR" type="surfaceshader"><input name="metalness" type="float" nodename="n399"/></standard_surface>"#
    ));
    assert_eq!(err(&deep).code(), "mtlx_limit");

    // A custom node that uses itself expands until the expansion limit.
    let recursive = r#"<materialx version="1.38">
      <nodedef name="ND_r" node="r"><output name="out" type="float"/></nodedef>
      <nodegraph name="NG_r" nodedef="ND_r"><r name="inner" type="float"/><output name="out" type="float" nodename="inner"/></nodegraph>
      <r name="top" type="float"/>
      <standard_surface name="SR" type="surfaceshader"><input name="metalness" type="float" nodename="top"/></standard_surface>
      <surfacematerial name="M" type="material"><input name="surfaceshader" type="surfaceshader" nodename="SR"/></surfacematerial>
    </materialx>"#;
    assert_eq!(err(recursive).code(), "mtlx_limit");
}

#[test]
fn missing_connections_and_bad_values_warn_and_use_defaults() {
    let text = wrap(
        r#"<standard_surface name="SR" type="surfaceshader">
             <input name="metalness" type="float" nodename="nowhere"/>
             <input name="specular_roughness" type="float" value="rough"/>
             <input name="base_color" type="color3" nodegraph="NG_missing"/>
           </standard_surface>"#,
    );
    let i = import(&text);
    let codes: Vec<&str> = i.report.warnings.iter().map(|w| w.code).collect();
    assert!(codes.contains(&WARN_MISSING_NODE), "{codes:?}");
    assert!(codes.contains(&WARN_BAD_VALUE), "{codes:?}");
    assert_eq!(out_node(&i, i.graph.output.metallic.as_ref()), &Node::Float(0.0));
    compile(&i.graph).expect("compiles");
}

#[test]
fn normals_pick_the_output_space() {
    let text = wrap(
        r#"<normal name="n" type="vector3"><input name="space" type="string" value="world"/></normal>
           <standard_surface name="SR" type="surfaceshader"><input name="normal" type="vector3" nodename="n"/></standard_surface>"#,
    );
    let i = import(&text);
    assert_eq!(i.graph.output.normal_space, NormalSpace::World);
    assert_eq!(out_node(&i, i.graph.output.normal.as_ref()), &Node::WorldNormal);
    compile(&i.graph).expect("compiles");
}

#[test]
fn settings_parse_from_the_meta_string() {
    assert_eq!(
        MaterialXSettings::from_meta_string(None).expect("none"),
        MaterialXSettings::default()
    );
    assert_eq!(
        MaterialXSettings::from_meta_string(Some("  ")).expect("blank"),
        MaterialXSettings::default()
    );
    let s = MaterialXSettings::from_meta_string(Some(
        "(shading_model: Unlit, alpha: Mask(0.4), emission_scale: 100.0, flip_v: false, material: Some(\"M\"))",
    ))
    .expect("parses");
    assert_eq!(s.shading_model, ShadingModel::Unlit);
    assert_eq!(s.alpha, AlphaPolicy::Mask(0.4));
    assert_eq!(s.material.as_deref(), Some("M"));
    let e = MaterialXSettings::from_meta_string(Some("(colour: 1)")).expect_err("unknown key");
    assert_eq!(e.code(), "mtlx_settings");

    let text = wrap(
        r#"<standard_surface name="SR" type="surfaceshader">
             <input name="emission" type="float" value="2"/>
             <input name="opacity" type="color3" value="0.5, 0.5, 0.5"/>
           </standard_surface>"#,
    );
    let i = import_materialx_str(&text, "assets", &s).expect("imports");
    assert!(i.graph.shading.unlit);
    assert_eq!(i.graph.shading.alpha_mode, GraphAlphaMode::Mask(0.4));
    assert!(i.graph.output.emissive.is_some() && i.graph.output.alpha.is_some());
    compile_unvalidated(&i.graph).expect("compiles");
}

#[test]
fn every_parameter_slot_fills_then_the_rest_stay_constants() {
    let mut inputs = String::new();
    let mut nodes = String::new();
    let mut prev = String::from("k0");
    for k in 0..20 {
        inputs.push_str(&format!(r#"<input name="k{k}" type="float" value="{k}.0"/>"#));
        if k > 0 {
            nodes.push_str(&format!(
                r#"<add name="s{k}" type="float"><input name="in1" type="float" {} /><input name="in2" type="float" interfacename="k{k}"/></add>"#,
                if k == 1 { r#"interfacename="k0""#.to_string() } else { format!(r#"nodename="{prev}""#) }
            ));
            prev = format!("s{k}");
        }
    }
    let text = wrap(&format!(
        r#"<nodegraph name="NG">{inputs}{nodes}<output name="out" type="float" nodename="{prev}"/></nodegraph>
           <standard_surface name="SR" type="surfaceshader"><input name="metalness" type="float" nodegraph="NG" output="out"/></standard_surface>"#
    ));
    let i = import(&text);
    assert_eq!(i.graph.params.len(), regia_matgraph::MAX_PARAMS);
    assert!(i.report.warnings.iter().any(|w| w.code == WARN_PARAM_LIMIT));
    compile(&i.graph).expect("compiles");
}

#[test]
fn the_report_serializes_its_lists() {
    let text = wrap(
        r#"<cellnoise2d name="cells" type="float"/>
           <standard_surface name="SR" type="surfaceshader"><input name="metalness" type="float" nodename="cells"/></standard_surface>"#,
    );
    let i = import(&text);
    let json = i.report.to_json();
    assert_eq!(json["unsupported"][0]["node"], "cells");
    assert_eq!(json["unsupported"][0]["category"], "cellnoise2d");
    assert_eq!(json["unsupported"][0]["code"], WARN_UNSUPPORTED_NODE);
    assert_eq!(json["shader"], "standard_surface");
    assert!(
        json["mapped"]
            .as_array()
            .is_some_and(|m| m.iter().any(|x| x["node"] == "SR"))
    );
}

#[test]
fn document_dirs_are_the_parent_path() {
    assert_eq!(document_dir("assets/materials/wood.mtlx"), "assets/materials");
    assert_eq!(document_dir("wood.mtlx"), "");
    assert_eq!(document_dir("assets\\m\\w.mtlx"), "assets/m");
}

/// A node graph output that passes an interface input bound back to its own graph (or two graphs
/// bound to each other) is a `mtlx_cycle` error. Before, the binding resolved itself through
/// `graph_output` without passing the node cycle check, recursing until the stack overflowed,
/// which aborts the cook. A binding the graph reads twice still resolves once.
#[test]
fn an_interface_binding_cycle_is_an_error_not_a_stack_overflow() {
    let own = wrap(
        r#"<nodegraph name="g">
             <input name="i" type="float" nodegraph="g" output="out"/>
             <output name="out" type="float" interfacename="i"/>
           </nodegraph>
           <standard_surface name="SR" type="surfaceshader"><input name="metalness" type="float" nodegraph="g" output="out"/></standard_surface>"#,
    );
    assert_eq!(err(&own).code(), "mtlx_cycle");
    let pair = wrap(
        r#"<nodegraph name="g">
             <input name="i" type="float" nodegraph="h" output="out"/>
             <output name="out" type="float" interfacename="i"/>
           </nodegraph>
           <nodegraph name="h">
             <input name="j" type="float" nodegraph="g" output="out"/>
             <output name="out" type="float" interfacename="j"/>
           </nodegraph>
           <standard_surface name="SR" type="surfaceshader"><input name="metalness" type="float" nodegraph="g" output="out"/></standard_surface>"#,
    );
    assert_eq!(err(&pair).code(), "mtlx_cycle");
    // Not a cycle: two outputs pass the same interface input, read by two shader inputs.
    let shared = wrap(
        r#"<nodegraph name="g">
             <input name="i" type="float" value="0.25"/>
             <output name="a" type="float" interfacename="i"/>
             <output name="b" type="float" interfacename="i"/>
           </nodegraph>
           <standard_surface name="SR" type="surfaceshader">
             <input name="metalness" type="float" nodegraph="g" output="a"/>
             <input name="specular_roughness" type="float" nodegraph="g" output="b"/>
           </standard_surface>"#,
    );
    let i = import(&shared);
    compile(&i.graph).expect("compiles");
}

/// A swizzle whose `channels` string is longer than four is refused before any node is emitted:
/// one `mtlx_unsupported_node` warning and a constant. Before, the importer emitted a node per
/// character (each a name search over the graph, so a hostile 100 KB string ran for hours) and,
/// past the node limit, failed the whole material.
#[test]
fn a_long_swizzle_channel_string_is_refused_up_front() {
    let channels = "r01".repeat(5000);
    let text = wrap(&format!(
        r#"<geomcolor name="vc" type="color4"/>
           <swizzle name="long" type="color3"><input name="in" type="color4" nodename="vc"/><input name="channels" type="string" value="{channels}"/></swizzle>
           <standard_surface name="SR" type="surfaceshader"><input name="base_color" type="color3" nodename="long"/></standard_surface>"#
    ));
    let i = import(&text);
    assert_eq!(i.report.unsupported().count(), 1, "{:?}", i.report.warnings);
    assert!(i.report.emitted_nodes < 50, "{}", i.report.emitted_nodes);
    compile(&i.graph).expect("compiles");
}

/// A tiled image with no texcoord and a tiling that is not a constant (a node graph interface
/// input, which becomes a material parameter) flips the mesh UV into MaterialX space before the
/// tiling and back after it, as with an explicit `texcoord`. Before, the default texcoord entered
/// the arithmetic unflipped and the image sampled upside down (`1 - v` at tiling 1, offset 0).
#[test]
fn a_tiled_image_with_varying_tiling_and_no_texcoord_flips_both_ways() {
    let text = wrap(
        r#"<nodegraph name="NG">
             <input name="tiling" type="vector2" value="1, 1"/>
             <tiledimage name="img" type="color3">
               <input name="file" type="filename" value="a.png"/>
               <input name="uvtiling" type="vector2" interfacename="tiling"/>
             </tiledimage>
             <output name="out" type="color3" nodename="img"/>
           </nodegraph>
           <standard_surface name="SR" type="surfaceshader"><input name="base_color" type="color3" nodegraph="NG" output="out"/></standard_surface>"#,
    );
    let i = import(&text);
    assert!(
        i.report.params.iter().any(|p| p.source == "NG.tiling"),
        "the tiling is a parameter: {:?}",
        i.report.params
    );
    let flips = i
        .graph
        .nodes
        .values()
        .filter(|n| matches!(n, Node::OneMinus(_)))
        .count();
    assert_eq!(flips, 2, "one flip into MaterialX space, one back");
    compile(&i.graph).expect("compiles");
}

/// A dropped shader input (Standard Surface `specular_color`: the lit model's specular is a fixed
/// F0) connected to an image is reported as lossy without being translated: its texture is not
/// declared, so the material neither cooks nor binds it, and a missing file behind an input the
/// material never reads cannot fail the cook. Before, the check resolved the connection and
/// declared `spec.png`.
#[test]
fn a_connected_dropped_input_declares_no_texture() {
    let text = wrap(
        r#"<image name="spec" type="color3"><input name="file" type="filename" value="spec.png"/></image>
           <standard_surface name="SR" type="surfaceshader"><input name="specular_color" type="color3" nodename="spec"/></standard_surface>"#,
    );
    let i = import(&text);
    assert!(i.graph.textures.is_empty(), "{:?}", i.graph.textures);
    assert!(i.report.textures.is_empty(), "{:?}", i.report.textures);
    assert!(
        i.report
            .warnings
            .iter()
            .any(|w| w.code == WARN_LOSSY_INPUT && w.node == "SR.specular_color"),
        "{:?}",
        i.report.warnings
    );
    compile(&i.graph).expect("compiles");
}

/// A tangent-space normal map that reaches the shader's normal through another node (two maps
/// blended by `mix`, one `normalize`d) cannot be applied, since only a map wired straight to the
/// normal is. The import says so with an `mtlx_lossy_input` warning on the shader's normal
/// instead of silently outputting the mesh normal. A map wired straight in still applies, with no
/// warning.
#[test]
fn a_normal_map_through_other_nodes_is_reported_not_dropped_silently() {
    let maps = r#"<image name="n1" type="vector3"><input name="file" type="filename" value="n1.png"/></image>
           <image name="n2" type="vector3"><input name="file" type="filename" value="n2.png"/></image>
           <normalmap name="m1" type="vector3"><input name="in" type="vector3" nodename="n1"/></normalmap>
           <normalmap name="m2" type="vector3"><input name="in" type="vector3" nodename="n2"/></normalmap>"#;
    let blended = wrap(&format!(
        r#"{maps}
           <mix name="blend" type="vector3"><input name="fg" type="vector3" nodename="m1"/><input name="bg" type="vector3" nodename="m2"/><input name="mix" type="float" value="0.5"/></mix>
           <standard_surface name="SR" type="surfaceshader"><input name="normal" type="vector3" nodename="blend"/></standard_surface>"#
    ));
    let i = import(&blended);
    let reported = |i: &MaterialXImport| {
        i.report
            .warnings
            .iter()
            .any(|w| w.code == WARN_LOSSY_INPUT && w.node == "SR.normal")
    };
    assert!(reported(&i), "{:?}", i.report.warnings);
    compile(&i.graph).expect("compiles");
    let normalized = wrap(&format!(
        r#"{maps}
           <normalize name="nn" type="vector3"><input name="in" type="vector3" nodename="m1"/></normalize>
           <standard_surface name="SR" type="surfaceshader"><input name="normal" type="vector3" nodename="nn"/></standard_surface>"#
    ));
    assert!(reported(&import(&normalized)));
    let direct = wrap(&format!(
        r#"{maps}
           <standard_surface name="SR" type="surfaceshader"><input name="normal" type="vector3" nodename="m1"/></standard_surface>"#
    ));
    let i = import(&direct);
    assert!(!reported(&i), "{:?}", i.report.warnings);
    assert_eq!(i.graph.output.normal_space, NormalSpace::Tangent);
}

/// With `alpha: Opaque` in the .meta, an opacity connected to an image is reported as dropped
/// without being translated: the image is not declared, cooked or bound. Before, the opacity was
/// resolved (declaring `cutout.png`) and then thrown away.
#[test]
fn an_opaque_policy_declares_no_coverage_texture() {
    let text = wrap(
        r#"<image name="cut" type="float"><input name="file" type="filename" value="cutout.png"/></image>
           <standard_surface name="SR" type="surfaceshader"><input name="opacity" type="color3" nodename="cut"/></standard_surface>"#,
    );
    let settings = MaterialXSettings {
        alpha: AlphaPolicy::Opaque,
        ..MaterialXSettings::default()
    };
    let i = import_materialx_str(&text, "assets/materials", &settings).expect("imports");
    assert!(i.graph.textures.is_empty(), "{:?}", i.graph.textures);
    assert_eq!(i.graph.shading.alpha_mode, GraphAlphaMode::Opaque);
    assert!(i.graph.output.alpha.is_none());
    assert!(
        i.report
            .warnings
            .iter()
            .any(|w| w.code == WARN_LOSSY_INPUT && w.node == "SR.opacity"),
        "{:?}",
        i.report.warnings
    );
    // The same document under the default policy still blends with the image as coverage.
    let auto = import(&text);
    assert_eq!(auto.graph.textures.len(), 1);
    assert_eq!(auto.graph.shading.alpha_mode, GraphAlphaMode::Blend);
    compile(&i.graph).expect("compiles");
}
