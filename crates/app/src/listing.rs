//! 매물 기록과 로컬 SQLite 저장소. 파일 구조는 docs/drive.md를 따른다.
//!
//! 현재 사용자의 로컬 파일만 읽고 쓴다. Drive 업로드 순서는 `sync` 모듈이 정한다. 다른 멤버 파일의 통합 조회는 아직 없다.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, params};
use rust_i18n::t;

use crate::photo::{self, PhotoKind, PreparedPhoto};

const SCHEMA_VERSION: i64 = 3;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_NOTE_CHARS: usize = 2000;
// 중고 매물 가격으로 충분하고 JS Number로 넘겨도 정밀도를 잃지 않는 범위.
const MAX_PRICE_AMOUNT: i64 = 1_000_000_000_000;

/// 소수 단위가 없는 통화만 받는다. 소수 단위 통화는 입력·표시 규칙을 정한 뒤 추가한다.
pub const CURRENCIES: [&str; 2] = ["KRW", "JPY"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Price {
    /// 통화의 최소 단위 정수.
    pub amount: i64,
    pub currency: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct NewListing {
    pub lat: f64,
    pub lng: f64,
    pub price: Option<Price>,
    pub note: String,
    pub photos: Vec<Arc<PreparedPhoto>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PhotoRef {
    pub id: String,
    pub kind: PhotoKind,
    pub local_path: PathBuf,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Listing {
    pub id: String,
    pub owner: String,
    pub lat: f64,
    pub lng: f64,
    pub price: Option<Price>,
    pub note: String,
    /// UTC Unix 밀리초.
    pub created_at_ms: i64,
    pub photos: Vec<PhotoRef>,
}

#[derive(Debug, thiserror::Error)]
pub enum ListingError {
    #[error("{0}")]
    InvalidInput(String),
    #[error("{}", t!("listing_error.unsupported_schema", version = .0))]
    UnsupportedSchema(i64),
    #[error("{}", t!("listing_error.storage", reason = .0))]
    Storage(String),
    #[error("{}", t!("error.internal", reason = .0))]
    Internal(String),
}

impl From<rusqlite::Error> for ListingError {
    fn from(e: rusqlite::Error) -> Self {
        ListingError::Storage(e.to_string())
    }
}

/// 가격 입력을 해석한다. 빈 입력은 가격 없음이다. 자릿수 구분 쉼표는 허용한다.
pub fn parse_price(amount: &str, currency: &str) -> Result<Option<Price>, ListingError> {
    let digits: String = amount.trim().chars().filter(|c| *c != ',').collect();
    if digits.is_empty() {
        return Ok(None);
    }
    if !CURRENCIES.contains(&currency) {
        return Err(invalid(t!("listing_error.currency")));
    }
    if !digits.chars().all(|c| c.is_ascii_digit()) {
        return Err(invalid(t!("listing_error.price_format")));
    }
    let amount = digits
        .parse::<i64>()
        .ok()
        .filter(|v| *v <= MAX_PRICE_AMOUNT)
        .ok_or_else(|| invalid(t!("listing_error.price_range")))?;
    Ok(Some(Price {
        amount,
        currency: currency.to_owned(),
    }))
}

pub fn validate(listing: &NewListing) -> Result<(), ListingError> {
    if !(-90.0..=90.0).contains(&listing.lat) || !(-180.0..=180.0).contains(&listing.lng) {
        return Err(invalid(t!("listing_error.position")));
    }
    if let Some(price) = &listing.price
        && (!(0..=MAX_PRICE_AMOUNT).contains(&price.amount)
            || !CURRENCIES.contains(&price.currency.as_str()))
    {
        return Err(invalid(t!("listing_error.price_range")));
    }
    if listing.note.chars().count() > MAX_NOTE_CHARS {
        return Err(invalid(t!(
            "listing_error.note_length",
            max = MAX_NOTE_CHARS
        )));
    }
    if listing.photos.len() > photo::MAX_PER_LISTING {
        return Err(invalid(t!(
            "photo_error.too_many",
            max = photo::MAX_PER_LISTING
        )));
    }
    Ok(())
}

/// 현재 사용자의 프로젝트 DB에 매물을 추가한다. 사진 파일을 먼저 쓰고 그 참조를 DB에 기록한다.
/// 새 매물과 사진은 업로드 대기 상태로 남고 `sync`가 Drive에 올린다.
pub async fn add(
    project_root: &str,
    owner: &str,
    listing: NewListing,
) -> Result<Listing, ListingError> {
    add_to(&OwnFiles::locate(project_root, owner)?, listing).await
}

pub async fn add_to(files: &OwnFiles, listing: NewListing) -> Result<Listing, ListingError> {
    validate(&listing)?;
    let files = files.clone();
    blocking(move || {
        insert(
            &mut open(&files.db)?,
            &files.photos,
            &files.owner,
            listing,
            now_ms()?,
        )
    })
    .await
}

/// 현재 사용자의 프로젝트 DB에서 삭제되지 않은 매물을 읽는다. 파일이 없으면 빈 목록이다.
pub async fn list_own(project_root: &str, owner: &str) -> Result<Vec<Listing>, ListingError> {
    let path = db_path(project_root, owner)?;
    let photos_dir = photos_dir(project_root)?;
    blocking(move || {
        if !path.exists() {
            return Ok(Vec::new());
        }
        select_live(&open(&path)?, &photos_dir)
    })
    .await
}

fn invalid(message: impl ToString) -> ListingError {
    ListingError::InvalidInput(message.to_string())
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, ListingError> + Send + 'static,
) -> Result<T, ListingError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ListingError::Internal(e.to_string()))?
}

fn now_ms() -> Result<i64, ListingError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| ListingError::Internal(e.to_string()))?;
    i64::try_from(elapsed.as_millis()).map_err(|e| ListingError::Internal(e.to_string()))
}

