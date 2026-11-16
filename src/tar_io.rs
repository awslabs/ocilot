//! Async tar framing. The `tar` crate is used only to encode and decode
//! 512-byte header blocks; every byte of I/O goes through tokio, so nothing
//! here blocks a runtime thread or needs a sync/async bridge.

// The reader half is only used by `Image::filesystem`, which needs `compression`.
#![cfg_attr(not(feature = "compression"), allow(dead_code))]

use std::io;
use std::path::{Path, PathBuf};

use tar::{Header, HeaderMode, PaxExtensions};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const BLOCK: u64 = 512;
/// GNU long-name/long-link and PAX records are buffered in memory; cap them so
/// a hostile header cannot force a huge allocation.
const MAX_EXTENSION: u64 = 1 << 20;

fn padded(size: u64) -> u64 {
    size.div_ceil(BLOCK) * BLOCK
}

fn invalid(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

async fn pad<W: AsyncWrite + Unpin + ?Sized>(out: &mut W, size: u64) -> io::Result<()> {
    let rem = (padded(size) - size) as usize;
    out.write_all(&[0; BLOCK as usize][..rem]).await
}

/// Write the end-of-archive marker and flush. The writer is not shut down.
pub(crate) async fn finish<W: AsyncWrite + Unpin + ?Sized>(out: &mut W) -> io::Result<()> {
    out.write_all(&[0; 2 * BLOCK as usize]).await?;
    out.flush().await
}

/// Append the contents of `src`, with paths relative to it. Each directory's
/// entries are sorted so the archive is reproducible; symlinks are followed,
/// matching `tar::Builder`.
pub(crate) async fn append_dir_all<W>(out: &mut W, src: &Path) -> io::Result<()>
where
    W: AsyncWrite + Unpin + ?Sized,
{
    let mut stack = vec![src.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut children = Vec::new();
        let mut entries = tokio::fs::read_dir(&dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            children.push(entry.path());
        }
        children.sort();
        let mut subdirs = Vec::new();
        for path in children {
            let meta = tokio::fs::metadata(&path).await?;
            let rel = path
                .strip_prefix(src)
                .map_err(|_| invalid("path outside source"))?;
            let mut header = Header::new_gnu();
            header.set_metadata_in_mode(&meta, HeaderMode::Complete);
            // GNU set_path rejects names over 100 bytes rather than truncating.
            header.set_path(rel)?;
            header.set_cksum();
            out.write_all(header.as_bytes()).await?;
            if meta.is_dir() {
                subdirs.push(path);
                continue;
            }
            let size = meta.len();
            let mut file = tokio::fs::File::open(&path).await?.take(size);
            if tokio::io::copy(&mut file, out).await? != size {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            pad(out, size).await?;
        }
        // Reversed so subdirectories are visited in sorted order.
        stack.extend(subdirs.into_iter().rev());
    }
    Ok(())
}

/// One member of an archive, with any GNU/PAX extension records that
/// precede it. Its data (`data_len` bytes) is still unread in the stream.
pub(crate) struct Member {
    /// Extension records plus the member's own header block(s), verbatim.
    pub raw_header: Vec<u8>,
    pub header: Header,
    /// Path after applying GNU long-name / PAX `path`.
    pub path: PathBuf,
    /// Bytes of data following `raw_header`, padding included.
    pub data_len: u64,
}

/// Read the next member, or `None` at end of archive. The caller must
/// consume exactly `data_len` bytes before calling again.
pub(crate) async fn next_member<R>(src: &mut R) -> io::Result<Option<Member>>
where
    R: AsyncRead + Unpin + ?Sized,
{
    let mut raw = Vec::new();
    let mut long_name: Option<Vec<u8>> = None;
    let mut pax_path: Option<Vec<u8>> = None;
    let mut pax_size: Option<u64> = None;
    loop {
        let mut block = [0u8; BLOCK as usize];
        if !read_block(src, &mut block).await? || block.iter().all(|b| *b == 0) {
            return match raw.is_empty() {
                true => Ok(None),
                false => Err(invalid("extension record without a member")),
            };
        }
        let header = Header::from_byte_slice(&block).clone();
        verify_checksum(&header)?;
        raw.extend_from_slice(&block);
        let kind = header.entry_type();
        if kind.is_gnu_longname() || kind.is_gnu_longlink() || kind.is_pax_local_extensions() {
            let size = header.entry_size()?;
            if size > MAX_EXTENSION {
                return Err(invalid("extension record too large"));
            }
            let start = raw.len();
            raw.resize(start + padded(size) as usize, 0);
            src.read_exact(&mut raw[start..]).await?;
            let data = &raw[start..start + size as usize];
            if kind.is_gnu_longname() {
                long_name = Some(trim_nul(data).to_vec());
            } else if kind.is_pax_local_extensions() {
                for ext in PaxExtensions::new(data) {
                    let ext = ext?;
                    match ext.key_bytes() {
                        b"path" => pax_path = Some(ext.value_bytes().to_vec()),
                        b"size" => {
                            let v = ext.value().map_err(|_| invalid("bad pax size"))?;
                            pax_size = Some(v.parse().map_err(|_| invalid("bad pax size"))?);
                        }
                        _ => {}
                    }
                }
            }
            continue;
        }
        if kind.is_gnu_sparse() {
            // Old-GNU sparse maps can continue into extra 512-byte blocks;
            // byte 504 of each one flags whether another follows.
            let mut extended = header.as_gnu().is_some_and(|g| g.is_extended());
            while extended {
                let start = raw.len();
                raw.resize(start + BLOCK as usize, 0);
                src.read_exact(&mut raw[start..]).await?;
                extended = raw[start + 504] == 1;
            }
        }
        let name = long_name
            .or(pax_path)
            .unwrap_or_else(|| header.path_bytes().into_owned());
        let path = PathBuf::from(String::from_utf8_lossy(&name).into_owned());
        let size = match pax_size {
            Some(size) => size,
            None => header.entry_size()?,
        };
        return Ok(Some(Member {
            raw_header: raw,
            header,
            path,
            data_len: padded(size),
        }));
    }
}

/// Copy every member for which `keep` returns true from `src` to `out`
/// byte-for-byte. A skipped member's extension records are dropped with it,
/// so they can't attach to the next member written.
pub(crate) async fn copy_members<R, W, F>(src: &mut R, out: &mut W, mut keep: F) -> io::Result<()>
where
    R: AsyncRead + Unpin + ?Sized,
    W: AsyncWrite + Unpin + ?Sized,
    F: FnMut(&Member) -> bool,
{
    while let Some(member) = next_member(src).await? {
        let mut data = (&mut *src).take(member.data_len);
        let copied = if keep(&member) {
            out.write_all(&member.raw_header).await?;
            tokio::io::copy(&mut data, out).await?
        } else {
            tokio::io::copy(&mut data, &mut tokio::io::sink()).await?
        };
        if copied != member.data_len {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
    }
    Ok(())
}

/// Fill `block`; `false` on a clean EOF before any byte was read.
async fn read_block<R>(src: &mut R, block: &mut [u8]) -> io::Result<bool>
where
    R: AsyncRead + Unpin + ?Sized,
{
    let mut filled = 0;
    while filled < block.len() {
        match src.read(&mut block[filled..]).await? {
            0 if filled == 0 => return Ok(false),
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            n => filled += n,
        }
    }
    Ok(true)
}

fn verify_checksum(header: &Header) -> io::Result<()> {
    let bytes = header.as_bytes();
    let sum = bytes[..148]
        .iter()
        .chain(&bytes[156..])
        .map(|b| u32::from(*b))
        .sum::<u32>()
        + 8 * 32;
    if sum != header.cksum()? {
        return Err(invalid("archive header checksum mismatch"));
    }
    Ok(())
}

fn trim_nul(data: &[u8]) -> &[u8] {
    data.iter()
        .position(|b| *b == 0)
        .map_or(data, |end| &data[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn names(bytes: &[u8]) -> Vec<(String, String, String)> {
        let mut archive = tar::Archive::new(bytes);
        archive
            .entries()
            .unwrap()
            .map(|e| {
                let mut e = e.unwrap();
                let path = e.path().unwrap().display().to_string();
                let link = e
                    .link_name()
                    .unwrap()
                    .map(|l| l.display().to_string())
                    .unwrap_or_default();
                let mut body = String::new();
                e.read_to_string(&mut body).unwrap();
                (path, link, body)
            })
            .collect()
    }

    fn fixture() -> (Vec<u8>, String, String) {
        let long = format!("usr/lib/{}/file.txt", "d".repeat(150));
        let target = format!("/opt/{}", "t".repeat(150));
        let mut b = tar::Builder::new(Vec::new());
        let mut h = Header::new_gnu();
        h.set_size(5);
        h.set_mode(0o644);
        b.append_data(&mut h, &long, &b"hello"[..]).unwrap();
        let mut h = Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        b.append_link(&mut h, "usr/bin/link", &target).unwrap();
        // PAX path + a long-named member that will be skipped.
        let pax_path = format!("var/{}/pax.txt", "p".repeat(150));
        b.append_pax_extensions([("path", pax_path.as_bytes())])
            .unwrap();
        let mut h = Header::new_ustar();
        h.set_path("var/short-pax").unwrap();
        h.set_size(3);
        h.set_cksum();
        b.append(&h, &b"pax"[..]).unwrap();
        let skipped = format!("skip/{}", "s".repeat(150));
        let mut h = Header::new_gnu();
        h.set_size(4);
        b.append_data(&mut h, &skipped, &b"gone"[..]).unwrap();
        let mut h = Header::new_gnu();
        h.set_size(4);
        b.append_data(&mut h, "after", &b"kept"[..]).unwrap();
        (b.into_inner().unwrap(), long, target)
    }

    #[tokio::test]
    async fn copy_preserves_extensions_and_drops_skipped() {
        let (input, long, target) = fixture();
        let mut out = Vec::new();
        copy_members(&mut input.as_slice(), &mut out, |m| {
            !m.path.starts_with("skip")
        })
        .await
        .unwrap();
        finish(&mut out).await.unwrap();
        let got = names(&out);
        let pax = format!("var/{}/pax.txt", "p".repeat(150));
        assert_eq!(
            got,
            vec![
                (long, String::new(), "hello".into()),
                ("usr/bin/link".into(), target, String::new()),
                (pax, String::new(), "pax".into()),
                ("after".into(), String::new(), "kept".into()),
            ]
        );
    }

    #[tokio::test]
    async fn truncated_member_errors() {
        let (input, ..) = fixture();
        // Drop the end marker and part of the last member's data block.
        let cut = &input[..input.len() - 1024 - 300];
        let res = copy_members(&mut &cut[..], &mut tokio::io::sink(), |_| true).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn dir_matches_tar_builder() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("oci-layout"), b"{}").unwrap();
        std::fs::create_dir_all(dir.path().join("blobs/sha256")).unwrap();
        std::fs::write(
            dir.path().join("blobs/sha256").join("a".repeat(64)),
            b"blob",
        )
        .unwrap();
        std::fs::write(dir.path().join("index.json"), b"[]").unwrap();

        let mut ours = Vec::new();
        append_dir_all(&mut ours, dir.path()).await.unwrap();
        finish(&mut ours).await.unwrap();

        let mut theirs = tar::Builder::new(Vec::new());
        theirs.append_dir_all(".", dir.path()).unwrap();
        let theirs = theirs.into_inner().unwrap();

        let mut a = names(&ours);
        let mut b = names(&theirs);
        b.retain(|(p, ..)| p != "./" && p != ".");
        a.sort();
        b.sort();
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn dir_rejects_names_too_long_for_header() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("n".repeat(120)), b"x").unwrap();
        let res = append_dir_all(&mut tokio::io::sink(), dir.path()).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn pax_size_overrides_header_size() {
        let mut b = tar::Builder::new(Vec::new());
        b.append_pax_extensions([("size", &b"7"[..])]).unwrap();
        let mut h = Header::new_ustar();
        h.set_path("big").unwrap();
        h.set_size(0);
        h.set_cksum();
        b.append(&h, &b"payload"[..]).unwrap();
        let input = b.into_inner().unwrap();
        let mut out = Vec::new();
        copy_members(&mut input.as_slice(), &mut out, |_| true)
            .await
            .unwrap();
        finish(&mut out).await.unwrap();
        assert_eq!(
            names(&out),
            vec![("big".into(), String::new(), "payload".into())]
        );
    }

    #[tokio::test]
    async fn oversized_extension_record_errors() {
        let mut h = Header::new_gnu();
        h.set_path("././@LongLink").unwrap();
        h.set_entry_type(tar::EntryType::GNULongName);
        h.set_size(MAX_EXTENSION + 1);
        h.set_cksum();
        let err = next_member(&mut &h.as_bytes()[..]).await.err().unwrap();
        assert!(err.to_string().contains("too large"), "{err}");
    }
}
