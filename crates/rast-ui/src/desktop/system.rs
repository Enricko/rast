//! What the taskbar shows about this device, its network and the local clock.
//! Everything is read locally from the browser or the OS; nothing is sent anywhere.

/// The device RAST is running on: the browser on the web, the computer natively.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Device {
    /// "Chrome on macOS" in a browser, the host name natively.
    pub(crate) name: String,
    /// "Computer", "Phone" or "Tablet".
    pub(crate) kind: &'static str,
    pub(crate) system: String,
    /// The browser and its version, or the RAST desktop app.
    pub(crate) client: String,
    pub(crate) screen: Option<String>,
    pub(crate) language: Option<String>,
    pub(crate) time_zone: Option<String>,
}

/// Browser connectivity. `navigator.connection` only exists in Chromium browsers.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Network {
    pub(crate) online: bool,
    /// The physical link ("wifi", "ethernet", "cellular") where the browser reveals it.
    pub(crate) link: Option<String>,
    /// The estimated quality class ("4g", "3g", …).
    pub(crate) effective: Option<String>,
    /// Estimated bandwidth in Mb/s.
    pub(crate) downlink: Option<f64>,
    /// Estimated round trip in milliseconds.
    pub(crate) rtt: Option<f64>,
    pub(crate) save_data: bool,
}

impl Network {
    /// A short name for the connection, such as "Wi-Fi" or "4G".
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub(crate) fn link_label(&self) -> Option<String> {
        let link = self.link.as_deref().and_then(|link| {
            Some(match link {
                "wifi" => "Wi-Fi",
                "ethernet" => "Ethernet",
                "cellular" => "Mobile data",
                "bluetooth" => "Bluetooth",
                "wimax" => "WiMAX",
                _ => return None,
            })
        });
        link.map(str::to_owned)
            .or_else(|| self.effective.as_deref().map(str::to_uppercase))
    }
}

/// Clock strings for the taskbar and the date panel.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Clock {
    /// "09:41".
    pub(crate) time: String,
    /// "Sat 21 Nov".
    pub(crate) short_date: String,
    /// "Saturday, 21 November 2026".
    pub(crate) long_date: String,
    /// "UTC+07:00".
    pub(crate) offset: String,
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const DAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

impl Clock {
    /// A fixed time for headless snapshots and tests.
    pub(crate) fn sample() -> Self {
        Self::at(2026, 11, 21, 9, 41, 7 * 3600)
    }

    pub(crate) fn at(year: i32, month: u32, day: u32, hour: u32, minute: u32, offset: i32) -> Self {
        let month_name = MONTHS[(month.clamp(1, 12) - 1) as usize];
        let weekday = DAYS[weekday(year, month, day)];
        let sign = if offset < 0 { '-' } else { '+' };
        let minutes = offset.unsigned_abs() / 60;
        Self {
            time: format!("{hour:02}:{minute:02}"),
            short_date: format!("{} {day} {}", &weekday[..3], &month_name[..3]),
            long_date: format!("{weekday}, {day} {month_name} {year}"),
            offset: format!("UTC{sign}{:02}:{:02}", minutes / 60, minutes % 60),
        }
    }
}

/// Day of the week for a Gregorian date, 0 for Sunday (Sakamoto's method).
fn weekday(year: i32, month: u32, day: u32) -> usize {
    const T: [i32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let month = month.clamp(1, 12);
    let year = if month < 3 { year - 1 } else { year };
    (year + year.div_euclid(4) - year.div_euclid(100)
        + year.div_euclid(400)
        + T[(month - 1) as usize]
        + day as i32)
        .rem_euclid(7) as usize
}

/// The browser and its major version from a user agent string.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) fn browser_from_agent(agent: &str) -> (&'static str, Option<String>) {
    const RULES: [(&str, &str); 11] = [
        ("EdgA/", "Edge"),
        ("EdgiOS/", "Edge"),
        ("Edg/", "Edge"),
        ("OPR/", "Opera"),
        ("SamsungBrowser/", "Samsung Internet"),
        ("FxiOS/", "Firefox"),
        ("Firefox/", "Firefox"),
        ("CriOS/", "Chrome"),
        ("Chromium/", "Chromium"),
        ("Chrome/", "Chrome"),
        ("Version/", "Safari"),
    ];
    for (token, name) in RULES {
        if let Some(at) = agent.find(token) {
            let version = agent[at + token.len()..]
                .split(|c: char| !c.is_ascii_digit())
                .next()
                .filter(|v| !v.is_empty())
                .map(str::to_owned);
            return (name, version);
        }
    }
    ("Browser", None)
}

