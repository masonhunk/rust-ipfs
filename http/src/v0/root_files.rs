use crate::v0::support::unshared::Unshared;
use crate::v0::support::{with_ipfs, StreamResponse, StringError};
use ipfs::{Ipfs, IpfsTypes};
use libipld::cid::{Cid, Codec};
use serde::Deserialize;
use std::borrow::Cow;
use std::convert::TryFrom;
use std::fmt;
use std::path::{PathBuf, Path};
use warp::{path, query, Filter, Rejection, Reply};
use bytes::{Bytes, BytesMut, buf::BufMut};
use tar::{Header, EntryType};
use futures::stream::TryStream;
use ipfs::unixfs::ll::file::FileMetadata;
use ipfs::unixfs::{ll::file::FileReadFailed, TraversalFailed, ll::file::visit::Cache};
use crate::v0::refs::{walk_path, IpfsPath};
use ipfs::unixfs::ll::dir::walk::{self, Walker, ContinuedWalk};
use ipfs::Block;
use async_stream::try_stream;

#[derive(Debug, Deserialize)]
pub struct CatArgs {
    // this could be an ipfs path
    arg: String,
    offset: Option<u64>,
    length: Option<u64>,
    // timeout: Option<?> // added in latest iterations
}

pub fn cat<T: IpfsTypes>(
    ipfs: &Ipfs<T>,
) -> impl Filter<Extract = impl Reply, Error = Rejection> + Clone {
    path!("cat")
        .and(with_ipfs(ipfs))
        .and(query::<CatArgs>())
        .and_then(cat_inner)
}

async fn cat_inner<T: IpfsTypes>(ipfs: Ipfs<T>, args: CatArgs) -> Result<impl Reply, Rejection> {

    let mut path = IpfsPath::try_from(args.arg.as_str()).map_err(StringError::from)?;
    path.set_follow_dagpb_data(false);

    let range = match (args.offset, args.length) {
        (Some(start), Some(len)) => Some(start..(start + len)),
        (Some(_start), None) => todo!("need to abstract over the range"),
        (None, Some(len)) => Some(0..len),
        (None, None) => None,
    };

    // FIXME: this is here until we have IpfsPath back at ipfs

    let (cid, _, _) = walk_path(&ipfs, path).await.map_err(StringError::from)?;

    if cid.codec() != Codec::DagProtobuf {
        return Err(StringError::from("unknown node type").into());
    }

    // TODO: timeout
    let stream = match ipfs::unixfs::cat(ipfs, cid, range).await {
        Ok(stream) => stream,
        Err(TraversalFailed::Walking(_, FileReadFailed::UnexpectedType(ut)))
            if ut.is_directory() =>
        {
            return Err(StringError::from("this dag node is a directory").into())
        }
        Err(e) => return Err(StringError::from(e).into()),
    };

    Ok(StreamResponse(Unshared::new(stream)))
}

#[derive(Deserialize)]
struct GetArgs {
    // this could be an ipfs path again
    arg: String,
}

pub fn get<T: IpfsTypes>(
    ipfs: &Ipfs<T>,
) -> impl Filter<Extract = impl Reply, Error = Rejection> + Clone {
    path!("get")
        .and(with_ipfs(ipfs))
        .and(query::<GetArgs>())
        .and_then(get_inner)
}

async fn get_inner<T: IpfsTypes>(ipfs: Ipfs<T>, args: GetArgs) -> Result<impl Reply, Rejection> {
    use futures::stream::TryStreamExt;

    let mut path = IpfsPath::try_from(args.arg.as_str()).map_err(StringError::from)?;
    path.set_follow_dagpb_data(false);

    // FIXME: this is here until we have IpfsPath back at ipfs
    let (cid, _, _) = walk_path(&ipfs, path).await.map_err(StringError::from)?;

    if cid.codec() != Codec::DagProtobuf {
        return Err(StringError::from("unknown node type").into());
    }

    Ok(StreamResponse(Unshared::new(walk(ipfs, cid).into_stream())))
}

