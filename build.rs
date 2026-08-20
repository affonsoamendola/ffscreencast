fn main() {
    #[cfg(target_os = "windows")]
    {
        println!("cargo:rerun-if-changed=app.manifest");
        winresource::WindowsResource::new()
            .set_manifest_file("app.manifest")
            .compile()
            .expect("failed to embed manifest");
    }
}
