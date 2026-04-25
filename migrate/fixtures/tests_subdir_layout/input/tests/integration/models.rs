use tests_subdir_layout::shout;

#[test]
fn shouts_loud() {
    assert_eq!(shout("hi"), "HI");
}

#[test]
fn shouts_empty() {
    assert_eq!(shout(""), "");
}
