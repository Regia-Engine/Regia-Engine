// SPDX-License-Identifier: MPL-2.0
//! Product viewer contracts (ADR-2480, ADR-2535, `docs/formats/jewelry-*.md`).
//!
//! A product viewer is built from parts that another tool produces (kits, recipes, option graphs,
//! material libraries), and it emits builds and reads product packages. At that boundary every
//! file is JSON with a `"schema": "jewelry.<name>/<major>"` field. This crate holds:
//!
//! - the JSON Schemas (2020-12) of the interfaces Regia owns, compiled in from `docs/formats/`
//!   ([`Interface::schema_text`]);
//! - a validator for the keywords those schemas use (`schema::Schema`, feature `validate`); it
//!   also reads the schemas the kit builder publishes for the files Regia consumes, and refuses a
//!   schema it cannot evaluate in full;
//! - the contract checks no schema can express (convex gem cuts that close, hashes that match,
//!   references, a package's files against its manifest), each refusal a [`ContractError`] with
//!   an agent-protocol F-code and a stable `contract_*` reason ([`check`], [`checks`]);
//! - RFC 8785 canonical JSON ([`jcs`]), the form every contract hash is taken over.
//!
//! The feature `validate` (on by default) is the only part that needs `regex`. Without it a web
//! player keeps the hashes, the [`Interface`] table and [`check`], which then runs the envelope and
//! the contract checks but no schema.
//!
//! The wire names keep the plan's `jewelry.` namespace; nothing here is specific to a supplier.
//!
//! The configurator runtime (ADR-2534, `book/agents/configurator.md`) sits on top: it reads an
//! option graph product record, a piece recipe, part kits and a material library through a
//! [`source::ContentSource`] (no file system, thread or network in the core, so it builds for the
//! browser and Android), holds the current picks in a [`runtime::Configurator`], refuses every
//! combination the snapshot does not sell with the shared [`resolve::resolve`], and writes the
//! `jewelry.build/1` JSON.

pub mod checks;
pub mod hull;
pub mod jcs;
#[cfg(feature = "validate")]
pub mod schema;
mod sha256;
// The configurator runtime (ADR-2534).
pub mod content;
pub mod graph;
pub mod resolve;
pub mod runtime;
pub mod source;
#[cfg(feature = "validate")]
pub mod vectors;

use serde_json::Value;

#[cfg(feature = "validate")]
pub use schema::{Schema, SchemaError, Violation};

/// Without the feature `validate` no schema can exist, so a `schema_override` is always `None`.
#[cfg(not(feature = "validate"))]
#[derive(Debug)]
pub enum Schema {}

pub use graph::ProductRecord;
pub use resolve::{Pick, Picks, Resolution, resolve};
pub use runtime::{BuildOptions, Camera, ConfigError, Configurator, SetMode, Tenant, Wear};
pub use source::{ContentSource, MemorySource, OpenRequest, load_product};

/// The major version every interface of this crate is at (`jewelry.<name>/1`).
pub const CONTRACT_MAJOR: u32 = 1;

/// Largest contract file accepted (bytes): a hostile-input bound.
pub const MAX_FILE_BYTES: usize = 64 * 1024 * 1024;

/// Most errors one check reports; the rest are counted in the last entry.
pub const MAX_ERRORS: usize = 100;

/// A boundary interface (plan `interfaces.md`, in the scope of the viewer track, ADR-2535).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Interface {
    /// `jewelry.part_kit/1` (`kit.json`): owned by Regia.
    PartKit,
    /// `jewelry.material_library/1` (`library.json`): owned by Regia.
    MaterialLibrary,
    /// `jewelry.build/1`: owned by Regia.
    Build,
    /// `jewelry.product_package/1` (a package folder's `manifest.json`): owned by Regia.
    ProductPackage,
    /// `jewelry.option_graph/1`: published by the kit builder; Regia reads it.
    OptionGraph,
    /// `jewelry.piece_recipe/1`: published by the kit builder; Regia reads it.
    PieceRecipe,
}

impl Interface {
    /// Every interface, Regia-owned first.
    pub const ALL: [Interface; 6] = [
        Self::PartKit,
        Self::MaterialLibrary,
        Self::Build,
        Self::ProductPackage,
        Self::OptionGraph,
        Self::PieceRecipe,
    ];

    /// The interfaces whose schema Regia owns and ships.
    pub const OWNED: [Interface; 4] = [
        Self::PartKit,
        Self::MaterialLibrary,
        Self::Build,
        Self::ProductPackage,
    ];

