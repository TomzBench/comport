use crate::wchar::from_wide;

#[test]
fn comport_test_wchar_arr() {
    // UTF-16 encoding for "Unicode\0"
    let s: &[u16] = &[
        0x0055, 0x006E, 0x0069, 0x0063, 0x006F, 0x0064, 0x0065, 0x0000,
    ];
    let p = &(&s[0] as *const u16) as *const *const u16;
    let term = unsafe { from_wide(*p) };
    assert_eq!("Unicode", term);
}

#[test]
fn comport_test_wchar() {
    let s: &[u8] = b"\x55\x00\x6E\x00\x69\x00\x63\x00\x6f\x00\x64\x00\x65\x00\x00";
    let term = unsafe { from_wide(s.as_ptr() as *const _) };
    assert_eq!("Unicode", term);
}
