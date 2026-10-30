//! Google Drive 어댑터. 경로 구조는 docs/drive.md를 따른다.

use std::time::Duration;

use rust_i18n::t;
use serde::{Deserialize, Serialize};

use crate::auth::DriveToken;

const FILES_URL: &str = "https://www.googleapis.com/drive/v3/files";
const UPLOAD_URL: &str = "https://www.googleapis.com/upload/drive/v3/files";
const APP_FOLDER: &str = "jipark-map";
const PROJECTS_FILE: &str = "projects.tsv";
const PROJECTS_HEADER: [&str; 4] = ["id", "alias", "owner", "drive_root"];
// id 컬럼 추가 전 형식. 읽을 때 폴더 이름으로 id를 채우고, 다음 저장 때 새 형식으로 바뀐다.
const LEGACY_PROJECTS_HEADER: [&str; 3] = ["alias", "owner", "drive_root"];
const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
const TSV_MIME: &str = "text/tab-separated-values";

const MAX_PROJECT_ID_LEN: usize = 64;
const MAX_ALIAS_CHARS: usize = 100;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_PROJECTS_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub id: String,
    pub alias: String,
    pub owner: String,
    pub drive_root: String,
}

#[derive(Debug, thiserror::Error)]
pub enum DriveError {
    #[error("{}", t!("drive_error.unauthorized"))]
    Unauthorized,
    #[error("{}", t!("drive_error.forbidden", status = .0))]
    Forbidden(u16),
    #[error("{}", t!("drive_error.unavailable", status = .0))]
    Unavailable(u16),
    #[error("{}", t!("drive_error.rejected", status = .0))]
    Rejected(u16),
    #[error("{}", t!("error.network", reason = .0))]
    Network(String),
    #[error("{0}")]
    InvalidInput(String),
    #[error("{}", t!("drive_error.project_exists", id = .0))]
    ProjectExists(String),
    #[error("{}", t!("drive_error.invalid_projects", file = PROJECTS_FILE, reason = .0))]
    InvalidProjects(String),
    #[error("{}", t!("error.internal", reason = .0))]
    Internal(String),
}

/// `jipark-map/projects.tsv`를 읽는다. 폴더나 파일이 아직 없으면 빈 목록이다.
pub async fn list_projects(token: &DriveToken) -> Result<Vec<Project>, DriveError> {
    let http = client()?;
    let Some(folder_id) = find_first(&http, token, &app_folder_query()).await? else {
        return Ok(Vec::new());
    };
    let Some(file_id) = find_first(&http, token, &projects_file_query(&folder_id)).await? else {
        return Ok(Vec::new());
    };

    let rows = parse_projects(&download_text(&http, token, &file_id).await?)?;
    resolve_ids(&http, token, rows).await
}

/// `jipark-map/{project_id}/` 폴더를 만들고 `projects.tsv`에 추가한다.
///
/// 폴더 생성 후 목록 갱신이 실패했다면 같은 ID로 다시 시도할 때 남은 폴더를 이어서 쓴다.
pub async fn create_project(
    token: &DriveToken,
    owner: &str,
    project_id: &str,
    alias: &str,
) -> Result<Project, DriveError> {
    let (project_id, alias) = validate_new_project(project_id, alias)?;
    let http = client()?;

    let app_folder = match find_first(&http, token, &app_folder_query()).await? {
        Some(id) => id,
        None => create_folder(&http, token, APP_FOLDER, "root").await?,
    };

    let projects_file = find_first(&http, token, &projects_file_query(&app_folder)).await?;
    let mut projects = match &projects_file {
        Some(file_id) => {
            let rows = parse_projects(&download_text(&http, token, file_id).await?)?;
            resolve_ids(&http, token, rows).await?
        }
        None => Vec::new(),
    };

    let project_query = format!(
        "name = '{project_id}' and mimeType = '{FOLDER_MIME}' and '{app_folder}' in parents and trashed = false"
    );
    let drive_root = match find_first(&http, token, &project_query).await? {
        Some(id) if projects.iter().any(|p| p.drive_root == id) => {
            return Err(DriveError::ProjectExists(project_id.to_owned()));
        }
        Some(id) => id,
        None => create_folder(&http, token, project_id, &app_folder).await?,
    };

    let project = Project {
        id: project_id.to_owned(),
        alias: alias.to_owned(),
        owner: owner.to_owned(),
        drive_root,
    };
    projects.push(project.clone());

    let file_id = match projects_file {
        Some(id) => id,
        None => create_file(&http, token, PROJECTS_FILE, TSV_MIME, &app_folder).await?,
    };
    upload_text(
        &http,
        token,
        &file_id,
        TSV_MIME,
        serialize_projects(&projects),
    )
    .await?;
    Ok(project)
}

