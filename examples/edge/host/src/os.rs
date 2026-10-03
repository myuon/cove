//! The three things this demo asks of the operating system beyond std.

/// The process's resident set now, in KiB, by `ps` — the same instrument
/// `examples/rules/host`'s measurements read, so the numbers compare.
pub fn rss_kib() -> u64 {
    std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .and_then(|out| String::from_utf8_lossy(&out.stdout).trim().parse().ok())
        .unwrap_or(0)
}

/// The process's peak resident set, in KiB.
#[cfg(unix)]
pub fn peak_rss_kib() -> u64 {
    // Safety: `getrusage` writes the struct it is handed and nothing else.
    let usage = unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) != 0 {
            return 0;
        }
        usage
    };
    let max = usage.ru_maxrss as u64;
    // macOS reports bytes and Linux kilobytes.
    if cfg!(target_os = "macos") {
        max / 1024
    } else {
        max
    }
}

#[cfg(not(unix))]
pub fn peak_rss_kib() -> u64 {
    0
}

/// Raises the open-file limit as far as the hard limit allows, so that ten
/// thousand sockets fit without a `ulimit -n` first, and answers what it is.
#[cfg(unix)]
pub fn raise_open_files() -> u64 {
    // Safety: `getrlimit` and `setrlimit` read and write the struct handed
    // to them and nothing else.
    unsafe {
        let mut limit: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
            return 0;
        }
        // macOS refuses `RLIM_INFINITY` and anything above
        // `kern.maxfilesperproc`, so try a few ceilings, highest first.
        for want in [65_536, 24_576, 10_240] {
            let want = (want as libc::rlim_t).min(limit.rlim_max);
            if want <= limit.rlim_cur {
                break;
            }
            let raised = libc::rlimit {
                rlim_cur: want,
                rlim_max: limit.rlim_max,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &raised) == 0 {
                return want as u64;
            }
        }
        limit.rlim_cur as u64
    }
}

#[cfg(not(unix))]
pub fn raise_open_files() -> u64 {
    0
}