fn check_component(part: &str) -> Result<&str, ListingError> {
    if part.is_empty() || part.starts_with('.') || part.contains(['/', '\\', '\0']) {
        return Err(ListingError::Internal(format!(
            "bad path component: {part:?}"
        )));
    }
    Ok(part)
}

/// `{data_dir}/projects/{drive_root}/{owner}.sqlite`. 이름은 Drive의 `{user_id}.sqlite`와 맞춘다.
fn db_path(project_root: &str, owner: &str) -> Result<PathBuf, ListingError> {
    Ok(data_dir()?
        .join("projects")
        .join(check_component(project_root)?)
        .join(format!("{}.sqlite", check_component(owner)?)))
}

/// 업로드 전 사진 원본을 보관한다. 파일 이름은 Drive의 `{guid}.{ext}`와 같다.
fn photos_dir(project_root: &str) -> Result<PathBuf, ListingError> {
    Ok(data_dir()?
        .join("projects")
        .join(check_component(project_root)?)
        .join("photos"))
}

#[cfg(target_os = "macos")]
fn data_dir() -> Result<PathBuf, ListingError> {
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .ok_or_else(|| ListingError::Internal("HOME is not set".to_owned()))?;
    Ok(PathBuf::from(home).join("Library/Application Support/dev.sharedmap.app"))
}

// Android 앱 데이터 경로는 Dioxus mobile에서 아직 확인하지 않았다.
#[cfg(not(target_os = "macos"))]
fn data_dir() -> Result<PathBuf, ListingError> {
    Err(ListingError::Internal(
        "local data directory is not implemented for this target".to_owned(),
    ))
}

fn open(path: &Path) -> Result<Connection, ListingError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| ListingError::Storage(e.to_string()))?;
    }
    let mut conn = Connection::open(path)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", true)?;
    migrate(&mut conn)?;
    Ok(conn)
}

