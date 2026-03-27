use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use russh_sftp::{
    client::SftpSession,
    protocol::{Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode},
    server,
};
use tokio::{
    io::{duplex, AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    sync::Mutex,
};

#[derive(Clone, Default)]
struct TestBackend {
    files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    next_handle: Arc<AtomicUsize>,
}

#[derive(Clone)]
struct TestHandler {
    backend: TestBackend,
    handles: Arc<Mutex<HashMap<String, String>>>,
}

impl TestHandler {
    fn new(backend: TestBackend) -> Self {
        Self {
            backend,
            handles: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl server::Handler for TestHandler {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        _attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let mut files = self.backend.files.lock().await;
        let entry = files.entry(filename.clone()).or_default();

        if pflags.contains(OpenFlags::TRUNCATE) {
            entry.clear();
        }

        let handle_id = self.backend.next_handle.fetch_add(1, Ordering::SeqCst);
        let handle = format!("handle-{handle_id}");
        self.handles.lock().await.insert(handle.clone(), filename);

        Ok(Handle { id, handle })
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        self.handles.lock().await.remove(&handle);
        Ok(ok_status(id))
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        let path = self
            .handles
            .lock()
            .await
            .get(&handle)
            .cloned()
            .ok_or(StatusCode::NoSuchFile)?;

        let files = self.backend.files.lock().await;
        let data = files.get(&path).ok_or(StatusCode::NoSuchFile)?;
        let start = offset as usize;

        if start >= data.len() {
            return Err(StatusCode::Eof);
        }

        let end = (start + len as usize).min(data.len());
        Ok(Data {
            id,
            data: data[start..end].to_vec(),
        })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        let path = self
            .handles
            .lock()
            .await
            .get(&handle)
            .cloned()
            .ok_or(StatusCode::NoSuchFile)?;

        let mut files = self.backend.files.lock().await;
        let file = files.get_mut(&path).ok_or(StatusCode::NoSuchFile)?;
        let start = offset as usize;
        let end = start + data.len();

        if file.len() < end {
            file.resize(end, 0);
        }

        file[start..end].copy_from_slice(&data);
        Ok(ok_status(id))
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        let path = self
            .handles
            .lock()
            .await
            .get(&handle)
            .cloned()
            .ok_or(StatusCode::NoSuchFile)?;

        let files = self.backend.files.lock().await;
        let size = files.get(&path).ok_or(StatusCode::NoSuchFile)?.len() as u64;

        Ok(Attrs {
            id,
            attrs: FileAttributes {
                size: Some(size),
                ..FileAttributes::default()
            },
        })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let files = self.backend.files.lock().await;
        let size = files.get(&path).ok_or(StatusCode::NoSuchFile)?.len() as u64;

        Ok(Attrs {
            id,
            attrs: FileAttributes {
                size: Some(size),
                ..FileAttributes::default()
            },
        })
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        self.backend.files.lock().await.remove(&filename);
        Ok(ok_status(id))
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        Ok(Name {
            id,
            files: vec![File::dummy(path)],
        })
    }
}

fn ok_status(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "Ok".to_owned(),
        language_tag: "en-US".to_owned(),
    }
}

async fn test_session(backend: TestBackend) -> SftpSession {
    let (client_stream, server_stream) = duplex(64 * 1024);
    server::run(server_stream, TestHandler::new(backend)).await;
    SftpSession::new(client_stream).await.unwrap()
}

#[tokio::test]
async fn write_at_persists_two_disjoint_ranges() {
    let session = test_session(TestBackend::default()).await;
    let file = session
        .open_random_access_with_flags(
            "/ranges.bin",
            OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::READ | OpenFlags::WRITE,
        )
        .await
        .unwrap();

    file.write_at(0, b"hello").await.unwrap();
    file.write_at(10, b"world").await.unwrap();

    assert_eq!(file.read_at(0, 15).await.unwrap(), b"hello\0\0\0\0\0world");
}

#[tokio::test]
async fn read_at_returns_requested_range_without_mutating_seek_position() {
    let session = test_session(TestBackend::default()).await;
    let mut file = session
        .open_with_flags(
            "/seek.bin",
            OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::READ | OpenFlags::WRITE,
        )
        .await
        .unwrap();

    file.write_all(b"0123456789abcdef").await.unwrap();
    file.rewind().await.unwrap();
    let slice = file.read_at(4, 4).await.unwrap();
    let position = file.stream_position().await.unwrap();
    let mut sequential = [0_u8; 4];
    file.read_exact(&mut sequential).await.unwrap();

    assert_eq!(slice, b"4567");
    assert_eq!(position, 0);
    assert_eq!(&sequential, b"0123");
}

#[tokio::test]
async fn concurrent_handles_can_write_disjoint_ranges_without_corruption() {
    let session = test_session(TestBackend::default()).await;
    let left = session
        .open_random_access_with_flags(
            "/concurrent.bin",
            OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::READ | OpenFlags::WRITE,
        )
        .await
        .unwrap();
    let right = session
        .open_random_access_with_flags("/concurrent.bin", OpenFlags::READ | OpenFlags::WRITE)
        .await
        .unwrap();

    let (left_result, right_result) =
        tokio::join!(left.write_at(0, b"AAAA"), right.write_at(8, b"BBBB"),);
    left_result.unwrap();
    right_result.unwrap();

    assert_eq!(left.read_at(0, 12).await.unwrap(), b"AAAA\0\0\0\0BBBB");
}

#[tokio::test]
async fn chunked_write_and_read_reassembles_expected_bytes() {
    let session = test_session(TestBackend::default()).await;
    let file = session
        .open_random_access_with_flags(
            "/chunks.bin",
            OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::READ | OpenFlags::WRITE,
        )
        .await
        .unwrap();

    let mut expected = vec![b'a'; 150_000];
    expected.extend(vec![b'b'; 150_000]);
    expected.extend(vec![b'c'; 32]);

    file.write_at(0, &expected).await.unwrap();

    let assembled = file.read_at(0, expected.len() as u32).await.unwrap();

    assert_eq!(assembled, expected);
}
