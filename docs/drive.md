# Google Drive 경로 구조

앱 데이터는 사용자 Drive의 `jipark-map/` 폴더 아래에 둔다.

```
jipark-map/
├── projects.tsv
└── {project_name}/
    ├── {user_id}.sqlite
    └── {guid}.{png|jpeg|jpg|gif}
```

## `jipark-map/projects.tsv`

현재 사용자가 접근할 수 있는 프로젝트 목록. 소유한 프로젝트와 초대받은 프로젝트를 모두 담는다.

UTF-8 TSV이며 첫 줄은 헤더다.

| 컬럼 | 설명 |
| --- | --- |
| `alias` | 프로젝트 별명. 화면 표시용 |
| `owner` | 프로젝트 소유자 |
| `drive_root` | 프로젝트 루트 폴더의 Drive 폴더 ID |

```
alias	owner	drive_root
동묘 투어	alice@example.com	1AbCdEfGhIjKlMnOpQrStUvWxYz
```

`drive_root`는 경로가 아닌 폴더 ID로 저장한다. 초대받은 프로젝트 폴더는 내 `jipark-map/` 아래에 있지 않고, 폴더 이름이 바뀌거나 같은 이름의 폴더가 생겨도 ID는 그대로이기 때문이다.

## `jipark-map/{project_name}/{user_id}.sqlite`

프로젝트 멤버별 매물 목록 DB. 각 사용자는 자기 파일만 수정하고 다른 멤버의 파일은 읽기 전용으로 내려받는다. 조회 시에는 프로젝트 안의 모든 `{user_id}.sqlite`를 인메모리 통합 뷰로 합친다.

매물마다 아래 정보를 저장한다.

- 추가 일시
- 추가한 사람 (파일 소유자 `user_id`와 같다)
- 위치
- 가격
- 비고

## `jipark-map/{project_name}/{guid}.{ext}`

매물 사진. 파일명은 GUID, 확장자는 `png`, `jpeg`, `jpg`, `gif` 중 하나. 사진을 올린 사용자의 DB에 참조를 저장한다.