/// The operating system and device kind from a user agent string. iPadOS
/// reports a Mac user agent; touch support tells the two apart.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) fn system_from_agent(agent: &str, touch_points: i32) -> (&'static str, &'static str) {
    let mac = agent.contains("Macintosh") || agent.contains("Mac OS X");
    if agent.contains("iPad") || (mac && touch_points > 1 && !agent.contains("iPhone")) {
        ("iPadOS", "Tablet")
    } else if agent.contains("iPhone") || agent.contains("iPod") {
        ("iOS", "Phone")
    } else if agent.contains("Android") {
        (
            "Android",
            if agent.contains("Mobile") {
                "Phone"
            } else {
                "Tablet"
            },
        )
    } else if agent.contains("Windows") {
        ("Windows", "Computer")
    } else if agent.contains("CrOS") {
        ("ChromeOS", "Computer")
    } else if mac {
        ("macOS", "Computer")
    } else if agent.contains("Linux") {
        ("Linux", "Computer")
    } else {
        ("Unknown system", "Computer")
    }
}

/// A fixed device for headless snapshots and tests.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn sample_device() -> Device {
    Device {
        name: "Chrome on macOS".into(),
        kind: "Computer",
        system: "macOS".into(),
        client: "Chrome 142".into(),
        screen: Some("1512 × 982 at 2×".into()),
        language: Some("en-US".into()),
        time_zone: Some("Asia/Jakarta".into()),
    }
}

#[cfg(target_arch = "wasm32")]
pub(crate) use web::{Watch, device, network};

#[cfg(target_arch = "wasm32")]
mod web {
    use js_sys::Reflect;
    use wasm_bindgen::{JsCast, JsValue, closure::Closure};

    use super::{Device, Network, browser_from_agent, system_from_agent};

    fn get(target: &JsValue, name: &str) -> Option<JsValue> {
        Reflect::get(target, &JsValue::from_str(name))
            .ok()
            .filter(|value| !value.is_undefined() && !value.is_null())
    }

    pub(crate) fn device() -> Device {
        let window = web_sys::window();
        let navigator = window.as_ref().map(web_sys::Window::navigator);
        let agent = navigator
            .as_ref()
            .and_then(|n| n.user_agent().ok())
            .unwrap_or_default();
        let touch = navigator
            .as_ref()
            .map_or(0, web_sys::Navigator::max_touch_points);
        let (browser, version) = browser_from_agent(&agent);
        let (system, kind) = system_from_agent(&agent, touch);
        let screen = window.as_ref().and_then(|w| {
            let screen = w.screen().ok()?;
            let (width, height) = (screen.width().ok()?, screen.height().ok()?);
            let ratio = w.device_pixel_ratio();
            let ratio = if (ratio - ratio.round()).abs() < 0.01 {
                format!("{}", ratio.round())
            } else {
                format!("{ratio:.2}")
            };
            Some(format!("{width} × {height} at {ratio}×"))
        });
        let time_zone = {
            let format =
                js_sys::Intl::DateTimeFormat::new(&js_sys::Array::new(), &js_sys::Object::new());
            get(&format.resolved_options(), "timeZone").and_then(|zone| zone.as_string())
        };
        Device {
            name: format!("{browser} on {system}"),
            kind,
            system: system.to_owned(),
            client: match version {
                Some(version) => format!("{browser} {version}"),
                None => browser.to_owned(),
            },
            screen,
            language: navigator.as_ref().and_then(web_sys::Navigator::language),
            time_zone,
        }
    }

    pub(crate) fn network() -> Network {
        let Some(navigator) = web_sys::window().map(|w| w.navigator()) else {
            return Network::default();
        };
        let mut network = Network {
            online: navigator.on_line(),
            ..Network::default()
        };
        if let Some(connection) = get(&navigator, "connection") {
            network.link = get(&connection, "type").and_then(|v| v.as_string());
            network.effective = get(&connection, "effectiveType").and_then(|v| v.as_string());
            network.downlink = get(&connection, "downlink").and_then(|v| v.as_f64());
            network.rtt = get(&connection, "rtt").and_then(|v| v.as_f64());
            network.save_data =
                get(&connection, "saveData").and_then(|v| v.as_bool()) == Some(true);
        }
        network
    }

    /// Wakes the app when the browser goes online or offline or the connection
    /// estimate changes, so the taskbar updates without polling.
    pub(crate) struct Watch {
        callback: Closure<dyn FnMut()>,
        connection: Option<web_sys::EventTarget>,
    }

    impl Watch {
        pub(crate) fn new(wake: rune::Waker) -> Option<Self> {
            let window = web_sys::window()?;
            let callback = Closure::<dyn FnMut()>::new(move || wake());
            let function = callback.as_ref().unchecked_ref();
            window
                .add_event_listener_with_callback("online", function)
                .ok()?;
            window
                .add_event_listener_with_callback("offline", function)
                .ok()?;
            let connection = get(&window.navigator(), "connection")
                .and_then(|value| value.dyn_into::<web_sys::EventTarget>().ok())
                .filter(|target| {
                    target
                        .add_event_listener_with_callback("change", function)
                        .is_ok()
                });
            Some(Self {
                callback,
                connection,
            })
        }
    }

    impl Drop for Watch {
        fn drop(&mut self) {
            let function = self.callback.as_ref().unchecked_ref();
            if let Some(window) = web_sys::window() {
                let _ = window.remove_event_listener_with_callback("online", function);
                let _ = window.remove_event_listener_with_callback("offline", function);
            }
            if let Some(connection) = &self.connection {
                let _ = connection.remove_event_listener_with_callback("change", function);
            }
        }
    }
}

