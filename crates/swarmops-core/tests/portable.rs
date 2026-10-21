//! The portable-name checks: which names every platform can hold (storage-core design, §2.1 P8).

use swarmops_core::portable::{Unportable, check_component, is_windows_device_name};

/// Windows takes a name for a device when what comes before its first `.` (or `:`), trailing
/// spaces aside, is `con`, `prn`, `aux`, `nul`, `com1` to `com9` or `lpt1` to `lpt9`, in any
/// ASCII case: `con.txt` and `NUL.tar.gz` are the devices too. A name that only starts or ends
/// like one is not.
#[test]
fn device_names_are_found_in_any_case_and_with_an_extension() {
    let devices = [
        "con", "CON", "Con", "prn", "PRN", "aux", "nul", "Nul", "com1", "COM5", "com9", "lpt1",
        "LPT7", "lpt9",
    ];
    for device in devices {
        assert!(is_windows_device_name(device), "{device:?}");
        assert_eq!(
            check_component(device),
            Err(Unportable::DeviceName),
            "{device:?}"
        );
    }
    for name in [
        "con.txt",
        "NUL.tar.gz",
        "Lpt9.md",
        "con.",
        "con ",
        "con .txt",
        "aux:x",
    ] {
        assert!(is_windows_device_name(name), "{name:?}");
    }

    let names = [
        "", "co", "conx", "icon", "console", "nul_", "com", "com0", "com10", "lpt", "lpt0",
        "lpt10", "x.con", ".con", " con", "c0n",
    ];
    for name in names {
        assert!(!is_windows_device_name(name), "{name:?}");
    }
    assert_eq!(check_component("console"), Ok(()));
}

/// A name that ends in a dot or a space is not one Windows holds: its path handling strips them,
/// so `x.` would be the file `x`. A dot or a space elsewhere is fine, and a device name with a
/// dot at its end is a device name first.
#[test]
fn trailing_dots_and_spaces_are_unportable() {
    for name in ["x.", "x..", "x ", "x. ", "v1.2.", ".", ".."] {
        assert_eq!(
            check_component(name),
            Err(Unportable::TrailingDotOrSpace),
            "{name:?}"
        );
    }
    for name in ["x", "x.y", ".x", "v1.2", "a b", "a-b_c.d"] {
        assert_eq!(check_component(name), Ok(()), "{name:?}");
    }
    assert_eq!(check_component("con."), Err(Unportable::DeviceName));
}

/// The characters Windows refuses in a file name, `\ / : * ? " < > |` and the control characters
/// (U+0000 to U+001F), are named, the first one first; any other character is fine. Each reason
/// says what it is.
#[test]
fn characters_windows_refuses_are_named() {
    let refused = [
        '\\', '/', ':', '*', '?', '"', '<', '>', '|', '\0', '\u{1}', '\t', '\u{1f}',
    ];
    for c in refused {
        let name = format!("a{c}b");
        assert_eq!(
            check_component(&name),
            Err(Unportable::Character(c)),
            "{name:?}"
        );
    }
    assert_eq!(check_component("a|b:c."), Err(Unportable::Character('|')));
    assert_eq!(check_component("caf\u{e9} \u{2014} \u{1f600}"), Ok(()));

    assert_eq!(Unportable::DeviceName.to_string(), "a Windows device name");
    assert_eq!(
        Unportable::TrailingDotOrSpace.to_string(),
        "it ends in a dot or a space"
    );
    assert_eq!(
        Unportable::Character(':').to_string(),
        "it holds the character ':'"
    );
}