fn walk<Types: IpfsTypes>(ipfs: Ipfs<Types>, root: Cid)
    -> impl TryStream<Ok = Bytes, Error = GetError> + 'static
{
    let mut cache: Option<Cache> = None;
    let mut tar_helper = TarHelper::with_buffer_sizes(16 * 1024);

    let mut root = Some(root);
    let mut maybe_walker: Option<Walker> = None;

    try_stream! {
        loop {
            // this mangling with the root and maybe_walker looks like this mainly because
            //
            // a) I could not come up with a simpler solution, as we cannot refactor the big match
            // to a function given the non-uniform yield points
            //
            // b) adding more code here, like with an tri-state enum, goes quickly over the current
            // #[recursion_limit = "512"], which is required due to how async_stream needs to parse
            // this function (tt-muncher).
            //
            // the next will be the root on first round, and pending_links on the next rounds. root
            // is read with `as_ref` and later dropped by assigning `None` to it on every
            // iteration.
            let next = root.as_ref()
                .or_else(|| maybe_walker.as_ref().map(|w| w.pending_links().0));

            // we either have a cid reference, or we are done
            let next = match next {
                Some(cid_ref) => cid_ref,
                None => return,
            };

            let Block { data, .. } = ipfs.get_block(next).await?;

            let res = match maybe_walker {
                None => {
                    // the HTTP api uses the final Cid name as the root name in the generated tar
                    // archive; it will be copied to Walker internally so it can be temporary.
                    let root_name = next.to_string();
                    Walker::start(&data, &root_name, &mut cache)?
                },
                Some(walker) => walker.continue_walk(&data, &mut cache)?,
            };

            // make sure only first round uses the `root` cid.
            root = None;

            let next_walker = match res {
                ContinuedWalk::File(segment, item) => {
                    let total_size = item.as_entry()
                        .total_file_size()
                        .expect("files do have total_size");

                    if segment.is_first() {
                        let path = item.as_entry().path();
                        let metadata = item
                            .as_entry()
                            .metadata()
                            .expect("files must have metadata");

                        for mut bytes in tar_helper.apply_file(path, metadata, total_size)?.iter_mut() {
                            if let Some(bytes) = bytes.take() {
                                yield bytes;
                            }
                        }
                    }

                    // even if the largest of files can have 256 kB blocks and about the same
                    // amount of content, try to consume it in small parts not to grow the buffers
                    // too much.

                    let mut n = 0usize;
                    let slice = segment.as_ref();
                    let total = slice.len();

                    while n < total {
                        let next = tar_helper.buffer_file_contents(&slice[n..]);
                        n += next.len();
                        yield next;
                    }

                    if segment.is_last() {
                        if let Some(zeroes) = tar_helper.pad(total_size) {
                            yield zeroes;
                        }
                    }

                    item.into_inner()
                },
                ContinuedWalk::Directory(item) => {

                    // only first instances of directorys will have the metadata
                    if let Some(metadata) = item.as_entry().metadata() {
                        let path = item.as_entry().path();

                        // TODO: this is still wrong
                        assert_ne!(path, Path::new(""), "had metadata but name was empty");

                        for mut bytes in tar_helper.apply_directory(path, metadata)?.iter_mut() {
                            if let Some(bytes) = bytes.take() {
                                yield bytes;
                            }
                        }
                    }

                    item.into_inner()
                },
                ContinuedWalk::Symlink(bytes, item) => {
                    let path = item.as_entry().path();
                    let target = std::str::from_utf8(bytes).map_err(|_| GetError::NonUtf8Symlink)?;
                    let target = Path::new(target);
                    let metadata = item.as_entry().metadata().expect("symlink must have metadata");

                    for mut bytes in tar_helper.apply_symlink(path, target, metadata)?.iter_mut() {
                        if let Some(bytes) = bytes.take() {
                            yield bytes;
                        }
                    }

                    item.into_inner()
                },
            };

            maybe_walker = next_walker;
        }
    }
}

