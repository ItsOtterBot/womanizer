//! INFRA-05 integration test: the reusable harness shuttles dummy data through all nine
//! named primitives end-to-end (Success Criterion #5).

/// Calls the reusable `run_smoke_test()` harness (D-12) and fails the test if any primitive
/// does not shuttle its dummy data successfully.
#[test]
fn primitives_shuttle_end_to_end() {
    womanizer_core::smoke::run_smoke_test().expect("smoke harness failed to shuttle dummy data");
}
