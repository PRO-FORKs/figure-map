//! 현재 사용자의 로컬 변경을 Drive 프로젝트 폴더에 올린다.
//!
//! 순서: 대기 사진 업로드 → DB 사본 생성 → `{user_id}.sqlite` 교체 → 올린 세대 기록.
//! 사진을 먼저 올려야 다른 멤버가 받은 DB가 아직 없는 사진을 가리키지 않는다.
//! 어느 단계에서 실패해도 로컬 기록은 그대로 두고, 남은 대기 상태로 다음 실행이 이어서 올린다.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

use rust_i18n::t;

use crate::auth::DriveToken;
use crate::drive::{DriveError, FolderUploader, RemoteFile};
use crate::listing::{self, ListingError, OwnFiles};

const SQLITE_MIME: &str = "application/vnd.sqlite3";
// 업로드 중 사진이 붙은 매물이 계속 추가되면 사본을 다시 만든다. 무한히 돌지 않게 횟수를 제한한다.
const MAX_ROUNDS: usize = 3;
const RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(2), Duration::from_secs(8)];

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error(transparent)]
    Drive(#[from] DriveError),
    #[error(transparent)]
    Local(#[from] ListingError),
    #[error("{}", t!("sync_error.photo_missing", file = .0))]
    PhotoMissing(String),
}

/// 업로드 대상. 실제 구현은 Drive이고 테스트는 가짜를 쓴다.
pub trait Remote {
    async fn upload_photo(
        &self,
        name: &str,
        mime_type: &str,
        bytes: Vec<u8>,
    ) -> Result<String, DriveError>;

    async fn upload_db(
        &self,
        file_id: Option<&str>,
        name: &str,
        bytes: Vec<u8>,
    ) -> Result<RemoteFile, DriveError>;
}

impl Remote for FolderUploader {
    async fn upload_photo(
        &self,
        name: &str,
        mime_type: &str,
        bytes: Vec<u8>,
    ) -> Result<String, DriveError> {
        self.upload_once(name, mime_type, bytes).await
    }

    async fn upload_db(
        &self,
        file_id: Option<&str>,
        name: &str,
        bytes: Vec<u8>,
    ) -> Result<RemoteFile, DriveError> {
        self.replace(file_id, name, SQLITE_MIME, bytes).await
    }
}

/// 현재 사용자의 대기 중인 사진과 매물 DB를 프로젝트 폴더에 올린다.
///
/// 같은 DB를 이미 올리는 중이면 기다리지 않고 돌아온다. 진행 중인 업로드가 끝난 뒤 한 번 더 돌아 그사이의 변경을 올린다.
pub async fn upload_own(
    token: &DriveToken,
    project_root: &str,
    owner: &str,
) -> Result<(), SyncError> {
    let files = OwnFiles::locate(project_root, owner)?;
    let Some(mut guard) = InFlight::acquire(&files.db) else {
        return Ok(());
    };
    let remote = FolderUploader::new(token, project_root)?;
    loop {
        upload_pending(&remote, &files, &RETRY_DELAYS).await?;
        if guard.finish_or_rerun() {
            return Ok(());
        }
    }
}

async fn upload_pending(
    remote: &impl Remote,
    files: &OwnFiles,
    retry_delays: &[Duration],
) -> Result<(), SyncError> {
    let status = listing::sync_status(files).await?;
    if !status.pending && status.pending_photos == 0 {
        return Ok(());
    }
    for _ in 0..MAX_ROUNDS {
        for photo in listing::pending_photos(files).await? {
            let bytes = match read_file(photo.path.clone()).await {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(SyncError::PhotoMissing(photo.file_name));
                }
                Err(e) => return Err(ListingError::Storage(e.to_string()).into()),
            };
            let file_id = retry(retry_delays, || {
                remote.upload_photo(&photo.file_name, &photo.mime, bytes.clone())
            })
            .await?;
            listing::mark_photo_uploaded(files, &photo.id, &file_id).await?;
        }

        let snapshot = listing::snapshot(files).await?;
        let result = async {
            if snapshot.pending_photos > 0 {
                return Ok(false);
            }
            let bytes = read_file(snapshot.path.clone())
                .await
                .map_err(|e| ListingError::Storage(e.to_string()))?;
            let name = files.remote_db_name();
            let uploaded = retry(retry_delays, || {
                remote.upload_db(snapshot.drive_file_id.as_deref(), &name, bytes.clone())
            })
            .await?;
            listing::mark_db_uploaded(files, snapshot.generation, &uploaded.id, &uploaded.version)
                .await?;
            Ok::<_, SyncError>(true)
        }
        .await;
        let _ = std::fs::remove_file(&snapshot.path);
        if result? {
            return Ok(());
        }
    }
    Ok(())
}

