//! Server components: parts of the operating system that RAST's apps use and
//! an administrator adds from App Center with one click (today: LibreOffice,
//! which AI Agent uses to show Word, Excel and PowerPoint files as pages).
//!
//! * Each component is a fixed list of Ubuntu packages, installed with apt
//!   from the server's own (signed) package sources; nothing else is ever
//!   passed to apt.
//! * The root terminal helper starts apt in a systemd unit of its own
//!   (`rast-component-<id>`), so a long download is not cut short and a
//!   restart of RAST does not stop it; a job follows the unit and shows
//!   apt's last line as its progress.
//! * Only administrators install them, and RAST never removes them.

use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use crate::{
    auth::AuthUser,
    error::{AppError, AppResult},
    jobs::{JobContext, JobSpec},
    state::AppState,
    terminal::{TerminalClient, TerminalError},
};

/// How long an install may take (a slow mirror and a few hundred MB).
const INSTALL_LIMIT: Duration = Duration::from_secs(60 * 60);
const POLL_EVERY: Duration = Duration::from_secs(2);

/// One part of a component: what it is for, its package and how to tell it
/// is there.
struct Part {
    id: &'static str,
    label: &'static str,
    package: &'static str,
    /// The same part built without a desktop (`<package>-nogui`).
    headless: Option<&'static str>,
    /// There when the part is installed.
    marker: &'static str,
}

struct Component {
    id: &'static str,
    name: &'static str,
    summary: &'static str,
    about: &'static str,
    used_by: &'static str,
    parts: &'static [Part],
    /// The package that says which build is installed, and its headless one.
    core: (&'static str, &'static str),
}

const COMPONENTS: &[Component] = &[Component {
    id: "libreoffice",
    name: "LibreOffice",
    summary: "Shows Word, Excel and PowerPoint files as pages",
    about: "AI Agent turns Word, Excel, PowerPoint, OpenDocument and RTF files into pages with LibreOffice, in a locked unit without network or access to the server's files.",
    used_by: "AI Agent: documents as pages",
    parts: &[
        Part {
            id: "writer",
            label: "Writer: text documents (docx, doc, odt, rtf)",
            package: "libreoffice-writer",
            headless: Some("libreoffice-writer-nogui"),
            marker: "/usr/lib/libreoffice/share/registry/writer.xcd",
        },
        Part {
            id: "calc",
            label: "Calc: spreadsheets (xlsx, xls, ods)",
            package: "libreoffice-calc",
            headless: Some("libreoffice-calc-nogui"),
            marker: "/usr/lib/libreoffice/share/registry/calc.xcd",
        },
        Part {
            id: "impress",
            label: "Impress: slides (pptx, ppt, odp)",
            package: "libreoffice-impress",
            headless: Some("libreoffice-impress-nogui"),
            marker: "/usr/lib/libreoffice/share/registry/impress.xcd",
        },
    ],
    core: ("libreoffice-core", "libreoffice-core-nogui"),
}];

/// The install that is running, by component (one at a time).
static RUNNING: Mutex<Option<(String, String)>> = Mutex::new(None);

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PartView {
    pub id: &'static str,
    pub label: &'static str,
    pub installed: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentView {
    pub id: &'static str,
    pub name: &'static str,
    pub summary: &'static str,
    pub about: &'static str,
    pub used_by: &'static str,
    pub parts: Vec<PartView>,
    /// `all`, `some` or `none`.
    pub installed: &'static str,
    /// The installed build's version, when there is one.
    pub version: Option<String>,
    /// What Install adds (apt package names).
    pub packages: Vec<&'static str>,
    pub can_install: bool,
    /// The running install's job, when there is one.
    pub job_id: Option<String>,
}

/// The components and what of them is installed.
pub async fn list(user: &AuthUser) -> Vec<ComponentView> {
    let running = RUNNING.lock().map(|r| r.clone()).unwrap_or(None);
    let mut views = Vec::new();
    for component in COMPONENTS {
        let parts: Vec<PartView> = component
            .parts
            .iter()
            .map(|part| PartView {
                id: part.id,
                label: part.label,
                installed: Path::new(part.marker).exists(),
            })
            .collect();
        let count = parts.iter().filter(|p| p.installed).count();
        let installed = if count == parts.len() {
            "all"
        } else if count > 0 {
            "some"
        } else {
            "none"
        };
        let version = if count > 0 {
            installed_version(component).await
        } else {
            None
        };
        views.push(ComponentView {
            id: component.id,
            name: component.name,
            summary: component.summary,
            about: component.about,
            used_by: component.used_by,
            packages: missing_packages(component, headless_build(component)),
            parts,
            installed,
            version,
            can_install: user.is_admin(),
            job_id: running
                .as_ref()
                .filter(|(id, _)| id == component.id)
                .map(|(_, job)| job.clone()),
        });
    }
    views
}

/// Whether to add the build without a desktop: when that build is the one
/// installed, or when there is none yet (a server needs no desktop). The
/// two builds replace each other's core, so a part always matches.
fn headless_build(component: &Component) -> bool {
    let (desktop, headless) = component.core;
    let has = |name: &str| Path::new(&format!("/var/lib/dpkg/info/{name}.list")).exists();
    has(headless) || !has(desktop)
}

fn missing_packages(component: &Component, headless: bool) -> Vec<&'static str> {
    component
        .parts
        .iter()
        .filter(|part| !Path::new(part.marker).exists())
        .map(|part| match (headless, part.headless) {
            (true, Some(name)) => name,
            _ => part.package,
        })
        .collect()
}

