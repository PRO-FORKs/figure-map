//! Google Maps 어댑터. webview 안에서 Maps JavaScript API를 띄운다.
//! 플랫폼별 Maps SDK 연결 여부는 아직 정하지 않았다(AGENTS.md 미확정 사항).

use std::sync::Arc;

use dioxus::document::Eval;
use dioxus::prelude::*;
use rust_i18n::t;

use crate::auth::DriveToken;
use crate::listing::{self, Listing, NewListing, OwnFiles, PhotoRef, SyncStatus};
use crate::location::{self, LocationError};
use crate::photo::{self, PreparedPhoto};
use crate::sync;
use serde::{Deserialize, Serialize};

const API_KEY: Option<&str> = option_env!("GOOGLE_MAPS_API_KEY");
const MAP_JS: &str = include_str!("map.js");
const ELEMENT_ID: &str = "project-map";
// 위치를 못 얻었을 때 보여줄 서울시청.
const FALLBACK: LatLng = LatLng {
    lat: 37.5665,
    lng: 126.978,
};

#[derive(Clone, Copy, PartialEq, Serialize)]
struct LatLng {
    lat: f64,
    lng: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MapInit<'a> {
    key: &'a str,
    element_id: &'a str,
    fallback: LatLng,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum MapCommand {
    ShowPosition { lat: f64, lng: f64, accuracy: f64 },
    LocateInBrowser,
    ShowListings { listings: Vec<ListingMarker> },
    ClearDraft,
}

#[derive(Serialize)]
struct ListingMarker {
    id: String,
    lat: f64,
    lng: f64,
    title: String,
}

impl From<&Listing> for ListingMarker {
    fn from(listing: &Listing) -> Self {
        let price = listing
            .price
            .as_ref()
            .map(|p| t!("listing.price", amount = p.amount, currency = p.currency).to_string());
        let title = [price, Some(listing.note.clone()).filter(|n| !n.is_empty())]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
        ListingMarker {
            id: listing.id.clone(),
            lat: listing.lat,
            lng: listing.lng,
            title,
        }
    }
}

#[derive(Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum MapEvent {
    Loaded,
    Located,
    LocationDenied,
    LocationFailed,
    LoadFailed,
    AuthFailed,
    Clicked { lat: f64, lng: f64 },
    ListingClicked { id: String },
}

#[derive(Clone, PartialEq)]
enum MapStatus {
    Loading,
    Locating,
    Event(MapEvent),
}

/// 현재 위치를 찾아 지도 중심을 옮긴다. 권한이 정해지지 않았으면 이때 요청한다.
fn locate(eval: Eval, mut status: Signal<MapStatus>) {
    status.set(MapStatus::Locating);
    spawn(async move {
        let event = match location::current_position().await {
            Ok(position) => {
                let command = MapCommand::ShowPosition {
                    lat: position.lat,
                    lng: position.lng,
                    accuracy: position.accuracy_m,
                };
                match eval.send(command) {
                    Ok(()) => MapEvent::Located,
                    Err(_) => MapEvent::LocationFailed,
                }
            }
            // 결과는 map.js가 이벤트로 돌려준다.
            Err(LocationError::Unsupported) => match eval.send(MapCommand::LocateInBrowser) {
                Ok(()) => return,
                Err(_) => MapEvent::LocationFailed,
            },
            Err(LocationError::Denied) => MapEvent::LocationDenied,
            Err(LocationError::Unavailable) => MapEvent::LocationFailed,
        };
        status.set(MapStatus::Event(event));
    });
}

