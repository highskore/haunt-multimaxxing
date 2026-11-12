#[test]
fn manifest_entry_is_the_built_wasm() {
    let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/haunt-plugin.toml")).unwrap();
    let entry = format!("entry = \"target/wasm32-wasip2/release/{}.wasm\"", env!("CARGO_PKG_NAME").replace('-', "_"));
    assert!(manifest.contains(&entry), "haunt-plugin.toml should have {entry}");
}

#[test]
fn every_setting_has_a_default() {
    let m: toml::Table = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/haunt-plugin.toml")).unwrap().parse().unwrap();
    for (k, v) in m["settings"].as_table().unwrap() {
        assert!(v.get("default").is_some(), "setting {k} has no default");
    }
    assert_eq!(m["capabilities"].get("pty_write"), None, "never answers prompts");
}
