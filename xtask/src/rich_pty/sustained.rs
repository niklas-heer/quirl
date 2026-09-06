//! Accelerated retention and lifecycle checks within one uninterrupted shell.
//!
//! Failure model: repeated commands can retain descriptors, reader threads,
//! children, history, or transcript allocations even though short fresh sessions
//! pass. Warmup emits 32 MiB through bounded child-terminal scrollback before a
//! further 32 MiB of measured churn. Child snapshots retain only recent rows, so
//! these source bytes do not prove that the parent transcript budget wrapped.
//! Each turn uses unique screen/output oracles
//! and fresh input readiness; cancellation must not execute the editable command.
//! The harness clears its own retained wire bytes only after settling a turn.
//!
//! Work is fixed at 128 output bursts, 64 error/pipeline/editor-cancel rounds,
//! and four foreground interruptions. Admission stops after 240 seconds, plus
//! the current bounded PTY operation and cleanup. Each screen/readiness/exit
//! observation has its own five-second ceiling within that aggregate budget. Linux samples bounded /proc
//! files at settled prompts; RSS has allocator headroom and is a regression
//! envelope, not proof of zero leaks. Other platforms still check responsiveness
//! and owned child cleanup. Accelerated churn does not establish real uptime.

use super::{
    STARTUP_MARKER, Session, SessionOptions, TaskError, VirtualScreen, contains, ensure_status,
    ensure_terminal_restored, key, read_bounded_fixture, screen_error, write_executable,
};
use nix::{
    errno::Errno,
    sys::signal::kill,
    unistd::{Pid, getpgid},
};
#[cfg(target_os = "linux")]
use std::fs;
use std::{
    io,
    path::Path,
    time::{Duration, Instant},
};
use unicode_width::UnicodeWidthStr;

const WARMUP_BURSTS: usize = 64;
const MEASURED_ROUNDS: usize = 64;
// Hosted macOS debug runs made steady sub-second progress but exhausted the
// former aggregate 120-second budget after 114/128 bursts or 58/64 rounds.
// Keep the workload and leak envelope intact while separating total execution
// allowance from the much tighter bound that detects an unresponsive command.
const RUN_LIMIT: Duration = Duration::from_secs(240);
const OBSERVATION_LIMIT: Duration = Duration::from_secs(5);
#[cfg(target_os = "linux")]
const PROC_BYTES_MAX: usize = 32 * 1024;
#[cfg(target_os = "linux")]
const DIRECTORY_ENTRIES_MAX: u64 = 256;
const FD_GROWTH_MAX: u64 = 8;
const THREAD_GROWTH_MAX: u64 = 8;
const RSS_GROWTH_BYTES_MAX: u64 = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
struct Resources {
    descriptors: u64,
    threads: u64,
    resident_bytes: u64,
    children: u64,
}