/// 현재 위치를 중심으로 지도를 보여준다. 위치를 못 얻으면 기본 위치를 보여준다.
/// 지도를 누르면 그 위치에 매물을 추가하는 입력란을 연다.
#[component]
pub fn CurrentLocationMap(project_root: String, owner: String, token: DriveToken) -> Element {
    let Some(key) = API_KEY else {
        return rsx! {
            p { {t!("map.no_api_key").to_string()} }
        };
    };
    let mut status = use_signal(|| MapStatus::Loading);
    let mut map = use_signal(|| None::<Eval>);
    let mut draft = use_signal(|| None::<LatLng>);
    let mut selected = use_signal(|| None::<String>);
    let mut listings = use_resource({
        let (project_root, owner) = (project_root.clone(), owner.clone());
        move || {
            let (project_root, owner) = (project_root.clone(), owner.clone());
            async move { listing::list_own(&project_root, &owner).await }
        }
    });

    let mut sync_runs = use_signal(|| 0usize);
    let mut sync_error = use_signal(|| None::<String>);
    let mut sync_status = use_resource({
        let (project_root, owner) = (project_root.clone(), owner.clone());
        move || {
            let (project_root, owner) = (project_root.clone(), owner.clone());
            async move { listing::sync_status(&OwnFiles::locate(&project_root, &owner)?).await }
        }
    });
    let upload = use_callback({
        let (project_root, owner) = (project_root.clone(), owner.clone());
        move |()| {
            let (project_root, owner, token) = (project_root.clone(), owner.clone(), token.clone());
            sync_runs += 1;
            sync_error.set(None);
            spawn(async move {
                let result = sync::upload_own(&token, &project_root, &owner).await;
                sync_runs -= 1;
                if let Err(e) = result {
                    sync_error.set(Some(e.to_string()));
                }
                sync_status.restart();
            });
        }
    });
    // 프로젝트를 열 때 지난 실행에서 못 올린 변경을 이어서 올린다.
    use_hook(move || upload.call(()));

    use_effect(move || {
        let (Some(eval), Some(Ok(list))) = (map(), &*listings.read()) else {
            return;
        };
        let listings = list.iter().map(ListingMarker::from).collect();
        // 지도 스크립트가 끝난 뒤라면 다시 열 때 새로 그리므로 무시한다.
        let _ = eval.send(MapCommand::ShowListings { listings });
    });

    let start = move |_| {
        spawn(async move {
            let mut eval = document::eval(MAP_JS);
            let init = MapInit {
                key,
                element_id: ELEMENT_ID,
                fallback: FALLBACK,
            };
            if eval.send(init).is_err() {
                status.set(MapStatus::Event(MapEvent::LoadFailed));
                return;
            }
            while let Ok(event) = eval.recv::<MapEvent>().await {
                match event {
                    MapEvent::Loaded => {
                        map.set(Some(eval));
                        locate(eval, status);
                    }
                    MapEvent::Clicked { lat, lng } => draft.set(Some(LatLng { lat, lng })),
                    MapEvent::ListingClicked { id } => selected.set(Some(id)),
                    event => status.set(MapStatus::Event(event)),
                }
            }
        });
    };

    let message = match status() {
        MapStatus::Loading => Some(t!("map.loading")),
        MapStatus::Locating => Some(t!("map.locating")),
        MapStatus::Event(
            MapEvent::Located
            | MapEvent::Loaded
            | MapEvent::Clicked { .. }
            | MapEvent::ListingClicked { .. },
        ) => None,
        MapStatus::Event(MapEvent::LocationDenied) => Some(t!("map.location_denied")),
        MapStatus::Event(MapEvent::LocationFailed) => Some(t!("map.location_failed")),
        MapStatus::Event(MapEvent::LoadFailed) => Some(t!("map.load_failed")),
        MapStatus::Event(MapEvent::AuthFailed) => Some(t!("map.auth_failed")),
    };

    rsx! {
        SyncPanel {
            status: sync_status.read().as_ref().map(|r| r.as_ref().cloned().map_err(|e| e.to_string())),
            running: sync_runs() > 0,
            error: sync_error(),
            on_upload: move |_| upload.call(()),
        }
        if let Some(message) = message {
            p { {message.to_string()} }
        }
        if let Some(eval) = map() {
            button {
                r#type: "button",
                disabled: status() == MapStatus::Locating,
                onclick: move |_| locate(eval, status),
                {t!("map.my_location").to_string()}
            }
            if let Some(position) = draft() {
                AddListingForm {
                    // 다른 위치를 누르면 입력 중인 내용은 유지하고 위치만 바꾼다.
                    position,
                    project_root,
                    owner,
                    on_close: move |saved: bool| {
                        draft.set(None);
                        let _ = eval.send(MapCommand::ClearDraft);
                        if saved {
                            listings.restart();
                            sync_status.restart();
                            upload.call(());
                        }
                    },
                }
            }
        }
        match &*listings.read() {
            Some(Err(e)) => rsx! {
                p { {t!("listing.load_failed", message = e).to_string()} }
                button { onclick: move |_| listings.restart(), {t!("common.retry").to_string()} }
            },
            Some(Ok(list)) if list.is_empty() && map().is_some() => rsx! {
                p { {t!("listing.tap_to_add").to_string()} }
            },
            Some(Ok(list)) => match selected().and_then(|id| list.iter().find(|l| l.id == id)) {
                Some(listing) => rsx! {
                    ListingDetail {
                        key: "{listing.id}",
                        listing: listing.clone(),
                        on_close: move |_| selected.set(None),
                    }
                },
                None => rsx! {},
            },
            _ => rsx! {},
        }
        div {
            id: ELEMENT_ID,
            style: "width: 100%; height: 480px;",
            onmounted: start,
        }
    }
}

