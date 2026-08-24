//! Registering the background scan with Windows Task Scheduler
//! (`background-scan.md`).
//!
//! EmFit does not schedule anything itself: no resident process, no timer, no
//! thread that sleeps for six hours. It hands Windows a task definition and
//! gets out of the way. Task Scheduler already knows how to skip a run on
//! battery, retry one the machine slept through, and stop one that overruns -
//! all of which would otherwise have to be written, and written worse.
//!
//! **The task runs as the logged-in user, deliberately.** Snapshots live under
//! that user's `%LOCALAPPDATA%`, so a task running as LocalSystem would write
//! where the app never looks. The cost is that background scans happen only
//! while someone is logged in, which is the trade the plan accepted.
//!
//! Registering and removing both need Administrator, so both go through
//! [`crate::service::elevation::run_elevated`] - one prompt, at the moment the
//! user asks for it.

use std::path::Path;

use crate::error::{Error, Result};
use crate::service::config::ScanInterval;
use crate::service::elevation;

/// What the task is called. Also how to find it in Task Scheduler, and how to
/// delete it without the app - which is why it is a plain, searchable name
/// rather than a GUID.
pub const TASK_NAME: &str = "EmFit Background Scan";

/// The flag the task runs EmFit with.
pub const BACKGROUND_SCAN_FLAG: &str = "--background-scan";

/// How long one run may take before Task Scheduler stops it. A scan is
/// seconds; an hour is a backstop against a run that has gone wrong, not a
/// budget anything should approach.
const TIME_LIMIT: &str = "PT1H";

/// Register (or replace) the background scan task.
///
/// `user` is the account it will run as - the logged-in one, captured before
/// elevation, because the elevating administrator may well be someone else.
pub fn register(exe: &Path, user: &str, interval: ScanInterval) -> Result<()> {
    let xml = task_xml(exe, user, interval);

    // `schtasks /XML` rather than a command line of flags: the conditions
    // below have no flag form, and it sidesteps quoting an executable path
    // inside a /TR argument inside a shell invocation.
    let path = std::env::temp_dir().join(format!("emfit-task-{}.xml", std::process::id()));
    // UTF-16 with a BOM: schtasks reads the file as Unicode and rejects UTF-8
    // outright, which is a confusing failure to debug from the outside.
    let mut bytes = vec![0xFF, 0xFE];
    for unit in xml.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    std::fs::write(&path, &bytes).map_err(|source| Error::Io {
        path: path.clone(),
        source,
    })?;

    let args = format!(
        "/Create /TN \"{TASK_NAME}\" /XML \"{}\" /F",
        path.to_string_lossy()
    );
    let result = run_schtasks(&args);
    let _ = std::fs::remove_file(&path);
    result
}

/// Remove the task. Succeeds when there was nothing to remove.
pub fn unregister() -> Result<()> {
    match run_schtasks(&format!("/Delete /TN \"{TASK_NAME}\" /F")) {
        Ok(()) => Ok(()),
        // Deleting a task that is not there is the state the caller wanted.
        Err(Error::Config { message }) if message.contains("cannot find") => Ok(()),
        Err(e) => Err(e),
    }
}

/// `schtasks.exe`, invoked so that it draws nothing.
///
/// It is a console program and EmFit is not, so without `CREATE_NO_WINDOW`
/// Windows gives it a console of its own - a black window that flashes up and
/// vanishes on every call. `Stdio::null` alone does not prevent that: it
/// silences the output, not the window.
#[cfg(windows)]
fn schtasks() -> std::process::Command {
    use std::os::windows::process::CommandExt;
    use windows::Win32::System::Threading::CREATE_NO_WINDOW;

    let mut command = std::process::Command::new("schtasks.exe");
    command
        .creation_flags(CREATE_NO_WINDOW.0)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    command
}

