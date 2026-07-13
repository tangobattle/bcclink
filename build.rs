extern crate embed_resource;

fn generate_rc(icon_path: Option<&str>) -> Result<String, Box<dyn std::error::Error>> {
    let major = std::env::var("CARGO_PKG_VERSION_MAJOR")?;
    let minor = std::env::var("CARGO_PKG_VERSION_MINOR")?;
    let patch = std::env::var("CARGO_PKG_VERSION_PATCH")?;
    // The ICON resource is what Explorer and the taskbar show for the exe;
    // the VERSIONINFO block is what the file Properties dialog shows.
    let icon = match icon_path {
        Some(path) => format!("1 ICON \"{path}\""),
        None => String::new(),
    };
    Ok(format!(
        r#"#include "winver.h"

{icon}

VS_VERSION_INFO VERSIONINFO
FILEVERSION    {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
BEGIN
BLOCK "StringFileInfo"
BEGIN
    BLOCK "040904b0"
    BEGIN
        VALUE "FileDescription", "Ring\0"
        VALUE "ProductName", "Ring\0"
        VALUE "ProductVersion", "{major}.{minor}.{patch}.0\0"
        VALUE "FileVersion", "{major}.{minor}.{patch}.0\0"
        VALUE "OriginalFilename", "ring.exe\0"
        VALUE "Info", "https://github.com/tangobattle/ring\0"
    END
END
BLOCK "VarFileInfo"
BEGIN
    VALUE "Translation", 0x0, 1200
END
END
"#
    ))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS")?;

    if target_os == "windows" {
        // Same scheme as tango's build.rs: render `resource.rc` into OUT_DIR
        // to keep the source tree clean, and reference the icon by absolute
        // path with forward slashes (RC string literals treat `\` as an
        // escape; both rc.exe and windres accept `/`). Unlike tango the
        // icon is checked in (assets/icon.ico, generated from
        // assets/logo.svg), but a missing icon still degrades to
        // VERSIONINFO-only rather than a broken build.
        let icon_file = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR")?)
            .join("assets")
            .join("icon.ico");
        let icon_path = icon_file
            .exists()
            .then(|| icon_file.to_string_lossy().replace('\\', "/"));
        let rc_path = std::path::Path::new(&std::env::var("OUT_DIR")?).join("resource.rc");
        std::fs::write(&rc_path, generate_rc(icon_path.as_deref())?)?;
        embed_resource::compile(&rc_path);
    }

    Ok(())
}