/// 프로젝트 ID는 Drive 폴더 이름과 쿼리 문자열에 들어가므로 안전한 문자만 허용한다.
pub fn validate_new_project<'a>(
    project_id: &'a str,
    alias: &'a str,
) -> Result<(&'a str, &'a str), DriveError> {
    let project_id = project_id.trim();
    let alias = alias.trim();
    if project_id.is_empty() || project_id.len() > MAX_PROJECT_ID_LEN {
        return Err(DriveError::InvalidInput(
            t!("drive_error.project_id_length", max = MAX_PROJECT_ID_LEN).into_owned(),
        ));
    }
    if !project_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(DriveError::InvalidInput(
            t!("drive_error.project_id_chars").into_owned(),
        ));
    }
    if alias.is_empty() || alias.chars().count() > MAX_ALIAS_CHARS {
        return Err(DriveError::InvalidInput(
            t!("drive_error.alias_length", max = MAX_ALIAS_CHARS).into_owned(),
        ));
    }
    if alias.chars().any(char::is_control) {
        return Err(DriveError::InvalidInput(
            t!("drive_error.alias_control_chars").into_owned(),
        ));
    }
    Ok((project_id, alias))
}

fn client() -> Result<reqwest::Client, DriveError> {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| DriveError::Internal(e.to_string()))
}

fn app_folder_query() -> String {
    format!(
        "name = '{APP_FOLDER}' and mimeType = '{FOLDER_MIME}' and 'root' in parents and trashed = false"
    )
}

fn projects_file_query(folder_id: &str) -> String {
    format!("name = '{PROJECTS_FILE}' and '{folder_id}' in parents and trashed = false")
}

#[derive(Deserialize)]
struct FileList {
    files: Vec<FileEntry>,
}

#[derive(Deserialize)]
struct FileEntry {
    id: String,
}

#[derive(Deserialize)]
struct FileName {
    name: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NewFile<'a> {
    name: &'a str,
    mime_type: &'a str,
    parents: [&'a str; 1],
}

// 같은 이름이 여럿이면 가장 먼저 만든 것을 쓴다. 중복 정리는 아직 하지 않는다.
async fn find_first(
    http: &reqwest::Client,
    token: &DriveToken,
    query: &str,
) -> Result<Option<String>, DriveError> {
    let response = http
        .get(FILES_URL)
        .bearer_auth(token.secret())
        .query(&[
            ("q", query),
            ("orderBy", "createdTime"),
            ("pageSize", "1"),
            ("fields", "files(id)"),
            ("spaces", "drive"),
        ])
        .send()
        .await
        .map_err(network)?;
    let list: FileList = check_status(response)?
        .json()
        .await
        .map_err(|e| DriveError::Internal(response_parse_error("files.list", e)))?;
    list.files
        .into_iter()
        .next()
        .map(|f| checked_id(f.id))
        .transpose()
}

// 이후 쿼리 문자열에 그대로 넣으므로 Drive ID 문자 집합만 허용한다.
fn checked_id(id: String) -> Result<String, DriveError> {
    if !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        Ok(id)
    } else {
        Err(DriveError::Internal(
            t!("drive_error.unexpected_file_id").into_owned(),
        ))
    }
}

async fn create_folder(
    http: &reqwest::Client,
    token: &DriveToken,
    name: &str,
    parent: &str,
) -> Result<String, DriveError> {
    create_file(http, token, name, FOLDER_MIME, parent).await
}

/// 메타데이터만으로 빈 파일이나 폴더를 만든다.
async fn create_file(
    http: &reqwest::Client,
    token: &DriveToken,
    name: &str,
    mime_type: &str,
    parent: &str,
) -> Result<String, DriveError> {
    let response = http
        .post(FILES_URL)
        .bearer_auth(token.secret())
        .query(&[("fields", "id")])
        .json(&NewFile {
            name,
            mime_type,
            parents: [parent],
        })
        .send()
        .await
        .map_err(network)?;
    let entry: FileEntry = check_status(response)?
        .json()
        .await
        .map_err(|e| DriveError::Internal(response_parse_error("files.create", e)))?;
    checked_id(entry.id)
}

async fn upload_text(
    http: &reqwest::Client,
    token: &DriveToken,
    file_id: &str,
    mime_type: &str,
    text: String,
) -> Result<(), DriveError> {
    let response = http
        .patch(format!("{UPLOAD_URL}/{file_id}"))
        .bearer_auth(token.secret())
        .query(&[("uploadType", "media"), ("fields", "id")])
        .header(
            reqwest::header::CONTENT_TYPE,
            format!("{mime_type}; charset=utf-8"),
        )
        .body(text)
        .send()
        .await
        .map_err(network)?;
    check_status(response)?;
    Ok(())
}