    /// The `<name>` of `jewelry.<name>/<major>` and of `exchange/<name>/<major>/`.
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::PartKit => "part_kit",
            Self::MaterialLibrary => "material_library",
            Self::Build => "build",
            Self::ProductPackage => "product_package",
            Self::OptionGraph => "option_graph",
            Self::PieceRecipe => "piece_recipe",
        }
    }

    /// The `schema` value of a current file: `jewelry.<name>/1`.
    pub fn schema_id(self) -> String {
        format!("jewelry.{}/{CONTRACT_MAJOR}", self.wire_name())
    }

    /// The interface of a wire name.
    pub fn from_wire_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|i| i.wire_name() == name)
    }

    /// `true` when Regia owns the schema (it ships in `docs/formats/`).
    pub fn owned_by_regia(self) -> bool {
        Self::OWNED.contains(&self)
    }

    /// The registry name in `cargo xtask check-formats` and `regia upgrade-project`.
    pub fn format_name(self) -> &'static str {
        match self {
            Self::PartKit => "jewelry-part-kit",
            Self::MaterialLibrary => "jewelry-material-library",
            Self::Build => "jewelry-build",
            Self::ProductPackage => "jewelry-product-package",
            Self::OptionGraph => "jewelry-option-graph",
            Self::PieceRecipe => "jewelry-piece-recipe",
        }
    }

    /// The format version of a current file: the `<major>` of its `schema` field.
    pub fn format_version(self) -> u32 {
        CONTRACT_MAJOR
    }

    /// The schema Regia ships (the crate's `assets/<format_name>.schema.json`, byte-identical to
    /// `docs/formats/`; ADR-1485), `None` for the interfaces other tools own.
    pub fn schema_text(self) -> Option<&'static str> {
        Some(match self {
            Self::PartKit => include_str!("../assets/jewelry-part-kit.schema.json"),
            Self::MaterialLibrary => {
                include_str!("../assets/jewelry-material-library.schema.json")
            }
            Self::Build => include_str!("../assets/jewelry-build.schema.json"),
            Self::ProductPackage => {
                include_str!("../assets/jewelry-product-package.schema.json")
            }
            Self::OptionGraph | Self::PieceRecipe => return None,
        })
    }

    /// The compiled schema Regia ships, `None` for the interfaces other tools own (or a shipped
    /// schema that fails to compile, which the crate's tests rule out).
    ///
    /// The compiled schema is kept per interface: re-parsing the text and recompiling its
    /// patterns on every [`check`] call dominated repeated checks.
    #[cfg(feature = "validate")]
    pub fn schema(self) -> Option<Schema> {
        /// One slot per [`Interface`] variant (the declaration order).
        fn slot(interface: Interface) -> &'static std::sync::OnceLock<Option<Schema>> {
            const COUNT: usize = 7;
            static SLOTS: [std::sync::OnceLock<Option<Schema>>; COUNT] =
                [const { std::sync::OnceLock::new() }; COUNT];
            &SLOTS[interface as usize]
        }
        slot(self)
            .get_or_init(|| Schema::parse(self.schema_text()?).ok())
            .clone()
    }

    /// The format's check-formats fingerprint: the canonical (RFC 8785) text of the shipped
    /// schema, so a schema edit without a version bump fails `cargo xtask check-formats`.
    pub fn schema_fingerprint(self) -> String {
        self.schema_text()
            .and_then(|t| serde_json::from_str::<Value>(t).ok())
            .and_then(|v| jcs::canonical(&v))
            .unwrap_or_default()
    }

    /// `regia upgrade-project` probe: `Ok(Some(major))` when `bytes` is a JSON file of this
    /// interface, `Ok(None)` for any other file (other JSON, other interfaces).
    pub fn probe_file_version(self, bytes: &[u8]) -> Result<Option<u32>, String> {
        // Cheap reject first: `upgrade-project` probes every `.json` file of a project.
        let needle = format!("\"jewelry.{}/", self.wire_name());
        if !bytes.windows(needle.len()).any(|w| w == needle.as_bytes()) {
            return Ok(None);
        }
        let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
            return Ok(None);
        };
        let Some(id) = value.get("schema").and_then(Value::as_str) else {
            return Ok(None);
        };
        match parse_schema_id(id) {
            Some((name, major)) if name == self.wire_name() => Ok(Some(major)),
            _ => Ok(None),
        }
    }
}

/// Splits `jewelry.<name>/<major>`.
pub fn parse_schema_id(id: &str) -> Option<(&str, u32)> {
    let rest = id.strip_prefix("jewelry.")?;
    let (name, major) = rest.split_once('/')?;
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') {
        return None;
    }
    // Plain decimal: `u32::from_str` also takes `+1`, and `01` would be a second spelling of 1.
    if major.is_empty()
        || !major.bytes().all(|b| b.is_ascii_digit())
        || (major.len() > 1 && major.starts_with('0'))
    {
        return None;
    }
    Some((name, major.parse().ok()?))
}

