use chrono::NaiveDate;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::LazyLock};
use unicode_normalization::UnicodeNormalization;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DeviceType {
    Plane,
    Quad,
    Other,
}

#[derive(Clone, Debug)]
pub struct Device {
    pub id: i64,
    pub name: String,
    pub kind: DeviceType,
    pub aliases: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct VideoSystem {
    pub id: i64,
    pub name: String,
    pub with_osd: bool,
    pub aliases: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Pilot {
    pub id: i64,
    pub aliases: Vec<String>,
    pub public: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventDate {
    pub start: NaiveDate,
    pub end: NaiveDate,
    pub precision: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Classification {
    pub event_name: String,
    pub event_date: Option<EventDate>,
    pub device_id: Option<i64>,
    pub video_system_id: Option<i64>,
    pub pilot_id: Option<i64>,
    pub colored: bool,
    pub issues: Vec<String>,
}

pub fn normalize(value: &str) -> String {
    value
        .nfkc()
        .collect::<String>()
        .to_lowercase()
        .trim()
        .replace(['-', ' '], "_")
        .split('_')
        .filter(|x| !x.is_empty())
        .collect::<Vec<_>>()
        .join("_")
}

pub fn parse_event_date(value: &str) -> Option<EventDate> {
    static DAY: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)(?:^|[^\d])(\d{1,2})[._-](\d{1,2})[._-](\d{4}|\d{2})(?:$|[^\d])").unwrap()
    });
    static MONTH: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)(?:^|[^\d])(\d{1,2})[._-](\d{4}|\d{2})(?:$|[^\d])").unwrap()
    });
    if let Some(c) = DAY.captures(value) {
        let d: u32 = c[1].parse().ok()?;
        let m: u32 = c[2].parse().ok()?;
        let y = year(&c[3]);
        let date = NaiveDate::from_ymd_opt(y, m, d)?;
        return Some(EventDate {
            start: date,
            end: date,
            precision: "day".into(),
        });
    }
    if let Some(c) = MONTH.captures(value) {
        let m: u32 = c[1].parse().ok()?;
        let y = year(&c[2]);
        let start = NaiveDate::from_ymd_opt(y, m, 1)?;
        let end = if m == 12 {
            NaiveDate::from_ymd_opt(y + 1, 1, 1)?.pred_opt()?
        } else {
            NaiveDate::from_ymd_opt(y, m + 1, 1)?.pred_opt()?
        };
        return Some(EventDate {
            start,
            end,
            precision: "month".into(),
        });
    }
    static YEAR: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)(?:^|[^\d])(\d{4})(?:$|[^\d])").unwrap());
    let c = YEAR.captures(value)?;
    let y: i32 = c[1].parse().ok()?;
    Some(EventDate {
        start: NaiveDate::from_ymd_opt(y, 1, 1)?,
        end: NaiveDate::from_ymd_opt(y, 12, 31)?,
        precision: "year".into(),
    })
}

fn year(value: &str) -> i32 {
    let y: i32 = value.parse().unwrap();
    if value.len() == 2 {
        2000 + y
    } else {
        y
    }
}

pub fn classify(
    relative_path: &str,
    devices: &[Device],
    systems: &[VideoSystem],
    pilots: &[Pilot],
    default_user_id: Option<i64>,
) -> Classification {
    let parts: Vec<&str> = relative_path.split('/').filter(|p| !p.is_empty()).collect();
    let event_name = parts.first().copied().unwrap_or_default().to_string();
    let mut result = Classification {
        event_date: parse_event_date(&event_name),
        event_name,
        device_id: None,
        video_system_id: None,
        pilot_id: None,
        colored: false,
        issues: vec![],
    };
    if parts.len() < 3 || parts.len() > 4 {
        result.issues.push("invalid_path_depth".into());
        return result;
    }
    let device_system = normalize(parts[1]);
    let (base, colored) = device_system
        .strip_suffix("_colored")
        .map(|v| (v, true))
        .unwrap_or((device_system.as_str(), false));
    result.colored = colored;
    let mut pairs = vec![];
    for d in devices {
        for da in &d.aliases {
            for s in systems {
                for sa in &s.aliases {
                    if normalize(&format!("{}_{}", da, sa)) == base {
                        pairs.push((d.id, s.id));
                    }
                }
            }
        }
    }
    pairs.sort_unstable();
    pairs.dedup();
    match pairs.as_slice() {
        [(d, s)] => {
            result.device_id = Some(*d);
            result.video_system_id = Some(*s);
        }
        [] => {
            let known_device = devices.iter().any(|d| {
                d.aliases
                    .iter()
                    .any(|a| base.starts_with(&format!("{}_", normalize(a))))
            });
            let known_system = systems.iter().any(|s| {
                s.aliases
                    .iter()
                    .any(|a| base.ends_with(&format!("_{}", normalize(a))))
            });
            if !known_device {
                result.issues.push("unknown_device".into());
            }
            if !known_system {
                result.issues.push("unknown_video_system".into());
            }
            if known_device && known_system {
                result
                    .issues
                    .push("invalid_device_system_combination".into());
            }
        }
        _ => result
            .issues
            .push("ambiguous_device_or_video_system".into()),
    }
    if parts.len() == 4 {
        let alias = normalize(parts[2]);
        let matches: Vec<_> = pilots
            .iter()
            .filter(|p| p.aliases.iter().any(|a| normalize(a) == alias))
            .collect();
        match matches.as_slice() {
            [p] => result.pilot_id = Some(p.id),
            [] => result.issues.push("unknown_pilot".into()),
            _ => result.issues.push("ambiguous_pilot".into()),
        }
    } else if let Some(id) = default_user_id {
        if pilots.iter().any(|p| p.id == id) {
            result.pilot_id = Some(id);
        } else {
            result.issues.push("inactive_default_user".into());
        }
    } else {
        result.issues.push("missing_default_user".into());
    }
    result
}

pub fn visible_pilot_ids(
    requester_admin: bool,
    requester_id: i64,
    pilots: &HashMap<i64, Pilot>,
) -> Vec<i64> {
    pilots
        .values()
        .filter(|p| requester_admin || p.id == requester_id || p.public)
        .map(|p| p.id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_dates() {
        assert_eq!(
            parse_event_date("ugor_19.09.26").unwrap().start,
            NaiveDate::from_ymd_opt(2026, 9, 19).unwrap()
        );
        assert_eq!(
            parse_event_date("karelia_08.26").unwrap().precision,
            "month"
        );
    }
    #[test]
    fn classifies_colored_and_default() {
        let d = Device {
            id: 1,
            name: "t1".into(),
            kind: DeviceType::Quad,
            aliases: vec!["t1".into()],
        };
        let s = VideoSystem {
            id: 2,
            name: "o4pro".into(),
            with_osd: false,
            aliases: vec!["o4pro".into()],
        };
        let c = classify(
            "ugor_19.09.26/t1_o4pro_colored/video.mp4",
            &[d],
            &[s],
            &[Pilot {
                id: 4,
                aliases: vec![],
                public: true,
            }],
            Some(4),
        );
        assert_eq!(c.device_id, Some(1));
        assert_eq!(c.video_system_id, Some(2));
        assert_eq!(c.pilot_id, Some(4));
        assert!(c.colored);
    }
}