/// Exercise child scrollback eviction, error recovery, and cancellation in one shell.
pub(super) fn check_sustained_session(binary: &Path) -> Result<(), TaskError> {
    let started = Instant::now();
    let deadline = started
        .checked_add(RUN_LIMIT)
        .ok_or_else(|| io::Error::other("sustained deadline overflowed"))?;
    let mut session = Session::new(binary, SessionOptions::default())?;
    install_fixtures(&session)?;
    wait_for(&mut session, deadline, "startup", |session| {
        contains(session.pty.output(), STARTUP_MARKER)
    })?;
    let shell = session
        .pty
        .child_pid()
        .ok_or_else(|| io::Error::other("sustained shell exited during startup"))?;
    let mut wire_bytes = 0_u64;
    for index in 0..WARMUP_BURSTS {
        ensure_time(started)?;
        burst(&mut session, deadline, index)?;
        clear_wire(&mut session, &mut wire_bytes);
    }
    let baseline = resources(shell)?;
    check_resources(baseline, baseline)?;
    println!("sustained warm baseline: {baseline:?}");
    for round in 0..MEASURED_ROUNDS {
        ensure_time(started)?;
        burst(&mut session, deadline, WARMUP_BURSTS.saturating_add(round))?;
        recover_from_error(&mut session, deadline, round)?;
        cancel_editor(&mut session, deadline, round)?;
        if round.checked_rem(16) == Some(15) {
            interrupt_child(&mut session, deadline, round)?;
        }
        if round.checked_rem(8) == Some(7) {
            let observed = resources(shell)?;
            check_resources(baseline, observed)?;
            println!("sustained round {}: {observed:?}", round.saturating_add(1));
        }
        clear_wire(&mut session, &mut wire_bytes);
    }
    ensure_time(started)?;
    execute_marker(
        &mut session,
        deadline,
        "/usr/bin/printf SUSTAIN_FINAL_READY",
        "SUSTAIN_FINAL_READY",
    )?;
    let start = session.pty.output().len();
    ensure_time(started)?;
    session.pty.send(key::CTRL_D)?;
    let exit_deadline = observation_deadline(deadline, Instant::now())?;
    ensure_status(
        session
            .pty
            .wait_exit_within(exit_deadline.saturating_duration_since(Instant::now()))?,
        0,
        "sustained session",
    )?;
    ensure_terminal_restored(&session, start, "sustained session")?;
    ensure_gone(shell, "sustained shell")?;
    wire_bytes =
        wire_bytes.saturating_add(u64::try_from(session.pty.output().len()).unwrap_or(u64::MAX));
    println!(
        "sustained completed: one session; 128 bursts; 131072 payload lines; 67239936 payload bytes; 64 error/pipeline/editor-cancel rounds; 4 child interruptions; {wire_bytes} observed wire bytes; {} ms elapsed; Linux-only FD/thread/RSS observations; no uptime equivalence",
        started.elapsed().as_millis()
    );
    Ok(())
}

fn install_fixtures(session: &Session) -> Result<(), TaskError> {
    write_executable(
        &session.private.path.join("sustained-burst"),
        "#!/bin/sh\nexec /usr/bin/awk -v id=\"$1\" 'BEGIN { payload=sprintf(\"%0512d\",0); for(i=0;i<1024;i++) print payload; print \"SUSTAIN_BURST_\" id }'\n",
    )?;
    write_executable(
        &session.private.path.join("sustained-never"),
        "#!/bin/sh\nprintf executed > CANCELLED_INPUT_EXECUTED\n",
    )?;
    write_executable(
        &session.private.path.join("sustained-job"),
        "#!/bin/sh\nprintf '%s\\n' \"$$\" > sustained-job.pid\nprintf 'SUSTAIN_JOB_%s\\n' \"$1\"\nexec /bin/sleep 30\n",
    )?;
    Ok(())
}

fn burst(session: &mut Session, deadline: Instant, index: usize) -> Result<(), TaskError> {
    let marker = format!("SUSTAIN_BURST_{index}");
    let started = Instant::now();
    let result = execute_marker(
        session,
        deadline,
        &format!("./sustained-burst {index}"),
        &marker,
    );
    if index < 3 || index.checked_rem(16) == Some(15) || result.is_err() {
        println!(
            "sustained burst {index}: completed={} elapsed_ms={}",
            result.is_ok(),
            started.elapsed().as_millis()
        );
    }
    result
}

fn execute_marker(
    session: &mut Session,
    deadline: Instant,
    command: &str,
    marker: &str,
) -> Result<(), TaskError> {
    session.pty.type_text(command)?;
    let start = session.pty.output().len();
    session.pty.send(key::ENTER)?;
    wait_screen(
        session,
        deadline,
        &format!("sustained command result {marker}"),
        |screen| has_output(screen, marker),
    )?;
    wait_ready(session, deadline, start)
}

