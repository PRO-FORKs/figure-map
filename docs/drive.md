# Google Drive 경로 구조

앱 데이터는 사용자 Drive의 `jipark-map/` 폴더 아래에 둔다.

```
jipark-map/
├── projects.csv
└── {project_name}/
    ├── {user_id}.sqlite
    └── {guid}.{png|jpeg|jpg|gif}
```

## `jipark-map/projects.csv`

현재 사용자가 접근할 수 있는 프로젝트 목록. 소유한 프로젝트와 초대받은 프로젝트를 모두 담는다.

UTF-8 CSV이며 첫 줄은 헤더다.

| 컬럼 | 설명 |
| --- | --- |
| `id` | 프로젝트 ID. 생성 시 프로젝트 폴더 이름과 같다 |
| `alias` | 프로젝트 별명. 화면 표시용 |
| `owner` | 프로젝트 소유자 |
| `drive_root` | 프로젝트 루트 폴더의 Drive 폴더 ID |

```
id	alias	owner	drive_root
dongmyo	동묘 투어	alice@example.com	1AbCdEfGhIjKlMnOpQrStUvWxYz
```

`id` 컬럼이 없는 구 형식(`alias	owner	drive_root`)도 읽는다. 이때 `id`는 `drive_root` 폴더의 현재 이름으로 채우고, 다음에 목록을 저장할 때 새 형식으로 다시 쓴다.

`drive_root`는 경로가 아닌 폴더 ID로 저장한다. 초대받은 프로젝트 폴더는 내 `jipark-map/` 아래에 있지 않고, 폴더 이름이 바뀌거나 같은 이름의 폴더가 생겨도 ID는 그대로이기 때문이다.

## `jipark-map/{project_name}/{user_id}.sqlite`

프로젝트 멤버별 매물 목록 DB. 각 사용자는 자기 파일만 수정하고 다른 멤버의 파일은 읽기 전용으로 내려받는다. 조회 시에는 프로젝트 안의 모든 `{user_id}.sqlite`를 인메모리 통합 뷰로 합친다.

매물마다 아래 정보를 저장한다.

- 추가 일시
- 추가한 사람 (파일 소유자 `user_id`와 같다)
- 위치
- 가격
- 비고

로컬 사본은 `~/Library/Application Support/dev.sharedmap.app/projects/{drive_root}/{user_id}.sqlite`(macOS)에 둔다. 업로드 순서는 아래 "업로드" 절을 따른다.

스키마 버전은 `PRAGMA user_version`으로 관리하며 현재 3이다(2에서 `photo`, 3에서 `sync_state`의 Drive 업로드 컬럼 추가). 앱보다 새 버전의 파일은 열지 않는다.

| 테이블 | 컬럼 |
| --- | --- |
| `listing` | `id`(UUIDv7), `owner`, `lat`, `lng`, `price_amount`(통화 최소 단위 정수), `price_currency`(ISO 4217, 현재 `KRW`·`JPY`), `note`, `created_at_ms`·`updated_at_ms`(UTC Unix ms), `deleted_at_ms`(tombstone), `local_generation` |
| `photo` | `id`(UUIDv7), `listing_id`, `file_name`(`{id}.{ext}`), `mime`, `size_bytes`, `sha256`, `width`·`height`(방향 보정 후), `position`, `drive_file_id`, `upload_status`(`pending`·`uploaded`), `created_at_ms`, `deleted_at_ms`, `local_generation` |
| `sync_state` | 한 행. `local_generation`은 로컬 변경마다 1씩 오르고 `uploaded_generation`은 마지막으로 올린 세대다. `drive_file_id`·`remote_version`은 올린 `{user_id}.sqlite`의 Drive 파일 ID와 버전, `uploaded_at_ms`는 마지막 업로드 시각(UTC Unix ms) |

## `jipark-map/{project_name}/{guid}.{ext}`

매물 사진. 파일명은 GUID, 확장자는 `png`, `jpeg`, `jpg`, `gif` 중 하나. 사진을 올린 사용자의 DB에 참조를 저장한다. 앱은 JPEG를 `jpg`로 저장한다.

로컬에서는 업로드 전까지 `projects/{drive_root}/photos/{guid}.{ext}`에 보관한다. 사진 파일을 먼저 쓰고 DB 트랜잭션으로 참조를 기록하며, 트랜잭션이 실패하면 방금 쓴 파일을 지운다.

첨부 시 검증과 메타데이터 처리:

- 20MB 이하, 가로·세로 12000px 이하, 매물당 10장까지. 실제로 디코딩되는 PNG·JPEG·GIF만 받는다.
- 픽셀은 다시 인코딩하지 않는다.
- JPEG: EXIF의 GPS IFD 항목과 값을 0으로 덮고 방향 등 나머지 EXIF는 남긴다. EXIF 구조를 해석할 수 없으면 EXIF 세그먼트 전체를 뺀다. XMP 세그먼트는 뺀다.
- PNG: `eXIf` 청크와 XMP `iTXt` 청크를 뺀다. 그래서 PNG의 EXIF 방향 정보는 사라진다.
- GIF: 그대로 둔다.

## 업로드

매물을 추가하면 로컬 DB에 트랜잭션으로 저장한 뒤 바로 업로드를 시도한다. 프로젝트를 열 때와 "지금 올리기" 버튼으로도 시도한다. 주기적 폴링은 하지 않는다.

1. `upload_status = 'pending'`인 사진을 `{guid}.{ext}`로 프로젝트 폴더에 올리고 `drive_file_id`를 기록한다. 같은 이름 파일이 이미 있으면 다시 올리지 않고 그 파일을 쓴다(업로드 직후 종료 대비).
2. SQLite backup API로 전체 페이지를 한 단계에 복사해 사본을 만든다. 사본은 `journal_mode = DELETE`로 바꿔 다른 멤버가 읽기 전용으로 열 수 있게 한다. 사본에 아직 올리지 않은 사진이 있으면(그사이 추가된 매물) 1로 돌아간다.
3. 사본으로 `{user_id}.sqlite`를 교체한다. 저장한 Drive 파일 ID가 없거나 404면 이름으로 찾고, 없으면 새로 만든다.
4. 사본이 담은 세대까지만 `uploaded_generation`으로 기록한다. 업로드 중 생긴 변경은 대기로 남아 다음 업로드에 반영된다.

- 업로드 대기 = `local_generation > uploaded_generation` 또는 대기 사진이 있음.
- 같은 DB의 업로드는 앱 안에서 한 번에 하나만 돈다. 진행 중에 다시 요청되면 끝난 뒤 한 번 더 돈다.
- 429·5xx·네트워크 오류는 2초, 8초 뒤 다시 시도한다. 그래도 실패하거나 다른 오류면 로컬 기록은 그대로 두고 화면에 오류를 보여준다.
- 사진 원본 로컬 사본은 업로드 뒤에도 지우지 않는다. 정리 정책은 아직 정하지 않았다.