async fn download_text(
    http: &reqwest::Client,
    token: &DriveToken,
    file_id: &str,
) -> Result<String, DriveError> {
    let mut response = check_status(
        http.get(format!("{FILES_URL}/{file_id}"))
            .bearer_auth(token.secret())
            .query(&[("alt", "media")])
            .send()
            .await
            .map_err(network)?,
    )?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(network)? {
        if bytes.len() + chunk.len() > MAX_PROJECTS_BYTES {
            return Err(DriveError::InvalidProjects(
                t!("drive_error.projects_too_large").into_owned(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes)
        .map_err(|_| DriveError::InvalidProjects(t!("drive_error.projects_not_utf8").into_owned()))
}

fn check_status(response: reqwest::Response) -> Result<reqwest::Response, DriveError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let code = status.as_u16();
    Err(match code {
        401 => DriveError::Unauthorized,
        403 => DriveError::Forbidden(code),
        429 | 500..=599 => DriveError::Unavailable(code),
        _ => DriveError::Rejected(code),
    })
}

fn response_parse_error(api: &str, e: reqwest::Error) -> String {
    t!("drive_error.response_parse", api = api, reason = e).into_owned()
}

fn network(e: reqwest::Error) -> DriveError {
    DriveError::Network(e.without_url().to_string())
}

/// `projects.tsv`의 한 줄. 구 형식 파일에서 읽은 줄은 `id`가 없다.
#[derive(Debug, PartialEq, Eq)]
struct ProjectRow {
    id: Option<String>,
    alias: String,
    owner: String,
    drive_root: String,
}

impl ProjectRow {
    fn with_id(self, id: String) -> Project {
        Project {
            id,
            alias: self.alias,
            owner: self.owner,
            drive_root: self.drive_root,
        }
    }
}

fn parse_projects(text: &str) -> Result<Vec<ProjectRow>, DriveError> {
    let mut lines = text
        .strip_prefix('\u{feff}')
        .unwrap_or(text)
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty());

    let Some((_, header)) = lines.next() else {
        return Ok(Vec::new());
    };
    let legacy = if header.split('\t').map(str::trim).eq(PROJECTS_HEADER) {
        false
    } else if header.split('\t').map(str::trim).eq(LEGACY_PROJECTS_HEADER) {
        true
    } else {
        return Err(DriveError::InvalidProjects(
            t!(
                "drive_error.projects_bad_header",
                header = PROJECTS_HEADER.join(", ")
            )
            .into_owned(),
        ));
    };

    lines
        .map(|(index, line)| {
            let fields: Vec<&str> = line.split('\t').map(str::trim).collect();
            let row = |id: Option<&str>, alias: &str, owner: &str, drive_root: &str| ProjectRow {
                id: id.map(str::to_owned),
                alias: alias.to_owned(),
                owner: owner.to_owned(),
                drive_root: drive_root.to_owned(),
            };
            match (legacy, fields.as_slice()) {
                _ if fields.iter().any(|f| f.is_empty()) => None,
                (false, [id, alias, owner, drive_root]) => {
                    Some(row(Some(id), alias, owner, drive_root))
                }
                (true, [alias, owner, drive_root]) => Some(row(None, alias, owner, drive_root)),
                _ => None,
            }
            .ok_or_else(|| {
                DriveError::InvalidProjects(
                    t!("drive_error.projects_bad_row", line = index + 1).into_owned(),
                )
            })
        })
        .collect()
}

/// 구 형식 줄의 id를 프로젝트 폴더 이름으로 채운다. 폴더 이름은 생성 시 프로젝트 ID와 같다.
async fn resolve_ids(
    http: &reqwest::Client,
    token: &DriveToken,
    rows: Vec<ProjectRow>,
) -> Result<Vec<Project>, DriveError> {
    let mut projects = Vec::with_capacity(rows.len());
    for row in rows {
        let id = match row.id.clone() {
            Some(id) => id,
            None => folder_name(http, token, &row.drive_root).await?,
        };
        projects.push(row.with_id(id));
    }
    Ok(projects)
}

async fn folder_name(
    http: &reqwest::Client,
    token: &DriveToken,
    folder_id: &str,
) -> Result<String, DriveError> {
    let response = http
        .get(format!("{FILES_URL}/{}", checked_id(folder_id.to_owned())?))
        .bearer_auth(token.secret())
        .query(&[("fields", "name")])
        .send()
        .await
        .map_err(network)?;
    let file: FileName = check_status(response)?
        .json()
        .await
        .map_err(|e| DriveError::Internal(response_parse_error("files.get", e)))?;
    // 사용자가 Drive에서 폴더 이름을 바꿨을 수 있다. TSV를 깨뜨리는 값만 거른다.
    if file.name.is_empty() || file.name.chars().any(char::is_control) {
        return Err(DriveError::Internal(
            t!("drive_error.unexpected_folder_name").into_owned(),
        ));
    }
    Ok(file.name)
}

fn serialize_projects(projects: &[Project]) -> String {
    let mut text = PROJECTS_HEADER.join("\t");
    text.push('\n');
    for p in projects {
        text.push_str(&[p.id.as_str(), &p.alias, &p.owner, &p.drive_root].join("\t"));
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(id: &str, alias: &str, owner: &str, drive_root: &str) -> Project {
        Project {
            id: id.to_owned(),
            alias: alias.to_owned(),
            owner: owner.to_owned(),
            drive_root: drive_root.to_owned(),
        }
    }

    #[test]
    fn parses_projects() {
        let text = "id\talias\towner\tdrive_root\ndongmyo\t동묘 투어\talice@example.com\t1AbC\n\n";
        assert_eq!(
            parse_projects(text).unwrap(),
            vec![ProjectRow {
                id: Some("dongmyo".to_owned()),
                alias: "동묘 투어".to_owned(),
                owner: "alice@example.com".to_owned(),
                drive_root: "1AbC".to_owned(),
            }]
        );
    }

    #[test]
    fn parses_legacy_projects_without_id() {
        let text = "alias\towner\tdrive_root\n동묘 투어\talice@example.com\t1AbC\n";
        let rows = parse_projects(text).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, None);
        assert_eq!(rows[0].drive_root, "1AbC");
        assert!(matches!(
            parse_projects("alias\towner\tdrive_root\nid\ta\tb\tc\n"),
            Err(DriveError::InvalidProjects(_))
        ));
    }

    #[test]
    fn handles_bom_crlf_and_empty_file() {
        let text = "\u{feff}id\talias\towner\tdrive_root\r\ni\ta\tb\tc\r\n";
        assert_eq!(parse_projects(text).unwrap().len(), 1);
        assert!(parse_projects("").unwrap().is_empty());
        assert!(
            parse_projects("id\talias\towner\tdrive_root\n")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn rejects_bad_header_or_row() {
        for text in [
            "name\towner\tdrive_root\n",
            "id\talias\towner\tdrive_root\ni\ta\tb\n",
            "id\talias\towner\tdrive_root\ni\ta\t\tc\n",
        ] {
            assert!(
                matches!(parse_projects(text), Err(DriveError::InvalidProjects(_))),
                "{text:?}"
            );
        }
    }

    #[test]
    fn serialized_projects_round_trip() {
        let projects = vec![
            project("dongmyo", "동묘 투어", "alice@example.com", "1AbC"),
            project("hwanghak", "황학동", "bob@example.com", "2XyZ"),
        ];
        let parsed: Vec<Project> = parse_projects(&serialize_projects(&projects))
            .unwrap()
            .into_iter()
            .map(|row| {
                let id = row.id.clone().unwrap();
                row.with_id(id)
            })
            .collect();
        assert_eq!(parsed, projects);
        assert!(parse_projects(&serialize_projects(&[])).unwrap().is_empty());
    }

    #[test]
    fn validates_new_project() {
        assert_eq!(
            validate_new_project(" dongmyo-2026 ", " 동묘 투어 ").unwrap(),
            ("dongmyo-2026", "동묘 투어")
        );
        for (id, alias) in [
            ("", "a"),
            ("a b", "a"),
            ("a'b", "a"),
            ("동묘", "a"),
            (&"a".repeat(MAX_PROJECT_ID_LEN + 1), "a"),
            ("a", ""),
            ("a", "a\tb"),
            ("a", "a\nb"),
        ] {
            assert!(
                matches!(
                    validate_new_project(id, alias),
                    Err(DriveError::InvalidInput(_))
                ),
                "{id:?} {alias:?}"
            );
        }
    }

    // 키가 없으면 t!는 키 문자열을 그대로 돌려주므로 실제 문구로 풀리는지 확인한다.
    #[test]
    fn error_messages_resolve_from_locale() {
        assert_eq!(
            DriveError::Forbidden(403).to_string(),
            "Drive 접근이 거부됐어 (HTTP 403)"
        );
        assert_eq!(
            DriveError::InvalidProjects("x".to_owned()).to_string(),
            "projects.tsv 형식이 잘못됐어: x"
        );
    }
}
