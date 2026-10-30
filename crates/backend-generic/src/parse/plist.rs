//! Parser for `diskutil info -plist` output. Pure string handling, so it compiles and
//! is unit-tested on every target, while the subprocess and `statfs` stay macOS-only.

/// `SolidState` of a `diskutil info -plist` document: `Some(true)` for solid state,
/// `Some(false)` for rotational media. `None` when the key is missing, is not a
/// boolean, or the device is a disk image (virtual media says nothing about hardware).
pub(crate) fn parse_solid_state(plist_xml: &str) -> Option<bool> {
    if plist_string(plist_xml, "BusProtocol") == Some("Disk Image") {
        return None;
    }
    let value = value_after_key(plist_xml, "SolidState")?.trim_start();
    if value.starts_with("<true/>") {
        Some(true)
    } else if value.starts_with("<false/>") {
        Some(false)
    } else {
        None
    }
}

/// Text that follows `<key>{key}</key>`, or `None` when the key is absent.
fn value_after_key<'a>(plist_xml: &'a str, key: &str) -> Option<&'a str> {
    let (_, rest) = plist_xml.split_once(format!("<key>{key}</key>").as_str())?;
    Some(rest)
}

/// Contents of `<key>{key}</key><string>VALUE</string>`, or `None` when the value is not a string.
fn plist_string<'a>(plist_xml: &'a str, key: &str) -> Option<&'a str> {
    let rest = value_after_key(plist_xml, key)?.trim_start();
    let (value, _) = rest.strip_prefix("<string>")?.split_once("</string>")?;
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::parse_solid_state;

    #[test]
    fn parse_solid_state_should_return_some_true_for_true_value() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>BusProtocol</key>
    <string>PCI-Express</string>
    <key>SolidState</key>
    <true/>
</dict>
</plist>
"#;
        let parsed = parse_solid_state(xml);
        assert_eq!(
            parsed,
            Some(true),
            "SolidState <true/> should parse to Some(true), got {parsed:?}"
        );
    }

    #[test]
    fn parse_solid_state_should_return_some_false_for_false_value() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>BusProtocol</key>
    <string>USB</string>
    <key>SolidState</key>
    <false/>
</dict>
</plist>
"#;
        let parsed = parse_solid_state(xml);
        assert_eq!(
            parsed,
            Some(false),
            "SolidState <false/> should parse to Some(false), got {parsed:?}"
        );
    }

    #[test]
    fn parse_solid_state_should_return_none_when_key_missing() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>BusProtocol</key>
    <string>PCI-Express</string>
    <key>Internal</key>
    <true/>
</dict>
</plist>
"#;
        let parsed = parse_solid_state(xml);
        assert_eq!(
            parsed, None,
            "a document without SolidState should give None, got {parsed:?}"
        );
    }

    #[test]
    fn parse_solid_state_should_return_none_when_value_is_not_boolean() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>SolidState</key>
    <string>yes</string>
</dict>
</plist>
"#;
        let parsed = parse_solid_state(xml);
        assert_eq!(
            parsed, None,
            "a non-boolean SolidState value should give None, got {parsed:?}"
        );
    }

    #[test]
    fn parse_solid_state_should_tolerate_whitespace_between_key_and_value() {
        let xml = "<dict>\n\t<key>SolidState</key>\n\t\n\t\t<false/>\n</dict>";
        let parsed = parse_solid_state(xml);
        assert_eq!(
            parsed,
            Some(false),
            "whitespace between SolidState key and value should be skipped, got {parsed:?}"
        );
    }

    #[test]
    fn parse_solid_state_should_return_none_for_disk_image_bus_protocol() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>BusProtocol</key>
    <string>Disk Image</string>
    <key>SolidState</key>
    <true/>
</dict>
</plist>
"#;
        let parsed = parse_solid_state(xml);
        assert_eq!(
            parsed, None,
            "a Disk Image bus protocol should give None even with SolidState set, got {parsed:?}"
        );
    }
}
