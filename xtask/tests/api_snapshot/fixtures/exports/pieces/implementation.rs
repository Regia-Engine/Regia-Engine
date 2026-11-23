pub struct Original;
pub struct Multi;
pub fn helper() {}
impl Original {
    #[cfg(feature = "extra")]
    pub fn new() -> Self {
        Self
    }
}
impl Original {
    #[deprecated(since = "0.9.0", note = "use new")]
    pub fn legacy() {}
}
