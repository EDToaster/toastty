//! End-to-end config loading via the public surface.

use std::path::PathBuf;

use toastty_config::{
    Color, Config, ConfigError, ConfigSource, CursorShape, ExtendBackground, ExtendBackgroundWhen,
    ExtendCondition, GridAlign, PaddingConfig,
};

#[test]
fn fully_populated_fixture_parses_via_load_from_path() {
    let path = fixture("full.toml");
    let cfg = Config::load_from_path(&path).expect("load full fixture");
    assert_eq!(cfg.font.family, "Fira Mono");
    assert_eq!(cfg.cursor.shape, CursorShape::Block);
    assert_eq!(cfg.scrollback.lines, 10_000);
    assert_eq!(cfg.theme.palette.len(), 16);

    // Spot-check a couple of palette entries against the published hex.
    assert_eq!(cfg.theme.palette[0], Color::from_hex("#000000").unwrap());
    assert_eq!(cfg.theme.palette[15], Color::from_hex("#ffffff").unwrap());

    // Window padding + extend_background + grid_align from the fixture.
    assert_eq!(
        cfg.window.extend_background_when,
        ExtendBackgroundWhen::AltScreen
    );
    assert_eq!(
        cfg.window.extend_background,
        ExtendBackground {
            horizontal: ExtendCondition::SolidLine,
            vertical: ExtendCondition::Never,
        }
    );
    assert_eq!(cfg.window.grid_align, GridAlign::Centered);
    assert_eq!(
        cfg.window.padding,
        PaddingConfig {
            top: 8,
            right: 8,
            bottom: 8,
            left: 8,
        }
    );
}

#[test]
fn write_then_read_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.toml");

    let orig = Config::defaults();
    let s = toml::to_string(&orig).unwrap();
    std::fs::write(&path, &s).unwrap();

    let loaded = Config::load_from_path(&path).unwrap();
    assert_eq!(loaded, orig);
}

#[test]
fn malformed_toml_returns_typed_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.toml");
    std::fs::write(&path, "[font\n").unwrap();
    let err = Config::load_from_path(&path).unwrap_err();
    assert!(matches!(err, ConfigError::Toml(_)));
}

#[test]
fn nonexistent_file_returns_io_error_with_path() {
    let path = PathBuf::from("/this/path/does/not/exist.toml");
    let err = Config::load_from_path(&path).unwrap_err();
    match err {
        ConfigError::Io(p, _) => assert_eq!(p, path),
        other => panic!("wrong error: {other:?}"),
    }
}

#[test]
fn load_default_works_in_clean_env() {
    // Simulate "no config file" by checking the resolver explicitly and
    // ensuring the fallback path returns Defaults.
    use toastty_config::test_support::resolve_with_env;
    let dir = tempfile::tempdir().unwrap();

    let resolved = resolve_with_env(Some(dir.path()), None).unwrap();
    assert!(!resolved.exists());
    let (cfg, src) = match Config::load_from_path(&resolved) {
        Ok(c) => (c, ConfigSource::File(resolved)),
        Err(_) => (Config::defaults(), ConfigSource::Defaults),
    };
    assert_eq!(cfg, Config::defaults());
    assert_eq!(src, ConfigSource::Defaults);
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}