/// Whether the task is currently registered. Needs no elevation - querying is
/// not a privileged operation.
pub fn is_registered() -> bool {
    #[cfg(windows)]
    {
        schtasks()
            .args(["/Query", "/TN", TASK_NAME])
            .status()
            .is_ok_and(|status| status.success())
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Start the task now, without waiting for its interval.
///
/// No elevation: starting a task you own is not privileged, and the task
/// itself carries the elevated run level. Returns once Windows has accepted
/// the request - the scan runs on its own after that.
pub fn run_now() -> Result<()> {
    #[cfg(windows)]
    {
        let status = schtasks()
            .args(["/Run", "/TN", TASK_NAME])
            .status()
            .map_err(|source| Error::Io {
                path: std::path::PathBuf::from("schtasks.exe"),
                source,
            })?;
        if !status.success() {
            return Err(Error::Config {
                message: format!(
                    "Windows would not start \"{TASK_NAME}\" (schtasks exited with {})",
                    status.code().unwrap_or(-1)
                ),
            });
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        Err(Error::UnsupportedPlatform {
            operation: "scheduled tasks".to_string(),
        })
    }
}

/// The logged-in account, as Task Scheduler wants it written.
///
/// Read before elevating: after a UAC prompt the environment may belong to a
/// different administrator account, and a task registered for *them* would
/// write its snapshots into their profile.
pub fn current_user() -> Option<String> {
    let user = std::env::var("USERNAME").ok().filter(|u| !u.is_empty())?;
    match std::env::var("USERDOMAIN") {
        Ok(domain) if !domain.is_empty() => Some(format!("{domain}\\{user}")),
        _ => Some(user),
    }
}

fn run_schtasks(args: &str) -> Result<()> {
    let code = elevation::run_elevated("schtasks.exe", args)?;
    if code == 0 {
        return Ok(());
    }
    Err(Error::Config {
        message: format!("schtasks exited with code {code}"),
    })
}

/// The Task Scheduler definition, as XML.
///
/// A logon trigger with a repetition rather than a daily one at a fixed time:
/// the machine may well be off at any hour chosen, and a scan is only useful
/// while there is someone logged in to open the app afterwards.
fn task_xml(exe: &Path, user: &str, interval: ScanInterval) -> String {
    let minutes = interval.minutes();
    let period = if minutes.is_multiple_of(60) {
        format!("PT{}H", minutes / 60)
    } else {
        format!("PT{minutes}M")
    };

    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Keeps the drives you chose in EmFit scanned, so opening it shows a current index. Remove it from EmFit's settings, or delete this task.</Description>
    <URI>\{name}</URI>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
      <Repetition>
        <Interval>{period}</Interval>
        <StopAtDurationEnd>false</StopAtDurationEnd>
      </Repetition>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>true</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>true</StopIfGoingOnBatteries>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <ExecutionTimeLimit>{TIME_LIMIT}</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
      <Arguments>{BACKGROUND_SCAN_FLAG}</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
        name = escape(TASK_NAME),
        user = escape(user),
        exe = escape(&exe.to_string_lossy()),
    )
}

/// XML-escape a value. Paths and account names can hold `&`, and a raw one
/// would make the whole definition unparseable.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xml(interval: ScanInterval) -> String {
        task_xml(
            Path::new(r"C:\Program Files\EmFit\EmFit.exe"),
            "PC\\ada",
            interval,
        )
    }

    #[test]
    fn the_definition_names_the_executable_and_the_flag() {
        let xml = xml(ScanInterval::SixHourly);
        assert!(xml.contains(r"<Command>C:\Program Files\EmFit\EmFit.exe</Command>"));
        assert!(xml.contains("<Arguments>--background-scan</Arguments>"));
    }

    #[test]
    fn it_runs_as_the_user_with_their_own_privileges() {
        let xml = xml(ScanInterval::Daily);
        assert!(xml.contains("<UserId>PC\\ada</UserId>"));
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(
            xml.contains("<RunLevel>HighestAvailable</RunLevel>"),
            "a raw volume read needs the elevated token"
        );
    }

    #[test]
    fn every_interval_becomes_a_duration() {
        assert!(xml(ScanInterval::Hourly).contains("<Interval>PT1H</Interval>"));
        assert!(xml(ScanInterval::SixHourly).contains("<Interval>PT6H</Interval>"));
        assert!(xml(ScanInterval::Daily).contains("<Interval>PT24H</Interval>"));
        assert!(xml(ScanInterval::Weekly).contains("<Interval>PT168H</Interval>"));
    }

    #[test]
    fn it_stays_out_of_the_way_on_battery() {
        let xml = xml(ScanInterval::SixHourly);
        assert!(xml.contains("<DisallowStartIfOnBatteries>true</DisallowStartIfOnBatteries>"));
        assert!(xml.contains("<StopIfGoingOnBatteries>true</StopIfGoingOnBatteries>"));
        assert!(
            xml.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>"),
            "a slow run must not stack up behind itself"
        );
    }

    #[test]
    fn values_that_could_break_the_xml_are_escaped() {
        let xml = task_xml(
            Path::new(r"C:\A & B\EmFit.exe"),
            "DOM\\a<b>",
            ScanInterval::Daily,
        );
        assert!(xml.contains(r"C:\A &amp; B\EmFit.exe"));
        assert!(xml.contains("DOM\\a&lt;b&gt;"));
        assert!(!xml.contains("a<b>"));
    }
}
