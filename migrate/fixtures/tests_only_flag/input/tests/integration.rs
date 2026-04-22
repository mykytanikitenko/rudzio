use tests_only_flag::ident;

#[test]
fn integration_passes() {
    assert_eq!(ident("hi"), "hi");
}
