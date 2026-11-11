//! Reading files of an addon directory: untrusted input. Only regular files that are
//! not links (symbolic or hard) are read, without blocking on a FIFO swapped in after the checks,
//! and never more than a limit.

use std::fs::{File, Metadata, OpenOptions};
use std::io::Read;
use std::path::Path;

/// Open a file for reading. On Unix without following a final symbolic link and without blocking
/// on a FIFO swapped in after the checks (`O_NOFOLLOW | O_NONBLOCK`; non-blocking mode does not
/// change reads of a regular file).
pub(crate) fn open_nofollow(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    options.open(path)
}

/// The number of hard links of the file when it has more than one (a file of the addon that is
/// also reachable from outside it: a hard link cannot be told from the original by its path);
/// `None` for a file with one link and on platforms that do not say.
pub(crate) fn extra_hard_links(meta: &Metadata) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(meta.nlink()).filter(|links| *links > 1)
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

/// Read at most `limit` bytes of `file`; `Ok(None)` when it holds more.
pub(crate) fn read_capped(file: File, limit: u64) -> std::io::Result<Option<Vec<u8>>> {
    let cap = limit.saturating_add(1);
    let mut bytes = Vec::new();
    file.take(cap).read_to_end(&mut bytes)?;
    Ok((u64::try_from(bytes.len()).is_ok_and(|read| read <= limit)).then_some(bytes))
}
