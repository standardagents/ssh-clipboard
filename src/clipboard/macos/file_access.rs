use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;

use anyhow::{Context, Result, bail};
use block2::RcBlock;
use objc2_foundation::{NSFileCoordinator, NSFileCoordinatorReadingOptions, NSString, NSURL};

/// Coordinate reads with iCloud/File Provider and document writers. In
/// particular, a launchd process must not rely on plain POSIX reads hydrating
/// dataless files. The accessor is synchronous and runs off the async executor.
pub(crate) fn coordinated_read<T>(path: &Path, read: impl FnOnce(&Path) -> Result<T>) -> Result<T> {
    let coordinator = NSFileCoordinator::new();
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
    let result = RefCell::new(None);
    let read = RefCell::new(Some(read));
    let accessor = RcBlock::new(|url: NonNull<NSURL>| {
        // SAFETY: Foundation supplies a valid URL for the duration of this
        // synchronous accessor. Nothing borrowed from it escapes the callback.
        #[allow(unsafe_code)]
        let url = unsafe { url.as_ref() };
        let value = (|| {
            let path = url.path().context("resolve coordinated file path")?;
            let read = read.borrow_mut().take().context("file accessor invoked twice")?;
            read(&PathBuf::from(path.to_string()))
        })();
        *result.borrow_mut() = Some(value);
    });
    let mut error = None;
    coordinator.coordinateReadingItemAtURL_options_error_byAccessor(
        &url,
        NSFileCoordinatorReadingOptions::WithoutChanges,
        Some(&mut error),
        &accessor,
    );
    if let Some(error) = error {
        bail!("coordinate copied file {}: {error}", path.display());
    }
    drop(accessor);
    result
        .into_inner()
        .context("file coordinator did not provide access")?
}
