//! A shared impl applies to every conditional definition, including reexported owners.

#[cfg(feature = "extra")]
pub trait Marker {}
#[cfg(not(feature = "extra"))]
pub trait Marker {}

mod implementation {
    use crate::Marker;

    #[cfg(feature = "extra")]
    pub struct Conditional;
    #[cfg(not(feature = "extra"))]
    pub struct Conditional;

    impl Conditional {
        pub fn shared() {}
    }
    impl Marker for Conditional {}
}

pub use implementation::Conditional as ExportedOwner;
impl Marker for u8 {}
