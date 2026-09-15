//! Bounded subprocess output — the shared half of the §6.1 output caps.
//!
//! Rusta spawns children in exactly two places: the `shell` tool (§6.12) and
//! the validators (§6.7). Both must bound *memory*, not merely the
//! observation text they end up showing — `wait_with_output()` reads a pipe
//! to EOF before any cap can apply, and a three-second `yes` measured
//! 5.26 GB of peak RSS, from a command no deny rule stops and `/auto`
//! approves. At the §7 defaults (60 s for shell, 600 s for validators) that
//! is an OOM.
//!
//! The mechanism lives here, once, because it carries an invariant that is
//! not obvious at either call site: [`capped_read`] keeps reading *past* the
//! cap and discards the excess rather than stopping. Stopping would leave
//! the child blocked on a full pipe until its timeout killed it, turning
//! every noisy-but-successful command into a timeout. A second copy could
//! lose that property silently, and only at the timeout boundary.
//!
//! The policy — how many bytes, and how the streams are combined — stays at
//! each call site, where it differs.

/// Drains `source` to EOF, keeping at most `cap` bytes.
///
/// Returns the kept prefix and whether anything was discarded. `None` for
/// `source` (a pipe the caller never opened) is an empty, untruncated read.
///
/// Callers must drain stdout and stderr **concurrently** — reading one to
/// EOF first deadlocks as soon as the other fills its kernel pipe buffer.
///
/// The kept prefix is cut on a byte count, so its tail may be a partial
/// UTF-8 sequence; convert with `String::from_utf8_lossy` or trim to a
/// boundary first.
pub async fn capped_read<R>(source: Option<R>, cap: usize) -> (Vec<u8>, bool)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt as _;

    let Some(mut source) = source else {
        return (Vec::new(), false);
    };
    let mut kept: Vec<u8> = Vec::new();
    let mut buffer = [0u8; 8192];
    let mut overflowed = false;
    loop {
        match source.read(&mut buffer).await {
            // A read error truncates the stream; treating it as a clean EOF
            // made a partial capture indistinguishable from a complete one.
            Err(_) => {
                overflowed = true;
                break;
            }
            Ok(0) => break,
            Ok(read) => {
                let room = cap.saturating_sub(kept.len());
                if read > room {
                    overflowed = true;
                }
                kept.extend_from_slice(&buffer[..read.min(room)]);
            }
        }
    }
    (kept, overflowed)
}

#[cfg(test)]
mod tests {
    use super::capped_read;

    #[tokio::test]
    async fn keeps_the_prefix_and_reports_the_overflow() {
        let data = vec![b'x'; 10_000];
        let (kept, overflowed) = capped_read(Some(&data[..]), 100).await;
        assert_eq!(kept.len(), 100);
        assert!(overflowed);

        let (kept, overflowed) = capped_read(Some(&b"short"[..]), 100).await;
        assert_eq!(kept, b"short");
        assert!(!overflowed);
    }

    #[tokio::test]
    async fn an_absent_pipe_is_an_empty_read() {
        let (kept, overflowed) = capped_read(None::<&[u8]>, 64).await;
        assert!(kept.is_empty());
        assert!(!overflowed);
    }

    /// The invariant this module exists for: the source is consumed to EOF
    /// even once the cap is full, so a child writing more than the cap never
    /// blocks on a full pipe waiting for a reader that has stopped.
    #[tokio::test]
    async fn drains_past_the_cap_rather_than_stopping() {
        let data = vec![b'y'; 50_000];
        let mut cursor = std::io::Cursor::new(data);
        let (kept, overflowed) = capped_read(Some(&mut cursor), 10).await;
        assert_eq!(kept.len(), 10);
        assert!(overflowed);
        assert_eq!(
            cursor.position(),
            50_000,
            "the source must be read to EOF, not abandoned at the cap"
        );
    }
}