/// One reason a contract file is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractError {
    /// The agent-protocol code: `F0002` (the file is wrong) or `F0003` (it names something that
    /// does not exist).
    pub code: &'static str,
    /// Stable lowercase reason (`contract_unknown_field`, ...), listed on the format pages.
    pub reason: &'static str,
    /// JSON pointer of the offending value (`""` for the whole file).
    pub path: String,
    /// What is wrong, for a person.
    pub message: String,
}

impl ContractError {
    /// An `F0002` refusal.
    pub fn invalid(reason: &'static str, path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: "F0002",
            reason,
            path: path.into(),
            message: message.into(),
        }
    }

    /// An `F0003` refusal (a reference to something absent).
    pub fn missing(reason: &'static str, path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: "F0003",
            reason,
            path: path.into(),
            message: message.into(),
        }
    }

    /// The error as the agent protocol and the CLI print it.
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "code": self.code,
            "reason": self.reason,
            "path": self.path,
            "message": self.message,
        })
    }
}

impl std::fmt::Display for ContractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let at = if self.path.is_empty() { "/" } else { &self.path };
        write!(f, "{} {} at {at}: {}", self.code, self.reason, self.message)
    }
}

/// Every reason [`check`] and [`check_bytes`] can answer with, for the format pages and tests.
pub const REASONS: &[(&str, &str, &str)] = &[
    (
        "contract_too_large",
        "F0002",
        "the file is over the byte limit, or a kit has too many facet planes",
    ),
    ("contract_not_json", "F0002", "the bytes are not UTF-8 JSON"),
    (
        "contract_no_schema_field",
        "F0002",
        "no `schema` string, or not `jewelry.<name>/<major>`",
    ),
    (
        "contract_unknown_interface",
        "F0002",
        "`schema` names an interface this engine does not know",
    ),
    (
        "contract_wrong_interface",
        "F0002",
        "the file is another interface than the one asked for",
    ),
    (
        "contract_unsupported_major",
        "F0002",
        "the major version is not one this engine reads",
    ),
    (
        "contract_no_schema",
        "F0003",
        "no schema is known for the interface (the kit builder owns it and none was given)",
    ),
    ("contract_missing_field", "F0002", "a required field is absent"),
    (
        "contract_unknown_field",
        "F0002",
        "a field the schema does not define (cost and client data are never allowed)",
    ),
    ("contract_wrong_type", "F0002", "a value of the wrong JSON type"),
    (
        "contract_bad_value",
        "F0002",
        "a value outside its enum or constant",
    ),
    ("contract_out_of_range", "F0002", "a number outside its bounds"),
    (
        "contract_bad_string",
        "F0002",
        "a string that breaks its pattern or length (keys, hashes, paths)",
    ),
    (
        "contract_bad_count",
        "F0002",
        "an array or object with too few or too many entries, or a repeated entry",
    ),
    (
        "contract_schema_mismatch",
        "F0002",
        "the value matches none (or several) of the allowed shapes",
    ),
    (
        "contract_too_deep",
        "F0002",
        "the document nests deeper than the validator evaluates, or the schema needs more evaluations than its budget",
    ),
    (
        "contract_duplicate_id",
        "F0002",
        "an id, name or key that must be unique appears twice",
    ),
    (
        "contract_duplicate_facet",
        "F0002",
        "two material entries claim the same option label",
    ),
    (
        "contract_unknown_reference",
        "F0003",
        "a reference to an id that the file does not define",
    ),
    (
        "contract_wrong_kind",
        "F0002",
        "a reference to an entry of the wrong kind (an anodize substrate that is not a conductor)",
    ),
    (
        "contract_bad_range",
        "F0002",
        "a min above its max, a default or step outside the range, an empty box",
    ),
    (
        "contract_bad_transform",
        "F0002",
        "a rotation quaternion or axis that is not unit length",
    ),
    (
        "contract_bad_lods",
        "F0002",
        "LOD levels not increasing, or screen sizes not decreasing",
    ),
    (
        "contract_bad_normal",
        "F0002",
        "a facet normal that is not unit length",
    ),
    (
        "contract_cut_no_facets",
        "F0002",
        "a cut with neither facets nor a mesh",
    ),
    (
        "contract_cut_unbounded",
        "F0002",
        "the facets of a convex cut do not close a solid",
    ),
    (
        "contract_cut_empty",
        "F0002",
        "the facets of a convex cut leave no solid",
    ),
    (
        "contract_cut_redundant_facet",
        "F0002",
        "a facet that touches the solid in no face (never decimate, never pad)",
    ),
    (
        "contract_cut_size_mismatch",
        "F0002",
        "the solid's length or width differs from `size_mm`",
    ),
    (
        "contract_cut_proportions_mismatch",
        "F0002",
        "the solid's crown height or pavilion depth differs from `proportions`",
    ),
    (
        "contract_bad_spectrum",
        "F0002",
        "spectral arrays of different lengths, or wavelengths not increasing",
    ),
    (
        "contract_hash_mismatch",
        "F0002",
        "`build_hash` or `package_hash` is not the hash of the fields it covers",
    ),
    (
        "contract_key_mismatch",
        "F0002",
        "a natural key that does not start with the file's house",
    ),
    (
        "contract_house_not_allowed",
        "F0002",
        "content of a house the tenant may not load",
    ),
    (
        "contract_inconsistent_resolution",
        "F0002",
        "a resolution whose fields disagree (orderable with no SKU, cart lines in manual mode, ...)",
    ),
    (
        "contract_package_incomplete",
        "F0002",
        "a package without exactly one recipe, options and materials file, or without a mesh",
    ),
    (
        "contract_package_size_mismatch",
        "F0002",
        "`sizes.total` is not the sum of the listed files",
    ),
    (
        "contract_package_file_missing",
        "F0003",
        "a file the manifest lists is absent",
    ),
    (
        "contract_package_file_mismatch",
        "F0002",
        "a listed file's size or BLAKE3 hash differs from the manifest",
    ),
    (
        "contract_package_has_cost",
        "F0002",
        "a JSON file of a package holds a cost, price or markup field",
    ),
];

