// Windows-only build step: embeds the exe icon and version resource
// (ProductName, FileDescription, FileVersion/ProductVersion) via the
// `winresource` crate.
//
// gpui (vendor/zui/crates/gpui/build.rs, feature "windows-manifest") already
// embeds its own RT_MANIFEST resource into every binary that links it,
// zeron included. `winresource::WindowsResource` only writes an RT_MANIFEST
// resource when `set_manifest`/`set_manifest_file` is called -- left unset
// (the default), this script emits only RT_VERSION and RT_ICON/RT_GROUP_ICON
// resources, which are a different resource type than RT_MANIFEST, so there
// is no duplicate-resource link error either way. Do not call
// `set_manifest`/`set_manifest_file` here; gpui owns the manifest.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let version = env!("CARGO_PKG_VERSION");
    let mut res = winresource::WindowsResource::new();
    res.set_icon("../../dist/zeron.ico")
        .set("ProductName", "Zeron")
        .set("FileDescription", "Zeron: control plane for coding agents")
        .set("FileVersion", version)
        .set("ProductVersion", version);
    if let Err(err) = res.compile() {
        panic!("winresource failed to compile the Windows resource: {err}");
    }
}
