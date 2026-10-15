use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    match env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("macos") => build_swift(),
        Ok("windows") => windows_resources(),
        _ => {}
    }
}

/// Icon, version details and manifest for Hop.exe.
fn windows_resources() {
    let dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("assets");
    for f in ["hop.ico", "hop.manifest"] {
        println!("cargo:rerun-if-changed=assets/{f}");
    }
    let v = |k: &str| env::var(k).unwrap_or_else(|_| "0".into());
    let (major, minor, patch) = (v("CARGO_PKG_VERSION_MAJOR"), v("CARGO_PKG_VERSION_MINOR"), v("CARGO_PKG_VERSION_PATCH"));
    let path = |f: &str| dir.join(f).display().to_string().replace('\\', "/");
    let rc = format!(
        r#"#include <winver.h>
1 ICON "{icon}"
1 24 "{manifest}"
1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEOS VOS_NT_WINDOWS32
FILETYPE VFT_APP
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "linpatr"
      VALUE "FileDescription", "Hop remote desktop"
      VALUE "FileVersion", "{major}.{minor}.{patch}"
      VALUE "InternalName", "hop"
      VALUE "LegalCopyright", "Open source, https://github.com/linpatr/hop"
      VALUE "OriginalFilename", "Hop.exe"
      VALUE "ProductName", "Hop"
      VALUE "ProductVersion", "{major}.{minor}.{patch}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        icon = path("hop.ico"),
        manifest = path("hop.manifest"),
    );
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("hop.rc");
    std::fs::write(&out, rc).unwrap();
    let _ = embed_resource::compile(&out, embed_resource::NONE).manifest_optional();
}

fn xcrun(args: &[&str]) -> String {
    let out = Command::new("xcrun").args(args).output().expect("xcrun");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn build_swift() {
    let src = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("mac/HopMac.swift");
    println!("cargo:rerun-if-changed={}", src.display());
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let arch = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "arm64",
        _ => "x86_64",
    };
    let target = format!("{arch}-apple-macos13.0");
    let lib = out.join("libhopmac.a");
    let status = Command::new("xcrun")
        .current_dir(&out)
        .args(["swiftc", "-parse-as-library", "-emit-library", "-static", "-module-name", "HopMac"])
        .args(["-swift-version", "5", "-O", "-target", &target])
        .arg(&src)
        .arg("-o")
        .arg(&lib)
        .status()
        .expect("run swiftc");
    assert!(status.success(), "swiftc failed");

    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=hopmac");

    let sdk = xcrun(&["--show-sdk-path"]);
    println!("cargo:rustc-link-search=native={sdk}/usr/lib/swift");
    let swiftc = xcrun(&["--find", "swiftc"]);
    let toolchain_lib = PathBuf::from(swiftc).parent().unwrap().parent().unwrap().join("lib/swift/macosx");
    println!("cargo:rustc-link-search=native={}", toolchain_lib.display());
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    for fw in [
        "ScreenCaptureKit",
        "VideoToolbox",
        "CoreMedia",
        "CoreVideo",
        "CoreGraphics",
        "AppKit",
        "ApplicationServices",
        "IOKit",
        "QuartzCore",
        "Foundation",
    ] {
        println!("cargo:rustc-link-lib=framework={fw}");
    }
}