fn migrate(conn: &mut Connection) -> Result<(), ListingError> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(ListingError::UnsupportedSchema(version));
    }
    if version < 1 {
        tx.execute_batch(
                "CREATE TABLE listing (
                    id TEXT PRIMARY KEY NOT NULL,
                    owner TEXT NOT NULL,
                    lat REAL NOT NULL CHECK (lat BETWEEN -90 AND 90),
                    lng REAL NOT NULL CHECK (lng BETWEEN -180 AND 180),
                    price_amount INTEGER CHECK (price_amount >= 0),
                    price_currency TEXT,
                    note TEXT NOT NULL DEFAULT '',
                    created_at_ms INTEGER NOT NULL,
                    updated_at_ms INTEGER NOT NULL,
                    deleted_at_ms INTEGER,
                    local_generation INTEGER NOT NULL,
                    CHECK ((price_amount IS NULL) = (price_currency IS NULL))
                ) STRICT;
                CREATE INDEX listing_live_created ON listing (deleted_at_ms, created_at_ms);
                CREATE TABLE sync_state (
                    id INTEGER PRIMARY KEY CHECK (id = 1),
                    local_generation INTEGER NOT NULL,
                    uploaded_generation INTEGER NOT NULL
                ) STRICT;
                INSERT INTO sync_state (id, local_generation, uploaded_generation) VALUES (1, 0, 0);",
        )?;
    }
    if version < 2 {
        tx.execute_batch(
            "CREATE TABLE photo (
                id TEXT PRIMARY KEY NOT NULL,
                listing_id TEXT NOT NULL REFERENCES listing (id),
                file_name TEXT NOT NULL UNIQUE,
                mime TEXT NOT NULL CHECK (mime IN ('image/png', 'image/jpeg', 'image/gif')),
                size_bytes INTEGER NOT NULL CHECK (size_bytes > 0),
                sha256 TEXT NOT NULL,
                width INTEGER NOT NULL CHECK (width > 0),
                height INTEGER NOT NULL CHECK (height > 0),
                position INTEGER NOT NULL,
                drive_file_id TEXT,
                upload_status TEXT NOT NULL DEFAULT 'pending'
                    CHECK (upload_status IN ('pending', 'uploaded')),
                created_at_ms INTEGER NOT NULL,
                deleted_at_ms INTEGER,
                local_generation INTEGER NOT NULL
            ) STRICT;
            CREATE INDEX photo_listing ON photo (listing_id, position);",
        )?;
    }
    if version < 3 {
        tx.execute_batch(
            "ALTER TABLE sync_state ADD COLUMN drive_file_id TEXT;
             ALTER TABLE sync_state ADD COLUMN remote_version TEXT;
             ALTER TABLE sync_state ADD COLUMN uploaded_at_ms INTEGER;",
        )?;
    }
    if version != SCHEMA_VERSION {
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    tx.commit()?;
    Ok(())
}

fn photo_file_name(id: &str, kind: PhotoKind) -> String {
    format!("{id}.{}", kind.extension())
}

/// 임시 파일에 다 쓴 뒤 이름을 바꿔 중간 종료 시 잘린 사진이 남지 않게 한다.
fn write_photo(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    let result = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// 업로드 대상 판단을 위해 변경마다 로컬 세대를 올린다.
fn insert(
    conn: &mut Connection,
    photos_dir: &Path,
    owner: &str,
    listing: NewListing,
    now_ms: i64,
) -> Result<Listing, ListingError> {
    let mut written = Vec::with_capacity(listing.photos.len());
    let result = (|| {
        if !listing.photos.is_empty() {
            std::fs::create_dir_all(photos_dir)
                .map_err(|e| ListingError::Storage(e.to_string()))?;
        }
        for photo in &listing.photos {
            let path = photos_dir.join(photo_file_name(&photo.id, photo.kind));
            write_photo(&path, &photo.bytes).map_err(|e| ListingError::Storage(e.to_string()))?;
            written.push(path);
        }
        insert_rows(conn, photos_dir, owner, &listing, now_ms)
    })();
    // DB에 참조가 없는 사진은 다른 곳에서 쓰지 않으므로 바로 지운다.
    if result.is_err() {
        for path in written {
            let _ = std::fs::remove_file(path);
        }
    }
    result
}

fn insert_rows(
    conn: &mut Connection,
    photos_dir: &Path,
    owner: &str,
    listing: &NewListing,
    now_ms: i64,
) -> Result<Listing, ListingError> {
    let id = uuid::Uuid::now_v7().to_string();
    let tx = conn.transaction()?;
    let generation: i64 = tx.query_row(
        "UPDATE sync_state SET local_generation = local_generation + 1 WHERE id = 1
         RETURNING local_generation",
        [],
        |row| row.get(0),
    )?;
    tx.execute(
        "INSERT INTO listing (id, owner, lat, lng, price_amount, price_currency, note,
                              created_at_ms, updated_at_ms, local_generation)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?9)",
        params![
            id,
            owner,
            listing.lat,
            listing.lng,
            listing.price.as_ref().map(|p| p.amount),
            listing.price.as_ref().map(|p| p.currency.as_str()),
            listing.note,
            now_ms,
            generation,
        ],
    )?;
    let mut photos = Vec::with_capacity(listing.photos.len());
    for (position, photo) in listing.photos.iter().enumerate() {
        let file_name = photo_file_name(&photo.id, photo.kind);
        tx.execute(
            "INSERT INTO photo (id, listing_id, file_name, mime, size_bytes, sha256, width, height,
                                position, created_at_ms, local_generation)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                photo.id,
                id,
                file_name,
                photo.kind.mime(),
                // 사진 크기와 개수는 photo 모듈 상한으로 i64 범위 안이다.
                photo.bytes.len() as i64,
                photo.sha256,
                photo.width,
                photo.height,
                position as i64,
                now_ms,
                generation,
            ],
        )?;
        photos.push(PhotoRef {
            id: photo.id.clone(),
            kind: photo.kind,
            local_path: photos_dir.join(file_name),
        });
    }
    tx.commit()?;
    Ok(Listing {
        id,
        owner: owner.to_owned(),
        lat: listing.lat,
        lng: listing.lng,
        price: listing.price.clone(),
        note: listing.note.clone(),
        created_at_ms: now_ms,
        photos,
    })
}

