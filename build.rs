// Embeds the application icon into the Windows executables. No effect elsewhere.
// The build script is compiled for the host, so the dependency and this code are
// both gated on the host being Windows, which is where the installer is built.
fn main() {
    println!("cargo:rerun-if-changed=assets/PoolRouter.ico");
    #[cfg(windows)]
    {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/PoolRouter.ico");
        resource.set("ProductName", "Pool Router");
        resource.set("FileDescription", "Pool Router");
        if let Err(error) = resource.compile() {
            println!("cargo:warning=icon resource not embedded: {error}");
        }
    }
}