fn recover_from_error(
    session: &mut Session,
    deadline: Instant,
    round: usize,
) -> Result<(), TaskError> {
    let missing = format!("quirl_sustained_missing_{round}");
    session.pty.type_text(&missing)?;
    let start = session.pty.output().len();
    session.pty.send(key::ENTER)?;
    wait_screen(
        session,
        deadline,
        "sustained unique error diagnostic",
        |screen| {
            screen
                .lines()
                .iter()
                .any(|line| line.contains("could not start") && line.contains(&missing))
        },
    )?;
    wait_ready(session, deadline, start)?;
    execute_marker(
        session,
        deadline,
        &format!("/usr/bin/printf sustain_pipe_{round} | /usr/bin/tr a-z A-Z"),
        &format!("SUSTAIN_PIPE_{round}"),
    )
}

fn cancel_editor(session: &mut Session, deadline: Instant, round: usize) -> Result<(), TaskError> {
    let command = format!("./sustained-never {round}");
    session.pty.type_text(&command)?;
    let expected = format!("> {command}");
    wait_screen(session, deadline, "sustained cancellable input", |screen| {
        screen.lines().iter().any(|line| line == &expected)
    })?;
    session.pty.send(key::CTRL_C)?;
    // Editing cancellation keeps the terminal lease, so it does not emit a
    // fresh mouse-enable sequence like child execution. Require the confirmed
    // nonempty editor to become empty before sending the next command.
    wait_screen(
        session,
        deadline,
        "sustained cancellation clears editor",
        |screen| {
            screen.lines().iter().any(|line| line.trim() == ">")
                && !screen.lines().iter().any(|line| line == &expected)
                && screen.text().contains("interactive input cancelled")
                && screen.bottom_line().contains("NORMAL")
        },
    )?;
    if session
        .private
        .path
        .join("CANCELLED_INPUT_EXECUTED")
        .exists()
    {
        return Err(io::Error::other("cancelled sustained editor input executed").into());
    }
    Ok(())
}

fn interrupt_child(
    session: &mut Session,
    deadline: Instant,
    round: usize,
) -> Result<(), TaskError> {
    session.pty.type_text(&format!("./sustained-job {round}"))?;
    let start = session.pty.output().len();
    session.pty.send(key::ENTER)?;
    let marker = format!("SUSTAIN_JOB_{round}");
    wait_screen(session, deadline, "sustained child running", |screen| {
        has_output(screen, &marker)
    })?;
    let bytes = read_bounded_fixture(&session.private.path.join("sustained-job.pid"), 32)?;
    let raw = std::str::from_utf8(&bytes)?.trim().parse::<i32>()?;
    if raw <= 1 {
        return Err(io::Error::other("invalid sustained child pid").into());
    }
    let child = Pid::from_raw(raw);
    let outer_group = session.pty.foreground_group()?;
    let quirl = session
        .pty
        .child_pid()
        .ok_or_else(|| io::Error::other("Quirl exited while its sustained child was running"))?;
    if outer_group != quirl || getpgid(Some(child))? == outer_group {
        return Err(io::Error::other(
            "sustained child did not retain a separate private terminal group",
        )
        .into());
    }
    session.pty.send(key::CTRL_C)?;
    wait_ready(session, deadline, start)?;
    ensure_gone(child, "interrupted sustained child")
}

fn ensure_gone(process: Pid, label: &str) -> Result<(), TaskError> {
    match kill(process, None) {
        Err(Errno::ESRCH) => Ok(()),
        other => Err(io::Error::other(format!(
            "{label} was not reaped: pid={process}, observation={other:?}"
        ))
        .into()),
    }
}

fn has_output(screen: &VirtualScreen, marker: &str) -> bool {
    screen.lines().iter().any(|line| {
        if line.trim() == marker {
            return true;
        }
        let Some(rest) = line.strip_prefix(marker) else {
            return false;
        };
        let rest = rest.trim();
        matches!(rest, "#" | "|") && UnicodeWidthStr::width(line.as_str()) == screen.columns()
    })
}