fn select_live(conn: &Connection, photos_dir: &Path) -> Result<Vec<Listing>, ListingError> {
    let mut stmt = conn.prepare(
        "SELECT id, owner, lat, lng, price_amount, price_currency, note, created_at_ms
         FROM listing WHERE deleted_at_ms IS NULL ORDER BY created_at_ms",
    )?;
    let rows = stmt.query_map([], |row| {
        let amount: Option<i64> = row.get(4)?;
        let currency: Option<String> = row.get(5)?;
        Ok(Listing {
            id: row.get(0)?,
            owner: row.get(1)?,
            lat: row.get(2)?,
            lng: row.get(3)?,
            price: amount
                .zip(currency)
                .map(|(amount, currency)| Price { amount, currency }),
            note: row.get(6)?,
            created_at_ms: row.get(7)?,
            photos: Vec::new(),
        })
    })?;
    let mut listings: Vec<Listing> = rows.collect::<Result<_, _>>()?;

    let mut stmt = conn.prepare(
        "SELECT photo.listing_id, photo.id, photo.file_name, photo.mime
         FROM photo JOIN listing ON listing.id = photo.listing_id
         WHERE photo.deleted_at_ms IS NULL AND listing.deleted_at_ms IS NULL
         ORDER BY photo.listing_id, photo.position",
    )?;
    let mut rows = stmt.query([])?;
    let index: std::collections::HashMap<String, usize> = listings
        .iter()
        .enumerate()
        .map(|(i, l)| (l.id.clone(), i))
        .collect();
    while let Some(row) = rows.next()? {
        let listing_id: String = row.get(0)?;
        let file_name: String = row.get(2)?;
        let mime: String = row.get(3)?;
        // 이름과 형식은 CHECK로 막지만 파일을 직접 고친 경우를 대비해 경로 밖을 가리키지 않게 한다.
        let (Some(&i), Some(kind), Ok(file_name)) = (
            index.get(&listing_id),
            PhotoKind::from_mime(&mime),
            check_component(&file_name),
        ) else {
            continue;
        };
        listings[i].photos.push(PhotoRef {
            id: row.get(1)?,
            kind,
            local_path: photos_dir.join(file_name),
        });
    }
    Ok(listings)
}

/// 현재 사용자의 로컬 DB와 사진 폴더.
#[derive(Clone, Debug)]
pub struct OwnFiles {
    pub owner: String,
    pub db: PathBuf,
    pub photos: PathBuf,
}

impl OwnFiles {
    pub fn locate(project_root: &str, owner: &str) -> Result<Self, ListingError> {
        Ok(Self {
            owner: owner.to_owned(),
            db: db_path(project_root, owner)?,
            photos: photos_dir(project_root)?,
        })
    }

    /// Drive의 `{user_id}.sqlite`와 같은 이름.
    pub fn remote_db_name(&self) -> String {
        format!("{}.sqlite", self.owner)
    }

