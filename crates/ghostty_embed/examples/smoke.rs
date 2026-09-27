fn main() {
    #[cfg(target_os = "macos")]
    unsafe {
        let mut argv: [*mut std::os::raw::c_char; 1] = [std::ptr::null_mut()];
        let status = ghostty_embed::ghostty_init(0, argv.as_mut_ptr());
        let info = ghostty_embed::ghostty_info();
        let version = std::slice::from_raw_parts(info.version as *const u8, info.version_len);
        println!("init={status} version={}", String::from_utf8_lossy(version));
        let config = ghostty_embed::ghostty_config_new();
        ghostty_embed::ghostty_config_load_default_files(config);
        ghostty_embed::ghostty_config_finalize(config);
        println!(
            "diagnostics={}",
            ghostty_embed::ghostty_config_diagnostics_count(config)
        );
        ghostty_embed::ghostty_config_free(config);
    }
}