#[derive(Debug)]
enum GetError {
    NonUtf8Symlink,
    InvalidFileName(Vec<u8>),
    Walk(walk::Error),
    Loading(ipfs::Error),
}

impl From<ipfs::Error> for GetError {
    fn from(e: ipfs::Error) -> Self {
        GetError::Loading(e)
    }
}

impl From<walk::Error> for GetError {
    fn from(e: walk::Error) -> Self {
        GetError::Walk(e)
    }
}

impl fmt::Display for GetError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        use GetError::*;
        match self {
            NonUtf8Symlink => write!(fmt, "symlink target could not be converted to utf-8"),
            Walk(e) => write!(fmt, "{}", e),
            Loading(e) => write!(fmt, "loading failed: {}", e),
            InvalidFileName(x) => write!(fmt, "filename cannot be put inside tar: {:?}", x),
        }
    }
}

impl std::error::Error for GetError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            GetError::Walk(e) => Some(e),
            _ => None,
        }
    }
}

/// Tar helper is internal to `get` implementation. It uses some private parts of the `tar-rs`
/// crate to append the headers and the contents to a pair of `bytes::Bytes` operated in a
/// round-robin fashion.
struct TarHelper {
    bufsize: usize,
    written: BytesMut,
    other: BytesMut,
    header: Header,
    long_filename_header: Header,
    zeroes: Bytes,
}

impl TarHelper {
    pub fn with_buffer_sizes(n: usize) -> Self {
        let written = BytesMut::with_capacity(n);
        let other = BytesMut::with_capacity(n);

        // these are 512 a piece
        let header = Self::new_default_header();
        let long_filename_header = Self::new_long_filename_header();
        let mut zeroes = BytesMut::with_capacity(512);
        for _ in 0..(512/8) {
            zeroes.put_u64(0);
        }
        assert_eq!(zeroes.len(), 512);
        let zeroes = zeroes.freeze();

        Self {
            bufsize: n,
            written,
            other,
            header,
            long_filename_header,
            zeroes,
        }
    }

    fn new_default_header() -> tar::Header {
        let mut header = tar::Header::new_gnu();
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);