/// 로컬 저장, 업로드 대기, Drive 저장 완료, 업로드 실패를 구분해 보여준다.
#[component]
fn SyncPanel(
    status: Option<Result<SyncStatus, String>>,
    running: bool,
    error: Option<String>,
    on_upload: EventHandler<()>,
) -> Element {
    let summary = match &status {
        None => None,
        Some(Err(e)) => Some(t!("sync.status_failed", message = e).to_string()),
        Some(Ok(s)) if s.pending && s.pending_photos > 0 => {
            Some(t!("sync.pending_with_photos", count = s.pending_photos).to_string())
        }
        Some(Ok(s)) if s.pending => Some(t!("sync.pending").to_string()),
        Some(Ok(SyncStatus {
            uploaded_at_ms: Some(at),
            ..
        })) => Some(t!("sync.uploaded", ago = ago(*at)).to_string()),
        Some(Ok(_)) => None,
    };
    rsx! {
        section {
            if running {
                p { {t!("sync.running").to_string()} }
            } else if let Some(summary) = summary {
                p { {summary} }
            }
            if let Some(message) = error {
                p { {t!("sync.failed", message = message).to_string()} }
            }
            button {
                r#type: "button",
                disabled: running,
                onclick: move |_| on_upload.call(()),
                {t!("sync.upload_now").to_string()}
            }
        }
    }
}

/// 시간대 정보 없이도 현지 시간과 어긋나지 않게 지난 시간으로 보여준다.
fn ago(at_ms: i64) -> String {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(at_ms);
    let minutes = (now_ms - at_ms).max(0) / 60_000;
    match minutes {
        0 => t!("sync.ago_now").to_string(),
        1..60 => t!("sync.ago_minutes", count = minutes).to_string(),
        60..1440 => t!("sync.ago_hours", count = minutes / 60).to_string(),
        _ => t!("sync.ago_days", count = minutes / 1440).to_string(),
    }
}