/// This computer, read once at startup.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn device() -> Device {
    if rune::is_headless() {
        return sample_device();
    }
    let system = os_name();
    let name = host_name().unwrap_or_else(|| "This computer".to_owned());
    let language = std::env::var("LC_ALL")
        .ok()
        .or_else(|| std::env::var("LANG").ok())
        .and_then(|lang| {
            let lang = lang.split('.').next()?.replace('_', "-");
            (!lang.is_empty() && lang != "C" && lang != "POSIX").then_some(lang)
        });
    Device {
        name,
        kind: "Computer",
        system,
        client: format!("RAST desktop app v{}", env!("CARGO_PKG_VERSION")),
        screen: None,
        language,
        time_zone: time_zone(),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn host_name() -> Option<String> {
    let name = std::env::var("COMPUTERNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .or_else(|| std::env::var("HOSTNAME").ok())?;
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

#[cfg(not(target_arch = "wasm32"))]
fn os_name() -> String {
    if cfg!(target_os = "linux") {
        // PRETTY_NAME, e.g. "Ubuntu 26.04 LTS".
        if let Ok(release) = std::fs::read_to_string("/etc/os-release")
            && let Some(name) = release
                .lines()
                .find_map(|line| line.strip_prefix("PRETTY_NAME="))
        {
            let name = name.trim().trim_matches('"');
            if !name.is_empty() {
                return name.to_owned();
            }
        }
        return "Linux".to_owned();
    }
    match std::env::consts::OS {
        "macos" => "macOS",
        "windows" => "Windows",
        other => other,
    }
    .to_owned()
}

#[cfg(not(target_arch = "wasm32"))]
fn time_zone() -> Option<String> {
    if let Ok(zone) = std::env::var("TZ") {
        let zone = zone.trim_start_matches(':').trim();
        if !zone.is_empty() {
            return Some(zone.to_owned());
        }
    }
    let target = std::fs::read_link("/etc/localtime").ok()?;
    let target = target.to_string_lossy();
    target
        .split_once("zoneinfo/")
        .map(|(_, zone)| zone.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weekdays_follow_the_gregorian_calendar() {
        assert_eq!(weekday(2026, 11, 21), 6, "a Saturday");
        assert_eq!(weekday(2000, 1, 1), 6, "a Saturday");
        assert_eq!(weekday(2024, 2, 29), 4, "a Thursday");
        assert_eq!(weekday(1970, 1, 1), 4, "a Thursday");
    }

    #[test]
    fn the_clock_formats_short_and_long_dates_and_the_offset() {
        let clock = Clock::at(2026, 11, 21, 9, 5, 7 * 3600);
        assert_eq!(clock.time, "09:05");
        assert_eq!(clock.short_date, "Sat 21 Nov");
        assert_eq!(clock.long_date, "Saturday, 21 November 2026");
        assert_eq!(clock.offset, "UTC+07:00");
        assert_eq!(
            Clock::at(2026, 1, 1, 0, 0, -(3 * 3600 + 1800)).offset,
            "UTC-03:30"
        );
    }

    #[test]
    fn user_agents_name_the_browser_and_system() {
        let chrome_mac = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36";
        assert_eq!(
            browser_from_agent(chrome_mac),
            ("Chrome", Some("142".into()))
        );
        assert_eq!(system_from_agent(chrome_mac, 0), ("macOS", "Computer"));
        assert_eq!(system_from_agent(chrome_mac, 5), ("iPadOS", "Tablet"));

        let edge = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36 Edg/142.0.3595.53";
        assert_eq!(browser_from_agent(edge), ("Edge", Some("142".into())));
        assert_eq!(system_from_agent(edge, 0), ("Windows", "Computer"));

        let safari_phone = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.1 Mobile/15E148 Safari/604.1";
        assert_eq!(
            browser_from_agent(safari_phone),
            ("Safari", Some("18".into()))
        );
        assert_eq!(system_from_agent(safari_phone, 5), ("iOS", "Phone"));

        let firefox_linux =
            "Mozilla/5.0 (X11; Linux x86_64; rv:144.0) Gecko/20100101 Firefox/144.0";
        assert_eq!(
            browser_from_agent(firefox_linux),
            ("Firefox", Some("144".into()))
        );
        assert_eq!(system_from_agent(firefox_linux, 0), ("Linux", "Computer"));

        let android = "Mozilla/5.0 (Linux; Android 15; Pixel 9) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Mobile Safari/537.36";
        assert_eq!(system_from_agent(android, 5), ("Android", "Phone"));
    }

    #[test]
    fn link_labels_prefer_the_physical_link() {
        let mut network = Network {
            online: true,
            effective: Some("4g".into()),
            ..Network::default()
        };
        assert_eq!(network.link_label().as_deref(), Some("4G"));
        network.link = Some("wifi".into());
        assert_eq!(network.link_label().as_deref(), Some("Wi-Fi"));
    }
}