fn wait_screen(
    session: &mut Session,
    deadline: Instant,
    description: &str,
    predicate: impl Fn(&VirtualScreen) -> bool,
) -> Result<(), TaskError> {
    wait_for(session, deadline, description, |session| {
        predicate(session.pty.screen())
    })
}

fn wait_ready(session: &mut Session, deadline: Instant, start: usize) -> Result<(), TaskError> {
    let deadline = observation_deadline(deadline, Instant::now())?;
    super::wait_for_rich_input_until(session, start, deadline)
}

fn wait_for(
    session: &mut Session,
    deadline: Instant,
    description: &str,
    predicate: impl Fn(&Session) -> bool,
) -> Result<(), TaskError> {
    let run_deadline = deadline;
    let deadline = observation_deadline(run_deadline, Instant::now())?;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let limit = if deadline == run_deadline {
                "240-second aggregate work limit"
            } else {
                "5-second observation limit"
            };
            return Err(screen_error(
                &format!("sustained {limit} while waiting for {description}"),
                session.pty.screen(),
            ));
        }
        if predicate(session) {
            return Ok(());
        }
        session
            .pty
            .drain_for(remaining.min(Duration::from_millis(16)))?;
    }
}

fn observation_deadline(run_deadline: Instant, now: Instant) -> Result<Instant, TaskError> {
    let deadline = now
        .checked_add(OBSERVATION_LIMIT)
        .ok_or_else(|| io::Error::other("sustained observation deadline overflowed"))?;
    Ok(deadline.min(run_deadline))
}

fn clear_wire(session: &mut Session, wire_bytes: &mut u64) {
    *wire_bytes =
        wire_bytes.saturating_add(u64::try_from(session.pty.output().len()).unwrap_or(u64::MAX));
    session.pty.clear_output();
}

fn ensure_time(started: Instant) -> Result<(), TaskError> {
    if started.elapsed() >= RUN_LIMIT {
        return Err(io::Error::other(
            "sustained session exceeded its 240-second aggregate work limit",
        )
        .into());
    }
    Ok(())
}

