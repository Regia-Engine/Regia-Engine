// Stand-ins for the runtime traits the derives emit code against.
//
// These mirror the *real* trait surfaces exactly where it matters for compilation: supertraits,
// required methods and provided defaults (`crates/regia_framework/src/script.rs`,
// `crates/regia_ai/src/task.rs`, `crates/regia_net/src/codec.rs`,
// `crates/regia_net/src/message.rs`). A fixture that type-checks against these therefore
// type-checks against the real traits; bodies are stubs, because the fixtures never run the
// generated implementations, only compile and link them.
// (Plain `//`, not `//!`: this file is `include!`d in the middle of each fixture.)

extern crate self as regia_ai;
extern crate self as regia_framework;
extern crate self as regia_net;

pub mod script {
    /// Stand-in for `regia_framework::script::Script`: the real trait is
    /// `Any + Send + Sync + 'static` with all-default methods, so an empty impl block
    /// type-checks identically.
    pub trait Script: std::any::Any + Send + Sync + 'static {}
    /// Stand-in for the re-exported `bevy_reflect::PartialReflect`: the derive's reflect
    /// accessor needs the script type to implement it, as the real one does.
    pub trait PartialReflect {}
    /// Stand-in for `regia_framework::script::ReflectScript`, including its
    /// `Send + Sync + 'static` supertraits and the defaulted reflect accessors.
    pub trait ReflectScript: Send + Sync + 'static {
        fn construct() -> Box<dyn Script>
        where
            Self: Sized;
        fn order() -> i32
        where
            Self: Sized;
        fn script_type_path() -> &'static str
        where
            Self: Sized;
        fn reflect_script(_script: &dyn Script) -> Option<&dyn PartialReflect>
        where
            Self: Sized,
        {
            None
        }
        fn reflect_script_mut(_script: &mut dyn Script) -> Option<&mut dyn PartialReflect>
        where
            Self: Sized,
        {
            None
        }
    }
    /// Stand-in for `regia_framework::script::downcast_reflect`, with the real bounds.
    pub fn downcast_reflect<T: Script + PartialReflect>(_script: &dyn Script) -> Option<&dyn PartialReflect> {
        None
    }
    /// Stand-in for `regia_framework::script::downcast_reflect_mut`, with the real bounds.
    pub fn downcast_reflect_mut<T: Script + PartialReflect>(
        _script: &mut dyn Script,
    ) -> Option<&mut dyn PartialReflect> {
        None
    }
}
pub mod task {
    use std::marker::PhantomData;