/// Validates a contract file from bytes: size, JSON, the `schema` field, the interface's schema,
/// then the contract checks. `expected` refuses any other interface; `schema_override` validates
/// against a published schema instead of the shipped one (and is required for the interfaces the
/// kit builder owns).
pub fn check_bytes(
    bytes: &[u8],
    expected: Option<Interface>,
    schema_override: Option<&Schema>,
) -> Result<Interface, Vec<ContractError>> {
    if bytes.len() > MAX_FILE_BYTES {
        return Err(vec![ContractError::invalid(
            "contract_too_large",
            "",
            format!("{} bytes, at most {MAX_FILE_BYTES}", bytes.len()),
        )]);
    }
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|e| vec![ContractError::invalid("contract_not_json", "", e.to_string())])?;
    check(&value, expected, schema_override)
}

/// Validates a parsed contract document. On success returns its interface. Without the feature
/// `validate` the schema step is skipped: the envelope and the contract checks still run.
pub fn check(
    value: &Value,
    expected: Option<Interface>,
    schema_override: Option<&Schema>,
) -> Result<Interface, Vec<ContractError>> {
    let one = |e: ContractError| Err(vec![e]);
    let Some(id) = value.get("schema").and_then(Value::as_str) else {
        return one(ContractError::invalid(
            "contract_no_schema_field",
            "/schema",
            "a contract file is an object with a `schema` string",
        ));
    };
    let Some((name, major)) = parse_schema_id(id) else {
        return one(ContractError::invalid(
            "contract_no_schema_field",
            "/schema",
            format!("`{id}` is not `jewelry.<name>/<major>`"),
        ));
    };
    let Some(interface) = Interface::from_wire_name(name) else {
        return one(ContractError::invalid(
            "contract_unknown_interface",
            "/schema",
            format!("no interface `{name}`"),
        ));
    };
    if let Some(want) = expected
        && want != interface
    {
        return one(ContractError::invalid(
            "contract_wrong_interface",
            "/schema",
            format!("expected {}, found {id}", want.schema_id()),
        ));
    }
    if major != CONTRACT_MAJOR {
        let message = if major > CONTRACT_MAJOR {
            format!(
                "`{id}`: schema major {major} is a newer version than this engine reads (major {CONTRACT_MAJOR}); upgrade the engine"
            )
        } else {
            format!(
                "`{id}`: schema major {major} is an older version than this engine reads (major {CONTRACT_MAJOR}); upgrade the file"
            )
        };
        return one(ContractError::invalid(
            "contract_unsupported_major",
            "/schema",
            message,
        ));
    }
    let mut errors = schema_errors(value, id, interface, schema_override)?;
    if errors.is_empty() {
        errors = checks::run(interface, value);
    }
    if errors.is_empty() {
        Ok(interface)
    } else {
        Err(cap(errors))
    }
}

