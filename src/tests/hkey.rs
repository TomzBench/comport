//! hkey
use regex::Regex;

#[test]
fn comport_test_hkey_parse() {
    let re = Regex::new("(vid_|pid_).{4}").unwrap();
    let caps: Vec<_> = re
        .find_iter(r#"\\?\usb#vid_2fe3&pid_0002&mi_00#7&123456"#)
        .map(|m| m.as_str()[4..].to_string())
        .collect();
    assert_eq!("2fe3", caps[0]);
    assert_eq!("0002", caps[1]);
}
