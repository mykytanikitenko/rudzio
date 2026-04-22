use integration_file::greet;

fn expected_prefix() -> &'static str {
    "hello, "
}

#[tokio::test]
async fn greets_alice() {
    let out = greet("alice");
    assert!(out.starts_with(expected_prefix()));
    assert!(out.contains("alice"));
}

#[test]
fn greets_bob_synchronously() {
    assert_eq!(greet("bob"), "hello, bob");
}
