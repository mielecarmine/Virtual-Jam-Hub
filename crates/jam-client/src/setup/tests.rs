use super::*;

#[test]
fn catalog_checks_input_and_output_buffers_with_directional_selectors() {
    let configs = [1, 2, 4].map(|channels| {
        cpal::SupportedStreamConfigRange::new(
            channels,
            cpal::SampleRate(audio::SAMPLE_RATE),
            cpal::SampleRate(audio::SAMPLE_RATE),
            cpal::SupportedBufferSize::Range { min: 128, max: 256 },
            cpal::SampleFormat::F32,
        )
    });
    for direction in [Direction::Input, Direction::Output] {
        assert_eq!(info("ASIO", &configs, direction).buffers, [128, 256]);
        assert_eq!(info("Mono", &configs[..1], direction).buffers, [128, 256]);
        assert!(info("Unavailable", &[], direction).buffers.is_empty());
    }
}

pub(crate) fn temporary_path(label: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    std::env::temp_dir()
        .join(format!(
            "jam-13-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
        .join("client.toml")
}

#[test]
fn preferences_survive_restart_and_replace_existing_file() {
    let path = temporary_path("persist");
    assert_eq!(Preferences::load(&path).unwrap(), Preferences::default());
    let mut preferences = Preferences {
        nickname: "Chitarra 🎸".into(),
        server: "[::1]:7777".into(),
        host: Some("ASIO".into()),
        input: Some("USB \"input\"".into()),
        output: Some("USB output".into()),
        buffer_frames: 64,
        ..Preferences::default()
    };
    preferences.save(&path).unwrap();
    assert_eq!(Preferences::load(&path).unwrap(), preferences);
    preferences.buffer_frames = 256;
    preferences.save(&path).unwrap();
    assert_eq!(Preferences::load(&path).unwrap(), preferences);
    assert_eq!(
        std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
        1
    );
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn telemetry_sharing_is_opt_in_by_default() {
    let mut preferences = Preferences::default();
    assert!(!preferences.share_telemetry);
    assert!(preferences.report_target().is_none());

    preferences.share_telemetry = true;
    preferences.telemetry_endpoint = Some("https://185.198.245.40/api/v1/logs/client".into());
    preferences.telemetry_token = Some("beta-key".into());
    let target = preferences.report_target().expect("opted in");
    assert_eq!(target.endpoint, "https://185.198.245.40/api/v1/logs/client");
    assert_eq!(target.beta_token.as_deref(), Some("beta-key"));
}

#[test]
fn partial_configs_use_defaults_and_corrupt_configs_report_errors() {
    let partial: Preferences = toml::from_str("nickname = 'bass'").unwrap();
    assert_eq!(partial.nickname, "bass");
    assert_eq!(partial.buffer_frames, 128);
    let path = temporary_path("corrupt");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "nickname = [broken").unwrap();
    assert!(Preferences::load(&path).is_err());
    // Failed reads never overwrite the original file.
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "nickname = [broken"
    );
    assert!(Preferences::default().save(path.parent().unwrap()).is_err());
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn validates_endpoints_utf8_nicknames_and_buffer_sizes() {
    let mut preferences = Preferences::default();
    assert!(preferences.endpoint().unwrap().is_ipv4());
    preferences.server = " [::1]:7777 ".into();
    assert!(preferences.endpoint().unwrap().is_ipv6());
    for server in [
        "host:7777",
        "127.0.0.1",
        "127.0.0.1:0",
        "0.0.0.0:7777",
        "[::]:7777",
    ] {
        preferences.server = server.into();
        assert!(preferences.endpoint().is_err(), "{server}");
    }
    preferences.server = "127.0.0.1:7777".into();
    for nickname in [String::new(), "  ".into(), "é".repeat(17)] {
        preferences.nickname = nickname;
        assert!(preferences.endpoint().is_err());
    }
    preferences.nickname = "é".repeat(16);
    assert!(preferences.endpoint().is_ok());
    preferences.buffer_frames = 512;
    assert!(preferences.endpoint().is_err());
}

#[test]
fn buffer_choices_intersect_both_devices_and_do_not_fallback_for_missing_usb() {
    let mut catalog = Catalog {
        inputs: vec![DeviceInfo {
            name: "USB in".into(),
            buffers: vec![64, 128],
        }],
        outputs: vec![DeviceInfo {
            name: "USB out".into(),
            buffers: vec![128, 256],
        }],
        default_input: Some("USB in".into()),
        default_output: Some("USB out".into()),
        ..Catalog::default()
    };
    let mut preferences = Preferences::default();
    assert_eq!(catalog.buffers(&preferences), [128]);
    preferences.input = Some("Unplugged USB".into());
    assert_eq!(catalog.buffers(&preferences), [] as [u32; 0]);
    preferences.input = Some("USB in".into());
    preferences.output = Some("USB out".into());
    assert_eq!(catalog.buffers(&preferences), [128]);
    catalog.outputs.clear();
    assert_eq!(catalog.buffers(&preferences), [] as [u32; 0]);
}
