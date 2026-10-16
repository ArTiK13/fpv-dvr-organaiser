use chrono::NaiveDate;
use fpv_catalog_bot::{
    classifier::*,
    downloads::{checked_path, range_bytes},
    scanner::safe_relative,
};
use std::collections::HashMap;
fn refs() -> (Vec<Device>, Vec<VideoSystem>, Vec<Pilot>) {
    (
        vec![Device {
            id: 1,
            name: "T1".into(),
            kind: DeviceType::Quad,
            aliases: vec!["t1".into()],
        }],
        vec![VideoSystem {
            id: 2,
            name: "O4".into(),
            with_osd: false,
            aliases: vec!["o4_pro".into()],
        }],
        vec![Pilot {
            id: 3,
            aliases: vec!["sergmi_sh".into()],
            public: false,
        }],
    )
}
#[test]
fn exact_aliases_allow_underscores_and_case() {
    let (d, s, p) = refs();
    let c = classify("home/T1_O4_PRO_COLORED/SERgmi_Sh/v.MP4", &d, &s, &p, None);
    assert!(c.issues.is_empty());
    assert_eq!(c.device_id, Some(1));
    assert_eq!(c.pilot_id, Some(3));
    assert!(c.colored);
    assert!(c.event_date.is_none());
}
#[test]
fn date_precision_and_overlap() {
    let a = parse_event_date("ugor_19.09.2026").unwrap();
    assert_eq!(a.start, NaiveDate::from_ymd_opt(2026, 9, 19).unwrap());
    let b = parse_event_date("karelia_02.24").unwrap();
    assert_eq!(b.end, NaiveDate::from_ymd_opt(2024, 2, 29).unwrap());
    assert_eq!(parse_event_date("event_2026").unwrap().precision, "year");
    assert!(parse_event_date("event_31.02.26").is_none());
}
#[test]
fn ambiguity_is_not_guessed() {
    let (mut d, s, p) = refs();
    d.push(Device {
        id: 4,
        name: "other".into(),
        kind: DeviceType::Plane,
        aliases: vec!["T1".into()],
    });
    let c = classify("event/t1_o4_pro/sergmi_sh/v.mp4", &d, &s, &p, None);
    assert!(c
        .issues
        .contains(&"ambiguous_device_or_video_system".into()));
    assert!(c.device_id.is_none());
}
#[test]
fn missing_depth_and_defaults() {
    let (d, s, p) = refs();
    assert!(classify("event/t1_o4_pro/v.mp4", &d, &s, &p, None)
        .issues
        .contains(&"missing_default_user".into()));
    assert_eq!(
        classify("event/t1_o4_pro/v.mp4", &d, &s, &p, Some(3)).pilot_id,
        Some(3)
    );
    assert!(classify("event/t1_o4_pro/p/extra/v.mp4", &d, &s, &p, None)
        .issues
        .contains(&"invalid_path_depth".into()));
}
#[test]
fn access_scopes() {
    let mut p = HashMap::new();
    p.insert(
        1,
        Pilot {
            id: 1,
            aliases: vec![],
            public: false,
        },
    );
    p.insert(
        2,
        Pilot {
            id: 2,
            aliases: vec![],
            public: true,
        },
    );
    p.insert(
        3,
        Pilot {
            id: 3,
            aliases: vec![],
            public: false,
        },
    );
    let mut ids = visible_pilot_ids(false, 1, &p);
    ids.sort();
    assert_eq!(ids, vec![1, 2]);
    assert_eq!(visible_pilot_ids(true, 1, &p).len(), 3);
}
#[test]
fn traversal_and_ranges() {
    for p in ["/a", "../a", "a/../b", "a//b", "a/./b"] {
        assert!(!safe_relative(p), "{p}");
    }
    assert!(safe_relative("событие/t1_o4/ник/v.MP4"));
    assert_eq!(range_bytes(Some("bytes=3-5"), 10).unwrap(), (3, 5, true));
    assert_eq!(range_bytes(Some("bytes=-3"), 10).unwrap(), (7, 9, true));
    assert!(range_bytes(Some("bytes=20-"), 10).is_err());
    assert!(range_bytes(Some("bytes=0-2,4-6"), 10).is_err());
}
#[tokio::test]
async fn cannot_download_through_symlink() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::NamedTempFile::new().unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.path(), root.path().join("v.mp4")).unwrap();
        assert!(checked_path(root.path(), "v.mp4").await.is_err());
    }
}
