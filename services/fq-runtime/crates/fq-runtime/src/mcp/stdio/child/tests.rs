//! Unit tests for the bounded stdio reader (#548). The transport half
//! is exercised end to end by `mcp::lifecycle`'s fault-injection tests,
//! which start a real child; what is worth isolating here is the bound
//! itself, because its interesting cases are about *where the bytes
//! arrive* rather than about MCP.

use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

use super::*;

const CAP: usize = 32;

async fn read_all(input: &'static [u8]) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    BufReader::new(LineCapped::new(input, CAP))
        .read_to_end(&mut out)
        .await?;
    Ok(out)
}

/// The counter is per line, not per stream: many lines, each within the
/// cap, pass however long the stream is.
#[tokio::test]
async fn every_line_within_the_cap_passes_however_many_there_are() {
    let input: &'static [u8] = b"one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\n";
    assert_eq!(read_all(input).await.expect("within the cap"), input);
}

/// A line past the cap fails the read rather than being buffered. This
/// is the fault the bound exists for: a server that streams without a
/// newline otherwise grows the daemon's memory for as long as it keeps
/// writing.
#[tokio::test]
async fn a_line_past_the_cap_fails_the_read() {
    let input: &'static [u8] = Box::leak(
        [&b"short\n"[..], &[b'x'; CAP + 1], b"\n"]
            .concat()
            .into_boxed_slice(),
    );
    let err = read_all(input).await.expect_err("must refuse");
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    assert!(err.to_string().contains("max_line_bytes"), "{err}");
}

/// Exactly the cap is allowed; the bound is on going *past* it, so the
/// documented number is a length a server may actually send.
#[tokio::test]
async fn a_line_of_exactly_the_cap_is_allowed() {
    let input: &'static [u8] = Box::leak([&[b'x'; CAP][..], b"\n"].concat().into_boxed_slice());
    assert_eq!(read_all(input).await.expect("at the cap").len(), CAP + 1);
}

/// Once tripped the reader stays tripped: the framing is already lost,
/// and the bytes that would resynchronise it are the ones being
/// refused. A caller that retried would otherwise resume mid-message.
#[tokio::test]
async fn the_refusal_is_terminal() {
    let mut reader = LineCapped::new(
        &b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nrecovered\n"[..],
        8,
    );
    let mut buf = [0u8; 64];
    assert_eq!(
        reader.read(&mut buf).await.expect_err("first read").kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(
        reader.read(&mut buf).await.expect_err("second read").kind(),
        io::ErrorKind::InvalidData,
        "a tripped reader must not hand out the rest of the stream"
    );
}

/// The bound is on the assembled line, not on one syscall: bytes that
/// arrive in many small reads still add up. A duplex delivers exactly
/// what is written, one write at a time, so this is the drip-feed
/// shape a slow server produces.
#[tokio::test]
async fn bytes_arriving_in_pieces_still_add_up_to_one_line() {
    let (mut writer, reader) = tokio::io::duplex(8);
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        for _ in 0..16 {
            if writer.write_all(b"xxxxxxxx").await.is_err() {
                return;
            }
        }
        let _ = writer.write_all(b"\n").await;
    });
    let mut line = Vec::new();
    let err = BufReader::new(LineCapped::new(reader, CAP))
        .read_until(b'\n', &mut line)
        .await
        .expect_err("128 bytes with no newline must be refused");
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
}