        header
    }

    fn new_long_filename_header() -> tar::Header {
        let mut long_filename_header = tar::Header::new_gnu();
        long_filename_header.set_mode(0o644);

        {
            let name = b"././@LongLink";
            let gnu_header = long_filename_header.as_gnu_mut().unwrap();
            // since we are reusing the header, zero out all of the bytes
            let written = name.iter().copied().chain(std::iter::repeat(0)).enumerate().take(gnu_header.name.len());
            // FIXME: could revert back to the slice copying code since we never change this
            for (i, b) in written {
                gnu_header.name[i] = b;
            }
        }

        long_filename_header.set_mtime(0);
        long_filename_header.set_uid(0);
        long_filename_header.set_gid(0);

        long_filename_header
    }

    fn apply_file(&mut self, path: &Path, metadata: &FileMetadata, total_size: u64) -> Result<[Option<Bytes>; 4], GetError> {
        let mut ret: [Option<Bytes>; 4] = Default::default();

        if let Err(e) = self.header.set_path(path) {
            let data = prepare_long_header(&mut self.header, &mut self.long_filename_header, path, e)?;

            self.written.put_slice(self.long_filename_header.as_bytes());
            ret[0] = Some(self.written.split().freeze());
            std::mem::swap(&mut self.written, &mut self.other);

            self.written.put_slice(data);
            self.written.put_u8(0);
            ret[1] = Some(self.written.split().freeze());
            std::mem::swap(&mut self.written, &mut self.other);

            ret[2] = self.pad(data.len() as u64 + 1);
        }

        self.header.set_size(total_size);
        self.header.set_entry_type(EntryType::Regular);
        Self::set_metadata(&mut self.header, metadata, 0o0644);
        self.header.set_cksum();

        self.written.put_slice(self.header.as_bytes());

        ret[3] = Some(self.written.split().freeze());
        std::mem::swap(&mut self.written, &mut self.other);

        Ok(ret)
    }

    fn buffer_file_contents(&mut self, contents: &[u8]) -> Bytes {
        assert!(!contents.is_empty());
        let remaining = contents.len();
        let taken = self.bufsize.min(remaining);

        // was initially thinking to check the capacity but we are round robining the buffers to
        // get a lucky chance at either of them being empty at this point
        self.written.put_slice(&contents[..taken]);
        let ret = self.written.split().freeze();
        std::mem::swap(&mut self.written, &mut self.other);
        ret
    }

    fn apply_directory(&mut self, path: &Path, metadata: &FileMetadata) -> Result<[Option<Bytes>; 4], GetError> {
        let mut ret: [Option<Bytes>; 4] = Default::default();

        if let Err(e) = self.header.set_path(path) {
            let data = prepare_long_header(&mut self.header, &mut self.long_filename_header, path, e)?;

            self.written.put_slice(self.long_filename_header.as_bytes());
            ret[0] = Some(self.written.split().freeze());
            std::mem::swap(&mut self.written, &mut self.other);

            self.written.put_slice(data);
            self.written.put_u8(0);
            ret[1] = Some(self.written.split().freeze());
            std::mem::swap(&mut self.written, &mut self.other);

            ret[2] = self.pad(data.len() as u64 + 1);
        }

        self.header.set_size(0);
        self.header.set_entry_type(EntryType::Directory);
        Self::set_metadata(&mut self.header, metadata, 0o0755);

        self.header.set_cksum();
        self.written.put_slice(self.header.as_bytes());

        ret[3] = Some(self.written.split().freeze());
        std::mem::swap(&mut self.written, &mut self.other);

        Ok(ret)
    }

    fn apply_symlink(&mut self, path: &Path, target: &Path, metadata: &FileMetadata) -> Result<[Option<Bytes>; 7], GetError> {
        let mut ret: [Option<Bytes>; 7] = Default::default();

        if let Err(e) = self.header.set_path(path) {
            let data = prepare_long_header(&mut self.header, &mut self.long_filename_header, path, e)?;

            self.written.put_slice(self.long_filename_header.as_bytes());
            ret[0] = Some(self.written.split().freeze());
            std::mem::swap(&mut self.written, &mut self.other);

            self.written.put_slice(data);
            self.written.put_u8(0);
            ret[1] = Some(self.written.split().freeze());
            std::mem::swap(&mut self.written, &mut self.other);

            ret[2] = self.pad(data.len() as u64 + 1);
        }

        if let Err(e) = self.header.set_link_name(target) {
            let data = path2bytes(target);

            if data.len() < self.header.as_old().linkname.len() {
                // this might be an /ipfs/QmFoo which we should error and not allow
                panic!("invalid link target: {:?} ({})", target, e)
            }

            self.long_filename_header.set_size(data.len() as u64 + 1);
            self.long_filename_header.set_entry_type(tar::EntryType::new(b'K'));
            self.long_filename_header.set_cksum();

            self.written.put_slice(self.long_filename_header.as_bytes());
            ret[3] = Some(self.written.split().freeze());
            std::mem::swap(&mut self.written, &mut self.other);

            self.written.put_slice(data);
            self.written.put_u8(0);
            ret[4] = Some(self.written.split().freeze());
            std::mem::swap(&mut self.written, &mut self.other);

            ret[5] = self.pad(data.len() as u64 + 1);
        }

        Self::set_metadata(&mut self.header, metadata, 0o0644);
        self.header.set_size(0);
        self.header.set_entry_type(tar::EntryType::Symlink);
        self.header.set_cksum();

        self.written.put_slice(self.header.as_bytes());
        ret[6] = Some(self.written.split().freeze());
        std::mem::swap(&mut self.written, &mut self.other);

        Ok(ret)
    }

    pub fn pad(&self, total_size: u64) -> Option<Bytes> {
        let padding = 512 - (total_size % 512);
        if padding < 512 {
            Some(self.zeroes.slice(..padding as usize))
        } else {
            None
        }
    }

    fn set_metadata(header: &mut tar::Header, metadata: &FileMetadata, default_mode: u32) {
        header.set_mode(metadata.mode()
            .map(|mode| mode & 0o7777)
            .unwrap_or(default_mode));

        header.set_mtime(metadata.mtime()
            .and_then(|(seconds, _)| if seconds >= 0 { Some(seconds as u64) } else { None })
            .unwrap_or(0));
    }
}