    fn snapshot_path(&self) -> PathBuf {
        self.db.with_extension("sqlite.upload")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncStatus {
    /// 아직 Drive에 올리지 않은 로컬 변경이 있다.
    pub pending: bool,
    pub pending_photos: usize,
    /// 마지막으로 DB를 올린 UTC Unix 밀리초.
    pub uploaded_at_ms: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingPhoto {
    pub id: String,
    pub file_name: String,
    pub mime: String,
    pub path: PathBuf,
}

/// 업로드용 일관된 DB 사본. 어느 로컬 세대까지 담았는지 함께 기록한다.
#[derive(Debug)]
pub struct Snapshot {
    pub path: PathBuf,
    pub generation: i64,
    pub drive_file_id: Option<String>,
    /// 사본에 담긴 아직 올리지 않은 사진 수. 0이 아니면 사본을 게시하지 않는다.
    pub pending_photos: usize,
}

pub async fn sync_status(files: &OwnFiles) -> Result<SyncStatus, ListingError> {
    let db = files.db.clone();
    blocking(move || {
        if !db.exists() {
            return Ok(SyncStatus {
                pending: false,
                pending_photos: 0,
                uploaded_at_ms: None,
            });
        }
        read_sync_status(&open(&db)?)
    })
    .await
}

pub async fn pending_photos(files: &OwnFiles) -> Result<Vec<PendingPhoto>, ListingError> {
    let (db, photos) = (files.db.clone(), files.photos.clone());
    blocking(move || select_pending_photos(&open(&db)?, &photos)).await
}

pub async fn mark_photo_uploaded(
    files: &OwnFiles,
    photo_id: &str,
    drive_file_id: &str,
) -> Result<(), ListingError> {
    let db = files.db.clone();
    let (photo_id, drive_file_id) = (photo_id.to_owned(), drive_file_id.to_owned());
    blocking(move || {
        open(&db)?.execute(
            "UPDATE photo SET upload_status = 'uploaded', drive_file_id = ?2 WHERE id = ?1",
            params![photo_id, drive_file_id],
        )?;
        Ok(())
    })
    .await
}

/// WAL 파일까지 반영된 사본을 만든다. 사본은 다른 멤버가 읽기 전용으로 열 수 있게 WAL을 끈다.
pub async fn snapshot(files: &OwnFiles) -> Result<Snapshot, ListingError> {
    let (db, path) = (files.db.clone(), files.snapshot_path());
    blocking(move || {
        let src = open(&db)?;
        make_snapshot(&src, &path)
    })
    .await
}

/// 사본이 담은 세대까지만 업로드 완료로 기록한다. 업로드 중 생긴 변경은 대기로 남는다.
pub async fn mark_db_uploaded(
    files: &OwnFiles,
    generation: i64,
    drive_file_id: &str,
    remote_version: &str,
) -> Result<(), ListingError> {
    let db = files.db.clone();
    let (drive_file_id, remote_version) = (drive_file_id.to_owned(), remote_version.to_owned());
    blocking(move || {
        record_db_upload(
            &open(&db)?,
            generation,
            &drive_file_id,
            &remote_version,
            now_ms()?,
        )
    })
    .await
}

fn read_sync_status(conn: &Connection) -> Result<SyncStatus, ListingError> {
    let (pending, uploaded_at_ms) = conn.query_row(
        "SELECT local_generation > uploaded_generation, uploaded_at_ms FROM sync_state WHERE id = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(SyncStatus {
        pending,
        pending_photos: count_pending_photos(conn)?,
        uploaded_at_ms,
    })
}

fn count_pending_photos(conn: &Connection) -> Result<usize, ListingError> {
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM photo WHERE upload_status = 'pending' AND deleted_at_ms IS NULL",
        [],
        |row| row.get(0),
    )?;
    usize::try_from(count).map_err(|e| ListingError::Internal(e.to_string()))
}

fn select_pending_photos(
    conn: &Connection,
    photos_dir: &Path,
) -> Result<Vec<PendingPhoto>, ListingError> {
    let mut stmt = conn.prepare(
        "SELECT id, file_name, mime FROM photo
         WHERE upload_status = 'pending' AND deleted_at_ms IS NULL
         ORDER BY created_at_ms, position",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut photos = Vec::new();
    for row in rows {
        let (id, file_name, mime) = row?;
        let path = photos_dir.join(check_component(&file_name)?);
        photos.push(PendingPhoto {
            id,
            file_name,
            mime,
            path,
        });
    }
    Ok(photos)
}

fn make_snapshot(src: &Connection, path: &Path) -> Result<Snapshot, ListingError> {
    // 이전 실행이 남긴 사본 위에 백업하면 페이지가 섞일 수 있어 지우고 시작한다.
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(ListingError::Storage(e.to_string())),
    }
    let mut dst = Connection::open(path)?;
    // 한 단계(-1)로 전체 페이지를 복사해야 복사 도중의 쓰기가 사본에 섞이지 않는다.
    // run_to_completion은 음수 단계를 받지 않아 step을 직접 부른다.
    match rusqlite::backup::Backup::new(src, &mut dst)?.step(-1)? {
        rusqlite::backup::StepResult::Done => {}
        other => {
            return Err(ListingError::Storage(format!(
                "snapshot did not complete: {other:?}"
            )));
        }
    }
    dst.pragma_update(None, "journal_mode", "DELETE")?;
    let (generation, drive_file_id) = dst.query_row(
        "SELECT local_generation, drive_file_id FROM sync_state WHERE id = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let pending_photos = count_pending_photos(&dst)?;
    Ok(Snapshot {
        path: path.to_owned(),
        generation,
        drive_file_id,
        pending_photos,
    })
}

fn record_db_upload(
    conn: &Connection,
    generation: i64,
    drive_file_id: &str,
    remote_version: &str,
    now_ms: i64,
) -> Result<(), ListingError> {
    conn.execute(
        "UPDATE sync_state
         SET uploaded_generation = max(uploaded_generation, ?1),
             drive_file_id = ?2, remote_version = ?3, uploaded_at_ms = ?4
         WHERE id = 1",
        params![generation, drive_file_id, remote_version, now_ms],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_generation(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT local_generation FROM sync_state WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .unwrap()
    }

    /// 테스트별 임시 사진 폴더. 끝나면 지운다.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("shared-map-test-{}", uuid::Uuid::now_v7())))
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn files_in(dir: &Path) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn jpeg_photo() -> Arc<PreparedPhoto> {
        let image = image::DynamicImage::ImageRgb8(image::ImageBuffer::from_pixel(
            4,
            3,
            image::Rgb([1, 2, 3]),
        ));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut bytes, image::ImageFormat::Jpeg)
            .unwrap();
        Arc::new(photo::prepare(bytes.get_ref()).unwrap())
    }

    fn memory() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        conn
    }

    fn new_listing(lat: f64, lng: f64) -> NewListing {
        NewListing {
            lat,
            lng,
            price: Some(Price {
                amount: 12_000,
                currency: "KRW".to_owned(),
            }),
            note: "피규어".to_owned(),
            photos: Vec::new(),
        }
    }

    #[test]
    fn inserts_and_lists() {
        let dir = TempDir::new();
        let mut conn = memory();
        let added = insert(
            &mut conn,
            &dir.0,
            "a@example.com",
            new_listing(37.5, 127.0),
            1_000,
        )
        .unwrap();
        let second = insert(
            &mut conn,
            &dir.0,
            "a@example.com",
            new_listing(35.0, 139.0),
            2_000,
        )
        .unwrap();
        assert_ne!(added.id, second.id);
        assert_eq!(select_live(&conn, &dir.0).unwrap(), vec![added, second]);
        assert_eq!(local_generation(&conn), 2);
        // 사진이 없으면 사진 폴더를 만들지 않는다.
        assert!(!dir.0.exists());
    }

    #[test]
    fn stores_photo_files_and_references() {
        let dir = TempDir::new();
        let mut conn = memory();
        let (first, second) = (jpeg_photo(), jpeg_photo());
        let mut listing = new_listing(37.5, 127.0);
        listing.photos = vec![first.clone(), second.clone()];
        let added = insert(&mut conn, &dir.0, "a@example.com", listing, 1_000).unwrap();

        assert_eq!(
            files_in(&dir.0),
            vec![format!("{}.jpg", first.id), format!("{}.jpg", second.id)]
        );
        assert_eq!(
            std::fs::read(&added.photos[0].local_path).unwrap(),
            first.bytes
        );
        assert_eq!(select_live(&conn, &dir.0).unwrap(), vec![added]);
        let pending: i64 = conn
            .query_row(
                "SELECT count(*) FROM photo WHERE upload_status = 'pending' AND drive_file_id IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pending, 2);
    }

    #[test]
    fn failed_insert_removes_written_photos() {
        let dir = TempDir::new();
        let mut conn = memory();
        let mut listing = new_listing(91.0, 0.0);
        listing.photos = vec![jpeg_photo()];
        assert!(insert(&mut conn, &dir.0, "a@example.com", listing, 1_000).is_err());
        assert!(files_in(&dir.0).is_empty());
        assert!(select_live(&conn, &dir.0).unwrap().is_empty());
    }

    #[test]
    fn migrates_v1_database_with_existing_rows() {
        let dir = TempDir::new();
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE listing (
                id TEXT PRIMARY KEY NOT NULL, owner TEXT NOT NULL, lat REAL NOT NULL,
                lng REAL NOT NULL, price_amount INTEGER, price_currency TEXT,
                note TEXT NOT NULL DEFAULT '', created_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL, deleted_at_ms INTEGER,
                local_generation INTEGER NOT NULL
            ) STRICT;
            CREATE TABLE sync_state (
                id INTEGER PRIMARY KEY, local_generation INTEGER NOT NULL,
                uploaded_generation INTEGER NOT NULL
            ) STRICT;
            INSERT INTO sync_state VALUES (1, 1, 0);
            INSERT INTO listing VALUES ('old', 'a@example.com', 1.0, 2.0, NULL, NULL, '', 5, 5, NULL, 1);
            PRAGMA user_version = 1;",
        )
        .unwrap();
        migrate(&mut conn).unwrap();
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let listings = select_live(&conn, &dir.0).unwrap();
        assert_eq!(listings.len(), 1);
        assert!(listings[0].photos.is_empty());
    }

    #[test]
    fn migration_is_idempotent_and_rejects_newer_schema() {
        let mut conn = memory();
        migrate(&mut conn).unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        assert!(matches!(
            migrate(&mut conn),
            Err(ListingError::UnsupportedSchema(v)) if v == SCHEMA_VERSION + 1
        ));
    }

    #[test]
    fn failed_insert_rolls_back_generation() {
        let mut conn = memory();
        // validate를 거치지 않은 값은 CHECK 제약이 막고 세대 증가도 함께 되돌린다.
        let dir = TempDir::new();
        assert!(
            insert(
                &mut conn,
                &dir.0,
                "a@example.com",
                new_listing(91.0, 0.0),
                1_000
            )
            .is_err()
        );
        assert_eq!(local_generation(&conn), 0);
        assert!(select_live(&conn, &dir.0).unwrap().is_empty());
    }

    #[test]
    fn parses_price() {
        assert_eq!(parse_price("  ", "KRW").unwrap(), None);
        assert_eq!(
            parse_price("12,000", "JPY").unwrap(),
            Some(Price {
                amount: 12_000,
                currency: "JPY".to_owned()
            })
        );
        assert!(parse_price("12.5", "KRW").is_err());
        assert!(parse_price("-1", "KRW").is_err());
        assert!(parse_price("1", "USD").is_err());
        assert!(parse_price("99999999999999999999", "KRW").is_err());
    }

    #[test]
    fn validates_listing() {
        assert!(validate(&new_listing(37.5, 127.0)).is_ok());
        assert!(validate(&new_listing(-91.0, 0.0)).is_err());
        assert!(validate(&new_listing(0.0, 180.5)).is_err());
        assert!(validate(&new_listing(f64::NAN, 0.0)).is_err());
        let mut long = new_listing(0.0, 0.0);
        long.note = "a".repeat(MAX_NOTE_CHARS + 1);
        assert!(validate(&long).is_err());
        let mut many = new_listing(0.0, 0.0);
        many.photos = vec![jpeg_photo(); photo::MAX_PER_LISTING + 1];
        assert!(validate(&many).is_err());
    }

    #[test]
    fn rejects_path_traversal() {
        assert!(db_path("..", "a@example.com").is_err());
        assert!(db_path("root", "a/b").is_err());
        assert!(db_path("", "a").is_err());
    }

    #[test]
    fn error_messages_resolve_from_locale() {
        assert_eq!(
            ListingError::UnsupportedSchema(9).to_string(),
            "지원하지 않는 매물 DB 스키마 버전이야: 9"
        );
    }
}