/// The schema step of [`check`].
#[cfg(feature = "validate")]
fn schema_errors(
    value: &Value,
    id: &str,
    interface: Interface,
    schema_override: Option<&Schema>,
) -> Result<Vec<ContractError>, Vec<ContractError>> {
    let shipped;
    let schema = match schema_override {
        Some(s) => s,
        None => match interface.schema() {
            Some(s) => {
                shipped = s;
                &shipped
            }
            None => {
                return Err(vec![ContractError::missing(
                    "contract_no_schema",
                    "/schema",
                    format!("{id} is published by the kit builder: pass its schema.json"),
                )]);
            }
        },
    };
    Ok(schema.validate(value).into_iter().map(from_violation).collect())
}

/// The schema step of [`check`] in a build without a validator: nothing to run.
#[cfg(not(feature = "validate"))]
fn schema_errors(
    _value: &Value,
    _id: &str,
    _interface: Interface,
    _schema_override: Option<&Schema>,
) -> Result<Vec<ContractError>, Vec<ContractError>> {
    Ok(Vec::new())
}

fn cap(mut errors: Vec<ContractError>) -> Vec<ContractError> {
    if errors.len() > MAX_ERRORS {
        let extra = errors.len() - MAX_ERRORS;
        errors.truncate(MAX_ERRORS);
        if let Some(last) = errors.last_mut() {
            last.message = format!("{} (and {extra} more)", last.message);
        }
    }
    errors
}

/// The contract reason of a schema keyword violation.
#[cfg(feature = "validate")]
fn from_violation(v: Violation) -> ContractError {
    let reason = match v.keyword {
        "required" | "dependentRequired" => "contract_missing_field",
        "additionalProperties" | "propertyNames" | "false" => "contract_unknown_field",
        "type" => "contract_wrong_type",
        "enum" | "const" | "not" => "contract_bad_value",
        "minimum" | "maximum" | "exclusiveMinimum" | "exclusiveMaximum" | "multipleOf" => {
            "contract_out_of_range"
        }
        "pattern" | "minLength" | "maxLength" => "contract_bad_string",
        "minItems" | "maxItems" | "uniqueItems" | "minProperties" | "maxProperties" | "contains"
        | "maxContains" => "contract_bad_count",
        "maxDepth" | "maxSteps" => "contract_too_deep",
        _ => "contract_schema_mismatch",
    };
    ContractError::invalid(reason, v.instance_path, v.message)
}

#[cfg(test)]
mod corr_low_tests {
    use serde_json::json;

    use super::*;

    /// Audit LOW (sprint-corr-configurator): `u32::from_str` takes a leading `+` and leading zeros,
    /// so `jewelry.part_kit/+1` and `/01` were read as major 1 (by `check` and by the
    /// `upgrade-project` probe).
    #[test]
    fn a_schema_major_is_plain_decimal_digits() {
        assert_eq!(parse_schema_id("jewelry.part_kit/1"), Some(("part_kit", 1)));
        assert_eq!(parse_schema_id("jewelry.part_kit/0"), Some(("part_kit", 0)));
        assert_eq!(parse_schema_id("jewelry.part_kit/12"), Some(("part_kit", 12)));
        for bad in [
            "jewelry.part_kit/+1",
            "jewelry.part_kit/01",
            "jewelry.part_kit/",
            "jewelry.part_kit/1 ",
            "jewelry.part_kit/-0",
        ] {
            assert_eq!(parse_schema_id(bad), None, "{bad}");
        }
        let probe = Interface::PartKit.probe_file_version(br#"{"schema": "jewelry.part_kit/+1"}"#);
        assert_eq!(probe, Ok(None));
    }

    /// Audit LOW (sprint-corr-configurator): an older major (0) was reported as "a newer version
    /// than this engine reads".
    #[test]
    fn an_older_major_is_not_called_newer() {
        let errs = match check(&json!({"schema": "jewelry.part_kit/0"}), None, None) {
            Err(errs) => errs,
            Ok(i) => panic!("major 0 accepted as {i:?}"),
        };
        assert_eq!(errs[0].reason, "contract_unsupported_major");
        assert!(!errs[0].message.contains("newer"), "{}", errs[0].message);
        assert!(errs[0].message.contains("older"), "{}", errs[0].message);
        let errs = match check(&json!({"schema": "jewelry.part_kit/2"}), None, None) {
            Err(errs) => errs,
            Ok(i) => panic!("major 2 accepted as {i:?}"),
        };
        assert!(errs[0].message.contains("newer"), "{}", errs[0].message);
    }
}