/// Returns the raw bytes we need to write as a new entry into the tar
fn prepare_long_header<'a>(header: &mut tar::Header, long_filename_header: &mut tar::Header, path: &'a Path, _error: std::io::Error) -> Result<&'a [u8], GetError> {

    #[cfg(unix)]
    /// On unix this operation can never fail.
    pub fn bytes2path(bytes: Cow<[u8]>) -> std::io::Result<Cow<Path>> {
        use std::ffi::{OsStr, OsString};
        use std::os::unix::prelude::*;

        Ok(match bytes {
            Cow::Borrowed(bytes) => Cow::Borrowed(Path::new(OsStr::from_bytes(bytes))),
            Cow::Owned(bytes) => Cow::Owned(PathBuf::from(OsString::from_vec(bytes))),
        })
    }

    #[cfg(windows)]
    /// On windows we cannot accept non-Unicode bytes because it
    /// is impossible to convert it to UTF-16.
    pub fn bytes2path(bytes: Cow<[u8]>) -> std::io::Result<Cow<Path>> {
        use std::ffi::{OsStr, OsString};
        use std::os::windows::prelude::*;

        return match bytes {
            Cow::Borrowed(bytes) => {
                let s = str::from_utf8(bytes).map_err(|_| not_unicode(bytes))?;
                Ok(Cow::Borrowed(Path::new(s)))
            }
            Cow::Owned(bytes) => {
                let s = String::from_utf8(bytes).map_err(|uerr| not_unicode(&uerr.into_bytes()))?;
                Ok(Cow::Owned(PathBuf::from(s)))
            }
        };

        fn not_unicode(v: &[u8]) -> io::Error {
            other(&format!(
                "only Unicode paths are supported on Windows: {}",
                String::from_utf8_lossy(v)
            ))
        }
    }

    // we **only** have utf8 paths as protobuf has already parsed this file
    // name and all of the previous as utf8.

    let data = path2bytes(path);

    let max = header.as_old().name.len();

    if data.len() < max {
        return Err(GetError::InvalidFileName(data.to_vec()));
    }

    // the plus one is documented as compliance to GNU tar, probably the null byte
    // termination?
    long_filename_header.set_size(data.len() as u64 + 1);
    long_filename_header.set_entry_type(tar::EntryType::new(b'L'));
    long_filename_header.set_cksum();

    // we still need to figure out the truncated path we put into the header
    let path = bytes2path(Cow::Borrowed(&data[..max]))
        .expect("quite certain we have no non-utf8 paths here");
    header.set_path(&path)
        .expect("we already made sure the path is of fitting length");

    Ok(data)
}

#[cfg(unix)]
fn path2bytes(p: &Path) -> &[u8] {
    use std::os::unix::prelude::*;
    p.as_os_str().as_bytes()
}

#[cfg(windows)]
fn path2bytes(p: &Path) -> &[u8] {
    use std::os::windows::prelude::*;
    p.as_os_str()
        .to_str()
        .expect("we should only have unicode compatible bytes even on windows")
        .as_bytes()
}
