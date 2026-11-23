include!("support.rs");
use regia_macros::BtTask as RegisterTask;
use task::{BtContext, BtStatus, BtTask, BtTaskRegistration};

#[expect(
    non_camel_case_types,
    reason = "this fixture exercises raw identifier derives"
)]
#[derive(Default, RegisterTask)]
struct r#wait<T: Default + Clone + Send + Sync + 'static>(T);
// The real `BtTask` requires `Send + Sync + 'static` and a `tick` implementation.
impl<T: Default + Clone + Send + Sync + 'static> BtTask for r#wait<T> {
    fn tick(&mut self, _: &mut BtContext<'_>) -> BtStatus {
        BtStatus::Success
    }
}

#[test]
fn generated_task_impl_type_checks() {
    let _: Box<dyn BtTask> = <r#wait<u8> as BtTaskRegistration>::construct();
}
