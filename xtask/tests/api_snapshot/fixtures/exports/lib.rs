//! Aliases, globs, cycles, conditional definitions and explicit module paths.

#[path = "pieces/implementation.rs"]
mod implementation;
mod relay {
    #[cfg(feature = "extra")]
    pub use crate::implementation::Original as Renamed;
}
#[cfg(any(feature = "extra"))]
pub use relay::Renamed as Exported;

#[cfg(feature = "extra")]
#[path = "pieces/conditional.rs"]
mod conditional;
#[cfg(any(feature = "extra"))]
pub use conditional::ThroughModule;

mod facade {
    #[cfg(feature = "extra")]
    pub use crate::implementation::*;
}
pub use facade::*;

#[path = "pieces/globbed.rs"]
mod globbed;
pub use globbed::*;

pub mod inline {
    #[path = "deep.rs"]
    pub mod deep;
}
mod cycle_a {
    pub use crate::cycle_b::*;
    pub struct CycleA;
}
mod cycle_b {
    pub use crate::cycle_a::*;
    pub struct CycleB;
}
pub use cycle_a::*;

#[cfg(feature = "extra")]
pub use implementation::Multi as Same;
#[cfg(not(feature = "extra"))]
pub use implementation::Multi as Same;

#[cfg(feature = "extra")]
mod conditional_relay {
    pub struct ThroughGlobModule;
}
#[cfg(any(feature = "extra"))]
pub use conditional_relay::*;
