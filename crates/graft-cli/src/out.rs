//! Standard output. Every line goes through these macros, which write to a
//! locked stdout and return an error instead of panicking, so a reader that
//! closes early (`graft log | head`) ends the command quietly: the error
//! converts to a silent success (see `error::Failure`'s `From<io::Error>`).

/// Writes one line to standard output.
macro_rules! outln {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        writeln!(std::io::stdout().lock(), $($arg)*)?;
    }};
}

/// Writes text to standard output, with no newline added.
macro_rules! out {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        write!(std::io::stdout().lock(), $($arg)*)?;
    }};
}