async fn read_file(path: PathBuf) -> std::io::Result<Vec<u8>> {
    tokio::task::spawn_blocking(move || std::fs::read(path))
        .await
        .map_err(std::io::Error::other)?
}

/// 요청이 많거나 일시적인 실패만 정해진 간격으로 다시 시도한다.
async fn retry<T, F, Fut>(delays: &[Duration], mut op: F) -> Result<T, DriveError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, DriveError>>,
{
    let mut delays = delays.iter();
    loop {
        match op().await {
            Err(e @ (DriveError::Unavailable(_) | DriveError::Network(_))) => match delays.next() {
                Some(delay) => tokio::time::sleep(*delay).await,
                None => return Err(e),
            },
            result => return result,
        }
    }
}

/// 같은 DB 파일의 업로드를 한 번에 하나로 묶는다. 값은 진행 중에 다시 요청됐는지 여부다.
fn in_flight() -> &'static Mutex<HashMap<PathBuf, bool>> {
    static IN_FLIGHT: OnceLock<Mutex<HashMap<PathBuf, bool>>> = OnceLock::new();
    IN_FLIGHT.get_or_init(Default::default)
}

struct InFlight {
    path: PathBuf,
    released: bool,
}

impl InFlight {
    fn acquire(path: &Path) -> Option<Self> {
        let mut map = in_flight().lock().unwrap_or_else(PoisonError::into_inner);
        match map.get_mut(path) {
            Some(rerun) => {
                *rerun = true;
                None
            }
            None => {
                map.insert(path.to_owned(), false);
                Some(Self {
                    path: path.to_owned(),
                    released: false,
                })
            }
        }
    }