#[component]
fn AddListingForm(
    position: LatLng,
    project_root: String,
    owner: String,
    on_close: EventHandler<bool>,
) -> Element {
    let mut price = use_signal(String::new);
    let mut currency = use_signal(|| listing::CURRENCIES[0].to_owned());
    let mut note = use_signal(String::new);
    let mut photos = use_signal(Vec::<Arc<PreparedPhoto>>::new);
    let mut processing = use_signal(|| 0usize);
    let mut saving = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);

    let attach = move |e: FormEvent| {
        error.set(None);
        for file in e.files() {
            if photos.read().len() + processing() >= photo::MAX_PER_LISTING {
                error.set(Some(
                    t!("photo_error.too_many", max = photo::MAX_PER_LISTING).to_string(),
                ));
                break;
            }
            // 읽기 전에 크기를 걸러 큰 파일을 메모리에 올리지 않는다.
            if file.size() > photo::MAX_BYTES {
                let reason = photo::PhotoError::TooLarge.to_string();
                error.set(Some(
                    t!(
                        "photo_error.attach_failed",
                        name = file.name(),
                        reason = reason
                    )
                    .to_string(),
                ));
                continue;
            }
            processing += 1;
            spawn(async move {
                let result = match file.read_bytes().await {
                    Ok(bytes) => photo::prepare_async(bytes.to_vec()).await,
                    Err(e) => Err(photo::PhotoError::ReadFailed(e.to_string())),
                };
                processing -= 1;
                match result {
                    Ok(prepared) => photos.push(prepared),
                    Err(e) => error.set(Some(
                        t!(
                            "photo_error.attach_failed",
                            name = file.name(),
                            reason = e.to_string()
                        )
                        .to_string(),
                    )),
                }
            });
        }
    };

    let submit = move |e: FormEvent| {
        e.prevent_default();
        if saving() || processing() > 0 {
            return;
        }
        let new = match listing::parse_price(&price.read(), &currency.read()) {
            Ok(price) => NewListing {
                lat: position.lat,
                lng: position.lng,
                price,
                note: note.read().trim().to_owned(),
                photos: photos(),
            },
            Err(e) => {
                error.set(Some(e.to_string()));
                return;
            }
        };
        if let Err(e) = listing::validate(&new) {
            error.set(Some(e.to_string()));
            return;
        }
        saving.set(true);
        error.set(None);
        let (project_root, owner) = (project_root.clone(), owner.clone());
        spawn(async move {
            let result = listing::add(&project_root, &owner, new).await;
            saving.set(false);
            match result {
                Ok(_) => on_close.call(true),
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };

    rsx! {
        form { onsubmit: submit,
            p {
                {t!("listing.position", lat = format!("{:.5}", position.lat), lng = format!("{:.5}", position.lng)).to_string()}
            }
            label {
                {t!("listing.price_label").to_string()}
                input {
                    value: "{price}",
                    inputmode: "numeric",
                    placeholder: t!("listing.price_placeholder").to_string(),
                    disabled: saving(),
                    oninput: move |e| price.set(e.value()),
                }
            }
            select {
                value: "{currency}",
                disabled: saving(),
                onchange: move |e| currency.set(e.value()),
                for code in listing::CURRENCIES {
                    option { key: "{code}", value: code, selected: currency() == code, {code} }
                }
            }
            label {
                {t!("listing.note_label").to_string()}
                textarea {
                    value: "{note}",
                    placeholder: t!("listing.note_placeholder").to_string(),
                    disabled: saving(),
                    oninput: move |e| note.set(e.value()),
                }
            }
            label {
                {t!("photo.attach_label").to_string()}
                input {
                    r#type: "file",
                    accept: photo::ACCEPT,
                    multiple: true,
                    disabled: saving() || photos.read().len() >= photo::MAX_PER_LISTING,
                    onchange: attach,
                }
            }
            if processing() > 0 {
                p { {t!("photo.processing", count = processing()).to_string()} }
            }
            ul {
                for (index, prepared) in photos.read().iter().enumerate() {
                    li { key: "{prepared.id}",
                        img {
                            src: "{prepared.thumbnail}",
                            alt: t!("photo.alt", index = index + 1).to_string(),
                            style: "max-width: 96px; max-height: 96px;",
                        }
                        button {
                            r#type: "button",
                            disabled: saving(),
                            onclick: move |_| {
                                photos.remove(index);
                            },
                            {t!("photo.remove").to_string()}
                        }
                    }
                }
            }
            button {
                r#type: "submit",
                disabled: saving() || processing() > 0,
                {t!("listing.submit").to_string()}
            }
            button {
                r#type: "button",
                disabled: saving(),
                onclick: move |_| on_close.call(false),
                {t!("common.cancel").to_string()}
            }
            if saving() {
                p { {t!("listing.saving").to_string()} }
            }
            if let Some(message) = error() {
                p { {t!("listing.save_failed", message = message).to_string()} }
            }
        }
    }
}

#[component]
fn ListingDetail(listing: Listing, on_close: EventHandler<()>) -> Element {
    let price = listing
        .price
        .as_ref()
        .map(|p| t!("listing.price", amount = p.amount, currency = p.currency).to_string());
    rsx! {
        section {
            if let Some(price) = price {
                p { {price} }
            }
            if !listing.note.is_empty() {
                p { style: "white-space: pre-wrap;", {listing.note.clone()} }
            }
            ul {
                for (index, photo) in listing.photos.iter().enumerate() {
                    li { key: "{photo.id}",
                        PhotoThumbnail { photo: photo.clone(), index }
                    }
                }
            }
            button { r#type: "button", onclick: move |_| on_close.call(()), {t!("common.close").to_string()} }
        }
    }
}

/// 상세를 열 때만 파일을 읽어 썸네일을 만든다.
#[component]
fn PhotoThumbnail(photo: PhotoRef, index: usize) -> Element {
    let thumbnail = use_resource(move || {
        let photo = photo.clone();
        async move { photo::thumbnail_from_file(photo.local_path, photo.kind).await }
    });
    match &*thumbnail.read() {
        None => rsx! {
            p { {t!("photo.loading").to_string()} }
        },
        Some(Ok(src)) => rsx! {
            img {
                src: "{src}",
                alt: t!("photo.alt", index = index + 1).to_string(),
                style: "max-width: 160px; max-height: 160px;",
            }
        },
        Some(Err(e)) => rsx! {
            p { {t!("photo.load_failed", message = e.to_string()).to_string()} }
        },
    }
}