    /// Stand-in for `regia_ai::task::BtStatus`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum BtStatus {
        Success,
        Failure,
        Running,
    }
    /// Stand-in for `regia_ai::task::BtContext`. The real context carries the world, the agent
    /// entity and the tick delta; the generated registration code never touches it, so only the
    /// lifetime is modelled.
    pub struct BtContext<'a>(PhantomData<&'a ()>);
    /// Stand-in for `regia_ai::task::BtTask`: the real trait is `Send + Sync + 'static` with a
    /// required `tick` and a defaulted `reset`.
    pub trait BtTask: Send + Sync + 'static {
        fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus;
        fn reset(&mut self, _ctx: &mut BtContext<'_>) {}
    }
    pub trait BtTaskRegistration {
        fn construct() -> Box<dyn BtTask>
        where
            Self: Sized;
        fn task_type_path() -> &'static str
        where
            Self: Sized;
    }
}
pub mod bits {
    pub struct BitWriter;
    impl BitWriter {
        pub fn write_bool(&mut self, _: bool) {}
        pub fn write_varint(&mut self, _: u64) {}
    }
    pub struct BitReader;
    impl BitReader {
        pub fn read_bool(&mut self) -> Result<bool, DecodeError> {
            Ok(false)
        }
        pub fn read_varint(&mut self) -> Result<u64, DecodeError> {
            Ok(0)
        }
    }
    #[derive(Debug)]
    pub enum DecodeError {
        Invalid(&'static str),
    }
}
pub mod math {
    /// Stand-in for `regia_core::math::Vec3` (the real type is glam's `Vec3`; only the identity
    /// of the type parameter matters for the codec impls below).
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub struct Vec3(pub f32, pub f32, pub f32);
    /// Stand-in for `regia_core::math::Quat`.
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub struct Quat(pub f32, pub f32, pub f32, pub f32);
}
pub mod codec {
    use crate::bits::{BitReader, BitWriter, DecodeError};
    /// Stand-in for `regia_net::codec::CodecContext`. The real context carries entity/asset
    /// mapping state and is borrowed for one encode or decode; the fixtures only need the
    /// lifetime.
    pub struct CodecContext<'a>(core::marker::PhantomData<&'a ()>);
    pub struct LayoutHasher;
    impl LayoutHasher {
        pub fn write_str(&mut self, _: &str) {}
    }
    pub struct Plain;
    pub struct MaxLen(pub usize);
    pub struct Quantize {
        pub min: f32,
        pub max: f32,
        pub bits: u32,
    }
    pub struct Angle {
        pub bits: u32,
    }
    pub struct UnitVec {
        pub bits: u32,
    }
    pub struct Rotation {
        pub bits: u32,
    }
    /// Stand-in for `regia_net::codec::ReplicateCodec`: `encode`/`decode` are required, and
    /// `wire_eq`/`encode_delta`/`decode_delta`/`layout` carry the real default bodies, so a
    /// derive that relies on the defaults (the generated enum codec omits the delta methods)
    /// type-checks exactly as it does against the real trait.
    pub trait ReplicateCodec: Sized + Clone {
        fn encode(&self, _: &mut BitWriter, _: &CodecContext<'_>);
        fn decode(_: &mut BitReader, _: &CodecContext<'_>) -> Result<Self, DecodeError>
        where
            Self: Sized;
        fn wire_eq(&self, other: &Self, cx: &CodecContext<'_>) -> bool {
            let _ = other;
            let _ = cx;
            true
        }
        fn encode_delta(&self, base: &Self, w: &mut BitWriter, cx: &CodecContext<'_>) {
            let _ = base;
            self.encode(w, cx);
        }
        fn decode_delta(_base: &Self, r: &mut BitReader, cx: &CodecContext<'_>) -> Result<Self, DecodeError>
        where
            Self: Sized,
        {
            Self::decode(r, cx)
        }
        fn layout(_: &mut LayoutHasher) {}
        fn keep_local(&mut self, _: &Self) {}
    }
    impl ReplicateCodec for u8 {
        fn encode(&self, _: &mut BitWriter, _: &CodecContext<'_>) {}
        fn decode(_: &mut BitReader, _: &CodecContext<'_>) -> Result<Self, DecodeError> {
            Ok(0)
        }
    }
    impl ReplicateCodec for f32 {
        fn encode(&self, _: &mut BitWriter, _: &CodecContext<'_>) {}
        fn decode(_: &mut BitReader, _: &CodecContext<'_>) -> Result<Self, DecodeError> {
            Ok(0.0)
        }
    }
    impl ReplicateCodec for String {
        fn encode(&self, _: &mut BitWriter, _: &CodecContext<'_>) {}
        fn decode(_: &mut BitReader, _: &CodecContext<'_>) -> Result<Self, DecodeError> {
            Ok(String::new())
        }
    }
    impl ReplicateCodec for crate::math::Vec3 {
        fn encode(&self, _: &mut BitWriter, _: &CodecContext<'_>) {}
        fn decode(_: &mut BitReader, _: &CodecContext<'_>) -> Result<Self, DecodeError> {
            Ok(crate::math::Vec3(0.0, 0.0, 0.0))
        }
    }
    impl ReplicateCodec for crate::math::Quat {
        fn encode(&self, _: &mut BitWriter, _: &CodecContext<'_>) {}
        fn decode(_: &mut BitReader, _: &CodecContext<'_>) -> Result<Self, DecodeError> {
            Ok(crate::math::Quat(0.0, 0.0, 0.0, 1.0))
        }
    }
    /// Stand-in for `regia_net::codec::FieldCodec`. The real impls are per codec *and value
    /// type* — `Plain` for any `T: ReplicateCodec`, `Quantize`/`Angle` for `f32`, `UnitVec` for
    /// `Vec3`, `Rotation` for `Quat`, `MaxLen` for `String` and for `Vec<T>` — so a fixture can
    /// never prove a usage the real impls reject (e.g. `Quantize` on a `u8` field).
    pub trait FieldCodec<T> {
        fn encode(&self, _: &T, _: &mut BitWriter, _: &CodecContext<'_>);
        fn decode(&self, _: &mut BitReader, _: &CodecContext<'_>) -> Result<T, DecodeError>
        where
            Self: Sized;
        fn eq(&self, _: &T, _: &T, _: &CodecContext<'_>) -> bool;
        fn layout(&self, _: &mut LayoutHasher);
    }
    impl<T: ReplicateCodec> FieldCodec<T> for Plain {
        fn encode(&self, _: &T, _: &mut BitWriter, _: &CodecContext<'_>) {}
        fn decode(&self, _: &mut BitReader, _: &CodecContext<'_>) -> Result<T, DecodeError> {
            Err(DecodeError::Invalid("fixture"))
        }
        fn eq(&self, _: &T, _: &T, _: &CodecContext<'_>) -> bool {
            true
        }
        fn layout(&self, _: &mut LayoutHasher) {}
    }
    macro_rules! stub_codec_impl {
        ($codec:ty, $value:ty) => {
            impl FieldCodec<$value> for $codec {
                fn encode(&self, _: &$value, _: &mut BitWriter, _: &CodecContext<'_>) {}
                fn decode(&self, _: &mut BitReader, _: &CodecContext<'_>) -> Result<$value, DecodeError> {
                    Err(DecodeError::Invalid("fixture"))
                }
                fn eq(&self, _: &$value, _: &$value, _: &CodecContext<'_>) -> bool {
                    true
                }
                fn layout(&self, _: &mut LayoutHasher) {}
            }
        };
    }
    stub_codec_impl!(Quantize, f32);
    stub_codec_impl!(Angle, f32);
    stub_codec_impl!(UnitVec, crate::math::Vec3);
    stub_codec_impl!(Rotation, crate::math::Quat);
    stub_codec_impl!(MaxLen, String);
    impl<T: ReplicateCodec> FieldCodec<Vec<T>> for MaxLen {
        fn encode(&self, _: &Vec<T>, _: &mut BitWriter, _: &CodecContext<'_>) {}
        fn decode(&self, _: &mut BitReader, _: &CodecContext<'_>) -> Result<Vec<T>, DecodeError> {
            Err(DecodeError::Invalid("fixture"))
        }
        fn eq(&self, _: &Vec<T>, _: &Vec<T>, _: &CodecContext<'_>) -> bool {
            true
        }
        fn layout(&self, _: &mut LayoutHasher) {}
    }
}
pub mod message {
    /// Stand-in for `regia_net::message::Direction`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Direction {
        ToServer,
        ToOwner,
        Broadcast,
    }
    /// Stand-in for `regia_net::message::Channel`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Channel {
        ReliableOrdered,
        ReliableUnordered,
        Unreliable,
        UnreliableSequenced,
    }
    /// Stand-in for `regia_net::message::ValidateContext`; only the lifetime is modelled.
    pub struct ValidateContext<'a>(core::marker::PhantomData<&'a ()>);
    /// Stand-in for `regia_net::message::Verdict`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Verdict {
        Accept,
        Reject,
    }
    pub trait NetMessage: crate::codec::ReplicateCodec + Send + Sync + 'static {
        const DIRECTION: Direction;
        const CHANNEL: Channel = Channel::ReliableOrdered;
        const RATE_LIMIT: Option<crate::settings::RateLimit> = None;
        const ALWAYS_ADMIT: bool = false;
        const ALIGN_TO_VIEW: bool = false;
        const SKIP_PREDICTING_OWNER: bool = false;
        const VERSION: u32 = 1;
        fn message_id() -> &'static str {
            std::any::type_name::<Self>()
        }
        fn validate(&self, _: &ValidateContext<'_>) -> Verdict {
            Verdict::Accept
        }
    }
    pub trait ToServerMessage: NetMessage {}
    pub trait ToOwnerMessage: NetMessage {}
    pub trait BroadcastMessage: NetMessage {}
}
pub mod settings {
    /// Stand-in for `regia_net::settings::RateLimit`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct RateLimit {
        pub count: u32,
        pub per_ticks: u32,
    }
}