    /// 진행 중에 다시 요청됐으면 false를 돌려 한 번 더 돌게 한다. 아니면 잠금을 풀고 true.
    /// 확인과 해제를 한 락 안에서 해야 그 사이에 들어온 요청을 놓치지 않는다.
    fn finish_or_rerun(&mut self) -> bool {
        let mut map = in_flight().lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(rerun) = map.get_mut(&self.path)
            && *rerun
        {
            *rerun = false;
            return false;
        }
        map.remove(&self.path);
        self.released = true;
        true
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if !self.released {
            in_flight()
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::listing::NewListing;
    use crate::photo;
    use rusqlite::Connection;
    use std::sync::Arc;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("shared-map-sync-{}", uuid::Uuid::now_v7())))
        }

        fn files(&self) -> OwnFiles {
            OwnFiles {
                owner: "a@example.com".to_owned(),
                db: self.0.join("a@example.com.sqlite"),
                photos: self.0.join("photos"),
            }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Debug, Clone, PartialEq)]
    enum Call {
        Photo(String),
        Db(Option<String>),
    }

    #[derive(Default)]
    struct FakeRemote {
        calls: Mutex<Vec<Call>>,
        photo_errors: Mutex<Vec<DriveError>>,
        db_errors: Mutex<Vec<DriveError>>,
        last_db: Mutex<Option<Vec<u8>>>,
        /// DB 업로드 도중 로컬 변경이 생기는 상황을 흉내 낸다.
        during_db_upload: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    }

    impl FakeRemote {
        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Remote for FakeRemote {
        async fn upload_photo(
            &self,
            name: &str,
            _mime_type: &str,
            _bytes: Vec<u8>,
        ) -> Result<String, DriveError> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Photo(name.to_owned()));
            match self.photo_errors.lock().unwrap().pop() {
                Some(e) => Err(e),
                None => Ok(format!("drive-{}", name.replace('.', "-"))),
            }
        }

        async fn upload_db(
            &self,
            file_id: Option<&str>,
            _name: &str,
            bytes: Vec<u8>,
        ) -> Result<RemoteFile, DriveError> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Db(file_id.map(str::to_owned)));
            if let Some(hook) = self.during_db_upload.lock().unwrap().take() {
                hook();
            }
            if let Some(e) = self.db_errors.lock().unwrap().pop() {
                return Err(e);
            }
            *self.last_db.lock().unwrap() = Some(bytes);
            Ok(RemoteFile {
                id: "db-file".to_owned(),
                version: "7".to_owned(),
            })
        }
    }

    fn block_on<F: Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(f)
    }

    fn jpeg() -> Arc<photo::PreparedPhoto> {
        let image = image::DynamicImage::ImageRgb8(image::ImageBuffer::from_pixel(
            2,
            2,
            image::Rgb([9, 8, 7]),
        ));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut bytes, image::ImageFormat::Jpeg)
            .unwrap();
        Arc::new(photo::prepare(bytes.get_ref()).unwrap())
    }

    fn listing_with_photos(count: usize) -> NewListing {
        NewListing {
            lat: 37.5,
            lng: 127.0,
            price: None,
            note: "의자".to_owned(),
            photos: (0..count).map(|_| jpeg()).collect(),
        }
    }

    fn add(files: &OwnFiles, count: usize) -> listing::Listing {
        block_on(listing::add_to(files, listing_with_photos(count))).unwrap()
    }

    fn status(files: &OwnFiles) -> listing::SyncStatus {
        block_on(listing::sync_status(files)).unwrap()
    }

    fn run(remote: &FakeRemote, files: &OwnFiles) -> Result<(), SyncError> {
        block_on(upload_pending(remote, files, &[]))
    }

    #[test]
    fn new_listing_is_pending_until_uploaded() {
        let dir = TempDir::new();
        let files = dir.files();
        let added = add(&files, 2);
        let before = status(&files);
        assert!(before.pending);
        assert_eq!(before.pending_photos, 2);
        assert_eq!(before.uploaded_at_ms, None);

        let remote = FakeRemote::default();
        run(&remote, &files).unwrap();

        let names: Vec<String> = added
            .photos
            .iter()
            .map(|p| format!("{}.jpg", p.id))
            .collect();
        assert_eq!(
            remote.calls(),
            vec![
                Call::Photo(names[0].clone()),
                Call::Photo(names[1].clone()),
                Call::Db(None),
            ]
        );
        let after = status(&files);
        assert!(!after.pending);
        assert_eq!(after.pending_photos, 0);
        assert!(after.uploaded_at_ms.is_some());

        // 올라간 DB는 WAL 없이 열리고 사진의 Drive 파일 ID를 담는다.
        let uploaded = dir.0.join("uploaded.sqlite");
        std::fs::write(&uploaded, remote.last_db.lock().unwrap().clone().unwrap()).unwrap();
        let conn =
            Connection::open_with_flags(&uploaded, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        let mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "delete");
        let ids: Vec<Option<String>> = conn
            .prepare("SELECT drive_file_id FROM photo ORDER BY position")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            ids,
            names
                .iter()
                .map(|n| Some(format!("drive-{}", n.replace('.', "-"))))
                .collect::<Vec<_>>()
        );
        assert!(!files.db.with_extension("sqlite.upload").exists());

        // 다음 변경은 기억해 둔 Drive 파일을 교체한다.
        add(&files, 0);
        run(&remote, &files).unwrap();
        assert_eq!(
            remote.calls().last(),
            Some(&Call::Db(Some("db-file".to_owned())))
        );
    }

    #[test]
    fn nothing_to_upload_makes_no_request() {
        let dir = TempDir::new();
        let files = dir.files();
        let remote = FakeRemote::default();
        run(&remote, &files).unwrap();
        assert!(remote.calls().is_empty());
        // 매물이 없으면 로컬 DB 파일도 만들지 않는다.
        assert!(!files.db.exists());
    }

    #[test]
    fn failed_photo_upload_keeps_everything_pending() {
        let dir = TempDir::new();
        let files = dir.files();
        add(&files, 1);
        let remote = FakeRemote::default();
        remote
            .photo_errors
            .lock()
            .unwrap()
            .push(DriveError::Forbidden(403));

        assert!(matches!(
            run(&remote, &files),
            Err(SyncError::Drive(DriveError::Forbidden(403)))
        ));
        assert_eq!(remote.calls().len(), 1);
        let after = status(&files);
        assert!(after.pending);
        assert_eq!(after.pending_photos, 1);

        run(&remote, &files).unwrap();
        assert!(!status(&files).pending);
    }

    #[test]
    fn failed_db_upload_does_not_reupload_photos() {
        let dir = TempDir::new();
        let files = dir.files();
        add(&files, 1);
        let remote = FakeRemote::default();
        remote
            .db_errors
            .lock()
            .unwrap()
            .push(DriveError::Unauthorized);

        assert!(run(&remote, &files).is_err());
        let after = status(&files);
        assert!(after.pending);
        assert_eq!(after.pending_photos, 0);
        assert!(!files.db.with_extension("sqlite.upload").exists());

        run(&remote, &files).unwrap();
        let calls = remote.calls();
        assert_eq!(
            calls.iter().filter(|c| matches!(c, Call::Photo(_))).count(),
            1
        );
        assert_eq!(calls.iter().filter(|c| matches!(c, Call::Db(_))).count(), 2);
        assert!(!status(&files).pending);
    }

    #[test]
    fn change_during_upload_stays_pending() {
        let dir = TempDir::new();
        let files = dir.files();
        add(&files, 0);
        let remote = FakeRemote::default();
        let during = files.clone();
        *remote.during_db_upload.lock().unwrap() = Some(Box::new(move || {
            // 업로드 응답을 기다리는 동안 다른 스레드에서 매물을 추가한 것과 같다.
            std::thread::spawn(move || block_on(listing::add_to(&during, listing_with_photos(1))))
                .join()
                .unwrap()
                .unwrap();
        }));

        run(&remote, &files).unwrap();
        let after = status(&files);
        assert!(after.pending);
        assert_eq!(after.pending_photos, 1);

        run(&remote, &files).unwrap();
        assert!(!status(&files).pending);
    }

    #[test]
    fn retries_transient_errors() {
        let dir = TempDir::new();
        let files = dir.files();
        add(&files, 0);
        let remote = FakeRemote::default();
        remote
            .db_errors
            .lock()
            .unwrap()
            .push(DriveError::Unavailable(503));

        block_on(upload_pending(&remote, &files, &[Duration::ZERO])).unwrap();
        assert_eq!(remote.calls().len(), 2);
        assert!(!status(&files).pending);
    }

    #[test]
    fn missing_photo_file_blocks_db_upload() {
        let dir = TempDir::new();
        let files = dir.files();
        let added = add(&files, 1);
        std::fs::remove_file(&added.photos[0].local_path).unwrap();
        let remote = FakeRemote::default();

        assert!(matches!(
            run(&remote, &files),
            Err(SyncError::PhotoMissing(_))
        ));
        assert!(remote.calls().is_empty());
        assert!(status(&files).pending);
    }

    #[test]
    fn serializes_uploads_of_same_file() {
        let path = std::env::temp_dir().join(format!("in-flight-{}", uuid::Uuid::now_v7()));
        let mut first = InFlight::acquire(&path).unwrap();
        assert!(InFlight::acquire(&path).is_none());
        // 진행 중에 요청이 있었으므로 한 번 더 돌고, 그다음에 풀린다.
        assert!(!first.finish_or_rerun());
        assert!(InFlight::acquire(&path).is_none());
        assert!(!first.finish_or_rerun());
        assert!(first.finish_or_rerun());
        drop(first);
        // 실패로 끝나 drop돼도 풀린다.
        let second = InFlight::acquire(&path).unwrap();
        drop(second);
        assert!(InFlight::acquire(&path).is_some());
    }

    #[test]
    fn error_messages_resolve_from_locale() {
        assert_eq!(
            SyncError::PhotoMissing("a.jpg".to_owned()).to_string(),
            "업로드할 사진 파일이 로컬에 없어: a.jpg"
        );
    }
}
