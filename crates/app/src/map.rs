//! Google Maps 어댑터. webview 안에서 Maps JavaScript API를 띄운다.
//! 플랫폼별 Maps SDK 연결 여부는 아직 정하지 않았다(AGENTS.md 미확정 사항).

use dioxus::document::Eval;
use dioxus::prelude::*;
use rust_i18n::t;

use crate::location::{self, LocationError};
use serde::{Deserialize, Serialize};

const API_KEY: Option<&str> = option_env!("GOOGLE_MAPS_API_KEY");
const MAP_JS: &str = include_str!("map.js");
const ELEMENT_ID: &str = "project-map";
// 위치를 못 얻었을 때 보여줄 서울시청.
const FALLBACK: LatLng = LatLng {
    lat: 37.5665,
    lng: 126.978,
};

#[derive(Clone, Copy, Serialize)]
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
}

#[derive(Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MapEvent {
    Loaded,
    Located,
    LocationDenied,
    LocationFailed,
    LoadFailed,
    AuthFailed,
}

#[derive(Clone, Copy, PartialEq)]
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
#[component]
pub fn CurrentLocationMap() -> Element {
    let Some(key) = API_KEY else {
        return rsx! {
            p { {t!("map.no_api_key").to_string()} }
        };
    };
    let mut status = use_signal(|| MapStatus::Loading);
    let mut map = use_signal(|| None::<Eval>);

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
                if event == MapEvent::Loaded {
                    map.set(Some(eval));
                    locate(eval, status);
                } else {
                    status.set(MapStatus::Event(event));
                }
            }
        });
    };

    let message = match status() {
        MapStatus::Loading => Some(t!("map.loading")),
        MapStatus::Locating => Some(t!("map.locating")),
        MapStatus::Event(MapEvent::Located | MapEvent::Loaded) => None,
        MapStatus::Event(MapEvent::LocationDenied) => Some(t!("map.location_denied")),
        MapStatus::Event(MapEvent::LocationFailed) => Some(t!("map.location_failed")),
        MapStatus::Event(MapEvent::LoadFailed) => Some(t!("map.load_failed")),
        MapStatus::Event(MapEvent::AuthFailed) => Some(t!("map.auth_failed")),
    };

    rsx! {
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
        }
        div {
            id: ELEMENT_ID,
            style: "width: 100%; height: 480px;",
            onmounted: start,
        }
    }
}