fn check_resources(
    baseline: Option<Resources>,
    observed: Option<Resources>,
) -> Result<(), TaskError> {
    let (Some(baseline), Some(observed)) = (baseline, observed) else {
        return Ok(());
    };
    let exceeded = observed.descriptors > baseline.descriptors.saturating_add(FD_GROWTH_MAX)
        || observed.threads > baseline.threads.saturating_add(THREAD_GROWTH_MAX)
        || observed.resident_bytes > baseline.resident_bytes.saturating_add(RSS_GROWTH_BYTES_MAX)
        || observed.children != 0;
    if exceeded {
        return Err(io::Error::other(format!("sustained resource retention exceeded warmed envelope: baseline={baseline:?}, observed={observed:?}; limits: fd +{FD_GROWTH_MAX}, threads +{THREAD_GROWTH_MAX}, RSS +{RSS_GROWTH_BYTES_MAX} bytes, direct children 0")).into());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn resources(process: Pid) -> Result<Option<Resources>, TaskError> {
    let root = std::path::PathBuf::from(format!("/proc/{process}"));
    let status = read_bounded_fixture(&root.join("status"), PROC_BYTES_MAX)?;
    let resident_bytes = parse_resident_bytes(std::str::from_utf8(&status)?)?;
    let children = read_bounded_fixture(
        &root.join("task").join(process.to_string()).join("children"),
        PROC_BYTES_MAX,
    )?;
    let children = u64::try_from(std::str::from_utf8(&children)?.split_whitespace().count())?;
    Ok(Some(Resources {
        descriptors: count_entries(&root.join("fd"))?,
        threads: count_entries(&root.join("task"))?,
        resident_bytes,
        children,
    }))
}

#[cfg(not(target_os = "linux"))]
fn resources(_process: Pid) -> Result<Option<Resources>, TaskError> {
    Ok(None)
}

#[cfg(target_os = "linux")]
fn count_entries(path: &Path) -> Result<u64, TaskError> {
    let mut count = 0_u64;
    for entry in fs::read_dir(path)? {
        entry?;
        count = count.saturating_add(1);
        if count > DIRECTORY_ENTRIES_MAX {
            return Err(io::Error::other("sustained /proc directory exceeded 256 entries").into());
        }
    }
    Ok(count)
}

#[cfg(any(target_os = "linux", test))]
fn parse_resident_bytes(status: &str) -> Result<u64, TaskError> {
    let row = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .ok_or_else(|| io::Error::other("sustained /proc status omitted VmRSS"))?;
    let mut fields = row.split_whitespace();
    let amount = fields
        .next()
        .ok_or_else(|| io::Error::other("VmRSS omitted size"))?
        .parse::<u64>()?;
    if fields.next() != Some("kB") || fields.next().is_some() {
        return Err(io::Error::other("VmRSS used an unsupported unit/shape").into());
    }
    amount
        .checked_mul(1024)
        .ok_or_else(|| io::Error::other("VmRSS byte count overflowed").into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observation_budget_detects_stalls_before_the_whole_workload_expires() {
        let now = Instant::now();
        let overall = now.checked_add(RUN_LIMIT).unwrap();
        assert_eq!(
            observation_deadline(overall, now).unwrap(),
            now.checked_add(Duration::from_secs(5)).unwrap()
        );
    }

    #[test]
    fn observation_budget_never_extends_a_near_or_expired_aggregate_deadline() {
        let now = Instant::now();
        for overall in [
            now.checked_add(Duration::from_secs(2)).unwrap(),
            now,
            now.checked_sub(Duration::from_secs(1)).unwrap(),
        ] {
            assert_eq!(observation_deadline(overall, now).unwrap(), overall);
        }
    }

    #[test]
    fn sustained_result_oracle_rejects_echo_and_nonedge_hashes() {
        for text in ["> DONE", "❯ printf DONE", "DONE#", "DONE #"] {
            let mut screen = VirtualScreen::new(4, 40, 0).unwrap();
            screen.feed(text.as_bytes());
            assert!(!has_output(&screen, "DONE"));
        }
        for text in ["DONE".to_owned(), format!("DONE{}#", " ".repeat(35))] {
            let mut screen = VirtualScreen::new(4, 40, 0).unwrap();
            screen.feed(text.as_bytes());
            assert!(has_output(&screen, "DONE"));
        }
    }

    #[test]
    fn resident_memory_requires_the_proc_unit_and_bounded_integer() {
        assert_eq!(
            parse_resident_bytes("Name: quirl\nVmRSS: 4096 kB\n").unwrap(),
            4 * 1024 * 1024
        );
        for status in [
            "VmRSS: 1 MB",
            "VmRSS: 1 kB extra",
            "VmRSS: 18446744073709551615 kB",
            "Name: quirl",
        ] {
            assert!(parse_resident_bytes(status).is_err());
        }
    }

    #[test]
    fn warmed_resource_envelope_rejects_each_leak_domain() {
        let base = Resources {
            descriptors: 10,
            threads: 4,
            resident_bytes: 64 * 1024 * 1024,
            children: 0,
        };
        assert!(check_resources(Some(base), Some(base)).is_ok());
        assert!(
            check_resources(
                Some(base),
                Some(Resources {
                    descriptors: 18,
                    threads: 12,
                    resident_bytes: 96 * 1024 * 1024,
                    children: 0,
                })
            )
            .is_ok()
        );
        for observed in [
            Resources {
                descriptors: 19,
                ..base
            },
            Resources {
                threads: 13,
                ..base
            },
            Resources {
                resident_bytes: 96 * 1024 * 1024 + 1,
                ..base
            },
            Resources {
                children: 1,
                ..base
            },
        ] {
            assert!(check_resources(Some(base), Some(observed)).is_err());
        }
    }
}