async fn installed_version(component: &Component) -> Option<String> {
    let (desktop, headless) = component.core;
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new("dpkg-query")
            .args([
                "-W",
                "-f=${Package}\t${db:Status-Abbrev}\t${Version}\n",
                desktop,
                headless,
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| {
            let mut fields = line.split('\t');
            let (_, status, version) = (fields.next()?, fields.next()?, fields.next()?);
            status.starts_with("ii").then(|| {
                version
                    .rsplit(':')
                    .next()
                    .unwrap_or(version)
                    .split('-')
                    .next()
                    .unwrap_or(version)
                    .to_string()
            })
        })
}

/// Start installing what a component is missing → the job's id.
pub async fn install(state: &AppState, user: &AuthUser, id: &str) -> AppResult<String> {
    if !user.is_admin() {
        return Err(AppError::Forbidden(
            "Server components are installed by administrators",
        ));
    }
    let component = COMPONENTS
        .iter()
        .find(|c| c.id == id)
        .ok_or(AppError::NotFound("server component not found"))?;
    let packages = missing_packages(component, headless_build(component));
    if packages.is_empty() {
        return Err(AppError::conflict(format!(
            "{} is installed already",
            component.name
        )));
    }
    {
        let mut running = RUNNING
            .lock()
            .map_err(|_| AppError::internal("install state"))?;
        if running.is_some() {
            return Err(AppError::conflict(
                "Another server component is being installed; try again when it is done",
            ));
        }
        *running = Some((component.id.to_string(), String::new()));
    }
    let actor = user.clone();
    let name = component.name;
    let component_id = component.id;
    let spec = JobSpec::new("package.component", move |ctx: JobContext| async move {
        let result = run_install(&ctx, &actor, component_id, name, &packages).await;
        if let Ok(mut running) = RUNNING.lock() {
            *running = None;
        }
        result
    })
    .actor(user)
    .message(format!("Installing {name}"));
    let job_id = match state.jobs.submit(spec).await {
        Ok(job_id) => job_id,
        Err(error) => {
            if let Ok(mut running) = RUNNING.lock() {
                *running = None;
            }
            return Err(error);
        }
    };
    if let Ok(mut running) = RUNNING.lock() {
        if let Some((_, job)) = running.as_mut() {
            if job.is_empty() {
                job.clone_from(&job_id);
            }
        }
    }
    state.events.publish(
        "package.changed",
        json!({"packageId": format!("component:{}", component.id), "reason": "queued", "jobId": job_id}),
    );
    Ok(job_id)
}

/// The root script that starts apt in its own unit.
fn start_script(unit: &str, packages: &[&str]) -> String {
    let list = packages.join(" ");
    // Package names come from the fixed list above (letters, digits, '-').
    let inner = format!(
        "apt-get update -q || true; exec apt-get install -y -q --no-install-recommends \
         -o DPkg::Lock::Timeout=600 {list}"
    );
    format!(
        "exec systemd-run --quiet --collect --service-type=exec --unit={unit} \
         --description='RAST: install a server component' --setenv=DEBIAN_FRONTEND=noninteractive \
         --property=TimeoutStopSec=600 -- /bin/sh -c '{inner}'"
    )
}

async fn run_install(
    ctx: &JobContext,
    user: &AuthUser,
    id: &str,
    name: &str,
    packages: &[&str],
) -> Result<Value, AppError> {
    if packages.iter().any(|p| {
        p.is_empty()
            || !p
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    }) {
        return Err(AppError::internal("an unusual package name"));
    }
    let client = TerminalClient::default();
    let account = user.username.to_ascii_lowercase();
    let unit = format!("rast-component-{id}");
    ctx.progress(0, 0, &format!("Starting apt for {}", packages.join(", ")))
        .await;
    let started = client
        .run(user.id, &account, &start_script(&unit, packages), 30)
        .await
        .map_err(helper_error)?;
    if started.exit_code != Some(0) {
        let said = last_line(&started.stderr)
            .or_else(|| last_line(&started.stdout))
            .unwrap_or_default();
        return Err(AppError::unavailable(format!(
            "apt could not be started: {said}"
        )));
    }
    let deadline = tokio::time::Instant::now() + INSTALL_LIMIT;
    let watch = format!(
        "systemctl show -p ActiveState --value {unit}; \
         journalctl -u {unit} -n 1 -o cat --no-pager 2>/dev/null | tail -n 1"
    );
    loop {
        tokio::time::sleep(POLL_EVERY).await;
        let seen = client
            .run(user.id, &account, &watch, 30)
            .await
            .map_err(helper_error)?;
        let mut lines = seen.stdout.lines();
        let active = lines.next().unwrap_or("").trim().to_string();
        let last = lines.next().unwrap_or("").trim().to_string();
        if !matches!(
            active.as_str(),
            "activating" | "active" | "deactivating" | "reloading"
        ) {
            // Done: the unit is collected whether apt succeeded or not, so
            // what is installed now decides.
            break;
        }
        if !last.is_empty() {
            ctx.progress(0, 0, &last).await;
        }
        if tokio::time::Instant::now() > deadline {
            return Err(AppError::unavailable(format!(
                "{name} is still installing after an hour; `journalctl -u {unit}` shows how far it got"
            )));
        }
    }
    let component = COMPONENTS
        .iter()
        .find(|c| c.id == id)
        .expect("a listed component");
    let left = missing_packages(component, headless_build(component));
    if !left.is_empty() {
        let tail = client
            .run(
                user.id,
                &account,
                &format!("journalctl -u {unit} -n 15 -o cat --no-pager 2>/dev/null"),
                30,
            )
            .await
            .map(|o| o.stdout)
            .unwrap_or_default();
        let said = tail
            .lines()
            .rev()
            .map(str::trim)
            .find(|l| l.starts_with("E:"))
            .or_else(|| tail.lines().rev().map(str::trim).find(|l| !l.is_empty()))
            .unwrap_or("apt stopped")
            .to_string();
        return Err(AppError::unavailable(format!(
            "{name} was not installed ({} missing): {said}",
            left.join(", ")
        )));
    }
    ctx.progress(1, 1, &format!("{name} is installed")).await;
    Ok(json!({ "component": id, "installed": packages }))
}

fn helper_error(error: TerminalError) -> AppError {
    match error {
        TerminalError::Off => AppError::unavailable(
            "Installing server components needs Terminal turned on (sudo rast terminal enable)",
        ),
        other => AppError::unavailable(format!("the terminal helper could not run apt: {other}")),
    }
}

fn last_line(text: &str) -> Option<String> {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn components_install_fixed_packages_only() {
        for component in COMPONENTS {
            for part in component.parts {
                for name in std::iter::once(part.package).chain(part.headless) {
                    assert!(
                        name.bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
                        "{name}"
                    );
                }
            }
        }
        let script = start_script(
            "rast-component-libreoffice",
            &["libreoffice-calc", "libreoffice-impress"],
        );
        assert!(script.starts_with("exec systemd-run "), "{script}");
        assert!(
            script.contains("--unit=rast-component-libreoffice"),
            "{script}"
        );
        assert!(
            script.contains("--no-install-recommends -o DPkg::Lock::Timeout=600 libreoffice-calc libreoffice-impress'"),
            "{script}"
        );
        assert!(
            script.contains("--setenv=DEBIAN_FRONTEND=noninteractive"),
            "{script}"
        );
    }

    #[test]
    fn a_missing_part_comes_in_the_build_that_is_there() {
        let office = &COMPONENTS[0];
        let all_headless: Vec<&str> = office.parts.iter().filter_map(|p| p.headless).collect();
        let all_desktop: Vec<&str> = office.parts.iter().map(|p| p.package).collect();
        let missing_headless = missing_packages(office, true);
        let missing_desktop = missing_packages(office, false);
        assert!(missing_headless.iter().all(|p| all_headless.contains(p)));
        assert!(missing_desktop.iter().all(|p| all_desktop.contains(p)));
    }
}
