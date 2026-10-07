fn main() {
    #[cfg(windows)]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon("app_icon.ico");
        res.set("FileDescription", "Apple TV Remote");
        res.set("ProductName", "Apple TV Remote");
        res.set("OriginalFilename", "AppleTVRemote-windows-x86_64.exe");
        if let Err(e) = res.compile() {
            eprintln!("Warning: failed to compile windows resources: {e}");
        }
    }
}
